// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions
// (firstNonEmpty, firstExisting, jsonStringValue, interactionsTextStep,
// openAIToolCallToInteractionsStep, setRawJSONValue, openAIReasoningTexts,
// openAIChatSSEPayload, openAIChatInteractionsPayload,
// setInteractionsUsageFromOpenAIChat, setOpenAIChatUsageFromInteractions,
// interactionsUsageInt) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What both directions share: reading fields as gjson does, the steps and
//! tool calls both formats hold, the `data:` lines of a stream, and the usage
//! fields each format names its own way.
//!
//! Deviations from upstream:
//! - JSON that isn't a string, read where upstream copies its text into a
//!   string, is written compactly.

use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::go;
use crate::json::{exact, int_of, object, path, set_path, str_of};

/// gjson `Get(at).String()` of `value`, `at` a dotted path of keys.
pub(super) fn text_at<'v>(value: &'v Value, at: &str) -> Cow<'v, str> {
    str_of(path(value, at))
}

/// gjson `Get(at).Int()` of `value`.
pub(super) fn int_at(value: &Value, at: &str) -> i64 {
    path(value, at).map_or(0, int_of)
}

/// `firstNonEmpty`: the first of `values` that isn't blank, as it is, or `""`.
pub(super) fn first_non_empty(values: &[&str]) -> String {
    values
        .iter()
        .find(|value| !value.trim().is_empty())
        .map_or_else(String::new, |value| (*value).to_owned())
}

/// `firstExisting`: the first value found, even `null`.
pub(super) fn first_existing<'v>(values: &[Option<&'v Value>]) -> Option<&'v Value> {
    values.iter().find_map(|value| *value)
}

/// gjson `ForEach`: an array's items, an object's values, or a value of any
/// other type itself.
pub(super) fn each(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(fields)) => fields.values().collect(),
        Some(other) => vec![other],
    }
}

/// `jsonStringValue`: a string as it is, other JSON as its text, or
/// `fallback` if there is none.
pub(super) fn json_string_value(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None => fallback.to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `setRawJSONValue` with `{}` to fall back on: a string holding JSON, once
/// trimmed, becomes that JSON; any other string stays one; other JSON is kept.
pub(super) fn raw_json_value(value: Option<&Value>) -> Value {
    match value {
        None => Value::Object(Map::new()),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            go::gjson_valid(trimmed.as_bytes())
                .then(|| exact::from_str(trimmed).ok())
                .flatten()
                .unwrap_or_else(|| Value::String(text.clone()))
        }
        Some(other) => other.clone(),
    }
}

/// `interactionsTextStep`: a step of `step_type` holding one text part.
pub(super) fn text_step(step_type: &str, text: String) -> Value {
    object([
        ("type", step_type.into()),
        (
            "content",
            Value::Array(vec![object([
                ("type", "text".into()),
                ("text", text.into()),
            ])]),
        ),
    ])
}

/// `openAIReasoningTexts`: a Chat Completions `reasoning_content`, a string
/// or a list of parts, as the texts that aren't blank.
pub(super) fn reasoning_texts(reasoning: &Value) -> Vec<String> {
    match reasoning {
        Value::String(text) if text.is_empty() => Vec::new(),
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .map(|item| first_non_empty(&[&text_at(item, "text"), &text_at(item, "content")]))
            .filter(|text| !text.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// `openAIToolCallToInteractionsStep`: a Chat Completions tool call as a
/// `function_call` step, its arguments as JSON where they hold some. `None`
/// for a call of another type, or without a `function`.
pub(super) fn tool_call_step(tool_call: &Value) -> Option<Value> {
    let tool_type = text_at(tool_call, "type");
    if !tool_type.is_empty() && tool_type != "function" {
        return None;
    }
    let function = tool_call.get("function")?;
    let mut step = Map::new();
    step.insert("type".into(), "function_call".into());
    step.insert("name".into(), text_at(function, "name").into());
    step.insert(
        "arguments".into(),
        raw_json_value(function.get("arguments")),
    );
    let id = text_at(tool_call, "id");
    if !id.is_empty() {
        step.insert("id".into(), id.into());
    }
    Some(Value::Object(step))
}

/// `openAIChatSSEPayload` (and `openAIChatInteractionsPayload`, the same):
/// a stream line's data. Text after a leading `data:`, or else the data of
/// each `data:` line, joined by line breaks, or else the text itself; each
/// trimmed.
pub(super) fn sse_payload(raw: &[u8]) -> Cow<'_, [u8]> {
    let trimmed = go::trim_space(raw);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return Cow::Borrowed(trimmed);
    }
    if let Some(data) = trimmed.strip_prefix(b"data:") {
        return Cow::Borrowed(go::trim_space(data));
    }
    let lines: Vec<&[u8]> = trimmed
        .split(|&byte| byte == b'\n')
        .filter_map(|line| go::trim_space(line).strip_prefix(b"data:"))
        .map(go::trim_space)
        .collect();
    if lines.is_empty() {
        Cow::Borrowed(trimmed)
    } else {
        Cow::Owned(lines.join(&b'\n'))
    }
}

/// `setInteractionsUsageFromOpenAIChat`: sets the Interactions usage fields
/// at `at` in `out` from a Chat Completions `usage`, each Interactions name
/// and its `total_` twin.
pub(super) fn interactions_usage_from_chat(out: &mut Value, at: &str, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let fields: [(&str, &[&str]); 5] = [
        ("prompt_tokens", &["input_tokens", "total_input_tokens"]),
        (
            "completion_tokens",
            &["output_tokens", "total_output_tokens"],
        ),
        ("total_tokens", &["total_tokens"]),
        (
            "prompt_tokens_details.cached_tokens",
            &["cached_tokens", "total_cached_tokens"],
        ),
        (
            "completion_tokens_details.reasoning_tokens",
            &["reasoning_tokens", "total_thought_tokens"],
        ),
    ];
    for (from, names) in fields {
        if let Some(value) = path(usage, from) {
            let count = int_of(value);
            for name in names {
                set_path(out, &format!("{at}.{name}"), count.into());
            }
        }
    }
}

/// `setOpenAIChatUsageFromInteractions`: sets the Chat Completions usage
/// fields at `at` in `out` from an Interactions usage, each from the first
/// of its Interactions names found.
pub(super) fn chat_usage_from_interactions(out: &mut Value, at: &str, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    let fields: [(&str, &[&str]); 5] = [
        ("prompt_tokens", &["input_tokens", "total_input_tokens"]),
        (
            "completion_tokens",
            &["output_tokens", "total_output_tokens"],
        ),
        ("total_tokens", &["total_tokens"]),
        (
            "prompt_tokens_details.cached_tokens",
            &["cached_tokens", "total_cached_tokens"],
        ),
        (
            "completion_tokens_details.reasoning_tokens",
            &["reasoning_tokens", "total_thought_tokens"],
        ),
    ];
    for (to, names) in fields {
        // `interactionsUsageInt`.
        if let Some(value) = names.iter().find_map(|name| path(usage, name)) {
            set_path(out, &format!("{at}.{to}"), int_of(value).into());
        }
    }
}

/// The current Unix time in nanoseconds, for the IDs upstream makes up.
pub(super) fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// The current Unix time in seconds.
pub(super) fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: the payload of each form of stream line (checked with
    // Go).
    #[test]
    fn sse_payloads() {
        let cases: [(&[u8], &[u8]); 7] = [
            (b"", b""),
            (b"  [DONE]\n", b"[DONE]"),
            (b"data: {\"a\":1}  ", b"{\"a\":1}"),
            (b"data:[DONE]", b"[DONE]"),
            (b"event: x\ndata: {\"a\":1}\n", b"{\"a\":1}"),
            (b"event: x\n data: 1 \ndata:2", b"1\n2"),
            (b"{\"a\":1}", b"{\"a\":1}"),
        ];
        for (raw, want) in cases {
            assert_eq!(sse_payload(raw).as_ref(), want, "{raw:?}");
        }
    }

    // Not upstream's: setRawJSONValue keeps a string that isn't JSON, and
    // reads one that is, once trimmed, each number as written (checked with
    // Go).
    #[test]
    fn raw_json_values() {
        assert_eq!(raw_json_value(None), json!({}));
        assert_eq!(raw_json_value(Some(&json!(" {\"q\":1} "))), json!({"q": 1}));
        assert_eq!(raw_json_value(Some(&json!("1.50"))).to_string(), "1.50");
        let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
        assert_eq!(raw_json_value(Some(&json!(spelled))).to_string(), spelled);
        assert_eq!(raw_json_value(Some(&json!("{\"q\""))), json!("{\"q\""));
        assert_eq!(raw_json_value(Some(&json!([1]))), json!([1]));
    }

    // Not upstream's: gjson's ForEach visits an object's values, and a
    // scalar (null included) once.
    #[test]
    fn each_is_gjson_for_each() {
        assert_eq!(each(Some(&json!({"a": 1, "b": 2}))), [&json!(1), &json!(2)]);
        assert_eq!(each(Some(&Value::Null)), [&Value::Null]);
        assert!(each(None).is_empty());
    }
}
