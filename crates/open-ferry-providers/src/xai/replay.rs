// Ported from CLIProxyAPI internal/runtime/executor/xai_reasoning_replay.go
// (applyXAIReasoningReplayCacheRequired, xaiReasoningReplayScopeFromRequest,
// xaiReasoningReplayIsolateSessionKey, filterXAIReasoningReplayItemsForInput,
// cacheXAIReasoningReplayFromCompleted,
// clearXAIReasoningReplayAfterCompaction and their helpers), and from
// insertCodexReasoningReplayItems in codex_executor_reasoning.go (v8.0.20,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reasoning replay for Grok.
//!
//! Clients drop the reasoning items of Grok's answers, and the assistant
//! text and tool calls they keep come back in another shape. So, as upstream
//! does, the reasoning, assistant message and tool-call items of each
//! completed response are kept in [`crate::codex::xai_replay_cache`] under
//! the request's model and session, and put back into the session's next
//! request, where they belong in its input. Only Claude and OpenAI Responses
//! clients are replayed. A client on the Responses WebSocket that sends a
//! `previous_response_id` is left alone: xAI already holds its state, and
//! replaying it too would repeat the turn.
//!
//! The session is the one the client names, found as the Codex replay finds
//! it ([`crate::codex::replay::session_key`]): Claude Code's session and
//! agent, else the Responses WebSocket execution session, else a
//! `prompt_cache_key`, window or turn metadata, else the client's session
//! headers. A request that names none isn't replayed or saved. A name the
//! client chose is namespaced by a hash of the proxy key it called with, so
//! two callers can't share reasoning or assistant text by choosing the same
//! name; without a proxy key such a name isn't replayed either. An
//! `execution:` session is the server's and is used as it is.
//!
//! What a replay puts back is chosen against the input: reasoning whose
//! `encrypted_content` the input already holds is left out; so are the
//! assistant message, when the input's last assistant message says the same,
//! and every tool call, unless the input has its result but not the call. If
//! the input's last assistant message isn't the one saved, the session has
//! moved on, and nothing is put back. The executor calls the three hooks
//! where upstream does: [`apply`] once the tools are clamped,
//! [`cache_completed`] for a `response.completed` event, and
//! [`clear_after_compaction`] after a successful compact call, since the
//! saved items name a history that no longer exists. A failed compact call
//! keeps them.
//!
//! Deviations from upstream:
//! - No session is made from the client's proxy API key: upstream falls back
//!   to `prompt-cache:` and a UUIDv5 of the key for OpenAI Chat Completions
//!   clients, which xAI replay never serves, so upstream never reaches that
//!   fallback either. The key does still namespace a session the client
//!   named, as upstream's does; it is only hashed in memory here, and never
//!   sent to xAI.
//! - Home mode's shared KV store isn't ported, so reading, writing or
//!   deleting an entry can't fail a request, and upstream's hook that tests
//!   a failing read has nothing to hook.
//! - The caller's proxy key comes from the request's observation, which the
//!   server fills in only when the proxy has keys of its own; upstream reads
//!   the same key from its gin context.
//! - [`apply`] never fails (upstream's `Required` form only returns an
//!   error for a Home read that it then ignores), but keeps upstream's
//!   `Result`, so the call stays as it is wired in.
//! - A payload that isn't valid JSON, or a `response.completed` event that
//!   isn't, has no `previous_response_id` or output where gjson reads what it
//!   can.

use std::collections::HashSet;

use open_ferry_core::exec::{ExecError, Format, Options, Request};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codex::replay::{
    align_call_ids, comparable_call_ids, insert_index, keep_tool_call, session_key, tool_call_keys,
};
use crate::codex::request::{base_model, format_is};
use crate::codex::xai_replay_cache::{Store, XaiReplayCache};
use crate::json::{eq_fold, get, str_at};

/// The session whose reasoning is replayed (`xaiReasoningReplayScope`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The model, without a thinking suffix.
    pub(super) model_name: String,
    /// The session the client named, namespaced by its caller.
    pub(super) session_key: String,
}

impl Scope {
    pub(super) fn valid(&self) -> bool {
        !self.model_name.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

/// Puts the session's kept reasoning back into `body`'s input
/// (`applyXAIReasoningReplayCacheRequired`).
pub(crate) fn apply(
    body: &mut Value,
    request: &Request,
    options: &Options,
) -> Result<Scope, ExecError> {
    let scope = scope_from_request(request, options, body);
    if !scope.valid() {
        return Ok(scope);
    }
    let Some(items) = XaiReplayCache::global().get(&scope.model_name, &scope.session_key) else {
        return Ok(scope);
    };
    let items = filter_items_for_input(body, &items);
    if !items.is_empty() {
        insert_items(body, &items);
    }
    Ok(scope)
}

/// Keeps the reasoning, assistant message and tool calls of a
/// `response.completed` event's response, or forgets what an earlier turn
/// kept if this one has nothing to replay
/// (`cacheXAIReasoningReplayFromCompleted`).
pub(crate) fn cache_completed(scope: &Scope, completed: &[u8]) {
    if !scope.valid() {
        return;
    }
    let completed: Value = serde_json::from_slice(completed).unwrap_or(Value::Null);
    let Some(Value::Array(output)) = get(&completed, "response.output") else {
        return;
    };
    let items: Vec<&Value> = output
        .iter()
        .filter(|item| {
            matches!(
                str_at(item, "type").trim(),
                "reasoning" | "message" | "function_call" | "custom_tool_call"
            )
        })
        .collect();
    let cache = XaiReplayCache::global();
    match cache.store(&scope.model_name, &scope.session_key, &items) {
        // A successful turn without anything to replay mustn't leave an
        // earlier turn's encrypted state to be put back later.
        Store::NoReplayableState => cache.delete(&scope.model_name, &scope.session_key),
        Store::Stored | Store::InvalidArgs => {}
    }
}

/// Forgets the session's kept reasoning once its history is compacted
/// (`clearXAIReasoningReplayAfterCompaction`).
pub(crate) fn clear_after_compaction(scope: &Scope) {
    if scope.valid() {
        XaiReplayCache::global().delete(&scope.model_name, &scope.session_key);
    }
}

/// `xaiReasoningReplayScopeFromRequest`: none unless the client speaks a
/// format replay serves, and isn't carrying its state in a WebSocket.
fn scope_from_request(request: &Request, options: &Options, body: &Value) -> Scope {
    if !enabled_for_source(&options.source_format) {
        return Scope::default();
    }
    // End-to-end WebSocket requests use upstream previous_response_id state.
    // Replaying encrypted reasoning as input as well would duplicate the turn.
    if options.downstream_websocket && has_previous_response_id(&request.payload) {
        return Scope::default();
    }
    let session_key = isolate_session_key(options, &session_key(request, options, body));
    Scope {
        model_name: base_model(&request.model).to_owned(),
        session_key,
    }
}

/// `xaiReasoningReplayEnabledForSource`.
fn enabled_for_source(format: &Format) -> bool {
    format_is(format, &Format::CLAUDE) || format_is(format, &Format::OPENAI_RESPONSE)
}

/// Whether the payload has a `previous_response_id` that isn't blank.
fn has_previous_response_id(payload: &[u8]) -> bool {
    let payload: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    !str_at(&payload, "previous_response_id").trim().is_empty()
}

/// `xaiReasoningReplayIsolateSessionKey`: namespaces a session key the
/// client chose by the proxy key it called with, so two callers can't share
/// encrypted reasoning or assistant text by reusing a `prompt_cache_key`,
/// window or session header. An `execution:` key is the server's and keeps
/// its form. A client's key without a caller key is disabled rather than
/// shared by everyone.
fn isolate_session_key(options: &Options, session_key: &str) -> String {
    let session_key = session_key.trim();
    if session_key.is_empty() {
        return String::new();
    }
    if session_key.starts_with("execution:") {
        return session_key.to_owned();
    }
    let api_key = options
        .observation
        .as_deref()
        .and_then(|observation| observation.context().client_key())
        .unwrap_or_default()
        .trim();
    if api_key.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(api_key.as_bytes());
    let caller: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("caller:{caller}:{session_key}")
}

/// What the replay puts back (`filterXAIReasoningReplayItemsForInput`): the
/// kept items that the input lacks, none at all if the input's last
/// assistant message isn't the one that was kept.
fn filter_items_for_input<'a>(body: &Value, items: &'a [Value]) -> Vec<&'a Value> {
    let Some(Value::Array(input)) = get(body, "input") else {
        return Vec::new();
    };

    let last_assistant = last_assistant_message(input);
    let cached_assistant = cached_assistant_message(items);
    let mut assistant_matches = false;
    if let (Some(last), Some(cached)) = (last_assistant, cached_assistant) {
        assistant_matches = assistant_content_equal(get(last, "content"), get(cached, "content"));
        // The history has an assistant message that isn't the cached one:
        // the cache is for another branch of the conversation.
        if !assistant_matches {
            return Vec::new();
        }
    }

    let mut calls = HashSet::new();
    let mut outputs = HashSet::new();
    for item in input {
        if matches!(
            str_at(item, "type").trim(),
            "function_call_output" | "custom_tool_call_output"
        ) {
            outputs.extend(comparable_call_ids(&str_at(item, "call_id")));
        }
        calls.extend(tool_call_keys(item));
    }

    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        match str_at(item, "type").trim() {
            "reasoning" => {
                if input_has_encrypted_content(input, &str_at(item, "encrypted_content")) {
                    continue;
                }
            }
            "message" => {
                if assistant_matches {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                if !keep_tool_call(item, &mut calls, &outputs) {
                    continue;
                }
            }
            _ => continue,
        }
        kept.push(item);
    }
    kept
}

/// Whether the input has a reasoning item whose `encrypted_content` is this
/// string (`xaiInputHasReasoningEncryptedContent`).
fn input_has_encrypted_content(input: &[Value], encrypted_content: &str) -> bool {
    !encrypted_content.is_empty()
        && input.iter().any(|item| {
            str_at(item, "type").trim() == "reasoning"
                && matches!(item.get("encrypted_content"), Some(Value::String(own))
                    if own == encrypted_content)
        })
}

/// The newest assistant message of the input, which may have no `type`
/// (`xaiInputLastAssistantMessage`).
fn last_assistant_message(input: &[Value]) -> Option<&Value> {
    input.iter().rev().find(|item| {
        let kind = str_at(item, "type");
        let kind = kind.trim();
        (kind.is_empty() || kind == "message") && eq_fold(str_at(item, "role").trim(), "assistant")
    })
}

/// The first assistant message that was kept (`xaiReplayAssistantMessage`).
fn cached_assistant_message(items: &[Value]) -> Option<&Value> {
    items.iter().find(|item| {
        str_at(item, "type").trim() == "message"
            && eq_fold(str_at(item, "role").trim(), "assistant")
    })
}

/// A part of an assistant message: its type, and its text or refusal
/// (`xaiAssistantMessagePart`).
type Part<'a> = (&'static str, &'a str);

/// Whether two messages' `content` says the same
/// (`xaiAssistantMessageContentEqual`): the same `output_text` and
/// `refusal` parts in the same order, where a string is one text part.
/// Content with anything else in it is never equal.
fn assistant_content_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (message_parts(left), message_parts(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// `xaiAssistantMessageParts`: none if the content isn't a string or an
/// array of only text and refusal parts, or has no part.
fn message_parts(content: Option<&Value>) -> Option<Vec<Part<'_>>> {
    match content? {
        Value::String(text) => Some(vec![("output_text", text)]),
        Value::Array(parts) => {
            let parts = parts
                .iter()
                .map(|part| match str_at(part, "type").trim() {
                    "output_text" => match part.get("text") {
                        Some(Value::String(text)) => Some(("output_text", text.as_str())),
                        _ => None,
                    },
                    "refusal" => match part.get("refusal") {
                        Some(Value::String(refusal)) => Some(("refusal", refusal.as_str())),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            (!parts.is_empty()).then_some(parts)
        }
        _ => None,
    }
}

/// Puts the items into the body's input, before the tool result they answer
/// or the assistant message they belong with
/// (`insertCodexReasoningReplayItems`). Whether anything was put in.
fn insert_items(body: &mut Value, items: &[&Value]) -> bool {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return false;
    };
    if items.is_empty() {
        return false;
    }
    let index = insert_index(input, items).min(input.len());
    let aligned = align_call_ids(input, items);
    let tail = input.split_off(index);
    input.extend(aligned);
    input.extend(tail);
    true
}

#[cfg(test)]
mod tests;
