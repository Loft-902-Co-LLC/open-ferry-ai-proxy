// Ported from CLIProxyAPI
// internal/runtime/executor/helps/responses_usage_helpers.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Fills in the token details a Responses `usage` object may lack, so that
//! clients reading `output_tokens_details.reasoning_tokens` or
//! `input_tokens_details.cached_tokens` find them.
//!
//! Deviations from upstream:
//! - A JSON payload that changes is written again by `serde_json`; sjson
//!   edits it in place. The translators' output reads the same either way.

use open_ferry_translate::go::trim_space;
use serde_json::{Value, json};

use super::gjson::{get_mut, str_at};

/// Adds `output_tokens_details.reasoning_tokens` and
/// `input_tokens_details.cached_tokens`, as 0, to a Responses body's or SSE
/// chunk's `usage` and `response.usage` where they're missing
/// (`EnsureResponsesUsageDetails`). Anything else comes back as it is.
pub(crate) fn ensure_responses_usage_details(payload: Vec<u8>) -> Vec<u8> {
    let trimmed = trim_space(&payload);
    match trimmed.first() {
        None => payload,
        Some(b'{') => match ensure_json(trimmed) {
            Some(updated) => updated,
            None => payload,
        },
        Some(_) if contains(&payload, b"data:") => ensure_sse(&payload).unwrap_or(payload),
        Some(_) => payload,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The JSON object with its usage details filled in, or `None` when nothing
/// changed.
fn ensure_json(data: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(data).ok()?;
    if str_at(&value, "object") == "response.compaction" {
        return None;
    }
    let changed = ensure_at(&mut value, "response.usage") | ensure_at(&mut value, "usage");
    changed.then(|| value.to_string().into_bytes())
}

/// SSE text with each `data:` line's JSON filled in, or `None` when nothing
/// changed.
fn ensure_sse(payload: &[u8]) -> Option<Vec<u8>> {
    let mut modified = false;
    let lines: Vec<Vec<u8>> = payload
        .split(|&b| b == b'\n')
        .map(|line| {
            if !trim_space(line).starts_with(b"data:") {
                return line.to_vec();
            }
            // A line with leading space keeps the bare prefix length, as
            // upstream's does.
            let prefix_len = if line.starts_with(b"data: ") { 6 } else { 5 };
            let Some(data) = line.get(prefix_len..).map(trim_space) else {
                return line.to_vec();
            };
            if data.first() != Some(&b'{') {
                return line.to_vec();
            }
            match ensure_json(data) {
                Some(updated) => {
                    modified = true;
                    let mut out = line.get(..prefix_len).unwrap_or_default().to_vec();
                    out.extend_from_slice(&updated);
                    out
                }
                None => line.to_vec(),
            }
        })
        .collect();
    modified.then(|| lines.join(&b'\n'))
}

/// `ensureUsageDetailsAt`. Returns whether it changed anything.
fn ensure_at(value: &mut Value, path: &str) -> bool {
    let Some(usage) = get_mut(value, path).and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for (details, key) in [
        ("output_tokens_details", "reasoning_tokens"),
        ("input_tokens_details", "cached_tokens"),
    ] {
        match usage.get_mut(details) {
            None => {
                usage.insert(details.to_owned(), json!({ key: 0 }));
                changed = true;
            }
            Some(Value::Object(object)) => {
                if matches!(object.get(key), None | Some(Value::Null)) {
                    object.insert(key.to_owned(), Value::from(0));
                    changed = true;
                }
            }
            Some(other) => {
                *other = json!({ key: 0 });
                changed = true;
            }
        }
    }
    changed
}

/// Whether `path` holds a usage object; for tests.
#[cfg(test)]
fn has_usage(value: &Value, path: &str) -> bool {
    super::gjson::get(value, path).is_some_and(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::gjson::int_at;

    fn ensure(raw: &str) -> String {
        String::from_utf8(ensure_responses_usage_details(raw.as_bytes().to_vec())).unwrap()
    }

    fn assert_details(json: &str, path: &str) {
        let value: Value = serde_json::from_str(json).unwrap();
        assert!(
            has_usage(&value, &format!("{path}.output_tokens_details")),
            "{json}"
        );
        assert!(
            has_usage(&value, &format!("{path}.input_tokens_details")),
            "{json}"
        );
        assert_eq!(
            int_at(
                &value,
                &format!("{path}.output_tokens_details.reasoning_tokens")
            ),
            0
        );
        assert_eq!(
            int_at(
                &value,
                &format!("{path}.input_tokens_details.cached_tokens")
            ),
            0
        );
    }

    #[test]
    fn non_stream_json() {
        let got = ensure(
            r#"{"id":"resp_1","object":"response","status":"completed","usage":{"input_tokens":84,"output_tokens":16,"total_tokens":100}}"#,
        );
        assert_details(&got, "usage");
        assert_eq!(
            got,
            r#"{"id":"resp_1","object":"response","status":"completed","usage":{"input_tokens":84,"output_tokens":16,"total_tokens":100,"output_tokens_details":{"reasoning_tokens":0},"input_tokens_details":{"cached_tokens":0}}}"#
        );
    }

    #[test]
    fn non_stream_json_with_data_substring() {
        let got = ensure(
            r#"{"id":"resp_1","object":"response","status":"completed","output":[{"type":"message","content":[{"type":"text","text":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUg"}]}],"usage":{"input_tokens":84,"output_tokens":16,"total_tokens":100}}"#,
        );
        assert_details(&got, "usage");
    }

    #[test]
    fn sse_data() {
        let got = ensure(
            r#"data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}}}"#,
        );
        let json = got.strip_prefix("data: ").expect("prefix kept");
        assert_details(json, "response.usage");
    }

    #[test]
    fn sse_event_data_multi_line() {
        let got = ensure(
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":84,\"output_tokens\":16,\"total_tokens\":100}}}\n\n",
        );
        assert!(got.starts_with("event: response.completed\n"), "{got}");
        assert!(got.ends_with("}}\n\n"), "{got}");
        let data = got
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("data line");
        assert_details(data, "response.usage");
    }

    #[test]
    fn preserves_existing_details() {
        let raw = r#"data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":3},"output_tokens":4,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":14}}}"#;
        assert_eq!(ensure(raw), raw);
    }

    #[test]
    fn handles_null_or_empty_details() {
        let got = ensure(
            r#"{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":null,"output_tokens":4,"output_tokens_details":{},"total_tokens":14}}"#,
        );
        assert_eq!(
            got,
            r#"{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":4,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":14}}"#
        );
    }

    #[test]
    fn non_json_and_done() {
        for raw in [
            "data: [DONE]",
            "[DONE]",
            ": keepalive",
            "",
            r#"{"type":"response.output_item.added"}"#,
            r#"{"object":"response.compaction","usage":{}}"#,
        ] {
            assert_eq!(ensure(raw), raw);
        }
    }
}
