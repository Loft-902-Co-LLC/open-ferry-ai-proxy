// Ported from CLIProxyAPI internal/runtime/executor/helps/claude_ratelimit.go,
// internal/runtime/executor/claude_executor_fast_error.go and the error
// classification in internal/runtime/executor/claude_executor_request.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Anthropic's rate-limit headers, and Claude's failed responses as
//! [`ExecError`]s.
//!
//! Anthropic's subscription windows (`5h` and `7d`) cover a whole account, so
//! a 429 that rejects one of them is credential-scoped. A 429 that only
//! rejects overage, or that doesn't name a window, stays with the model. A
//! fast-mode request turned away for lack of usage credits is
//! request-scoped: no other credential would do better, and this one is
//! fine for other requests.
//!
//! Deviations from upstream:
//! - Every error from a response carries the response's headers, without
//!   `Content-Encoding`, `Content-Length` and `Transfer-Encoding`; upstream
//!   keeps headers only on a fast-mode request's error.
//! - The rejected-window log lines are debug-level.
//! - A deadline more than about 292 years ahead (Go's longest duration) is
//!   kept as a very long wait, where Go's arithmetic overflows.
//! - HTTP dates don't have their weekday checked; Go checks only that it's a
//!   weekday name.

use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use http::HeaderMap;
use open_ferry_core::exec::{ErrorKind, ExecError};

use crate::json::{lower_trim, str_at};

/// The random grace added to a reset, in whole seconds.
const FUZZ_MIN_SECONDS: u64 = 1;
const FUZZ_MAX_SECONDS: u64 = 30;

/// The first value of a header, or `""` (`getHeaderCaseInsensitive`).
pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default()
}

fn lower_header(headers: &HeaderMap, name: &str) -> String {
    lower_trim(&header_value(headers, name))
}

/// Whether the headers reject one of the account-wide windows, the 5-hour
/// or the 7-day one (`ClaudeHeadersIndicateUnifiedRateLimitRejection`).
pub(crate) fn unified_rejection(headers: &HeaderMap) -> bool {
    let unified = lower_header(headers, "anthropic-ratelimit-unified-status");
    let status_5h = lower_header(headers, "anthropic-ratelimit-unified-5h-status");
    if status_5h == "rejected" {
        return true;
    }
    let status_7d = lower_header(headers, "anthropic-ratelimit-unified-7d-status");
    if status_7d == "rejected" {
        return true;
    }
    if unified != "rejected" {
        return false;
    }
    let status_7d_oi = lower_header(headers, "anthropic-ratelimit-unified-7d_oi-status");
    !overage_only_rejection(headers, &status_5h, &status_7d, &status_7d_oi)
}

fn window_allowed(status: &str) -> bool {
    status == "allowed" || status == "allowed_warning"
}

/// Whether only overage (or the `7d_oi` window) is rejected while the
/// shared windows are healthy (`isOverageOrFableOnlyRejection`).
fn overage_only_rejection(
    headers: &HeaderMap,
    status_5h: &str,
    status_7d: &str,
    status_7d_oi: &str,
) -> bool {
    if status_5h == "rejected" || status_7d == "rejected" {
        return false;
    }
    let overage_status = lower_header(headers, "anthropic-ratelimit-unified-overage-status");
    let disabled_reason = header_value(
        headers,
        "anthropic-ratelimit-unified-overage-disabled-reason",
    );
    let claim = lower_header(headers, "anthropic-ratelimit-unified-representative-claim");
    let overage_rejected = status_7d_oi == "rejected"
        || overage_status == "rejected"
        || !disabled_reason.trim().is_empty()
        || claim.contains("overage");
    if !overage_rejected {
        return false;
    }
    let allowed_5h = window_allowed(status_5h);
    let allowed_7d = window_allowed(status_7d);
    if allowed_5h && allowed_7d {
        return true;
    }
    // Anthropic often leaves out one window's status while its utilization
    // is 0; only a valid utilization below 1 counts as healthy.
    if allowed_7d
        && status_5h.is_empty()
        && utilization_healthy(&header_value(
            headers,
            "anthropic-ratelimit-unified-5h-utilization",
        ))
    {
        return true;
    }
    allowed_5h
        && status_7d.is_empty()
        && utilization_healthy(&header_value(
            headers,
            "anthropic-ratelimit-unified-7d-utilization",
        ))
}

fn utilization_healthy(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    raw.parse::<f64>()
        .is_ok_and(|u| u.is_finite() && (0.0..1.0).contains(&u))
}

/// How long until a rejected window resets, plus 1 to 30 seconds of random
/// grace; `None` when the headers give no future reset
/// (`ParseClaudeRateLimitReset`).
pub(crate) fn rate_limit_reset(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    rate_limit_reset_with_fuzz(headers, now, FUZZ_MIN_SECONDS, FUZZ_MAX_SECONDS)
}

fn rate_limit_reset_with_fuzz(
    headers: &HeaderMap,
    now: DateTime<Utc>,
    min_fuzz: u64,
    max_fuzz: u64,
) -> Option<Duration> {
    let unified = lower_header(headers, "anthropic-ratelimit-unified-status");
    let status_5h = lower_header(headers, "anthropic-ratelimit-unified-5h-status");
    let status_7d = lower_header(headers, "anthropic-ratelimit-unified-7d-status");
    let status_7d_oi = lower_header(headers, "anthropic-ratelimit-unified-7d_oi-status");
    let overage_only = overage_only_rejection(headers, &status_5h, &status_7d, &status_7d_oi);

    let mut deadlines: Vec<DateTime<Utc>> = Vec::new();
    let mut rejected: Vec<&str> = Vec::new();
    for (status, window) in [
        (&unified, "unified"),
        (&status_5h, "5h"),
        (&status_7d, "7d"),
        (&status_7d_oi, "7d_oi"),
    ] {
        if status == "rejected" {
            rejected.push(window);
        }
    }

    // Retry-After describes the credential, unless only overage is rejected.
    if !overage_only {
        let raw = header_value(headers, "retry-after");
        if !raw.is_empty() {
            rejected.push("retry-after");
            if let Some(when) = parse_retry_after(&raw, now).filter(|when| *when > now) {
                deadlines.push(when);
            }
        }
    }
    let mut window_reset = |rejected_window: bool, name: &str| {
        if rejected_window
            && let Some(when) =
                parse_unix_or_timestamp(&header_value(headers, name)).filter(|when| *when > now)
        {
            deadlines.push(when);
        }
    };
    window_reset(
        status_5h == "rejected",
        "anthropic-ratelimit-unified-5h-reset",
    );
    window_reset(
        status_7d == "rejected",
        "anthropic-ratelimit-unified-7d-reset",
    );
    window_reset(
        status_7d_oi == "rejected" && !overage_only,
        "anthropic-ratelimit-unified-7d_oi-reset",
    );

    let unified_rejected = !overage_only
        && (unified == "rejected"
            || status_5h == "rejected"
            || status_7d == "rejected"
            || status_7d_oi == "rejected"
            || (unified.is_empty() && !window_allowed(&status_5h) && !window_allowed(&status_7d)));
    if unified_rejected {
        let raw = header_value(headers, "anthropic-ratelimit-unified-reset");
        if !raw.is_empty() {
            if !rejected.contains(&"unified") {
                rejected.push("unified");
            }
            if let Some(when) = parse_unix_or_timestamp(&raw).filter(|when| *when > now) {
                deadlines.push(when);
            }
        }
    }

    let Some(latest) = deadlines.into_iter().max() else {
        if !rejected.is_empty() {
            tracing::debug!(
                rejected_windows = rejected.join(","),
                "claude: rate limit window rejected without a reset; the generic backoff applies"
            );
        }
        return None;
    };
    let base = (latest - now).to_std().ok()?;
    let fuzz = Duration::from_secs(fuzz_seconds(min_fuzz, max_fuzz));
    let wait = base.saturating_add(fuzz);
    tracing::debug!(
        rejected_windows = rejected.join(","),
        wait_seconds = wait.as_secs(),
        "claude: parsed rate limit reset headers"
    );
    Some(wait)
}

fn fuzz_seconds(min: u64, max: u64) -> u64 {
    if max <= min {
        return min;
    }
    rand::random_range(min..=max)
}

/// Go's `strconv.ParseFloat`, close enough for header values.
fn parse_float(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok()
}

/// A reset given as Unix seconds, RFC 3339 or an HTTP date
/// (`parseUnixOrTimestamp`).
fn parse_unix_or_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(seconds) = parse_float(raw).filter(|s| *s > 0.0) {
        if !seconds.is_finite() || seconds >= 9_223_372_036_854_775_807.0 {
            return None;
        }
        let whole = seconds.trunc();
        let nanos = ((seconds - whole) * 1e9) as u32;
        return Utc.timestamp_opt(whole as i64, nanos).single();
    }
    if let Ok(when) = DateTime::parse_from_rfc3339(raw) {
        return Some(when.with_timezone(&Utc));
    }
    parse_http_date(raw)
}

/// `Retry-After` as seconds, an HTTP date or RFC 3339
/// (`parseRetryAfterHeader`).
fn parse_retry_after(raw: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(seconds) = parse_float(raw).filter(|s| *s > 0.0) {
        // Go's Duration holds at most 2^63 - 1 nanoseconds; past that the
        // conversion goes negative, and the deadline isn't in the future.
        // `seconds` is positive, so it isn't NaN.
        let nanos = seconds * 1e9;
        if nanos >= 9_223_372_036_854_775_807.0 {
            return None;
        }
        return chrono::TimeDelta::from_std(Duration::from_nanos(nanos as u64))
            .ok()
            .and_then(|delta| now.checked_add_signed(delta));
    }
    if let Some(when) = parse_http_date(raw) {
        return Some(when);
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|when| when.with_timezone(&Utc))
}

/// An HTTP date in any of the three forms HTTP/1.1 allows (Go's
/// `http.ParseTime`): IMF-fixdate, RFC 850 and ANSI C's `asctime`.
pub(crate) fn parse_http_date(raw: &str) -> Option<DateTime<Utc>> {
    if let Some((_, rest)) = raw.split_once(", ") {
        // `Sun, 06 Nov 1994 08:49:37 GMT`
        if let Ok(when) = NaiveDateTime::parse_from_str(rest, "%d %b %Y %H:%M:%S GMT") {
            return Some(when.and_utc());
        }
        // `Sunday, 06-Nov-94 08:49:37 GMT`, with any zone name.
        let (stamp, zone) = rest.rsplit_once(' ')?;
        if zone.is_empty() || !zone.bytes().all(|b| b.is_ascii_alphabetic()) {
            return None;
        }
        return NaiveDateTime::parse_from_str(stamp, "%d-%b-%y %H:%M:%S")
            .ok()
            .map(|when| when.and_utc());
    }
    // `Sun Nov  6 08:49:37 1994`
    let (_, rest) = raw.split_once(' ')?;
    NaiveDateTime::parse_from_str(rest, "%b %e %H:%M:%S %Y")
        .ok()
        .map(|when| when.and_utc())
}

/// A response's headers, as an error carries them: without the ones that
/// describe the body's bytes on the wire.
pub(crate) fn error_headers(headers: &HeaderMap) -> HeaderMap {
    let mut headers = headers.clone();
    headers.remove(http::header::CONTENT_ENCODING);
    headers.remove(http::header::CONTENT_LENGTH);
    headers.remove(http::header::TRANSFER_ENCODING);
    headers
}

/// A failed response as an error: its status, body, headers and when the
/// rate limit resets, scoped as upstream scopes a 429
/// (`classifyClaudeUpstreamErrorWithCooling`). With `model_level_cooling`,
/// even an account-wide rejection stays with the model.
pub(crate) fn classify(
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    model_level_cooling: bool,
) -> ExecError {
    let mut error = ExecError::upstream(status, String::from_utf8_lossy(body));
    error.headers = error_headers(headers);
    if (400..600).contains(&status) {
        error.retry_after = rate_limit_reset(headers, Utc::now());
    }
    if status == 429 {
        if !model_level_cooling && unified_rejection(headers) {
            return error.with_credential_scoped();
        }
        if fast_mode_credits(body) {
            return error.with_request_scoped();
        }
    }
    error
}

/// Whether a body is Anthropic refusing fast mode for lack of usage credits
/// (`claudeBodyIndicatesFastModeCredits`).
pub(crate) fn fast_mode_credits(body: &[u8]) -> bool {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let mut message = parsed
        .as_ref()
        .map(|value| str_at(value, "error.message"))
        .unwrap_or_default();
    if message.is_empty() {
        message = String::from_utf8_lossy(body).into_owned();
    }
    let message = open_ferry_translate::go::to_lower(&message);
    message.contains("fast request rejected")
        || (message.contains("fast")
            && (message.contains("usage credits") || message.contains("credits are required")))
}

/// A fast-mode request's error: request-scoped unless it is the
/// credential's, and without a status when the response was a success
/// (`wrapClaudeFastRequestError`). Other requests' errors pass unchanged.
pub(crate) fn wrap_fast(fast: bool, status: u16, mut error: ExecError) -> ExecError {
    if !fast {
        return error;
    }
    error.status = if (200..300).contains(&status) {
        0
    } else {
        status
    };
    error.request_scoped = !error.credential_scoped;
    error
}

/// A fast-mode request's failed response, passed back as it came
/// (`newClaudeFastDirectResponseError`): only a 429 gets a reset, and only
/// an account-wide rejection is the credential's.
pub(crate) fn fast_direct_error(status: u16, headers: &HeaderMap, body: &[u8]) -> ExecError {
    let mut error = ExecError::upstream(status, String::from_utf8_lossy(body));
    error.headers = error_headers(headers);
    if status == 429 {
        error.retry_after = rate_limit_reset(headers, Utc::now());
        error.credential_scoped = unified_rejection(headers);
    }
    error.request_scoped = !error.credential_scoped;
    error
}

/// An error without a status, as upstream's plain Go errors are.
pub(crate) fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::new(ErrorKind::Upstream, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).unwrap()
    }

    fn reset(pairs: &[(&'static str, &str)]) -> Option<Duration> {
        rate_limit_reset_with_fuzz(&headers(pairs), now(), 0, 0)
    }

    fn unix(offset: i64) -> String {
        (now() + TimeDelta::seconds(offset)).timestamp().to_string()
    }

    // TestParseClaudeRateLimitReset.
    #[test]
    fn reset_takes_the_latest_rejected_window() {
        assert_eq!(reset(&[]), None);
        let in_5h = unix(3600);
        let in_7d = unix(7200);
        assert_eq!(
            reset(&[
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-reset", &in_5h),
                ("anthropic-ratelimit-unified-7d-status", "rejected"),
                ("anthropic-ratelimit-unified-7d-reset", &in_7d),
            ]),
            Some(Duration::from_secs(7200))
        );
        // A window that isn't rejected doesn't count.
        assert_eq!(
            reset(&[
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-5h-reset", &in_5h),
                ("anthropic-ratelimit-unified-status", "allowed"),
            ]),
            None
        );
        // The unified reset applies when no window is said to be allowed.
        assert_eq!(
            reset(&[("anthropic-ratelimit-unified-reset", &in_5h)]),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(
            reset(&[("retry-after", "120")]),
            Some(Duration::from_secs(120))
        );
        let date = (now() + TimeDelta::seconds(90))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        assert_eq!(
            reset(&[("retry-after", &date)]),
            Some(Duration::from_secs(90))
        );
        // A reset in the past gives nothing.
        assert_eq!(
            reset(&[
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-reset", &unix(-10)),
            ]),
            None
        );
    }

    // Reset times past what Go's or chrono's clock holds are passed over or
    // kept as a long wait, without overflowing.
    #[test]
    fn extreme_resets_dont_overflow() {
        let rejected = "anthropic-ratelimit-unified-5h-status";
        for raw in [
            "9223372036854775807",
            "9223372036854775808",
            "9223371974719179007",
            "8210266876800",
            "1e300",
            "9999-12-31T23:59:59Z",
        ] {
            for name in ["retry-after", "anthropic-ratelimit-unified-reset"] {
                let _ = reset(&[(name, raw)]);
            }
            let _ = reset(&[
                (rejected, "rejected"),
                ("anthropic-ratelimit-unified-5h-reset", raw),
            ]);
            let _ = rate_limit_reset(&headers(&[("retry-after", raw)]), now());
        }
        assert_eq!(reset(&[("retry-after", "1e300")]), None);
        assert_eq!(
            reset(&[("retry-after", "9223372036")]),
            Some(Duration::from_secs(9_223_372_036))
        );
    }

    #[test]
    fn reset_adds_a_bounded_grace() {
        let pairs = [("retry-after", "10")];
        for _ in 0..20 {
            let wait = rate_limit_reset(&headers(&pairs), now()).unwrap().as_secs();
            assert!((11..=40).contains(&wait), "{wait}");
        }
    }

    // TestParseClaudeRateLimitReset_OverageOnly.
    #[test]
    fn overage_only_rejection_ignores_retry_after_and_stays_with_the_model() {
        let pairs = [
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ("retry-after", "120"),
        ];
        assert_eq!(reset(&pairs), None);
        assert!(!unified_rejection(&headers(&pairs)));
        // An omitted window counts as healthy only with a utilization below 1.
        let omitted = [
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.00"),
        ];
        assert!(!unified_rejection(&headers(&omitted)));
        let exhausted = [
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        ];
        assert!(unified_rejection(&headers(&exhausted)));
    }

    // TestClaudeHeadersIndicateUnifiedRateLimitRejection.
    #[test]
    fn unified_rejection_needs_a_shared_window() {
        assert!(unified_rejection(&headers(&[(
            "anthropic-ratelimit-unified-5h-status",
            "REJECTED"
        )])));
        assert!(unified_rejection(&headers(&[(
            "anthropic-ratelimit-unified-7d-status",
            "rejected"
        )])));
        assert!(unified_rejection(&headers(&[(
            "anthropic-ratelimit-unified-status",
            "rejected"
        )])));
        assert!(!unified_rejection(&headers(&[(
            "anthropic-ratelimit-unified-status",
            "allowed"
        )])));
        assert!(!unified_rejection(&HeaderMap::new()));
    }

    #[test]
    fn parses_http_dates_like_go() {
        let expected = Utc.with_ymd_and_hms(1994, 11, 6, 8, 49, 37).unwrap();
        for raw in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(parse_http_date(raw), Some(expected), "{raw}");
        }
        assert_eq!(parse_http_date("06 Nov 1994"), None);
        assert_eq!(parse_http_date("soon"), None);
        assert_eq!(
            parse_unix_or_timestamp("1994-11-06T08:49:37Z"),
            Some(expected)
        );
        assert_eq!(
            parse_unix_or_timestamp("784111777.5"),
            Some(expected + TimeDelta::milliseconds(500))
        );
        assert_eq!(parse_unix_or_timestamp("-5"), None);
        assert_eq!(parse_retry_after("1e30", now()), None);
    }

    // TestClassifyClaudeUpstreamError_*.
    #[test]
    fn classifies_429s_by_scope() {
        let unified = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("content-length", "2"),
            ("x-request-id", "req_1"),
        ]);
        let error = classify(429, &unified, b"{}", false);
        assert_eq!(error.status, 429);
        assert!(error.credential_scoped && !error.request_scoped);
        assert!(error.headers.get("content-length").is_none());
        assert_eq!(error.headers["x-request-id"], "req_1");
        assert!(!classify(429, &unified, b"{}", true).credential_scoped);

        let fast = br#"{"type":"error","error":{"type":"rate_limit_error","message":"Usage credits are required for fast mode"}}"#;
        let error = classify(429, &HeaderMap::new(), fast, false);
        assert!(error.request_scoped && !error.credential_scoped);
        assert_eq!(error.message, String::from_utf8_lossy(fast));

        let plain = classify(429, &HeaderMap::new(), b"slow down", false);
        assert!(!plain.request_scoped && !plain.credential_scoped);
        let retry = classify(529, &headers(&[("retry-after", "5")]), b"overloaded", false);
        assert!(retry.retry_after.is_some());
        assert_eq!(
            classify(200, &headers(&[("retry-after", "5")]), b"", false).retry_after,
            None
        );
    }

    #[test]
    fn recognizes_fast_mode_credit_refusals() {
        assert!(fast_mode_credits(b"Fast request rejected"));
        assert!(fast_mode_credits(
            br#"{"error":{"message":"FAST mode needs usage credits"}}"#
        ));
        assert!(!fast_mode_credits(
            br#"{"error":{"message":"Number of request tokens has exceeded your rate limit"}}"#
        ));
        assert!(!fast_mode_credits(
            br#"{"error":{"message":"rate limited"},"x":"fast usage credits"}"#
        ));
    }

    // TestClaudeFastRequestError_*.
    #[test]
    fn fast_errors_stay_with_the_request_unless_account_wide() {
        let error = wrap_fast(true, 0, plain_error("connection reset"));
        assert!(error.request_scoped);
        assert_eq!(error.status, 0);
        let validated = wrap_fast(true, 200, ExecError::upstream(502, "bad stream"));
        assert_eq!(validated.status, 0);
        let credential = wrap_fast(
            true,
            429,
            classify(
                429,
                &headers(&[("anthropic-ratelimit-unified-7d-status", "rejected")]),
                b"{}",
                false,
            ),
        );
        assert!(credential.credential_scoped && !credential.request_scoped);
        assert_eq!(credential.status, 429);
        let untouched = wrap_fast(false, 200, ExecError::upstream(502, "bad stream"));
        assert_eq!(untouched.status, 502);
        assert!(!untouched.request_scoped);
    }

    #[test]
    fn fast_direct_errors_keep_the_response() {
        let response = headers(&[
            ("content-encoding", "gzip"),
            ("content-type", "application/json"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ]);
        let error = fast_direct_error(429, &response, br#"{"error":{}}"#);
        assert_eq!(error.status, 429);
        assert_eq!(error.message, r#"{"error":{}}"#);
        assert!(error.headers.get("content-encoding").is_none());
        assert_eq!(error.headers["content-type"], "application/json");
        assert!(error.credential_scoped && !error.request_scoped);
        let error = fast_direct_error(400, &response, b"bad");
        assert!(error.request_scoped && !error.credential_scoped);
        assert_eq!(error.retry_after, None);
    }
}
