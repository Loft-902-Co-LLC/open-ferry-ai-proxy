// Ported from CLIProxyAPI internal/runtime/executor/helps/usage_helpers.go
// (FilterSSEUsageMetadata, StripUsageMetadataFromJSON, JSONPayload)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The lines of Gemini's SSE stream, before they are translated.
//!
//! Gemini repeats its running `usageMetadata` on chunks before the last, and
//! the translators count usage wherever they find it, so a chunk without a
//! finish reason has its usage renamed to `cpaUsageMetadata`, which they
//! don't read. Gemini can also send the finish reason and the usage in
//! separate chunks: a finish chunk without usage is remembered by its
//! `traceId` for ten minutes, and the next chunk with usage and the same
//! trace keeps its usage.
//!
//! Deviations from upstream:
//! - A changed chunk is written out again as compact JSON, with the renamed
//!   field last, where upstream edits the bytes in place; the fields and
//!   values are the same.
//! - At most 4096 trace IDs are remembered, the oldest forgotten first;
//!   upstream's map has no limit.
//! - Data that Go's JSON reader accepts but `serde_json` can't read (invalid
//!   UTF-8, very deep nesting) passes unchanged.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use open_ferry_translate::go::trim_space;
use serde_json::Value;

use crate::json;

/// How long a finish chunk without usage is remembered.
const STOP_MEMORY: Duration = Duration::from_secs(10 * 60);

/// How many of them are remembered at most.
const STOP_MEMORY_LIMIT: usize = 4096;

/// The trace IDs of finish chunks without usage, and when they came.
static STOPS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `rememberStopWithoutUsage`.
fn remember_stop(trace_id: &str) {
    let mut stops = STOPS.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    stops.retain(|_, at| now.duration_since(*at) < STOP_MEMORY);
    if stops.len() >= STOP_MEMORY_LIMIT && !stops.contains_key(trace_id) {
        let oldest = stops
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(trace, _)| trace.clone());
        if let Some(oldest) = oldest {
            stops.remove(&oldest);
        }
    }
    stops.insert(trace_id.to_owned(), now);
}

/// Forgets `trace_id`, returning whether it was remembered.
fn forget_stop(trace_id: &str) -> bool {
    let mut stops = STOPS.lock().unwrap_or_else(PoisonError::into_inner);
    stops
        .remove(trace_id)
        .is_some_and(|at| at.elapsed() < STOP_MEMORY)
}

/// The finish reason of a Gemini chunk, or of one wrapped in `response`.
fn finish_reason(chunk: &Value) -> Option<&Value> {
    json::get(chunk, "candidates.0.finishReason")
        .or_else(|| json::get(chunk, "response.candidates.0.finishReason"))
}

/// Whether the chunk has a finish reason that isn't blank.
fn is_terminal(chunk: &Value) -> bool {
    finish_reason(chunk).is_some_and(|reason| !json::str_of(Some(reason)).trim().is_empty())
}

/// `hasUsageMetadata`.
fn has_usage(chunk: &Value) -> bool {
    json::exists(chunk, "usageMetadata") || json::exists(chunk, "response.usageMetadata")
}

/// `StripUsageMetadataFromJSON` for a parsed chunk: renames the usage of a
/// chunk without a finish reason. Returns whether it changed anything.
fn strip_usage(chunk: &mut Value) -> bool {
    if is_terminal(chunk) || !has_usage(chunk) {
        return false;
    }
    let mut changed = false;
    for (from, to) in [
        ("usageMetadata", "cpaUsageMetadata"),
        ("response.usageMetadata", "response.cpaUsageMetadata"),
    ] {
        if let Some(usage) = json::get(chunk, from).cloned() {
            json::set(chunk, to, usage);
            json::delete(chunk, from);
            changed = true;
        }
    }
    changed
}

/// [`strip_usage`] for JSON text, with the changed JSON if there is any.
fn strip_usage_text(text: &[u8]) -> Option<Vec<u8>> {
    let mut chunk: Value = serde_json::from_slice(trim_space(text)).ok()?;
    if !strip_usage(&mut chunk) {
        return None;
    }
    serde_json::to_vec(&chunk).ok()
}

/// `FilterSSEUsageMetadata`: renames the usage of each chunk on the `data:`
/// lines of `payload` that isn't the last, or of `payload` as bare JSON
/// when it has no `data:` lines.
pub(crate) fn filter_sse_usage_metadata(payload: &[u8]) -> Cow<'_, [u8]> {
    if payload.is_empty() {
        return Cow::Borrowed(payload);
    }
    let mut lines: Vec<Cow<'_, [u8]>> = payload.split(|&b| b == b'\n').map(Cow::Borrowed).collect();
    let mut modified = false;
    let mut found_data = false;
    for line in &mut lines {
        let trimmed = trim_space(line);
        if trimmed.is_empty() || !trimmed.starts_with(b"data:") {
            continue;
        }
        found_data = true;
        let Some(data_at) = line.windows(5).position(|window| window == b"data:") else {
            continue;
        };
        let Ok(mut chunk) = serde_json::from_slice::<Value>(trim_space(&line[data_at + 5..]))
        else {
            continue;
        };
        let trace_id = json::str_of(chunk.get("traceId"));
        if !trace_id.is_empty() {
            if is_terminal(&chunk) && !has_usage(&chunk) {
                remember_stop(&trace_id);
                continue;
            }
            if has_usage(&chunk) && forget_stop(&trace_id) {
                continue;
            }
        }
        if !strip_usage(&mut chunk) {
            continue;
        }
        let Ok(cleaned) = serde_json::to_vec(&chunk) else {
            continue;
        };
        let mut rebuilt = line[..data_at].to_vec();
        rebuilt.extend_from_slice(b"data: ");
        rebuilt.extend_from_slice(&cleaned);
        *line = Cow::Owned(rebuilt);
        modified = true;
    }
    if modified {
        return Cow::Owned(lines.join(&b'\n'));
    }
    if !found_data && let Some(cleaned) = strip_usage_text(payload) {
        return Cow::Owned(cleaned);
    }
    Cow::Borrowed(payload)
}

/// `JSONPayload`: the JSON object on an SSE line, without its `data:`
/// prefix, or `None` for a blank line, `[DONE]`, an event name or anything
/// else that isn't an object.
pub(crate) fn json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = trim_space(line);
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = trim_space(rest);
    }
    trimmed.starts_with(b"{").then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn filter(payload: &str) -> String {
        String::from_utf8(filter_sse_usage_metadata(payload.as_bytes()).into_owned()).unwrap()
    }

    /// Keeps the tests that remember traces apart.
    static TRACES: Mutex<()> = Mutex::new(());

    fn parse(line: &str) -> Value {
        serde_json::from_slice(json_payload(line.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn renames_the_usage_of_chunks_before_the_last() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"totalTokenCount":3}}"#;
        let got = parse(&filter(line));
        assert_eq!(
            got,
            json!({"candidates":[{"content":{"parts":[{"text":"a"}]}}],"cpaUsageMetadata":{"totalTokenCount":3}})
        );

        // The same inside `response`, and without `data:`.
        let raw = r#" {"response":{"usageMetadata":{"totalTokenCount":3},"candidates":[{}]}} "#;
        let got: Value = serde_json::from_str(&filter(raw)).unwrap();
        assert_eq!(
            got,
            json!({"response":{"candidates":[{}],"cpaUsageMetadata":{"totalTokenCount":3}}})
        );

        // Other lines are kept as they are.
        let frame = format!("event: message\n  {line}\r\n\n");
        let got = filter(&frame);
        assert!(got.starts_with("event: message\n  data: {"), "{got}");
        assert!(got.ends_with("}\n\n"), "{got}");
        assert!(got.contains("cpaUsageMetadata"), "{got}");
    }

    #[test]
    fn keeps_the_usage_of_the_last_chunk() {
        for payload in [
            r#"data: {"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":3}}"#,
            r#"data: {"response":{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{}}}"#,
            r#"data: {"candidates":[{}]}"#,
            "data: not json",
            "data: [DONE]",
            "",
            "{not json",
        ] {
            assert!(
                matches!(
                    filter_sse_usage_metadata(payload.as_bytes()),
                    Cow::Borrowed(_)
                ),
                "{payload}"
            );
        }
        // A blank finish reason isn't one.
        let got = filter(r#"data: {"candidates":[{"finishReason":" "}],"usageMetadata":{}}"#);
        assert!(got.contains("cpaUsageMetadata"), "{got}");
    }

    #[test]
    fn keeps_usage_that_follows_a_finish_without_it() {
        let _serial = TRACES.lock().unwrap_or_else(PoisonError::into_inner);
        let finish = r#"data: {"candidates":[{"finishReason":"STOP"}],"traceId":"sse-test-split"}"#;
        let usage = r#"data: {"candidates":[{}],"usageMetadata":{"totalTokenCount":33},"traceId":"sse-test-split"}"#;
        assert_eq!(filter(finish), finish);
        assert_eq!(filter(usage), usage);
        // Only once.
        assert!(filter(usage).contains("cpaUsageMetadata"));

        // A chunk of another trace is renamed as usual.
        filter(finish);
        let other = usage.replace("sse-test-split", "sse-test-other");
        assert!(filter(&other).contains("cpaUsageMetadata"));
        assert_eq!(filter(usage), usage);
    }

    #[test]
    fn remembers_a_bounded_number_of_traces() {
        let _serial = TRACES.lock().unwrap_or_else(PoisonError::into_inner);
        remember_stop("sse-test-first");
        for index in 0..STOP_MEMORY_LIMIT {
            remember_stop(&format!("sse-test-bound-{index}"));
        }
        assert!(STOPS.lock().unwrap().len() <= STOP_MEMORY_LIMIT);
        assert!(!forget_stop("sse-test-first"));
        assert!(forget_stop(&format!(
            "sse-test-bound-{}",
            STOP_MEMORY_LIMIT - 1
        )));
    }

    #[test]
    fn finds_the_json_of_a_line() {
        assert_eq!(
            json_payload(b"  data: {\"a\":1} \r"),
            Some(&b"{\"a\":1}"[..])
        );
        assert_eq!(json_payload(b"{\"a\":1}"), Some(&b"{\"a\":1}"[..]));
        for line in [
            &b""[..],
            b"   ",
            b"[DONE]",
            b"data: [DONE]",
            b"event: message",
            b"data:",
            b"data: [1]",
            b": comment",
        ] {
            assert_eq!(
                json_payload(line),
                None,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }
}
