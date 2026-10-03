// Ported from normalizeResponsesWebsocketRequestWithIncrementalState,
// normalizeResponseCreateRequest, normalizeResponseSubsequentRequest,
// shouldReplaceWebsocketTranscript, inputHasCodexLocalCompactionSummary,
// codexLocalCompactionMessageText, inputSatisfiesPendingToolCalls,
// normalizeResponseTranscriptReplacement, mergeResponsesWebsocketInput and
// its helpers, dedupeResponsesWebsocketMergeFunctionCalls and
// normalizeResponsesWebsocketPassthroughRequest in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket_requests.go, and
// inputContainsFullTranscript and inputWithoutCompactionItems in
// sdk/api/handlers/openai/openai_responses_websocket_prewarm.go (v8.0.10,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A client's WebSocket request as an HTTP Responses request: the whole
//! transcript so far, with the turn's input merged in, since an HTTP
//! upstream keeps no state between turns.

use std::collections::HashSet;
use std::fmt;

use open_ferry_translate::go;

use super::repair::{InputItem, dedupe_input_items, is_tool_call, is_tool_output};
use crate::errors::ErrorMessage;
use crate::json::{self, Val, str_at};

/// A request that starts a turn (`wsRequestTypeCreate`).
pub(super) const TYPE_CREATE: &str = "response.create";

/// A request that adds to the turn in flight (`wsRequestTypeAppend`).
pub(super) const TYPE_APPEND: &str = "response.append";

/// How Codex opens a summary of another model's work, which it sends in place
/// of the history after compacting locally
/// (`codexLocalCompactionSummaryPrefix`).
pub(super) const LOCAL_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// A request to send, and what the session keeps as its last request.
pub(super) type Normalized = (Vec<u8>, Vec<u8>);

/// A 400 with `text`.
pub(super) fn bad_request(text: impl Into<String>) -> ErrorMessage {
    ErrorMessage::new(400, text)
}

/// The 400 for an input that isn't an array.
pub(super) fn input_not_array() -> ErrorMessage {
    bad_request("websocket request requires array field: input")
}

/// The 400 for a request of a type the WebSocket doesn't take.
pub(super) fn unsupported_type(request_type: &str) -> ErrorMessage {
    bad_request(format!(
        "unsupported websocket request type: {request_type}"
    ))
}

/// The request's `type`, trimmed.
pub(super) fn request_type(payload: &[u8]) -> String {
    str_at(payload, "type").trim().to_owned()
}

/// `raw` as the HTTP request to send, given the session's last request and
/// the output of the response to it
/// (`normalizeResponsesWebsocketRequestWithIncrementalState`).
///
/// `allow_incremental` sends a request that names its previous response as
/// it is, and `allow_compaction_bypass` sends a compacted transcript as it
/// is instead of merging it with the history.
pub(super) fn normalize(
    raw: &[u8],
    last_request: &[u8],
    last_output: &[u8],
    last_response_id: &str,
    pending_call_ids: &[String],
    allow_incremental: bool,
    allow_compaction_bypass: bool,
) -> Result<Normalized, ErrorMessage> {
    let kind = request_type(raw);
    match kind.as_str() {
        TYPE_CREATE if last_request.is_empty() => normalize_create(raw),
        TYPE_CREATE | TYPE_APPEND => normalize_subsequent(
            raw,
            last_request,
            last_output,
            last_response_id,
            pending_call_ids,
            allow_incremental,
            allow_compaction_bypass,
        ),
        _ => Err(unsupported_type(&kind)),
    }
}

/// The first `response.create` of a transcript (`normalizeResponseCreateRequest`).
pub(super) fn normalize_create(raw: &[u8]) -> Result<Normalized, ErrorMessage> {
    if json::get(raw, "input").is_some_and(|input| !input.is_array()) {
        return Err(input_not_array());
    }
    let mut normalized = json::delete(raw, "type");
    normalized = json::set_bool(&normalized, "stream", true);
    if json::get(&normalized, "input").is_none() {
        normalized = json::set_raw(&normalized, "input", b"[]");
    }
    if str_at(&normalized, "model").trim().is_empty() {
        return Err(bad_request("missing model in response.create request"));
    }
    Ok((normalized.clone(), normalized))
}

/// A request after the first (`normalizeResponseSubsequentRequest`).
fn normalize_subsequent(
    raw: &[u8],
    last_request: &[u8],
    last_output: &[u8],
    last_response_id: &str,
    pending_call_ids: &[String],
    allow_incremental: bool,
    allow_compaction_bypass: bool,
) -> Result<Normalized, ErrorMessage> {
    if last_request.is_empty() {
        return Err(bad_request(
            "websocket request received before response.create",
        ));
    }
    let Some(next_input) = json::get(raw, "input").filter(Val::is_array) else {
        return Err(input_not_array());
    };

    // A client that compacted its history sends the new transcript whole;
    // merging it would repeat stale items and orphan tool calls.
    if should_replace_transcript(raw, next_input) {
        let normalized = transcript_replacement(raw, last_request);
        return Ok((normalized.clone(), normalized));
    }

    if allow_incremental {
        let mut previous = str_at(raw, "previous_response_id").trim().to_owned();
        if previous.is_empty() {
            if !input_satisfies_pending_calls(next_input, pending_call_ids) {
                let normalized = transcript_replacement(raw, last_request);
                return Ok((normalized.clone(), normalized));
            }
            last_response_id.trim().clone_into(&mut previous);
        }
        if !previous.is_empty() {
            let normalized = json::delete(raw, "type");
            let normalized = json::set_str(&normalized, "previous_response_id", &previous);
            let normalized = inherit(normalized, last_request);
            return Ok((normalized.clone(), normalized));
        }
    }

    let merged = if allow_compaction_bypass && input_contains_full_transcript(next_input) {
        tracing::info!(
            items = next_input.array().len(),
            "responses websocket: full transcript detected, skipping stale merge"
        );
        next_input.raw.to_vec()
    } else {
        let append = if input_contains_full_transcript(next_input) {
            input_without_compaction_items(next_input)
        } else {
            next_input.raw.to_vec()
        };
        merge_input(last_request, last_output, &append)
            .map_err(|err| bad_request(err.to_string()))?
    };

    let normalized = json::delete(raw, "type");
    let normalized = json::delete(&normalized, "previous_response_id");
    let normalized = inherit(normalized, last_request);
    let Some(normalized) = json::try_set_raw(&normalized, "input", &merged) else {
        return Err(bad_request(
            "failed to merge websocket input: cannot set array element for non-numeric key 'input'",
        ));
    };
    Ok((normalized.clone(), normalized))
}

/// `normalized` with the last request's model and instructions where it has
/// none, and streaming on.
fn inherit(mut normalized: Vec<u8>, last_request: &[u8]) -> Vec<u8> {
    if json::get(&normalized, "model").is_none() {
        let model = str_at(last_request, "model");
        let model = model.trim();
        if !model.is_empty() {
            normalized = json::set_str(&normalized, "model", model);
        }
    }
    if json::get(&normalized, "instructions").is_none()
        && let Some(instructions) = json::get(last_request, "instructions")
    {
        normalized = json::set_raw(&normalized, "instructions", instructions.raw);
    }
    json::set_bool(&normalized, "stream", true)
}

/// Whether `raw` starts the transcript over: its input holds the model's
/// earlier output, or a local compaction summary
/// (`shouldReplaceWebsocketTranscript`).
pub(super) fn should_replace_transcript(raw: &[u8], next_input: Val<'_>) -> bool {
    let kind = request_type(raw);
    if kind != TYPE_CREATE && kind != TYPE_APPEND {
        return false;
    }
    let previous_response_id = json::get(raw, "previous_response_id");
    if previous_response_id.is_some_and(|id| !id.str().trim().is_empty()) {
        return false;
    }
    if !next_input.is_array() {
        return false;
    }
    if kind == TYPE_CREATE
        && previous_response_id.is_none()
        && has_local_compaction_summary(next_input)
    {
        return true;
    }
    next_input.array().iter().any(|item| {
        let item_type = field(item, "type");
        match item_type.as_str() {
            "function_call" | "custom_tool_call" => true,
            "message" => field(item, "role") == "assistant",
            _ => false,
        }
    })
}

/// gjson's `String()` of `key` in `item`, trimmed.
fn field(item: &Val<'_>, key: &str) -> String {
    item.get(key)
        .map(|value| value.str().trim().to_owned())
        .unwrap_or_default()
}

/// Whether `input` is a Codex local compaction: developer and user messages,
/// perhaps after the tools, one of them the summary
/// (`inputHasCodexLocalCompactionSummary`).
pub(super) fn has_local_compaction_summary(input: Val<'_>) -> bool {
    if !input.is_array() {
        return false;
    }
    let mut has_summary = false;
    for (index, item) in input.array().iter().enumerate() {
        let item_type = field(item, "type");
        if item_type == "additional_tools" {
            let Some(tools) = item.get("tools").filter(Val::is_array) else {
                return false;
            };
            if index != 0 || field(item, "role") != "developer" {
                return false;
            }
            let typed = tools
                .array()
                .iter()
                .all(|tool| tool.is_object() && !field(tool, "type").is_empty());
            if !typed {
                return false;
            }
            continue;
        }
        if !item_type.is_empty() && item_type != "message" {
            return false;
        }
        let role = field(item, "role");
        if role != "user" && role != "developer" {
            return false;
        }
        if role == "user"
            && local_compaction_message_text(*item)
                .strip_prefix(LOCAL_SUMMARY_PREFIX)
                .is_some_and(|rest| rest.starts_with('\n'))
        {
            has_summary = true;
        }
    }
    has_summary
}

/// A message's text: its content string, or its `input_text` parts
/// together (`codexLocalCompactionMessageText`).
fn local_compaction_message_text(message: Val<'_>) -> String {
    let Some(content) = message.get("content") else {
        return String::new();
    };
    if content.is_string() {
        return content.str();
    }
    if !content.is_array() {
        return String::new();
    }
    content
        .array()
        .iter()
        .filter(|part| field(part, "type") == "input_text")
        .map(|part| part.get("text").map(|text| text.str()).unwrap_or_default())
        .collect()
}

/// Whether `input` answers every tool call still pending
/// (`inputSatisfiesPendingToolCalls`).
fn input_satisfies_pending_calls(input: Val<'_>, pending_call_ids: &[String]) -> bool {
    if pending_call_ids.is_empty() {
        return true;
    }
    if !input.is_array() {
        return false;
    }
    let outputs: HashSet<String> = input
        .array()
        .iter()
        .filter(|item| is_tool_output(&field(item, "type")))
        .map(|item| field(item, "call_id"))
        .filter(|id| !id.is_empty())
        .collect();
    pending_call_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .all(|id| outputs.contains(id))
}

/// `raw` as a transcript on its own, with the last request's model and
/// instructions where it has none (`normalizeResponseTranscriptReplacement`).
pub(super) fn transcript_replacement(raw: &[u8], last_request: &[u8]) -> Vec<u8> {
    let normalized = json::delete(raw, "type");
    let normalized = json::delete(&normalized, "previous_response_id");
    inherit(normalized, last_request)
}

/// Whether `input` carries compaction items, which mean the client sent the
/// whole transcript (`inputContainsFullTranscript`).
pub(super) fn input_contains_full_transcript(input: Val<'_>) -> bool {
    input.is_array() && input.array().iter().any(is_compaction_item)
}

fn is_compaction_item(item: &Val<'_>) -> bool {
    let item_type = item.get("type").map(|kind| kind.str()).unwrap_or_default();
    item_type == "compaction" || item_type == "compaction_summary"
}

/// `input` without its compaction items (`inputWithoutCompactionItems`).
fn input_without_compaction_items(input: Val<'_>) -> Vec<u8> {
    let items = input.array();
    join_raw(
        items
            .iter()
            .filter(|item| !is_compaction_item(item))
            .map(|item| item.raw),
    )
}

/// `[` the items `,` `]`, as written.
fn join_raw<'a>(items: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = vec![b'['];
    for (index, raw) in items.enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.extend_from_slice(raw);
    }
    out.push(b']');
    out
}

/// Why a merge failed: where, and the error Go's `encoding/json` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MergeError {
    context: &'static str,
    cause: DecodeError,
}

/// An error Go's `encoding/json` would report decoding the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DecodeError {
    /// Not JSON (`*json.SyntaxError`).
    Syntax,
    /// JSON of the wrong type (`*json.UnmarshalTypeError`): what it was, and
    /// what it was decoded into.
    Type(&'static str, &'static str),
}

impl MergeError {
    /// The decode error behind it.
    #[cfg(test)]
    pub(super) fn cause(&self) -> &DecodeError {
        &self.cause
    }
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            DecodeError::Syntax => write!(f, "{}: invalid JSON", self.context),
            DecodeError::Type(value, target) => write!(
                f,
                "{}: json: cannot unmarshal {value} into Go {target}",
                self.context
            ),
        }
    }
}

/// Go's name for the kind of JSON `raw` is.
fn json_kind(raw: &[u8]) -> &'static str {
    match raw.first() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't' | b'f') => "bool",
        _ => "number",
    }
}

/// The last request's input, followed by the last response's output and then
/// `append`, with tool calls and IDs deduplicated
/// (`mergeResponsesWebsocketInput`).
pub(super) fn merge_input(
    last_request: &[u8],
    last_output: &[u8],
    append: &[u8],
) -> Result<Vec<u8>, MergeError> {
    let previous = previous_input(last_request).map_err(|cause| MergeError {
        context: "invalid previous request input",
        cause,
    })?;
    let mut items = previous.map(merge_items).unwrap_or_default();

    let output = go::trim_space(last_output);
    if output.first() == Some(&b'[')
        && json::valid(output)
        && let Some(output) = Val::parse(output)
    {
        if input_contains_full_transcript(output) {
            items.retain(|item| item.item_type != "compaction_trigger");
        }
        items.extend(merge_items(output));
    }

    let append = match go::trim_space(append) {
        b"" => b"[]".as_slice(),
        trimmed => trimmed,
    };
    let appended = if json::gjson_valid(append) {
        Val::parse(append)
    } else {
        None
    };
    let request_error = |cause| MergeError {
        context: "invalid request input",
        cause,
    };
    match appended {
        None => return Err(request_error(DecodeError::Syntax)),
        Some(value) if value.is_null() => {}
        Some(value) if !value.is_array() => {
            return Err(request_error(DecodeError::Type(
                json_kind(value.raw),
                "value of type []json.RawMessage",
            )));
        }
        Some(value) => items.extend(merge_items(value)),
    }

    let items = dedupe_input_items(dedupe_function_calls(items));
    Ok(join_raw(items.iter().map(|item| item.raw.as_slice())))
}

/// The last request's input, or `None` when it has none, matching keys as
/// Go's `encoding/json` does: case-insensitively, the last one winning,
/// though any earlier one of the wrong type still fails
/// (`responsesWebsocketPreviousInputNoCopy`).
fn previous_input(last_request: &[u8]) -> Result<Option<Val<'_>>, DecodeError> {
    if !json::valid(last_request) {
        return Err(DecodeError::Syntax);
    }
    let Some(root) = Val::parse(last_request) else {
        return Err(DecodeError::Syntax);
    };
    if root.is_null() {
        return Ok(None);
    }
    if !root.is_object() {
        return Err(DecodeError::Type(
            json_kind(root.raw),
            r#"value of type struct { Input []json.RawMessage "json:\"input\"" }"#,
        ));
    }
    let mut input = None;
    for (key, value) in root.members() {
        if !json::fold_eq(&key, "input") {
            continue;
        }
        if !value.is_null() && !value.is_array() {
            return Err(DecodeError::Type(
                json_kind(value.raw),
                "struct field .input of type []json.RawMessage",
            ));
        }
        input = Some(value);
    }
    Ok(input.filter(|input| !input.is_null()))
}

/// The items of an array, with what dedupe reads from each: matching keys
/// case-insensitively, the last one winning, and reading values as gjson
/// does (`appendResponsesWebsocketMergeInputResult`).
fn merge_items(input: Val<'_>) -> Vec<InputItem> {
    input
        .array()
        .into_iter()
        .map(|raw| {
            let mut item = InputItem {
                raw: raw.raw.to_vec(),
                ..InputItem::default()
            };
            if raw.is_object() {
                for (key, value) in raw.members() {
                    let value = value.str().trim().to_owned();
                    if json::fold_eq(&key, "type") {
                        item.item_type = value;
                    } else if json::fold_eq(&key, "id") {
                        item.id = value;
                    } else if json::fold_eq(&key, "call_id") {
                        item.call_id = value;
                    }
                }
            }
            item
        })
        .collect()
}

/// `items` with only the first tool call for each call ID
/// (`dedupeResponsesWebsocketMergeFunctionCalls`).
fn dedupe_function_calls(items: Vec<InputItem>) -> Vec<InputItem> {
    let mut seen: HashSet<String> = HashSet::new();
    items
        .into_iter()
        .filter(|item| {
            !is_tool_call(&item.item_type)
                || item.call_id.is_empty()
                || seen.insert(item.call_id.clone())
        })
        .collect()
}

/// A request for an upstream WebSocket that keeps the session's state, sent
/// as it is but for a model and streaming
/// (`normalizeResponsesWebsocketPassthroughRequest`).
pub(super) fn normalize_passthrough(raw: &[u8], model: &str) -> Result<Vec<u8>, ErrorMessage> {
    if !json::valid(raw) {
        return Err(bad_request("invalid websocket request JSON"));
    }
    let kind = request_type(raw);
    if kind != TYPE_CREATE && kind != TYPE_APPEND {
        return Err(unsupported_type(&kind));
    }
    let mut normalized = raw.to_vec();
    if str_at(&normalized, "model").trim().is_empty() {
        let model = model.trim();
        if model.is_empty() {
            return Err(bad_request("missing model in response.create request"));
        }
        normalized = json::set_str(&normalized, "model", model);
    }
    Ok(json::set_bool(&normalized, "stream", true))
}
