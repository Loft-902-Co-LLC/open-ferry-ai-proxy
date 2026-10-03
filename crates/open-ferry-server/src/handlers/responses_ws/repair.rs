// Ported from websocketToolOutputCache, websocketToolSessionRefCounter,
// responsesWebsocketToolCacheTurn and their methods,
// websocketDownstreamSessionKey, retainResponsesWebsocketToolCaches,
// releaseResponsesWebsocketToolCaches, prepareResponsesWebsocketFallbackTurn,
// repairResponsesWebsocketToolCallsWithCachesMode,
// parseResponsesWebsocketRepairRequest, repairResponsesToolCallItems,
// recordResponsesWebsocketToolCallsFromPayloadWithCache,
// isResponsesToolCallType and isResponsesToolCallOutputType in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket_toolcall_repair.go, and
// parseResponsesWebsocketInputItem, responsesWebsocketMetadataString and
// dedupeResponsesWebsocketInputItems in
// sdk/api/handlers/openai/openai_responses_websocket_requests.go (v8.0.10,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tool-call repair: the tool calls and outputs a client's sessions have
//! seen, kept so a request that lost one half of a pair can have it put back,
//! or the orphan dropped, before it goes over HTTP.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use http::HeaderMap;
use open_ferry_translate::go;

use crate::json::{self, Val};

/// The most items a cache keeps per session
/// (`websocketToolOutputCacheMaxPerSession`).
const MAX_PER_SESSION: usize = 256;

/// One session's cached items by call ID, oldest first.
#[derive(Debug, Default)]
struct SessionItems {
    items: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
}

/// Tool items by session and call ID (`websocketToolOutputCache`).
#[derive(Debug, Default)]
pub(super) struct ToolCache {
    sessions: HashMap<String, SessionItems>,
}

impl ToolCache {
    /// Keeps `item` for `call_id`, evicting the session's oldest past the
    /// limit (`record`).
    pub(super) fn record(&mut self, session_key: &str, call_id: &str, item: &[u8]) {
        let session_key = session_key.trim();
        let call_id = call_id.trim();
        if session_key.is_empty() || call_id.is_empty() {
            return;
        }
        let session = self.sessions.entry(session_key.to_owned()).or_default();
        if session
            .items
            .insert(call_id.to_owned(), item.to_vec())
            .is_none()
        {
            session.order.push_back(call_id.to_owned());
        }
        while session.order.len() > MAX_PER_SESSION {
            if let Some(evicted) = session.order.pop_front() {
                session.items.remove(&evicted);
            }
        }
    }

    /// The item kept for `call_id` (`get`).
    pub(super) fn get(&self, session_key: &str, call_id: &str) -> Option<&[u8]> {
        let session_key = session_key.trim();
        let call_id = call_id.trim();
        if session_key.is_empty() || call_id.is_empty() {
            return None;
        }
        let item = self.sessions.get(session_key)?.items.get(call_id)?;
        (!item.is_empty()).then_some(item.as_slice())
    }

    fn delete_session(&mut self, session_key: &str) {
        self.sessions.remove(session_key.trim());
    }
}

/// The tool outputs and calls sessions have seen, and how many sockets use
/// each session key.
#[derive(Debug, Default)]
pub(super) struct ToolCaches {
    pub(super) outputs: ToolCache,
    pub(super) calls: ToolCache,
    refs: HashMap<String, usize>,
}

impl ToolCaches {
    /// Counts a socket using `session_key` (`retainResponsesWebsocketToolCaches`).
    pub(super) fn retain(&mut self, session_key: &str) {
        let session_key = session_key.trim();
        if !session_key.is_empty() {
            *self.refs.entry(session_key.to_owned()).or_default() += 1;
        }
    }

    /// Stops counting a socket, and forgets the session once none uses it
    /// (`releaseResponsesWebsocketToolCaches`).
    pub(super) fn release(&mut self, session_key: &str) {
        let session_key = session_key.trim();
        if session_key.is_empty() {
            return;
        }
        match self.refs.get_mut(session_key) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                self.refs.remove(session_key);
                self.outputs.delete_session(session_key);
                self.calls.delete_session(session_key);
            }
        }
    }
}

/// The caches every socket shares (upstream's `defaultWebsocketToolOutputCache`,
/// `defaultWebsocketToolCallCache` and `defaultWebsocketToolSessionRefs`).
static CACHES: LazyLock<Mutex<ToolCaches>> = LazyLock::new(Mutex::default);

/// The shared caches, locked.
pub(super) fn caches() -> MutexGuard<'static, ToolCaches> {
    CACHES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The key the client's tool items are kept under: its request ID, or the
/// session ID it sent (`websocketDownstreamSessionKey`).
pub(super) fn session_key(headers: &HeaderMap) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
            .unwrap_or_default()
    };
    let request_id = header("x-client-request-id");
    if !request_id.is_empty() {
        return request_id;
    }
    let metadata = header("x-codex-turn-metadata");
    if !metadata.is_empty() && json::valid(metadata.as_bytes()) {
        let session_id = json::get(metadata.as_bytes(), "session_id")
            .map(|id| id.str().trim().to_owned())
            .unwrap_or_default();
        if !session_id.is_empty() {
            return session_id;
        }
    }
    let session_id = header("session-id");
    if !session_id.is_empty() {
        return session_id;
    }
    header("session_id")
}

/// An input item and what dedupe and repair read from it
/// (`responsesWebsocketInputItem`).
#[derive(Clone, Debug, Default)]
pub(super) struct InputItem {
    pub(super) raw: Vec<u8>,
    pub(super) item_type: String,
    pub(super) id: String,
    pub(super) call_id: String,
}

impl InputItem {
    /// Reads `raw`'s `type`, `id` and `call_id`, matching keys as Go's
    /// `encoding/json` does: case-insensitively, the last one winning
    /// (`parseResponsesWebsocketInputItem`).
    pub(super) fn parse(raw: &[u8]) -> Self {
        let mut item = Self {
            raw: raw.to_vec(),
            ..Self::default()
        };
        let trimmed = go::trim_space(raw);
        if trimmed.first() != Some(&b'{') || !json::valid(trimmed) {
            return item;
        }
        let Some(object) = Val::parse(trimmed) else {
            return item;
        };
        for (key, value) in object.members() {
            if json::fold_eq(&key, "type") {
                item.item_type = metadata_string(value.raw);
            } else if json::fold_eq(&key, "id") {
                item.id = metadata_string(value.raw);
            } else if json::fold_eq(&key, "call_id") {
                item.call_id = metadata_string(value.raw);
            }
        }
        item
    }
}

/// A metadata value as text: a string unescaped, `null` as empty, and
/// anything else as written, trimmed (`responsesWebsocketMetadataString`).
pub(super) fn metadata_string(raw: &[u8]) -> String {
    let raw = go::trim_space(raw);
    if raw.is_empty() || raw == b"null" {
        return String::new();
    }
    if raw[0] == b'"' && json::valid(raw) {
        return Val { raw, index: 0 }.str().trim().to_owned();
    }
    String::from_utf8_lossy(raw).trim().to_owned()
}

/// Whether `item_type` is a tool call (`isResponsesToolCallType`).
pub(super) fn is_tool_call(item_type: &str) -> bool {
    matches!(item_type.trim(), "function_call" | "custom_tool_call")
}

/// Whether `item_type` is a tool call's output
/// (`isResponsesToolCallOutputType`).
pub(super) fn is_tool_output(item_type: &str) -> bool {
    matches!(
        item_type.trim(),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// Whether `item` is a whole tool call: an object with a `call_id`, a
/// `name`, and its `arguments` or `input`, all strings
/// (`isCompleteResponsesWebsocketToolCall`).
pub(super) fn is_complete_tool_call(item: Option<Val<'_>>) -> bool {
    let Some(item) = item.filter(Val::is_object) else {
        return false;
    };
    let non_empty_string = |key: &str| {
        item.get(key)
            .is_some_and(|value| value.is_string() && !value.str().trim().is_empty())
    };
    if !non_empty_string("call_id") || !non_empty_string("name") {
        return false;
    }
    let is_string = |key: &str| item.get(key).is_some_and(|value| value.is_string());
    match item
        .get("type")
        .map(|kind| kind.str())
        .as_deref()
        .map(str::trim)
    {
        Some("function_call") => is_string("arguments"),
        Some("custom_tool_call") => is_string("input"),
        _ => false,
    }
}

/// The tool items a turn saw, kept back until the turn succeeds
/// (`responsesWebsocketToolCacheTurn`).
#[derive(Debug)]
pub(super) struct ToolCacheTurn {
    session_key: String,
    outputs: HashMap<String, Vec<u8>>,
    output_order: Vec<String>,
    calls: HashMap<String, Vec<u8>>,
    call_order: Vec<String>,
}

impl ToolCacheTurn {
    /// A turn for `session_key`, or `None` without one
    /// (`newResponsesWebsocketToolCacheTurn`).
    pub(super) fn new(session_key: &str) -> Option<Self> {
        let session_key = session_key.trim();
        (!session_key.is_empty()).then(|| Self {
            session_key: session_key.to_owned(),
            outputs: HashMap::new(),
            output_order: Vec::new(),
            calls: HashMap::new(),
            call_order: Vec::new(),
        })
    }

    /// Notes the whole tool calls in an event (`recordResponse`).
    pub(super) fn record_response(&mut self, payload: &[u8]) {
        for item in complete_tool_calls(payload) {
            let kind = item.get("type").map(|kind| kind.str()).unwrap_or_default();
            let call_id = item.get("call_id").map(|id| id.str()).unwrap_or_default();
            self.record_raw_item(&kind, &call_id, item.raw);
        }
    }

    /// Notes an input item, when it is a tool call or output
    /// (`recordInputItem`).
    pub(super) fn record_input_item(&mut self, item: &InputItem) {
        self.record_raw_item(&item.item_type, &item.call_id, &item.raw);
    }

    fn record_raw_item(&mut self, item_type: &str, call_id: &str, raw: &[u8]) {
        let output = is_tool_output(item_type);
        if !output && !is_tool_call(item_type) {
            return;
        }
        let call_id = call_id.trim();
        if call_id.is_empty() || go::trim_space(raw).is_empty() {
            return;
        }
        let (items, order) = if output {
            (&mut self.outputs, &mut self.output_order)
        } else {
            (&mut self.calls, &mut self.call_order)
        };
        if items.insert(call_id.to_owned(), raw.to_vec()).is_none() {
            order.push(call_id.to_owned());
        }
    }

    /// Keeps what the turn saw in the shared caches (`commit`).
    pub(super) fn commit(self) {
        let mut caches = caches();
        for call_id in &self.output_order {
            caches
                .outputs
                .record(&self.session_key, call_id, &self.outputs[call_id]);
        }
        for call_id in &self.call_order {
            caches
                .calls
                .record(&self.session_key, call_id, &self.calls[call_id]);
        }
    }
}

/// The whole tool calls an event carries: a `response.completed`'s output, or
/// an `response.output_item.added` or `.done`'s item.
fn complete_tool_calls(payload: &[u8]) -> Vec<Val<'_>> {
    let kind = json::get(payload, "type")
        .map(|kind| kind.str())
        .unwrap_or_default();
    match kind.trim() {
        "response.completed" => json::get(payload, "response.output")
            .filter(Val::is_array)
            .map(|output| output.array())
            .unwrap_or_default()
            .into_iter()
            .filter(|item| is_complete_tool_call(Some(*item)))
            .collect(),
        "response.output_item.added" | "response.output_item.done" => {
            let item = json::get(payload, "item");
            if is_complete_tool_call(item) {
                item.into_iter().collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Keeps the whole tool calls in an event in `cache`
/// (`recordResponsesWebsocketToolCallsFromPayloadWithCache`).
pub(super) fn record_tool_calls_from_payload(
    cache: &mut ToolCache,
    session_key: &str,
    payload: &[u8],
) {
    if session_key.trim().is_empty() || payload.is_empty() {
        return;
    }
    for item in complete_tool_calls(payload) {
        let call_id = item.get("call_id").map(|id| id.str()).unwrap_or_default();
        cache.record(session_key, call_id.trim(), item.raw);
    }
}

/// Repairs a request bound for HTTP with the shared caches, noting its tool
/// items in a turn that commits only if the request succeeds
/// (`prepareResponsesWebsocketFallbackTurn`).
pub(super) fn prepare_fallback_turn(
    session_key: &str,
    payload: &[u8],
) -> (Vec<u8>, Option<ToolCacheTurn>) {
    let mut turn = ToolCacheTurn::new(session_key);
    let repaired = repair(&mut caches(), session_key, payload, false, turn.as_mut());
    (repaired, turn)
}

/// Repairs `payload`'s tool calls: puts back the call or output a pair lost
/// when `caches` have it, drops orphans, and dedupes items by ID. With
/// `record`, the request's own tool items are kept in `caches` first
/// (`repairResponsesWebsocketToolCallsWithCachesMode`).
pub(super) fn repair(
    caches: &mut ToolCaches,
    session_key: &str,
    payload: &[u8],
    record: bool,
    turn: Option<&mut ToolCacheTurn>,
) -> Vec<u8> {
    let Some((input, previous_response_id)) = parse_repair_request(payload) else {
        return payload.to_vec();
    };
    let raw_items = input.array();
    let items: Vec<InputItem> = raw_items
        .iter()
        .map(|item| InputItem::parse(item.raw))
        .collect();
    let session_key = session_key.trim();
    let updated = if session_key.is_empty() {
        dedupe_input_items(items)
    } else {
        let allow_orphans = !metadata_string(previous_response_id.unwrap_or_default()).is_empty();
        repair_items(caches, session_key, items, allow_orphans, record, turn)
    };
    let unchanged = updated.len() == raw_items.len()
        && updated
            .iter()
            .zip(&raw_items)
            .all(|(item, raw)| item.raw == raw.raw);
    if unchanged {
        return payload.to_vec();
    }
    let raws: Vec<&[u8]> = updated.iter().map(|item| item.raw.as_slice()).collect();
    let Some(replacement) = json::compact_html(&raws) else {
        return payload.to_vec();
    };
    let mut out = Vec::with_capacity(payload.len() - input.raw.len() + replacement.len());
    out.extend_from_slice(&payload[..input.index]);
    out.extend_from_slice(&replacement);
    out.extend_from_slice(&payload[input.index + input.raw.len()..]);
    out
}

/// A repairable request's input array and its `previous_response_id` as
/// written, matching keys as Go's `encoding/json` does
/// (`parseResponsesWebsocketRepairRequest`).
fn parse_repair_request(payload: &[u8]) -> Option<(Val<'_>, Option<&[u8]>)> {
    if !json::valid(payload) {
        return None;
    }
    let root = Val::parse(payload).filter(Val::is_object)?;
    let mut input = None;
    let mut previous_response_id = None;
    for (key, value) in root.members() {
        if json::fold_eq(&key, "input") {
            if !value.is_array() && !value.is_null() {
                return None;
            }
            input = Some(value);
        } else if json::fold_eq(&key, "previous_response_id") {
            previous_response_id = Some(value.raw);
        }
    }
    let input = input.filter(Val::is_array)?;
    Some((input, previous_response_id))
}

/// Pairs tool calls with their outputs (`repairResponsesToolCallItems`).
fn repair_items(
    caches: &mut ToolCaches,
    session_key: &str,
    items: Vec<InputItem>,
    allow_orphans: bool,
    record: bool,
    mut turn: Option<&mut ToolCacheTurn>,
) -> Vec<InputItem> {
    let mut output_present: HashSet<String> = HashSet::new();
    let mut call_present: HashSet<String> = HashSet::new();
    for item in &items {
        if let Some(turn) = turn.as_deref_mut() {
            turn.record_input_item(item);
        }
        if item.call_id.is_empty() {
            continue;
        }
        if is_tool_output(&item.item_type) {
            output_present.insert(item.call_id.clone());
            if record {
                caches.outputs.record(session_key, &item.call_id, &item.raw);
            }
        } else if is_tool_call(&item.item_type) {
            call_present.insert(item.call_id.clone());
            if record {
                caches.calls.record(session_key, &item.call_id, &item.raw);
            }
        }
    }
    let mut filtered = Vec::with_capacity(items.len());
    let mut inserted_calls: HashSet<String> = HashSet::new();
    for item in items {
        if is_tool_output(&item.item_type) {
            if item.call_id.is_empty() {
                // Codex sends named results for heartbeats and delegation
                // with no call before them.
                let named = item.item_type == "function_call_output"
                    && json::get(&item.raw, "name")
                        .is_some_and(|name| name.is_string() && !name.str().trim().is_empty());
                if named {
                    filtered.push(item);
                }
                continue;
            }
            if call_present.contains(&item.call_id) || allow_orphans {
                filtered.push(item);
                continue;
            }
            if let Some(cached) = caches.calls.get(session_key, &item.call_id) {
                if inserted_calls.insert(item.call_id.clone()) {
                    filtered.push(InputItem::parse(cached));
                    call_present.insert(item.call_id.clone());
                }
                filtered.push(item);
            }
            continue;
        }
        if !is_tool_call(&item.item_type) {
            filtered.push(item);
            continue;
        }
        if item.call_id.is_empty() {
            continue;
        }
        if output_present.contains(&item.call_id) || allow_orphans {
            filtered.push(item);
            continue;
        }
        if let Some(cached) = caches.outputs.get(session_key, &item.call_id) {
            let cached = InputItem::parse(cached);
            output_present.insert(item.call_id.clone());
            filtered.push(item);
            filtered.push(cached);
        }
    }
    dedupe_input_items(filtered)
}

/// Keeps one item per ID: the last, unless that would drop a call an output
/// still refers to for one that none does
/// (`dedupeResponsesWebsocketInputItems`).
pub(super) fn dedupe_input_items(items: Vec<InputItem>) -> Vec<InputItem> {
    let referenced: HashSet<&str> = items
        .iter()
        .filter(|item| {
            matches!(
                item.item_type.as_str(),
                "function_call_output" | "custom_tool_call_output"
            ) && !item.call_id.is_empty()
        })
        .map(|item| item.call_id.as_str())
        .collect();
    let mut keep: HashMap<&str, (usize, bool)> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if item.id.is_empty() {
            continue;
        }
        let is_referenced = !item.call_id.is_empty() && referenced.contains(item.call_id.as_str());
        match keep.get(item.id.as_str()) {
            Some(&(_, kept_referenced)) if !is_referenced && kept_referenced => {}
            _ => {
                keep.insert(item.id.as_str(), (index, is_referenced));
            }
        }
    }
    let keep: HashMap<String, usize> = keep
        .into_iter()
        .map(|(id, (index, _))| (id.to_owned(), index))
        .collect();
    items
        .into_iter()
        .enumerate()
        .filter(|(index, item)| item.id.is_empty() || keep.get(&item.id) == Some(index))
        .map(|(_, item)| item)
        .collect()
}
