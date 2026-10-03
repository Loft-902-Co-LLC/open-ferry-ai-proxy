// Ported from CLIProxyAPI internal/translator/common/gemini.go (IsGeminiThoughtPart,
// MergeAdjacentGeminiContents, ContentHasGeminiFunctionResponse, ReorderGeminiUserParts,
// MergeAdjacentGeminiUserContents, ContainsJSONRef, SetGeminiFunctionResponseResult and
// SetGeminiFunctionResponseRaw) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for the `contents` of Gemini requests, shared by the translators
//! that build them.
//!
//! Gemini wants user and model turns to alternate, and rejects a user turn
//! whose text comes after a function response. These helpers merge
//! consecutive user turns and move text ahead of function responses. Model
//! turns are never merged: that would shift part indices, which thought
//! signatures depend on.
//!
//! Upstream's `SplitGeminiFunctionResponseTurns` and
//! `ContentHasGeminiFunctionCall` are not ported: only the Antigravity
//! translators call them.
//!
//! Deviations from upstream:
//! - Turns and parts are parsed JSON rather than raw bytes, so the empty turn
//!   upstream skips can't occur.
//! - Where a function result holding a `$ref` is stored as a string,
//!   [`set_gemini_function_response_result`] writes it as compact JSON;
//!   upstream copies the client's raw text. [`set_gemini_function_response_raw`]
//!   stores the text it is given, trimmed, as upstream does.
//! - [`set_gemini_function_response_raw`] stores `""` for text that isn't
//!   valid JSON. Upstream stores whatever gjson makes of it, which can leave
//!   the request invalid JSON.

use serde_json::Value;

use crate::json::{bool_of, set_path, str_of};

/// Reports whether a Gemini part holds the model's hidden thoughts.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "used by the translators from Gemini clients")
)]
pub(crate) fn is_gemini_thought_part(part: &Value) -> bool {
    part.get("thought").is_some_and(bool_of)
}

/// Merges consecutive user turns into one, moving their text ahead of their
/// function responses ([`reorder_gemini_user_parts`]). Turns without parts are
/// dropped, unless there is only one turn. Model turns are kept apart, so the
/// part indices thought signatures refer to don't move.
///
/// Claude requests can carry system messages mid-conversation, which become
/// user reminder turns; this joins them to the user turns around them.
pub(crate) fn merge_adjacent_gemini_contents(contents: Vec<Value>) -> Vec<Value> {
    merge_user_turns(contents, true, |_, _| true)
}

/// Merges consecutive user turns into one, like
/// [`merge_adjacent_gemini_contents`], but leaves turns holding a function
/// response apart, and keeps parts in their order.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "used by the OpenAI Responses to Gemini translator"
    )
)]
pub(crate) fn merge_adjacent_gemini_user_contents(contents: Vec<Value>) -> Vec<Value> {
    merge_user_turns(contents, false, |last, content| {
        !content_has_gemini_function_response(last)
            && !content_has_gemini_function_response(content)
    })
}

/// Joins each user turn onto a user turn before it, when `may_merge` agrees.
fn merge_user_turns(
    contents: Vec<Value>,
    reorder: bool,
    may_merge: impl Fn(&Value, &Value) -> bool,
) -> Vec<Value> {
    if contents.len() <= 1 {
        return contents;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    for content in contents {
        if !matches!(content.get("parts"), Some(Value::Array(parts)) if !parts.is_empty()) {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && str_of(last.get("role")) == "user"
            && str_of(content.get("role")) == "user"
            && may_merge(last, &content)
        {
            let mut parts = match last.get_mut("parts").map(Value::take) {
                Some(Value::Array(parts)) => parts,
                _ => Vec::new(),
            };
            if let Some(Value::Array(more)) = content.get("parts") {
                parts.extend(more.iter().cloned());
            }
            if reorder {
                parts = reorder_gemini_user_parts(parts);
            }
            last["parts"] = Value::Array(parts);
            continue;
        }
        merged.push(content);
    }
    merged
}

/// Reports whether a Gemini turn holds a function response part
/// (`functionResponse` or `function_response`).
pub(crate) fn content_has_gemini_function_response(content: &Value) -> bool {
    match content.get("parts") {
        Some(Value::Array(parts)) => parts.iter().any(is_function_response),
        Some(Value::Object(parts)) => parts.values().any(is_function_response),
        _ => false,
    }
}

fn is_function_response(part: &Value) -> bool {
    part.get("functionResponse").is_some() || part.get("function_response").is_some()
}

/// Moves the text parts of a user turn ahead of its other parts, keeping each
/// group's order, when text follows a function response. Vertex AI rejects a
/// turn with text after a function response.
pub(crate) fn reorder_gemini_user_parts(parts: Vec<Value>) -> Vec<Value> {
    let mut seen_response = false;
    let text_follows_response = parts.iter().any(|part| {
        if is_function_response(part) {
            seen_response = true;
            false
        } else {
            seen_response && part.get("text").is_some()
        }
    });
    if !text_follows_response {
        return parts;
    }
    let (mut text, other): (Vec<Value>, Vec<Value>) = parts
        .into_iter()
        .partition(|part| part.get("text").is_some());
    text.extend(other);
    text
}

/// Reports whether `value` holds, at any depth, a `$ref` key with a string
/// value.
pub(crate) fn contains_json_ref(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields
            .iter()
            .any(|(key, child)| (key == "$ref" && child.is_string()) || contains_json_ref(child)),
        Value::Array(items) => items.iter().any(contains_json_ref),
        _ => false,
    }
}

/// Stores a function result at `path` of a Gemini part, or `""` if there is
/// none.
///
/// A result holding a `$ref` is stored as a JSON string instead: Gemini would
/// read the `$ref` as a reference to a media part and reject the request. It
/// goes under `result` when `path` ends in `response`, as Gemini wants
/// `response` to be an object.
pub(crate) fn set_gemini_function_response_result(
    part: &mut Value,
    path: &str,
    result: Option<Value>,
) {
    match result {
        None => {
            set_path(part, path, Value::String(String::new()));
        }
        Some(result) if contains_json_ref(&result) => {
            set_ref_result(part, path, result.to_string());
        }
        Some(result) => {
            set_path(part, path, result);
        }
    }
}

/// [`set_gemini_function_response_result`] for a result given as JSON text.
/// Blank text stores `""`.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "used by the OpenAI Responses to Gemini translator"
    )
)]
pub(crate) fn set_gemini_function_response_raw(part: &mut Value, path: &str, raw: &str) {
    let trimmed = raw.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(result) if contains_json_ref(&result) => set_ref_result(part, path, trimmed.to_owned()),
        Ok(result) => {
            set_path(part, path, result);
        }
        Err(_) => {
            set_path(part, path, Value::String(String::new()));
        }
    }
}

/// Stores the text of a result holding a `$ref` as a string.
fn set_ref_result(part: &mut Value, path: &str, text: String) {
    let target = if path.ends_with("response") {
        format!("{path}.result")
    } else {
        path.to_owned()
    };
    set_path(part, &target, Value::String(text));
}

#[cfg(test)]
mod tests;
