// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (parseXAIWebsocketError, xaiBareWebsocketErrorStatus,
// mapXAIWebsocketReadError, mapXAIWebsocketWriteError) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI's error events, and the errors of a broken connection.
//!
//! An `error` event with a status is read as Codex's are (see
//! [`crate::codex::websocket::errors`]), then given xAI's status and wait
//! for its body (see [`crate::xai::errors`]): a 403 for credentials xAI no
//! longer takes is a 401, and the free tier's exhausted usage waits a day.
//! Any other event with an `error` field is a bare error: its status is
//! its `status`, else `status_code`, else a positive number in its
//! `error.code`, `error.status` or `code`, else 400 for a request
//! validation error, else 500. Its body holds the status and the event's
//! error.
//!
//! Deviations from upstream:
//! - A bare error's status beyond an HTTP status's range is 500.
//! - The errors of a broken connection are Codex's, with Codex's executor
//!   named as xAI's in their text.

use std::time::SystemTime;

use open_ferry_core::exec::ExecError;
use serde_json::{Value, json};

use crate::codex::websocket::errors::{self as codex, Failure};
use crate::json::{get, int_at, set, str_at};
use crate::redact::Secrets;
use crate::xai::errors::status_error;

/// How Codex's executor names itself in a failure's text.
const CODEX_PREFIX: &str = "codex websockets executor: ";

/// How xAI's executor names itself.
const XAI_PREFIX: &str = "xai websockets executor: ";

/// The error for xAI's error event `event`, whose text is `raw`
/// (`parseXAIWebsocketError`); `None` when it isn't one. Its message has
/// the `secrets` redacted.
pub(super) fn parse_error(
    event: &Value,
    raw: &[u8],
    secrets: &Secrets,
    now: SystemTime,
) -> Option<ExecError> {
    if let Some((mut error, _, _)) = codex::parse_ws_error(event, false, secrets, now) {
        let xai = status_error(error.status, raw);
        error.status = xai.status;
        if xai.retry_after.is_some() {
            error.retry_after = xai.retry_after;
        }
        return Some(error);
    }
    let error = get(event, "error")?;
    if raw.is_empty() {
        return None;
    }
    let mut status = int_at(event, "status");
    if status <= 0 {
        status = int_at(event, "status_code");
    }
    if status <= 0 {
        status = bare_status(event);
    }
    let status = u16::try_from(status).unwrap_or(500);
    let mut out = json!({"type": "error", "status": status});
    set(&mut out, "error", error.clone());
    Some(
        status_error(status, out.to_string().as_bytes())
            .redacted(secrets)
            .into(),
    )
}

/// A bare error's status (`xaiBareWebsocketErrorStatus`).
fn bare_status(event: &Value) -> i64 {
    for path in ["error.code", "error.status", "code"] {
        let raw = str_at(event, path);
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if let Ok(status) = raw.parse::<i64>()
            && status > 0
        {
            return status;
        }
    }
    let message = str_at(event, "error.message");
    if message.contains(r#""code":"400""#) || message.contains("Request validation error") {
        return 400;
    }
    500
}

/// The error for a failed read (`mapXAIWebsocketReadError`).
pub(super) fn read_error(failure: &Failure) -> ExecError {
    rename(codex::error(failure))
}

/// The error for a failed send (`mapXAIWebsocketWriteError`).
pub(super) fn write_error(disconnect: Option<u16>, failure: &Failure) -> ExecError {
    rename(codex::write_error(disconnect, failure))
}

/// Names xAI's executor in place of Codex's.
fn rename(mut error: ExecError) -> ExecError {
    if let Some(rest) = error.message.strip_prefix(CODEX_PREFIX) {
        error.message = format!("{XAI_PREFIX}{rest}");
    }
    error
}
