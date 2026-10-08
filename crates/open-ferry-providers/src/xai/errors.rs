// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_response.go
// (xaiStatusErr, isXAIBadCredentialsBody) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The error for xAI's error status and body.
//!
//! The body is the message, as it came. Two failures change it: a 403 for
//! credentials xAI no longer takes becomes a 401, so that the credential
//! manager treats it as an expired key rather than a payment failure; and a
//! 429 for the free tier's exhausted usage asks to wait 24 hours, its
//! rolling window. Any other 429 is left to the manager's backoff.
//!
//! Deviations from upstream: none.

use std::time::Duration;

use open_ferry_translate::go::to_lower;
use serde_json::Value;

use crate::codex::terminal::StatusError;
use crate::json::{get, str_of};

/// How long the free tier's exhausted usage lasts: its rolling window
/// (`xaiFreeUsageExhaustedCooldown`).
pub(crate) const FREE_USAGE_EXHAUSTED_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

/// What an expired or revoked credential's error says.
const BAD_CREDENTIALS: &str = "bad-credentials";

/// What an expired or revoked credential's error message says.
const NOT_VALIDATED: &str = "access token could not be validated";

/// The error for xAI's error `status` and `body` (`xaiStatusErr`).
pub(crate) fn status_error(status: u16, body: &[u8]) -> StatusError {
    let mut error = StatusError::new(status, String::from_utf8_lossy(body));
    if body.is_empty() {
        return error;
    }
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if status == 403 && is_bad_credentials_body(body, &parsed) {
        error.status = 401;
        return error;
    }
    if status != 429 {
        return error;
    }
    let code = to_lower(&str_of(get(&parsed, "code")));
    let mut message = to_lower(&str_of(get(&parsed, "error")));
    if message.is_empty() {
        message = to_lower(&String::from_utf8_lossy(body));
    }
    if code.contains("free-usage-exhausted")
        || message.contains("free-usage-exhausted")
        || message.contains("included free usage")
    {
        error.retry_after = Some(FREE_USAGE_EXHAUSTED_COOLDOWN);
    }
    error
}

/// Whether an error body says the credential isn't valid any more, rather
/// than that it may not do something (`isXAIBadCredentialsBody`).
fn is_bad_credentials_body(body: &[u8], parsed: &Value) -> bool {
    let has = |path: &str, needle: &str| to_lower(&str_of(get(parsed, path))).contains(needle);
    if ["code", "error.code", "body.error.code"]
        .into_iter()
        .any(|path| has(path, BAD_CREDENTIALS))
    {
        return true;
    }
    if [
        "error",
        "error.message",
        "message",
        "body.error",
        "body.error.message",
    ]
    .into_iter()
    .any(|path| has(path, NOT_VALIDATED))
    {
        return true;
    }
    let raw = to_lower(&String::from_utf8_lossy(body));
    raw.contains(BAD_CREDENTIALS) || raw.contains(NOT_VALIDATED)
}

#[cfg(test)]
mod tests;
