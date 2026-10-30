// Ported from CLIProxyAPI internal/runtime/executor/openai_compat_executor.go
// (openAICompatErrorEvent, openAICompatStreamDataError,
// newOpenAICompatStatusError, openAICompatRetryAfter) (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An OpenAI-compatible provider's failures as errors: a response with an
//! error status, and an error that arrives inside the stream.
//!
//! A 429 waits as long as its `Retry-After` header says, in seconds or as an
//! HTTP date. Without one, a body that says a tokens-per-minute limit was
//! exceeded waits a minute; any other 429 names no wait.
//!
//! Deviations from upstream:
//! - A `Retry-After` too long for Go's `time.Duration` (about 292 years)
//!   wraps around in Go; here it is kept.
//! - A body is read with `serde_json`: one that isn't valid JSON, or that
//!   `serde_json` can't read (invalid UTF-8, very deep nesting), has no
//!   fields, where gjson may still find some in it; and of duplicate keys the
//!   last counts, where gjson reads the first.

use std::time::Duration;

use chrono::{DateTime, Utc};
use http::HeaderMap;
use open_ferry_translate::go::json_valid;
use serde_json::Value;

use crate::claude::ratelimit::{header_value, parse_http_date};
use crate::codex::terminal::StatusError;
use crate::json::{eq_fold, exists, get, int_at, lower_trim, str_at};

/// How long a tokens-per-minute 429 without `Retry-After` waits
/// (`openAICompatTPMFallbackRetryAfter`).
const TPM_FALLBACK_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Where a stream error payload may give its status, in order.
const STATUS_PATHS: [&str; 6] = [
    "status",
    "status_code",
    "error.status",
    "error.status_code",
    "response.error.status",
    "response.error.status_code",
];

/// The error for a response with an error status
/// (`newOpenAICompatStatusError`): the body as the message, and for a 429
/// when to try again.
pub(crate) fn status_error(status: u16, headers: &HeaderMap, body: &[u8]) -> StatusError {
    StatusError {
        retry_after: retry_after_at(status, headers, body, Utc::now()),
        ..StatusError::new(status, String::from_utf8_lossy(body))
    }
}

/// How long to wait after a response with `status`, as of `now`
/// (`openAICompatRetryAfter`). Only a 429 names a wait.
pub(crate) fn retry_after_at(
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    now: DateTime<Utc>,
) -> Option<Duration> {
    if status != 429 {
        return None;
    }
    let raw = header_value(headers, "retry-after");
    let raw = raw.trim();
    if !raw.is_empty() {
        if let Ok(seconds) = raw.parse::<i64>()
            && let Ok(seconds) = u64::try_from(seconds)
        {
            return Some(Duration::from_secs(seconds));
        }
        if let Some(deadline) = parse_http_date(raw) {
            return Some((deadline - now).to_std().unwrap_or_default());
        }
    }

    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = lower_trim(&str_at(&parsed, "error.code"));
    let message = lower_trim(&str_at(&parsed, "error.message"));
    if code.contains("tpmratelimitexceeded")
        || (message.contains("tokens per minute")
            && message.contains("limit")
            && message.contains("exceeded"))
    {
        return Some(TPM_FALLBACK_RETRY_AFTER);
    }
    None
}

/// Whether an SSE event name marks an error (`openAICompatErrorEvent`).
pub(crate) fn is_error_event(name: &str) -> bool {
    eq_fold(name, "error") || eq_fold(name, "response.error") || eq_fold(name, "response.failed")
}

/// A necessary condition only; every uncertain payload keeps the old parser.
/// Without a backslash, JSON keys decode to their literal UTF-8 spelling.
/// Each accepted data path needs an exact `error` or `type` key, or both
/// exact `code`/`message` keys. Nested response errors also need `error`.
/// Escapes force parsing, including escaped keys. Inspecting `type`'s KEY
/// rather than its value covers every Unicode EqualFold variant (long s,
/// mixed case, etc.); event names use the original Unicode-aware predicate.
/// Duplicate keys cannot invent a missing key; the full path retains serde's
/// last-key-wins semantics. Needles inside values are harmless false positives.
fn might_report_stream_error(payload: &[u8], event: &str) -> bool {
    let contains = |needle: &[u8]| memchr::memmem::find(payload, needle).is_some();
    is_error_event(event)
        || memchr::memchr(b'\\', payload).is_some()
        || contains(b"\"error\"")
        || contains(b"\"type\"")
        || (contains(b"\"code\"") && contains(b"\"message\""))
}

/// The error a stream frame's data reports, if it reports one
/// (`openAICompatStreamDataError`): one with an `error` or `response.error`
/// that isn't null, an error `type`, both `code` and `message`, or any data
/// under an error event. The status is the first one from 400 to 599 the
/// payload gives, else 502, and the message is the payload.
pub(crate) fn stream_data_error(payload: &[u8], event: &str) -> Option<StatusError> {
    if !might_report_stream_error(payload, event) {
        return None;
    }
    if payload.is_empty() || !json_valid(payload) {
        return None;
    }
    let parsed: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let has_error = ["error", "response.error"]
        .iter()
        .any(|path| get(&parsed, path).is_some_and(|node| !node.is_null()));
    let has_top_level_fields = exists(&parsed, "code") && exists(&parsed, "message");
    if !has_error
        && !is_error_event(&str_at(&parsed, "type"))
        && !is_error_event(event)
        && !has_top_level_fields
    {
        return None;
    }
    let status = STATUS_PATHS
        .iter()
        .map(|path| int_at(&parsed, path))
        .find(|status| (400..=599).contains(status))
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(502);
    Some(StatusError::new(status, String::from_utf8_lossy(payload)))
}

// The implementation before the raw-byte precheck, kept as a differential oracle.
#[cfg(test)]
fn stream_data_error_oracle(payload: &[u8], event: &str) -> Option<StatusError> {
    if payload.is_empty() || !json_valid(payload) {
        return None;
    }
    let parsed: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let has_error = ["error", "response.error"]
        .iter()
        .any(|path| get(&parsed, path).is_some_and(|node| !node.is_null()));
    let has_top_level_fields = exists(&parsed, "code") && exists(&parsed, "message");
    if !has_error
        && !is_error_event(&str_at(&parsed, "type"))
        && !is_error_event(event)
        && !has_top_level_fields
    {
        return None;
    }
    let status = STATUS_PATHS
        .iter()
        .map(|path| int_at(&parsed, path))
        .find(|status| (400..=599).contains(status))
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or(502);
    Some(StatusError::new(status, String::from_utf8_lossy(payload)))
}

#[cfg(test)]
mod tests {
    // Ports TestOpenAICompatRetryAfter from
    // internal/runtime/executor/openai_compat_executor_retry_test.go, with
    // checks of the stream error payloads.
    use super::*;
    use chrono::TimeZone as _;
    use http::HeaderValue;

    #[test]
    fn stream_precheck_matches_the_old_parser() {
        let events = [
            "",
            "message",
            "error",
            "ReSpOnSe.FaIlEd",
            "reſponſe.error",
            "reſponſe.failed",
            "Kerror",
        ];
        let mut bodies: Vec<Vec<u8>> = vec![
            br#""#.to_vec(),
            b"not json".to_vec(),
            b"null".to_vec(),
            vec![0xff],
            br#"{"error":null}"#.to_vec(),
            br#"{"error":null,"error":{"status":429}}"#.to_vec(),
            br#"{"error":{"status":503},"error":null}"#.to_vec(),
            br#"{"response":{"error":{"status_code":401}}}"#.to_vec(),
            br#"{"response.error":{"status":503}}"#.to_vec(),
            br#"{"code":null}"#.to_vec(),
            br#"{"code":null,"message":null}"#.to_vec(),
            br#"{"type":"response.error","type":"message"}"#.to_vec(),
            br#"{"type":"message","type":"response.failed"}"#.to_vec(),
            br#"{"error":"unterminated"#.to_vec(),
            br#"{"choices":[{"delta":{"content":"ok"}}]}"#.to_vec(),
        ];
        for key in ["error", "response", "type", "code", "message", "status"] {
            for index in 0..key.len() {
                let escaped = format!(
                    "{}\\u{:04x}{}",
                    &key[..index],
                    key.as_bytes()[index],
                    &key[index + 1..]
                );
                bodies.push(
                    format!(r#"{{"{escaped}":"error","message":"details","error":null}}"#)
                        .into_bytes(),
                );
            }
        }
        bodies.push(br#"{"\u0072esponse":{"\u0065rror":{"status":403}}}"#.to_vec());
        let mut invalid_utf8 = b"{\"type\":\"error\",\"message\":\"".to_vec();
        invalid_utf8.push(0xff);
        invalid_utf8.extend_from_slice(b"\"}");
        bodies.push(invalid_utf8);
        let mut seed = 0x158a_c3d9_u64;
        for _ in 0..1024 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut body = serde_json::json!({"choices":[{"delta":{"content":"ordinary text"}}], "status":400+(seed%200)});
            let object = body.as_object_mut().unwrap();
            if seed & 1 != 0 {
                object.insert(
                    "error".into(),
                    if seed & 2 == 0 {
                        Value::Null
                    } else {
                        serde_json::json!({"status":429})
                    },
                );
            }
            if seed & 4 != 0 {
                object.insert("response".into(), serde_json::json!({"error": if seed & 8 == 0 { Value::Null } else { serde_json::json!("failure") }}));
            }
            if seed & 16 != 0 {
                object.insert(
                    "type".into(),
                    serde_json::json!(events[(seed as usize >> 7) % events.len()]),
                );
            }
            if seed & 32 != 0 {
                object.insert("code".into(), serde_json::json!(seed));
            }
            if seed & 64 != 0 {
                object.insert("message".into(), serde_json::json!("details"));
            }
            bodies.push(serde_json::to_vec(&body).unwrap());
        }
        for body in bodies {
            for event in events {
                assert_eq!(
                    stream_data_error(&body, event),
                    stream_data_error_oracle(&body, event),
                    "body={body:?}, event={event:?}"
                );
            }
        }
        assert!(!might_report_stream_error(
            br#"{"choices":[{"delta":{"content":"hello"}}]}"#,
            "message"
        ));
        assert!(is_error_event("reſponſe.error"));
    }

    fn headers(retry_after: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_str(retry_after).unwrap());
        headers
    }

    #[test]
    fn retry_after() {
        let now = Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap();
        let date = (now + chrono::TimeDelta::seconds(23))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let cases: [(&str, u16, HeaderMap, &str, Option<u64>); 7] = [
            ("delta seconds header", 429, headers("17"), "", Some(17)),
            ("http date header", 429, headers(&date), "", Some(23)),
            (
                "explicit TPM code fallback",
                429,
                HeaderMap::new(),
                r#"{"error":{"code":"ModelAccountTpmRateLimitExceeded","message":"TPM limit exceeded"}}"#,
                Some(60),
            ),
            (
                "TPM message fallback",
                429,
                HeaderMap::new(),
                r#"{"error":{"message":"TPM (Tokens Per Minute) limit of this model is exceeded"}}"#,
                Some(60),
            ),
            (
                "provider header wins over fallback",
                429,
                headers("5"),
                r#"{"error":{"code":"ModelAccountTpmRateLimitExceeded"}}"#,
                Some(5),
            ),
            (
                "generic 429 has no invented deadline",
                429,
                HeaderMap::new(),
                r#"{"error":{"code":"rate_limit"}}"#,
                None,
            ),
            ("non-429 ignores header", 503, headers("30"), "", None),
        ];
        for (name, status, headers, body, want) in cases {
            let got = retry_after_at(status, &headers, body.as_bytes(), now);
            assert_eq!(got, want.map(Duration::from_secs), "{name}");
        }
    }

    #[test]
    fn retry_after_edge_cases() {
        let now = Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap();
        assert_eq!(
            retry_after_at(429, &headers("Thu, 03 Sep 2026 11:00:00 GMT"), b"", now),
            Some(Duration::ZERO),
            "a date in the past waits for nothing"
        );
        assert_eq!(
            retry_after_at(429, &headers(" 0 "), b"", now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after_at(
                429,
                &headers("-3"),
                br#"{"error":{"code":"tpmRateLimitExceeded"}}"#,
                now
            ),
            Some(Duration::from_secs(60)),
            "a negative header falls back to the body"
        );
        assert_eq!(
            retry_after_at(429, &headers("soon"), b"not json", now),
            None
        );
    }

    #[test]
    fn status_errors_keep_the_body() {
        let error = status_error(429, &headers("7"), br#"{"error":"slow down"}"#);
        assert_eq!(error.status, 429);
        assert_eq!(error.message, r#"{"error":"slow down"}"#);
        assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(
            status_error(500, &HeaderMap::new(), b"").text(),
            "status 500"
        );
    }

    #[test]
    fn error_events() {
        assert!(is_error_event("error"));
        assert!(is_error_event("Response.Error"));
        assert!(is_error_event("response.failed"));
        assert!(!is_error_event("ping"));
        assert!(!is_error_event(""));
    }

    #[test]
    fn stream_data_errors() {
        let error = |payload: &str, event: &str| stream_data_error(payload.as_bytes(), event);
        assert_eq!(error(r#"{"choices":[]}"#, ""), None);
        assert_eq!(error(r#"{"error":null}"#, ""), None, "a null error is none");
        assert_eq!(error(r#"{"code":1}"#, ""), None, "code needs message");
        assert_eq!(error("{", "error"), None, "not JSON");
        assert_eq!(error("", "error"), None);

        let found = error(r#"{"error":{"message":"boom"}}"#, "").unwrap();
        assert_eq!(
            (found.status, found.message.as_str()),
            (502, r#"{"error":{"message":"boom"}}"#)
        );
        let found = error(r#"{"response":{"error":{"status_code":503}}}"#, "").unwrap();
        assert_eq!(found.status, 503);
        let found = error(
            r#"{"type":"Response.Failed","status":200,"status_code":"429"}"#,
            "",
        )
        .unwrap();
        assert_eq!(
            found.status, 429,
            "the first status in range, as gjson reads it"
        );
        let found = error(r#"{"code":"x","message":null,"error":{"status":600}}"#, "").unwrap();
        assert_eq!(found.status, 502);
        let found = error(r#"{"detail":"x"}"#, "response.error").unwrap();
        assert_eq!(found.status, 502);
        let found = error("[1]", "error").unwrap();
        assert_eq!(found.message, "[1]");
    }
}
