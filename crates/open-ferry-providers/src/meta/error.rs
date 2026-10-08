// Ported from CLIProxyAPI internal/runtime/executor/meta_executor.go
// (parseMetaRetryAfter, metaRateLimitError, isMetaSubscriptionQuota),
// meta_executor_execute.go (wrapMetaUpstreamError, metaStreamEventError,
// metaNotFoundCooldown) and meta_test.go (TestMetaExecutor_ParseRetryAfter,
// TestMetaExecutor_NotFoundCooldown) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Meta's errors: what a failure status or an `error` event in the stream
//! means for the credential.
//!
//! A 429 for the subscription's quota is the credential's, so the others
//! may still serve; any 429 or 404 that says when the limit resets is
//! retried then. A model Meta doesn't know (404) cools down for five
//! minutes unless it says better.
//!
//! Deviations from upstream:
//! - A body that isn't JSON is read as having no fields, where gjson reads
//!   what it can from it.

use std::time::{Duration, SystemTime};

use open_ferry_translate::go::to_lower;
use serde_json::Value;

use crate::codex::terminal::{StatusError, wait_until};
use crate::json::{exists, int_at, str_at};

/// How long a model Meta answers 404 for is left alone when the body gives
/// no reset time (`metaNotFoundCooldown`).
pub(super) const NOT_FOUND_COOLDOWN: Duration = Duration::from_secs(5 * 60);

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

/// How long until the limit a 429 or 404 body names resets, from its
/// `error.resets_at` (a Unix time), if that is ahead (`parseMetaRetryAfter`).
pub(super) fn parse_retry_after(status: u16, body: &[u8], now: SystemTime) -> Option<Duration> {
    if (status != 429 && status != 404) || body.is_empty() {
        return None;
    }
    wait_until(int_at(&parse(body), "error.resets_at"), now)
}

/// Whether a 429 says the subscription's quota ran out, in its message or
/// in a rate-limit or quota code that comes with a reset time
/// (`isMetaSubscriptionQuota`).
fn is_subscription_quota(status: u16, body: &[u8]) -> bool {
    if status != 429 || body.is_empty() {
        return false;
    }
    let parsed = parse(body);
    let message = to_lower(&str_at(&parsed, "error.message"));
    let code = to_lower(&str_at(&parsed, "error.code"));
    if message.contains("subscription quota") || message.contains("quota exhausted") {
        return true;
    }
    (code == "rate_limit_exceeded" || code.contains("quota")) && exists(&parsed, "error.resets_at")
}

/// The error for Meta's failure `status` and `body` (`wrapMetaUpstreamError`).
pub(super) fn wrap_upstream_error(status: u16, body: &[u8]) -> StatusError {
    wrap_upstream_error_at(status, body, SystemTime::now())
}

/// [`wrap_upstream_error`] at `now`.
fn wrap_upstream_error_at(status: u16, body: &[u8], now: SystemTime) -> StatusError {
    let mut error = StatusError::new(status, String::from_utf8_lossy(body));
    match status {
        429 => {
            error.retry_after = parse_retry_after(status, body, now);
            error.credential_scoped = is_subscription_quota(status, body);
        }
        404 => {
            error.retry_after = parse_retry_after(status, body, now).or(Some(NOT_FOUND_COOLDOWN));
        }
        _ => {}
    }
    error
}

/// The error an `error` or `response.failed` event of the stream is, whose
/// `data` is `event` (`metaStreamEventError`). Its status is the event's
/// `error.code` when that is an HTTP error status, else 502.
pub(super) fn stream_event_error(event: &Value, data: &[u8]) -> Option<StatusError> {
    let kind = str_at(event, "type");
    if kind != "error" && kind != "response.failed" {
        return None;
    }
    let status = u16::try_from(int_at(event, "error.code"))
        .ok()
        .filter(|code| (400..=599).contains(code))
        .unwrap_or(502);
    Some(wrap_upstream_error(status, data))
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use serde_json::json;

    use super::*;

    fn unix_after(now: SystemTime, wait: Duration) -> u64 {
        now.checked_add(wait)
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_secs())
    }

    fn quota(resets_at: u64) -> String {
        json!({"error": {
            "code": "rate_limit_exceeded",
            "message": "Subscription quota exhausted.",
            "resets_at": resets_at,
            "type": "rate_limit_error",
        }})
        .to_string()
    }

    fn minutes(wait: Duration) -> f64 {
        wait.as_secs_f64() / 60.0
    }

    // TestMetaExecutor_ParseRetryAfter.
    #[test]
    fn retry_after_comes_from_resets_at() {
        let now = SystemTime::now();
        let body = quota(unix_after(now, Duration::from_secs(45 * 60)));
        let wait = parse_retry_after(429, body.as_bytes(), now).expect("a wait");
        assert!(
            (40.0..=50.0).contains(&minutes(wait)),
            "waits {:.1} minutes",
            minutes(wait)
        );

        // Only a 429 or a 404 has one.
        assert_eq!(parse_retry_after(200, body.as_bytes(), now), None);
        assert_eq!(parse_retry_after(500, body.as_bytes(), now), None);
        assert!(parse_retry_after(404, body.as_bytes(), now).is_some());

        // A time in the past, none at all, and no body.
        let past = quota(unix_after(now, Duration::ZERO).saturating_sub(3600));
        assert_eq!(parse_retry_after(429, past.as_bytes(), now), None);
        assert_eq!(parse_retry_after(429, br#"{"error":{}}"#, now), None);
        assert_eq!(parse_retry_after(429, b"", now), None);
        assert_eq!(parse_retry_after(429, b"not json", now), None);
    }

    #[test]
    fn subscription_quota_is_the_credentials() {
        let now = SystemTime::now();
        let resets = unix_after(now, Duration::from_secs(2 * 3600));
        let error = wrap_upstream_error_at(429, quota(resets).as_bytes(), now);
        assert_eq!(error.status, 429);
        assert!(error.credential_scoped);
        let wait = error.retry_after.expect("a wait");
        assert!((60.0..=180.0).contains(&minutes(wait)));
        assert!(error.message.contains("Subscription quota exhausted."));
        assert!(!error.request_scoped);

        for (body, scoped) in [
            // The message alone.
            (
                r#"{"error":{"message":"Your SUBSCRIPTION QUOTA is gone"}}"#,
                true,
            ),
            (r#"{"error":{"message":"Quota exhausted for today"}}"#, true),
            // A code needs a reset time with it.
            (
                r#"{"error":{"code":"monthly_quota_reached","resets_at":0}}"#,
                true,
            ),
            (
                r#"{"error":{"code":"rate_limit_exceeded","resets_at":1}}"#,
                true,
            ),
            (r#"{"error":{"code":"rate_limit_exceeded"}}"#, false),
            (r#"{"error":{"code":"monthly_quota_reached"}}"#, false),
            // Another 429.
            (r#"{"error":{"message":"slow down"}}"#, false),
            ("too many requests", false),
            ("", false),
        ] {
            let error = wrap_upstream_error_at(429, body.as_bytes(), now);
            assert_eq!(error.credential_scoped, scoped, "{body}");
            assert_eq!(error.message, body);
        }

        // Only a 429 is a quota.
        let error = wrap_upstream_error_at(403, quota(resets).as_bytes(), now);
        assert!(!error.credential_scoped);
        assert_eq!(error.retry_after, None);
    }

    // TestMetaExecutor_NotFoundCooldown, which upstream runs through the
    // credential manager; here the error carries the cooldown.
    #[test]
    fn unknown_models_cool_down() {
        let body = r#"{"error":{"type":"invalid_request_error","code":"model_not_found","message":"model not found"}}"#;
        let error = wrap_upstream_error(404, body.as_bytes());
        assert_eq!(error.status, 404);
        assert_eq!(error.retry_after, Some(NOT_FOUND_COOLDOWN));
        assert!(!error.credential_scoped);
        assert_eq!(error.message, body);

        // Without a body too, and the body's reset time wins over it.
        assert_eq!(
            wrap_upstream_error(404, b"").retry_after,
            Some(NOT_FOUND_COOLDOWN)
        );
        let now = SystemTime::now();
        let resets = unix_after(now, Duration::from_secs(3600));
        let error = wrap_upstream_error_at(404, quota(resets).as_bytes(), now);
        let wait = error.retry_after.expect("a wait");
        assert!((55.0..=65.0).contains(&minutes(wait)));
    }

    #[test]
    fn other_statuses_keep_their_body() {
        for status in [400, 401, 403, 500, 503] {
            let error = wrap_upstream_error(status, b"bad");
            assert_eq!((error.status, error.message.as_str()), (status, "bad"));
            assert_eq!(error.retry_after, None);
            assert!(!error.credential_scoped && !error.request_scoped);
        }
        // An empty body reads as the status, as upstream's `Error()` does.
        let error: open_ferry_core::exec::ExecError = wrap_upstream_error(500, b"").into();
        assert_eq!(error.message, "status 500");
    }

    #[test]
    fn error_events_become_errors() {
        let event = |text: &str| -> (Value, String) { (parse(text.as_bytes()), text.to_owned()) };
        for (text, status) in [
            (
                r#"{"type":"error","error":{"code":429,"message":"slow"}}"#,
                429,
            ),
            (r#"{"type":"response.failed","error":{"code":503}}"#, 503),
            // A string code reads as the number it holds, as gjson's does.
            (r#"{"type":"error","error":{"code":"418"}}"#, 418),
            (
                r#"{"type":"error","error":{"code":"invalid_request"}}"#,
                502,
            ),
            (r#"{"type":"error","error":{"code":399}}"#, 502),
            (r#"{"type":"error","error":{"code":600}}"#, 502),
            (r#"{"type":"error"}"#, 502),
        ] {
            let (value, data) = event(text);
            let error = stream_event_error(&value, data.as_bytes()).expect(text);
            assert_eq!(error.status, status, "{text}");
            assert_eq!(error.message, text);
        }
        for text in [
            r#"{"type":"response.completed"}"#,
            r#"{"type":"response.output_text.delta","delta":"x"}"#,
            r#"{"error":{"code":500}}"#,
            "[DONE]",
        ] {
            let (value, data) = event(text);
            assert!(
                stream_event_error(&value, data.as_bytes()).is_none(),
                "{text}"
            );
        }

        // A quota in the stream is the credential's too.
        let text =
            r#"{"type":"error","error":{"code":429,"message":"Subscription quota exhausted."}}"#;
        let (value, data) = event(text);
        let error = stream_event_error(&value, data.as_bytes()).expect("an error");
        assert!(error.credential_scoped);
    }
}
