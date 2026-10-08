// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_response.go
// (sanitizeXAIInputEncryptedContent, normalizeXAIInputReasoningItems,
// mergeAdjacentXAIInputReasoningSummaries, canMergeXAIReasoningSummary,
// appendXAIReasoningSummary, xaiNormalizeReasoningSummaryEventLine,
// xaiNormalizeReasoningSummaryEventName, xaiNormalizeReasoningSummaryData,
// xaiNormalizeReasoningSummaryDataEvents, xaiNormalizeReasoningSummaryIndex,
// xaiNormalizeReasoningOutputItems, xaiNormalizeReasoningOutputItem,
// xaiNormalizeReasoningSummaryItems) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reasoning, both ways.
//!
//! In a request's input, a reasoning item's `null` `content` and
//! `encrypted_content` are dropped, and so is an `encrypted_content` that
//! doesn't have the shape of xAI's (another provider's signature, say, from
//! a conversation that changed models), taking a compaction item with it.
//! A reasoning item with nothing but a summary then joins the reasoning item
//! before it.
//!
//! xAI streams its reasoning as `reasoning_text`, where OpenAI's Responses
//! API has a summary, so its events become the summary ones:
//! `response.reasoning_text.delta` becomes
//! `response.reasoning_summary_text.delta`; `response.reasoning_text.done`
//! becomes `response.reasoning_summary_text.done` followed by
//! `response.reasoning_summary_part.done`; a `reasoning_text` content part's
//! events become summary part events; `content_index` becomes
//! `summary_index`; and a reasoning item's `reasoning_text` content becomes
//! its summary.
//!
//! Deviations from upstream:
//! - A changed body or event is written again by `serde_json`, where sjson
//!   edits it in place; keys keep their order.

use open_ferry_translate::go::trim_space;
use open_ferry_translate::signature::inspect_grok_encrypted_content;
use serde_json::Value;

use super::response::{parse, write};
use crate::json::{self, get, set, str_of};

/// A reasoning item's type.
const REASONING: &str = "reasoning";

/// xAI's reasoning content part type.
const REASONING_TEXT: &str = "reasoning_text";

/// OpenAI's reasoning summary part type.
const SUMMARY_TEXT: &str = "summary_text";

/// The tag of an SSE event line (`xaiEventTag`).
const EVENT_TAG: &[u8] = b"event:";

/// `normalizeXAIInputReasoningItems`: drops a reasoning input item's `null`
/// `content` and `encrypted_content`, then joins summary-only reasoning
/// items to the one before.
pub(crate) fn normalize_input_reasoning_items(body: &mut Value) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    for item in input.iter_mut() {
        if str_of(item.get("type")) != REASONING {
            continue;
        }
        for key in ["content", "encrypted_content"] {
            if item.get(key).is_some_and(Value::is_null) {
                json::delete(item, key);
            }
        }
    }
    merge_adjacent_summaries(body);
}

/// Why an item's `encrypted_content` can't go to xAI, if it can't.
fn invalid_encrypted_content(content: &Value) -> Option<String> {
    let kind = match content {
        Value::String(text) => {
            return inspect_grok_encrypted_content(text)
                .err()
                .map(|error| error.to_string());
        }
        Value::Null => return Some("encrypted_content is null".to_owned()),
        // gjson's names for the types.
        Value::Bool(false) => "False",
        Value::Bool(true) => "True",
        Value::Number(_) => "Number",
        Value::Array(_) | Value::Object(_) => "JSON",
    };
    Some(format!("encrypted_content must be a string, got {kind}"))
}

/// `sanitizeXAIInputEncryptedContent`: drops each reasoning input item's
/// `encrypted_content`, and each compaction input item, whose
/// `encrypted_content` isn't a string with the shape of xAI's. When it drops
/// any, summary-only reasoning items join the one before.
pub(crate) fn sanitize_input_encrypted_content(body: &mut Value) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    let mut dropped = 0_usize;
    let mut first: Option<(String, String)> = None;
    for mut item in std::mem::take(input) {
        let item_type = str_of(item.get("type")).trim().to_owned();
        if item_type != REASONING && item_type != "compaction" {
            input.push(item);
            continue;
        }
        let Some(reason) = item
            .get("encrypted_content")
            .and_then(invalid_encrypted_content)
        else {
            input.push(item);
            continue;
        };
        dropped += 1;
        let is_reasoning = item_type == REASONING;
        first.get_or_insert((item_type, reason));
        if is_reasoning {
            json::delete(&mut item, "encrypted_content");
            input.push(item);
        }
    }
    let Some((first_item_type, first_reason)) = first else {
        return;
    };
    tracing::debug!(
        component = "xai_encrypted_content_sanitizer",
        dropped,
        first_item_type = %first_item_type,
        first_reason = %first_reason,
        "xai executor: removed invalid encrypted_content before upstream"
    );
    merge_adjacent_summaries(body);
}

/// `mergeAdjacentXAIInputReasoningSummaries`: appends the summary of each
/// reasoning input item that has nothing but a summary to the reasoning
/// item before it.
fn merge_adjacent_summaries(body: &mut Value) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    for item in std::mem::take(input) {
        if let Some(previous) = input.last_mut()
            && can_merge(previous, &item)
            && let (Some(Value::Array(summary)), Some(Value::Array(more))) =
                (previous.get_mut("summary"), item.get("summary"))
        {
            summary.extend(more.iter().cloned());
            continue;
        }
        input.push(item);
    }
}

/// `canMergeXAIReasoningSummary`: both are reasoning items with a summary,
/// and `current` has nothing but its type and a non-empty summary.
fn can_merge(previous: &Value, current: &Value) -> bool {
    if str_of(previous.get("type")) != REASONING || str_of(current.get("type")) != REASONING {
        return false;
    }
    if !previous.get("summary").is_some_and(Value::is_array) {
        return false;
    }
    let Some(Value::Array(summary)) = current.get("summary") else {
        return false;
    };
    !summary.is_empty()
        && current
            .as_object()
            .is_some_and(|fields| fields.keys().all(|key| key == "type" || key == "summary"))
}

/// `xaiNormalizeReasoningSummaryEventName`.
fn normalize_event_name(name: &[u8]) -> &[u8] {
    match name {
        b"response.reasoning_text.delta" => b"response.reasoning_summary_text.delta",
        b"response.reasoning_text.done" => b"response.reasoning_summary_part.done",
        other => other,
    }
}

/// `xaiNormalizeReasoningSummaryEventLine`: an `event:` line naming
/// `event_name`, or, when that is empty, the line's own event, renamed as
/// its data is. A line with neither comes back as it is.
pub(crate) fn normalize_summary_event_line(line: &[u8], event_name: &str) -> Vec<u8> {
    let name = if event_name.is_empty() {
        line.strip_prefix(EVENT_TAG)
            .map(trim_space)
            .unwrap_or_default()
    } else {
        event_name.as_bytes()
    };
    let name = normalize_event_name(name);
    if name.is_empty() {
        return line.to_vec();
    }
    [b"event: ".as_slice(), name].concat()
}

/// `xaiNormalizeReasoningSummaryIndex`: `content_index` becomes
/// `summary_index`, unless there is one.
fn normalize_summary_index(event: &mut Value) {
    if let Some(index) = event.get("content_index").cloned()
        && event.get("summary_index").is_none()
    {
        set(event, "summary_index", index);
    }
    json::delete(event, "content_index");
}

/// `xaiNormalizeReasoningSummaryData`: an event's data with xAI's reasoning
/// text renamed to OpenAI's summary. Anything else comes back as it is.
pub(crate) fn normalize_summary_data(data: Vec<u8>) -> Vec<u8> {
    if data.is_empty() {
        return data;
    }
    let Some(mut event) = parse(&data) else {
        return data;
    };
    if normalize_summary_event(&mut event) {
        write(&event, data)
    } else {
        data
    }
}

/// `xaiNormalizeReasoningSummaryDataEvents`: [`normalize_summary_data`],
/// except that `response.reasoning_text.done` becomes two events,
/// `response.reasoning_summary_text.done` (keeping its `text`) and then
/// `response.reasoning_summary_part.done`.
pub(crate) fn normalize_summary_data_events(data: Vec<u8>) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return vec![data];
    }
    let Some(mut event) = parse(&data) else {
        return vec![data];
    };
    if str_of(event.get("type")) != "response.reasoning_text.done" {
        return vec![if normalize_summary_event(&mut event) {
            write(&event, data)
        } else {
            data
        }];
    }
    let mut text_done = event.clone();
    set(
        &mut text_done,
        "type",
        Value::from("response.reasoning_summary_text.done"),
    );
    normalize_summary_index(&mut text_done);
    normalize_summary_event(&mut event);
    vec![write(&text_done, Vec::new()), write(&event, data)]
}

/// [`normalize_summary_data`] on parsed data. Returns whether it changed.
fn normalize_summary_event(event: &mut Value) -> bool {
    let mut changed = true;
    match str_of(event.get("type")).as_str() {
        "response.reasoning_text.delta" => {
            set(
                event,
                "type",
                Value::from("response.reasoning_summary_text.delta"),
            );
            normalize_summary_index(event);
        }
        "response.reasoning_text.done" => {
            set(
                event,
                "type",
                Value::from("response.reasoning_summary_part.done"),
            );
            set(event, "part.type", Value::from(SUMMARY_TEXT));
            if let Some(text) = event.get("text") {
                let text = str_of(Some(text));
                set(event, "part.text", Value::String(text));
            }
            json::delete(event, "text");
            normalize_summary_index(event);
        }
        kind @ ("response.content_part.added" | "response.content_part.done")
            if str_of(get(event, "part.type")) == REASONING_TEXT =>
        {
            let renamed = if kind == "response.content_part.added" {
                "response.reasoning_summary_part.added"
            } else {
                "response.reasoning_summary_part.done"
            };
            set(event, "type", Value::from(renamed));
            set(event, "part.type", Value::from(SUMMARY_TEXT));
            normalize_summary_index(event);
        }
        _ => changed = false,
    }
    if let Some(item) = event.get_mut("item") {
        changed |= normalize_output_item(item);
    }
    if let Some(Value::Array(output)) = json::get_mut(event, "response.output") {
        for item in output {
            changed |= normalize_output_item(item);
        }
    }
    changed
}

/// `xaiNormalizeReasoningOutputItem`: a reasoning item's `reasoning_text`
/// summary parts become `summary_text`, and its `reasoning_text` content,
/// if any, becomes its summary in place of its content. Returns whether it
/// changed.
pub(crate) fn normalize_output_item(item: &mut Value) -> bool {
    if str_of(item.get("type")) != REASONING {
        return false;
    }
    let mut changed = false;
    if let Some(Value::Array(summary)) = item.get_mut("summary") {
        changed |= rename_summary_parts(summary);
    }
    let Some(Value::Array(content)) = item.get("content") else {
        return changed;
    };
    let mut parts: Vec<Value> = content
        .iter()
        .filter(|part| str_of(part.get("type")) == REASONING_TEXT)
        .cloned()
        .collect();
    if parts.is_empty() {
        return changed;
    }
    rename_summary_parts(&mut parts);
    set(item, "summary", Value::Array(parts));
    json::delete(item, "content");
    true
}

/// `xaiNormalizeReasoningSummaryItems`: each `reasoning_text` part becomes
/// `summary_text`. Returns whether any did.
fn rename_summary_parts(parts: &mut [Value]) -> bool {
    let mut changed = false;
    for part in parts {
        if str_of(part.get("type")) == REASONING_TEXT {
            changed |= set(part, "type", Value::from(SUMMARY_TEXT));
        }
    }
    changed
}

#[cfg(test)]
mod tests;
