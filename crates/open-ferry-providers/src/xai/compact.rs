// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_execute.go
// (executeCompactRequest's body, xaiInputHasItemType,
// xaiRemoveInputItemsByType, xaiBuildCompactionTriggerStreamChunks,
// xaiBuildCompactionBaseResponse, xaiCompactionOutputItem,
// xaiCompactionResponseID, xaiCompactionItemID, xaiBuildSSEFrame)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Compact calls: the body sent to `/responses/compact`, and the stream a
//! `compaction_trigger` request gets back.
//!
//! A compact call sends no tools, output limit, sampling settings or
//! `compaction_trigger` items, and keeps the client's
//! `previous_response_id`, which other calls drop. Its JSON answer becomes,
//! for a streaming request, the six events of one response whose output is
//! the compaction item: `response.created`, `response.in_progress`,
//! `response.output_item.added`, a `keepalive`, `response.output_item.done`
//! and `response.completed`.
//!
//! Deviations from upstream:
//! - A changed body or event is written by `serde_json`; keys keep their
//!   order.

use std::time::SystemTime;

use open_ferry_translate::json::exact;
use serde_json::{Value, json};

use super::request::Prepared;
use super::tools;
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::{delete, get, int_of, set, str_of};

/// The input item that asks a streaming request to compact.
pub(crate) const COMPACTION_TRIGGER: &str = "compaction_trigger";

/// Fields a compact call never sends.
const DROPPED_FIELDS: [&str; 7] = [
    "stream",
    "tools",
    "max_output_tokens",
    "temperature",
    "top_p",
    "top_k",
    "stop",
];

/// The fields of the request that the compaction stream's responses
/// repeat.
const ECHOED_FIELDS: [&str; 15] = [
    "instructions",
    "max_output_tokens",
    "max_tool_calls",
    "parallel_tool_calls",
    "previous_response_id",
    "prompt_cache_key",
    "reasoning",
    "text",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
    "metadata",
];

/// Whether `payload`'s input holds an item of `item_type`
/// (`xaiInputHasItemType`).
pub(crate) fn input_has_item_type(payload: &[u8], item_type: &str) -> bool {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .as_ref()
        .and_then(|payload| payload.get("input"))
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| str_of(item.get("type")) == item_type)
        })
}

/// Drops the input items of `item_type` (`xaiRemoveInputItemsByType`).
pub(crate) fn remove_input_items_by_type(body: &mut Value, item_type: &str) {
    if let Some(Value::Array(input)) = body.get_mut("input") {
        input.retain(|item| str_of(item.get("type")) != item_type);
    }
}

/// Adjusts a prepared OpenAI Responses body for `/responses/compact`
/// (`executeCompactRequest`); `payload` is the client's request.
pub(crate) fn shape_body(body: &mut Value, payload: &[u8]) {
    delete(body, "stream");
    delete(body, "tools");
    // The tools are gone, so a choice of one of them goes too.
    tools::normalize_tool_choice_for_tools(body);
    for field in DROPPED_FIELDS {
        delete(body, field);
    }
    remove_input_items_by_type(body, COMPACTION_TRIGGER);
    // Read keeping each number's text, so a numeric ID is sent as gjson's
    // `String` gives it: `-0` stays `-0`.
    let payload = exact::from_slice(payload).unwrap_or(Value::Null);
    let previous = str_of(get(&payload, "previous_response_id"));
    let previous = previous.trim();
    if !previous.is_empty() {
        set(body, "previous_response_id", Value::from(previous));
    }
}

/// Seconds and nanoseconds since the Unix epoch.
fn unix(now: SystemTime) -> (i64, u128) {
    let since = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        since.as_nanos(),
    )
}

/// The compaction stream's response ID (`xaiCompactionResponseID`): the
/// compaction's own, as a response ID, or one made of the time.
pub(crate) fn response_id(compact: &Value, now: SystemTime) -> String {
    let id = str_of(get(compact, "id"));
    let id = id.trim();
    if id.is_empty() {
        return format!("resp_xai_compaction_{}", unix(now).1);
    }
    if id.starts_with("resp_") {
        return id.to_owned();
    }
    format!("resp_{}", id.strip_prefix("cmp_").unwrap_or(id))
}

/// The compaction item's ID for `response_id` (`xaiCompactionItemID`).
fn item_id(response_id: &str) -> String {
    match response_id.strip_prefix("resp_") {
        Some(suffix) if !suffix.is_empty() => format!("cmp_{suffix}"),
        _ => format!("cmp_{response_id}"),
    }
}

/// The compaction's first output item, with a type and an ID
/// (`xaiCompactionOutputItem`).
pub(crate) fn output_item(compact: &Value, response_id: &str) -> Value {
    let mut item = match get(compact, "output.0") {
        Some(item @ (Value::Object(_) | Value::Array(_))) => item.clone(),
        _ => json!({"type": "compaction"}),
    };
    if get(&item, "type").is_none() {
        set(&mut item, "type", Value::from("compaction"));
    }
    if get(&item, "id").is_none() {
        set(&mut item, "id", Value::from(item_id(response_id)));
    }
    item
}

/// A response of the compaction stream (`xaiBuildCompactionBaseResponse`).
fn base_response(
    prepared: &Prepared,
    compact: &Value,
    response_id: &str,
    created_at: i64,
    status: &str,
) -> Value {
    let mut response = json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": null,
        "output": [],
    });
    let model = str_of(get(compact, "model"));
    if !model.is_empty() {
        set(&mut response, "model", Value::from(model));
    } else if !prepared.base_model.is_empty() {
        set(
            &mut response,
            "model",
            Value::from(prepared.base_model.as_str()),
        );
    }
    for field in ECHOED_FIELDS {
        if let Some(value) = get(&prepared.body, field) {
            set(&mut response, field, value.clone());
        }
    }
    response
}

/// One SSE frame (`xaiBuildSSEFrame`).
fn sse_frame(event: &str, data: &[u8]) -> Vec<u8> {
    [b"event: ", event.as_bytes(), b"\ndata: ", data, b"\n\n"].concat()
}

/// The six frames a `compaction_trigger` request gets for the compact
/// call's answer `data` (`xaiBuildCompactionTriggerStreamChunks`).
pub(crate) fn trigger_stream_chunks(
    prepared: &Prepared,
    data: &[u8],
    now: SystemTime,
) -> Vec<Vec<u8>> {
    let compact: Value = serde_json::from_slice(data).unwrap_or(Value::Null);
    let response_id = response_id(&compact, now);
    let (now_secs, _) = unix(now);
    let at = |field: &str| match int_of(get(&compact, field)) {
        0 => now_secs,
        time => time,
    };
    let created_at = at("created_at");
    let completed_at = at("completed_at");
    let item = output_item(&compact, &response_id);

    let mut created = base_response(prepared, &compact, &response_id, created_at, "in_progress");
    let mut in_progress = created.clone();
    let mut completed = base_response(prepared, &compact, &response_id, created_at, "completed");
    let mut model = str_of(get(&prepared.original, "model"));
    if model.is_empty() {
        model.clone_from(&prepared.base_model);
    }
    if model.is_empty() {
        model = str_of(get(&compact, "model"));
    }
    if !model.is_empty() {
        set(&mut created, "model", Value::from(model.as_str()));
        set(&mut in_progress, "model", Value::from(model));
    }
    set(&mut completed, "completed_at", Value::from(completed_at));
    set(&mut completed, "output", Value::Array(vec![item.clone()]));
    if let Some(usage) = get(&compact, "usage") {
        set(&mut completed, "usage", usage.clone());
    }

    let created = json!({"type": "response.created", "sequence_number": 0, "response": created});
    let in_progress =
        json!({"type": "response.in_progress", "sequence_number": 1, "response": in_progress});
    let added = json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0, "item": item});
    let keepalive = json!({"type": "keepalive", "sequence_number": 3});
    let done = json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": item});
    let completed =
        json!({"type": "response.completed", "sequence_number": 5, "response": completed});
    let frame = |event: &str, data: &Value| sse_frame(event, data.to_string().as_bytes());
    vec![
        frame("response.created", &created),
        frame("response.in_progress", &in_progress),
        frame("response.output_item.added", &added),
        frame("keepalive", &keepalive),
        frame("response.output_item.done", &done),
        sse_frame(
            "response.completed",
            &ensure_responses_usage_details(completed.to_string().into_bytes()),
        ),
    ]
}

#[cfg(test)]
mod tests;
