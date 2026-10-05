// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_reasoning.go
// (applyCodexReasoningReplayCache, codexReasoningReplayScopeFromRequest,
// codexReasoningReplaySessionKey, insertCodexReasoningReplayTurns,
// cacheCodexReasoningReplayFromCompleted,
// clearCodexReasoningReplayOnInvalidSignature and their helpers)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reasoning replay for Claude clients.
//!
//! A Claude client keeps the text and tool calls of Codex's answers but not
//! Codex's encrypted reasoning items, which let Codex carry its reasoning
//! into the next turn, and its tool calls come back in Claude's shape, with
//! IDs Claude accepts. So, as upstream does, the reasoning items and tool
//! calls of each completed turn are saved in [`super::replay_cache`] under
//! the request's model and session, and put back in the session's next
//! request, where they belong in its input.
//!
//! The session is the one the client names: Claude Code's session and agent
//! ([`crate::claude_code_session`]), else the Responses WebSocket execution
//! session, else a `prompt_cache_key`, Codex window ID or Codex turn
//! metadata in the body or payload, else Codex's turn metadata, window,
//! session or conversation headers. A request that names none isn't
//! replayed or saved.
//!
//! Each saved turn starts with a marker holding hashes of what it answered:
//! the input items of its request, its assistant message and its call IDs.
//! Those hashes only place a turn's items within the session's entry, before
//! the tool call, tool result or assistant message of a later request that
//! they belong with; they never choose the entry, which only the model and
//! the session do. A turn whose place isn't in the request is left out, and
//! an item the request already has isn't added again. When Codex rejects a
//! reasoning signature, the session's entry is dropped.
//!
//! Deviations from upstream:
//! - No session is made from the client's proxy API key: upstream falls back
//!   to `prompt-cache:` and a UUIDv5 of the key for OpenAI Chat Completions
//!   clients. Replay only runs for Claude clients, so upstream never reaches
//!   that fallback either.
//! - Home mode's shared KV store isn't ported, so reading or clearing an
//!   entry can't fail a request.
//! - A request has no metadata of its own here, so only the options'
//!   execution session is read; upstream reads the request's after the
//!   options'. Upstream reads the options' headers and then the gin
//!   request's, which are the same headers here, so they are read once.
//! - Fingerprints hash each item's compact JSON as `serde_json` writes it,
//!   where upstream hashes its bytes as they came. They are only compared
//!   with fingerprints made the same way, in this process.
//! - A payload, or turn metadata, that isn't valid JSON names no session,
//!   where gjson reads what it can.

use std::collections::{HashMap, HashSet};

use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_translate::codex::claude::{sanitize_tool_id, shorten_call_id};
use open_ferry_translate::go::to_lower;
use open_ferry_translate::json::exact;
use open_ferry_translate::signature::inspect_gpt_reasoning_signature;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::replay_cache::{ReplayCache, TURN_TYPE};
use super::request::{Kind, base_model, format_is};
use super::terminal::classification;
use crate::claude_code_session;
use crate::json::{eq_fold, get, set, str_at, str_of};

/// What separates the items of a fingerprint.
const ITEM_SEPARATOR: &[u8] = b"\0item\0";

/// Where a request's turn is replayed from and saved to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The model, as it goes to Codex.
    model: String,
    /// The session the client named.
    session_key: String,
    /// The fingerprint of the request's input items.
    request_fingerprint: String,
}

impl Scope {
    fn valid(&self) -> bool {
        !self.model.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

/// Puts the session's saved turns into the body of a `/responses` call
/// from a Claude client, and notes where its own turn is saved.
pub(crate) fn prepare(kind: Kind, request: &Request, options: &Options, body: &mut Value) -> Scope {
    if !matches!(kind, Kind::Execute | Kind::Stream) {
        return Scope::default();
    }
    apply(request, options, body)
}

/// `applyCodexReasoningReplayCache`.
fn apply(request: &Request, options: &Options, body: &mut Value) -> Scope {
    let scope = scope_from_request(request, options, body);
    if scope.valid()
        && let Some(items) = ReplayCache::global().get(&scope.model, &scope.session_key)
    {
        insert_turns(body, &items);
    }
    scope
}

/// Saves the turn of a `response.completed` or `response.done` event.
pub(crate) fn on_completed(scope: &Scope, event: &Value) {
    if matches!(
        str_at(event, "type").as_str(),
        "response.completed" | "response.done"
    ) {
        save(scope, event);
    }
}

/// Drops the session's entry when Codex rejected a reasoning signature
/// (`clearCodexReasoningReplayOnInvalidSignature`).
pub(crate) fn on_failure(scope: &Scope, status: u16, body: &[u8]) {
    if !scope.valid() {
        return;
    }
    let parsed = serde_json::from_slice(body).unwrap_or(Value::Null);
    if let Some(("thinking_signature_invalid", _)) = classification(status, body, &parsed) {
        ReplayCache::global().delete(&scope.model, &scope.session_key);
    }
}

/// `codexReasoningReplayScopeFromRequest`: none unless the client speaks
/// Claude.
fn scope_from_request(request: &Request, options: &Options, body: &Value) -> Scope {
    if !format_is(&options.source_format, &Format::CLAUDE) {
        return Scope::default();
    }
    let mut model = trimmed(body, "model");
    if model.is_empty() {
        model = base_model(&request.model).to_owned();
    }
    Scope {
        model,
        session_key: session_key(request, options, body),
        request_fingerprint: prefix_fingerprint(input_items(body)),
    }
}

/// gjson's `Array()` of the body's `input`: its items, the value alone if it
/// isn't an array, or nothing if it is missing or null.
fn input_items(body: &Value) -> &[Value] {
    match get(body, "input") {
        Some(Value::Array(items)) => items,
        None | Some(Value::Null) => &[],
        Some(other) => std::slice::from_ref(other),
    }
}

/// `codexReasoningReplaySessionKey`, without the API key fallback.
pub(crate) fn session_key(request: &Request, options: &Options, body: &Value) -> String {
    if format_is(&options.source_format, &Format::CLAUDE)
        && let Some(scope) =
            claude_code_session::execution_scope(&request.payload, &options.headers)
    {
        return scope;
    }
    let execution = options
        .metadata
        .execution_session_id
        .as_deref()
        .unwrap_or_default()
        .trim();
    if !execution.is_empty() {
        return format!("execution:{execution}");
    }
    let key = session_key_from_payload(body);
    if !key.is_empty() {
        return key;
    }
    if !request.payload.is_empty() {
        let payload = exact::from_slice(&request.payload).unwrap_or(Value::Null);
        let key = session_key_from_payload(&payload);
        if !key.is_empty() {
            return key;
        }
    }
    session_key_from_headers(&options.headers)
}

/// `codexReasoningReplaySessionKeyFromPayload`.
fn session_key_from_payload(payload: &Value) -> String {
    let prompt_cache_key = trimmed(payload, "prompt_cache_key");
    if !prompt_cache_key.is_empty() {
        return format!("prompt-cache:{prompt_cache_key}");
    }
    let window = trimmed(payload, "client_metadata.x-codex-window-id");
    if !window.is_empty() {
        return format!("window:{window}");
    }
    let turn_metadata = trimmed(payload, "client_metadata.x-codex-turn-metadata");
    if !turn_metadata.is_empty() {
        return session_key_from_turn_metadata(&turn_metadata);
    }
    String::new()
}

/// `codexReasoningReplaySessionKeyFromHeaders`.
fn session_key_from_headers(headers: &http::HeaderMap) -> String {
    let turn_metadata = headers
        .get("x-codex-turn-metadata")
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    if !turn_metadata.is_empty() {
        let key = session_key_from_turn_metadata(&turn_metadata);
        if !key.is_empty() {
            return key;
        }
    }
    let window = claude_code_session::header_value(headers, "x-codex-window-id");
    if !window.is_empty() {
        return format!("window:{window}");
    }
    // Upstream's `Session_id`, `session_id` and `Session-Id`; names don't
    // depend on case here.
    for name in ["session_id", "session-id"] {
        let session = claude_code_session::header_value(headers, name);
        if !session.is_empty() {
            return format!("session-id:{session}");
        }
    }
    let conversation = claude_code_session::header_value(headers, "conversation_id");
    if !conversation.is_empty() {
        return format!("conversation_id:{conversation}");
    }
    String::new()
}

/// `codexReasoningReplaySessionKeyFromTurnMetadata`.
fn session_key_from_turn_metadata(turn_metadata: &str) -> String {
    let Ok(metadata) = serde_json::from_str::<Value>(turn_metadata) else {
        return String::new();
    };
    let prompt_cache_key = trimmed(&metadata, "prompt_cache_key");
    if !prompt_cache_key.is_empty() {
        return format!("prompt-cache:{prompt_cache_key}");
    }
    let window = trimmed(&metadata, "window_id");
    if !window.is_empty() {
        return format!("window:{window}");
    }
    String::new()
}

/// Whether the input has a reasoning item whose `encrypted_content` is a
/// valid signature (`codexInputHasValidReasoningEncryptedContent`).
fn input_has_valid_reasoning(input: &[Value]) -> bool {
    input.iter().any(|item| {
        str_at(item, "type").trim() == "reasoning"
            && matches!(item.get("encrypted_content"), Some(Value::String(encrypted))
                if inspect_gpt_reasoning_signature(encrypted).is_ok())
    })
}

/// The saved items of one turn, and what its marker says about it.
#[derive(Default)]
struct ReplayTurn<'a> {
    marked: bool,
    assistant_fingerprint: String,
    request_fingerprint: String,
    call_ids: Vec<String>,
    items: Vec<&'a Value>,
}

/// Puts the saved turns into the body's input, each where it belongs
/// (`insertCodexReasoningReplayTurns`). Whether anything was put in.
fn insert_turns(body: &mut Value, replay: &[Value]) -> bool {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return false;
    };
    if replay.is_empty() {
        return false;
    }
    let original = std::mem::take(input);
    let turns = split_turns(replay);
    // What goes before each input item, and after the last.
    let mut insertions: Vec<Vec<Value>> = vec![Vec::new(); original.len() + 1];
    let mut used = HashSet::new();
    let mut prefixes = PrefixFingerprints::new(&original);
    let mut fallback_end = original.len() as isize - 1;
    let mut inserted = false;
    for turn in turns.iter().rev() {
        if turn.items.is_empty() {
            continue;
        }
        let (index, items) = if turn.marked {
            let Some(anchor) = anchor_index(&original, turn, fallback_end, &used, &mut prefixes)
            else {
                continue;
            };
            used.insert(anchor);
            if turn.request_fingerprint.is_empty() {
                fallback_end = anchor as isize - 1;
            }
            (anchor, filter_turn_items(&original, &turn.items))
        } else {
            let items = filter_items_for_input(&original, &turn.items);
            if items.is_empty() {
                continue;
            }
            (insert_index(&original, &items), items)
        };
        if items.is_empty() {
            continue;
        }
        let items = align_call_ids(&original, &items);
        let slot = &mut insertions[index];
        let later = std::mem::replace(slot, items);
        slot.extend(later);
        inserted = true;
    }
    if !inserted {
        *input = original;
        return false;
    }

    let mut insertions = insertions.into_iter();
    let mut rebuilt = Vec::with_capacity(original.len() + replay.len());
    for item in original {
        rebuilt.extend(insertions.next().unwrap_or_default());
        rebuilt.push(item);
    }
    rebuilt.extend(insertions.flatten());
    *input = rebuilt;
    true
}

/// Splits saved items into turns at their markers
/// (`splitCodexReasoningReplayTurns`). Items before the first marker, as a
/// replaced entry holds, are a turn with no marker.
fn split_turns(items: &[Value]) -> Vec<ReplayTurn<'_>> {
    let mut turns = Vec::new();
    let mut current = ReplayTurn::default();
    for item in items {
        if str_at(item, "type").trim() != TURN_TYPE {
            current.items.push(item);
            continue;
        }
        let marker = ReplayTurn {
            marked: true,
            assistant_fingerprint: trimmed(item, "assistant_fingerprint"),
            request_fingerprint: trimmed(item, "request_fingerprint"),
            call_ids: match item.get("call_ids") {
                Some(Value::Array(ids)) => ids
                    .iter()
                    .map(|id| str_of(Some(id)).trim().to_owned())
                    .filter(|id| !id.is_empty())
                    .collect(),
                _ => Vec::new(),
            },
            items: Vec::new(),
        };
        let previous = std::mem::replace(&mut current, marker);
        if !previous.items.is_empty() {
            turns.push(previous);
        }
    }
    if !current.items.is_empty() {
        turns.push(current);
    }
    turns
}

/// Where a marked turn goes (`codexReasoningReplayTurnAnchorIndex`): before
/// the newest unused tool call or tool result with one of its call IDs, else
/// before the newest unused assistant message that matches its own, looking
/// no later than `fallback_end` unless the turn knows its request's input.
/// That input must then be what precedes the place. A turn with neither call
/// IDs nor a message goes where an unmarked one would.
fn anchor_index(
    input: &[Value],
    turn: &ReplayTurn<'_>,
    fallback_end: isize,
    used: &HashSet<usize>,
    prefixes: &mut PrefixFingerprints<'_>,
) -> Option<usize> {
    let last = input.len() as isize - 1;
    let search_end = if turn.request_fingerprint.is_empty() {
        fallback_end.min(last)
    } else {
        last
    };
    let candidates = || (0..=search_end).rev().map(|index| index as usize);
    let mut matches_request = |index: usize| {
        turn.request_fingerprint.is_empty() || prefixes.at(index) == turn.request_fingerprint
    };
    if !turn.call_ids.is_empty() {
        let call_ids: HashSet<String> = turn
            .call_ids
            .iter()
            .flat_map(|id| comparable_call_ids(id))
            .collect();
        for index in candidates() {
            if used.contains(&index) || !matches_request(index) {
                continue;
            }
            let item = &input[index];
            if !matches!(
                str_at(item, "type").trim(),
                "function_call"
                    | "custom_tool_call"
                    | "function_call_output"
                    | "custom_tool_call_output"
            ) {
                continue;
            }
            if comparable_call_ids(&str_at(item, "call_id"))
                .iter()
                .any(|id| call_ids.contains(id))
            {
                return Some(index);
            }
        }
    }
    if !turn.assistant_fingerprint.is_empty() {
        for index in candidates() {
            if used.contains(&index) || !matches_request(index) {
                continue;
            }
            if assistant_fingerprint(&input[index]) == turn.assistant_fingerprint {
                return Some(index);
            }
        }
    }
    if turn.call_ids.is_empty() && turn.assistant_fingerprint.is_empty() {
        return Some(insert_index(input, &turn.items));
    }
    None
}

/// A marked turn's items that the input doesn't already have
/// (`filterCodexReasoningReplayTurnItems`): reasoning whose encrypted
/// content isn't there, and tool calls that aren't there but whose result
/// is.
fn filter_turn_items<'a>(input: &[Value], items: &[&'a Value]) -> Vec<&'a Value> {
    let mut reasoning = HashSet::new();
    let mut outputs = HashSet::new();
    let mut calls = HashSet::new();
    for item in input {
        match str_at(item, "type").trim() {
            "reasoning" => {
                let encrypted = trimmed(item, "encrypted_content");
                if !encrypted.is_empty() {
                    reasoning.insert(encrypted);
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                outputs.extend(comparable_call_ids(&str_at(item, "call_id")));
            }
            _ => {}
        }
        calls.extend(tool_call_keys(item));
    }

    let mut kept = Vec::with_capacity(items.len());
    for &item in items {
        match str_at(item, "type").trim() {
            "reasoning" => {
                if reasoning.contains(&trimmed(item, "encrypted_content")) {
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

/// An unmarked turn's items that the input doesn't already have
/// (`filterCodexReasoningReplayItemsForInput`): reasoning if the input has
/// no valid reasoning, and tool calls that aren't there but whose result is.
fn filter_items_for_input<'a>(input: &[Value], items: &[&'a Value]) -> Vec<&'a Value> {
    let has_reasoning = input_has_valid_reasoning(input);
    let mut outputs = HashSet::new();
    let mut calls = HashSet::new();
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
    for &item in items {
        match str_at(item, "type").trim() {
            "reasoning" => {
                if has_reasoning {
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

/// Whether a saved tool call is new to the input and has its result there,
/// noting it as there if so.
pub(crate) fn keep_tool_call(
    item: &Value,
    calls: &mut HashSet<String>,
    outputs: &HashSet<String>,
) -> bool {
    let keys = tool_call_keys(item);
    if keys.is_empty() || keys.iter().any(|key| calls.contains(key)) {
        return false;
    }
    let has_output = comparable_call_ids(&str_at(item, "call_id"))
        .iter()
        .any(|id| outputs.contains(id));
    if !has_output {
        return false;
    }
    calls.extend(keys);
    true
}

/// Where unmarked items go (`codexReasoningReplayInsertIndex`): before the
/// first tool result that has no call ID or one of theirs, else before the
/// last assistant message, else before the first item that isn't a system
/// or developer message, else at the end.
pub(crate) fn insert_index(input: &[Value], items: &[&Value]) -> usize {
    let call_ids: HashSet<String> = items
        .iter()
        .filter(|item| is_tool_call(item))
        .flat_map(|item| comparable_call_ids(&str_at(item, "call_id")))
        .collect();
    if !call_ids.is_empty() {
        let found = input.iter().position(|item| {
            matches!(
                str_at(item, "type").trim(),
                "function_call_output" | "custom_tool_call_output"
            ) && {
                let call_id = trimmed(item, "call_id");
                call_id.is_empty() || call_ids.contains(&call_id)
            }
        });
        if let Some(index) = found {
            return index;
        }
    }
    if let Some(index) = input
        .iter()
        .rposition(|item| message_role(item).as_deref() == Some("assistant"))
    {
        return index;
    }
    input
        .iter()
        .position(|item| !matches!(message_role(item).as_deref(), Some("developer" | "system")))
        .unwrap_or(input.len())
}

/// The items to insert, with each tool call's ID changed to the one its
/// result has in the input, which may be the shortened ID a Claude client
/// sees (`codexAlignReasoningReplayToolCallIDs`).
pub(crate) fn align_call_ids(input: &[Value], items: &[&Value]) -> Vec<Value> {
    let mut outputs: HashMap<String, String> = HashMap::new();
    for item in input {
        if !matches!(
            str_at(item, "type").trim(),
            "function_call_output" | "custom_tool_call_output"
        ) {
            continue;
        }
        let call_id = trimmed(item, "call_id");
        if call_id.is_empty() {
            continue;
        }
        for candidate in comparable_call_ids(&call_id) {
            outputs.insert(candidate, call_id.clone());
        }
    }

    items
        .iter()
        .map(|&item| {
            let mut item = item.clone();
            if outputs.is_empty() || !is_tool_call(&item) {
                return item;
            }
            let call_id = trimmed(&item, "call_id");
            let output_id = comparable_call_ids(&call_id)
                .iter()
                .find_map(|candidate| outputs.get(candidate).filter(|id| !id.is_empty()))
                .cloned();
            if let Some(output_id) = output_id
                && output_id != call_id
            {
                set(&mut item, "call_id", Value::from(output_id));
            }
            item
        })
        .collect()
}

pub(crate) fn is_tool_call(item: &Value) -> bool {
    matches!(
        str_at(item, "type").trim(),
        "function_call" | "custom_tool_call"
    )
}

/// The role of a message, in lowercase (`codexReplayMessageRole`).
pub(crate) fn message_role(item: &Value) -> Option<String> {
    let kind = trimmed(item, "type");
    let role = to_lower(str_at(item, "role").trim());
    if role.is_empty() || (!kind.is_empty() && kind != "message") {
        return None;
    }
    Some(role)
}

/// `type:call ID` for each form of a tool call's ID
/// (`codexReplayToolCallKeys`).
pub(crate) fn tool_call_keys(item: &Value) -> Vec<String> {
    let kind = trimmed(item, "type");
    if kind != "function_call" && kind != "custom_tool_call" {
        return Vec::new();
    }
    comparable_call_ids(&str_at(item, "call_id"))
        .into_iter()
        .map(|id| format!("{kind}:{id}"))
        .collect()
}

/// A call ID, and the ID a Claude client sees for it if that differs
/// (`codexReplayComparableCallIDs`): the response translators make it a
/// valid `tool_use` ID and shorten it to 64 bytes.
pub(crate) fn comparable_call_ids(call_id: &str) -> Vec<String> {
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return Vec::new();
    }
    let visible = shorten_call_id(&sanitize_tool_id(call_id)).into_owned();
    if visible.is_empty() || visible == call_id {
        vec![call_id.to_owned()]
    } else {
        vec![call_id.to_owned(), visible]
    }
}

/// The hash of an assistant message's text and refusals, or `""` if it has
/// none or holds anything else (`codexReplayAssistantMessageFingerprint`).
fn assistant_fingerprint(item: &Value) -> String {
    let kind = trimmed(item, "type");
    if (!kind.is_empty() && kind != "message") || !eq_fold(str_at(item, "role").trim(), "assistant")
    {
        return String::new();
    }
    let mut text = String::new();
    match item.get("content") {
        Some(Value::String(content)) => text.push_str(content),
        Some(Value::Array(parts)) => {
            for part in parts {
                match str_at(part, "type").trim() {
                    "input_text" | "output_text" => text.push_str(&str_at(part, "text")),
                    "refusal" => {
                        text.push_str("\0refusal\0");
                        text.push_str(&str_at(part, "refusal"));
                    }
                    _ => return String::new(),
                }
            }
        }
        _ => return String::new(),
    }
    if text.is_empty() {
        return String::new();
    }
    hex(&Sha256::digest(text.as_bytes()))
}

/// The fingerprint of a run of input items (`codexReplayInputPrefixFingerprint`).
fn prefix_fingerprint(items: &[Value]) -> String {
    let mut hasher = Sha256::new();
    for item in items {
        absorb(&mut hasher, item);
    }
    hex(&hasher.finalize())
}

fn absorb(hasher: &mut Sha256, item: &Value) {
    hasher.update(ITEM_SEPARATOR);
    hasher.update(item.to_string().as_bytes());
}

/// The fingerprints of each prefix of the input, from one pass of hashing
/// (`codexReplayPrefixFingerprints`): the anchor search asks for many, and
/// hashing each from the start would be quadratic in a long input.
struct PrefixFingerprints<'a> {
    items: &'a [Value],
    hasher: Sha256,
    /// `sums[end]` is the fingerprint of `items[..end]`, made as needed.
    sums: Vec<String>,
}

impl<'a> PrefixFingerprints<'a> {
    fn new(items: &'a [Value]) -> Self {
        let hasher = Sha256::new();
        let sums = vec![hex(&hasher.clone().finalize())];
        Self {
            items,
            hasher,
            sums,
        }
    }

    /// The fingerprint of the first `end` items, or `""` past the end.
    fn at(&mut self, end: usize) -> &str {
        if end > self.items.len() {
            return "";
        }
        while self.sums.len() <= end {
            absorb(&mut self.hasher, &self.items[self.sums.len() - 1]);
            self.sums.push(hex(&self.hasher.clone().finalize()));
        }
        &self.sums[end]
    }
}

/// Saves the reasoning items and tool calls of a completed response as a
/// turn of the session (`cacheCodexReasoningReplayFromCompleted`).
fn save(scope: &Scope, completed: &Value) {
    if !scope.valid() {
        return;
    }
    let Some(Value::Array(output)) = get(completed, "response.output") else {
        return;
    };
    let mut items = Vec::new();
    let mut call_ids = Vec::new();
    let mut assistant = String::new();
    for item in output {
        match str_at(item, "type").trim() {
            "reasoning" => items.push(item),
            "function_call" | "custom_tool_call" => {
                items.push(item);
                let call_id = trimmed(item, "call_id");
                if !call_id.is_empty() {
                    call_ids.push(call_id);
                }
            }
            "message" => {
                let fingerprint = assistant_fingerprint(item);
                if !fingerprint.is_empty() {
                    assistant = fingerprint;
                }
            }
            _ => {}
        }
    }
    if items.is_empty() {
        return;
    }

    let mut hasher = Sha256::new();
    hasher.update(scope.request_fingerprint.as_bytes());
    hasher.update(b"\0assistant\0");
    hasher.update(assistant.as_bytes());
    for call_id in &call_ids {
        hasher.update(b"\0call\0");
        hasher.update(call_id.as_bytes());
    }
    for item in &items {
        absorb(&mut hasher, item);
    }
    let mut marker = Map::new();
    marker.insert("type".into(), TURN_TYPE.into());
    marker.insert("id".into(), hex(&hasher.finalize()).into());
    if !assistant.is_empty() {
        marker.insert("assistant_fingerprint".into(), assistant.into());
    }
    if !scope.request_fingerprint.is_empty() {
        marker.insert(
            "request_fingerprint".into(),
            scope.request_fingerprint.clone().into(),
        );
    }
    if !call_ids.is_empty() {
        marker.insert("call_ids".into(), call_ids.into());
    }
    let marker = Value::Object(marker);
    let mut turn = vec![&marker];
    turn.extend(items);
    ReplayCache::global().append(&scope.model, &scope.session_key, &turn);
}

/// gjson's `String()` at `path`, trimmed.
pub(crate) fn trimmed(value: &Value, path: &str) -> String {
    str_at(value, path).trim().to_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests;
