// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_errors.go
// (parseCodexWebsocketErrorWithCooling, buildCodexWebsocketErrorPayload,
// isCodexWebsocketConnectionLimitError, parseCodexWebsocketErrorHeaders) and
// codex_websockets_connection.go (mapCodexWebsocketWriteError,
// shouldRetryCodexWebsocketSend, mapCodexWebsocketReadError) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Failures of the Responses WebSocket as upstream's errors: error events,
//! closes, and broken connections.
//!
//! Upstream's errors are gorilla's and Go's, recognized later by their text;
//! a [`Failure`] keeps that text (`websocket: close 1006 (abnormal closure):
//! unexpected EOF`, `i/o timeout`, ...) so the same failures classify the
//! same way, and also marks them with a [`TransportFault`].
//!
//! Deviations from upstream:
//! - An error event's status above 65535 becomes 500.
//! - The secrets the connection sent (its credential headers after the
//!   custom ones, each cookie, the URL's credentials, the proxy's password
//!   and the credential's key or tokens) are redacted from an error event's
//!   body, a close reason and the text of another connection failure if
//!   they are of eight bytes or more, as in every client error; see
//!   [`crate::redact`] and its `Policy::Client`.
//! - A message past the size limit ends the connection with `websocket: read
//!   limit exceeded`; upstream sets no limit.

use std::time::{Duration, SystemTime};

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::exec::{ErrorKind, ExecError, TransportFault};
use serde_json::{Map, Value};
use tokio_tungstenite::tungstenite;

use crate::codex::terminal::{is_usage_limit, parse_retry_after, status_text};
use crate::json::{get, int_at, str_at};
use crate::redact::{Policy, Secrets};

/// The body of the error for a message too big for Codex.
pub(super) const MESSAGE_TOO_BIG_BODY: &str = r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#;

/// The close code for a message too big.
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;

/// A message too big for Codex (`codexWebsocketMessageTooBigError`): status
/// 413, this request's fault only.
pub(super) fn message_too_big() -> ExecError {
    ExecError::upstream(413, MESSAGE_TOO_BIG_BODY).with_request_scoped()
}

/// Why reading from or writing to the connection failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Codex closed the connection with `code` (gorilla's `CloseError`).
    Close { code: u16, reason: String },
    /// Anything else, with its text; `transient` for a network failure that
    /// may clear on its own.
    Other { message: String, transient: bool },
}

impl Failure {
    /// The connection ended without a close (gorilla reads an EOF as close
    /// 1006).
    pub(super) fn eof() -> Self {
        Self::Close {
            code: 1006,
            reason: "unexpected EOF".to_owned(),
        }
    }

    /// Nothing came for the idle timeout.
    pub(super) fn timeout() -> Self {
        Self::Other {
            message: "codex websockets executor: read: i/o timeout".to_owned(),
            transient: true,
        }
    }

    /// The connection was closed on our side.
    pub(crate) fn closed() -> Self {
        Self::Other {
            message: "codex websockets executor: use of closed network connection".to_owned(),
            transient: true,
        }
    }

    /// The session's reader stopped without a word.
    pub(super) fn channel_closed() -> Self {
        Self::Other {
            message: "codex websockets executor: session read channel closed".to_owned(),
            transient: false,
        }
    }

    /// Codex sent a binary message.
    pub(super) fn binary() -> Self {
        Self::Other {
            message: "codex websockets executor: unexpected binary message".to_owned(),
            transient: false,
        }
    }

    /// The failure for an error of the WebSocket library.
    pub(super) fn from_ws(error: &tungstenite::Error) -> Self {
        use tungstenite::error::ProtocolError;
        match error {
            tungstenite::Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
                Self::eof()
            }
            tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
                Self::eof()
            }
            tungstenite::Error::Io(io) => Self::Other {
                message: format!("codex websockets executor: {io}"),
                transient: true,
            },
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                Self::closed()
            }
            tungstenite::Error::Capacity(_) => Self::Other {
                message: "websocket: read limit exceeded".to_owned(),
                transient: false,
            },
            other => Self::Other {
                message: format!("codex websockets executor: {other}"),
                transient: false,
            },
        }
    }

    /// The failure with every copy of the `secrets` the connection sent in
    /// its text redacted, as a client's error is (see [`crate::redact`]):
    /// Codex's close reason, or a network error, may quote the token.
    pub(crate) fn redacted(self, secrets: &Secrets) -> Self {
        match self {
            Self::Close { code, reason } => Self::Close {
                code,
                reason: secrets.text(reason, Policy::Client),
            },
            Self::Other { message, transient } => Self::Other {
                message: secrets.text(message, Policy::Client),
                transient,
            },
        }
    }

    /// The text upstream's error would have.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Close { code, reason } => {
                let mut text = format!("websocket: close {code}{}", close_description(*code));
                if !reason.is_empty() {
                    text.push_str(": ");
                    text.push_str(reason);
                }
                text
            }
            Self::Other { message, .. } => message.clone(),
        }
    }
}

/// What gorilla's `CloseError` says after a close code.
fn close_description(code: u16) -> &'static str {
    match code {
        1000 => " (normal)",
        1001 => " (going away)",
        1002 => " (protocol error)",
        1003 => " (unsupported data)",
        1005 => " (no status)",
        1006 => " (abnormal closure)",
        1007 => " (invalid payload data)",
        1008 => " (policy violation)",
        1009 => " (message too big)",
        1010 => " (mandatory extension missing)",
        1011 => " (internal server error)",
        1015 => " (TLS handshake error)",
        _ => "",
    }
}

/// The error for a failed read (`mapCodexWebsocketReadError`): close 1009
/// is a message too big, anything else keeps its text.
pub(crate) fn error(failure: &Failure) -> ExecError {
    if let Failure::Close {
        code: CLOSE_MESSAGE_TOO_BIG,
        ..
    } = failure
    {
        return message_too_big();
    }
    let error = ExecError::new(ErrorKind::Upstream, failure.text());
    match failure {
        Failure::Close {
            code: 1000 | 1001 | 1006,
            ..
        } => error.with_transport(TransportFault::Lifecycle),
        Failure::Other {
            transient: true, ..
        } => error.with_transport(TransportFault::Transient),
        _ => error,
    }
}

/// The error for a failed send (`mapCodexWebsocketWriteError`): a message
/// too big when Codex closed with 1009, else the send's own error.
pub(crate) fn write_error(disconnect: Option<u16>, failure: &Failure) -> ExecError {
    if disconnect == Some(CLOSE_MESSAGE_TOO_BIG) {
        return message_too_big();
    }
    error(failure)
}

/// Whether a failed send is worth one more try on a new connection
/// (`shouldRetryCodexWebsocketSend`): not when it was the request's fault.
pub(crate) fn should_retry(error: &ExecError) -> bool {
    !error.request_scoped
}

/// An error event: its error, its status, and the body upstream builds for
/// it (`parseCodexWebsocketErrorWithCooling`). `None` when the event isn't
/// one: not of type `error`, or without a positive status.
///
/// The body keeps upstream's shape: the status, then the event's `body` and
/// its `error`, or the event's `error`, or a `server_error` with the
/// status's text. A usage limit is scoped to the credential unless
/// `model_level_cooling` keeps it to the model; a connection limit without
/// a reset time may be retried at once.
pub(crate) fn parse_ws_error(
    event: &Value,
    model_level_cooling: bool,
    secrets: &Secrets,
    now: SystemTime,
) -> Option<(ExecError, u16, String)> {
    if str_at(event, "type").trim() != "error" {
        return None;
    }
    let mut status = int_at(event, "status");
    if status == 0 {
        status = int_at(event, "status_code");
    }
    if status <= 0 {
        return None;
    }
    let status = u16::try_from(status).unwrap_or(500);
    let out = error_body(event, status);
    let raw = out.to_string();
    let mut error = ExecError::upstream(status, secrets.text(raw.clone(), Policy::Client));
    error.headers = error_headers(event);
    error.credential_scoped = is_usage_limit(&out) && !model_level_cooling;
    error.retry_after = parse_retry_after(status, &raw, &out, now)
        .or_else(|| is_connection_limit(event).then_some(Duration::ZERO));
    Some((error, status, raw))
}

/// The body upstream makes of an error event
/// (`buildCodexWebsocketErrorPayload`).
fn error_body(event: &Value, status: u16) -> Value {
    let mut out = Map::new();
    out.insert("status".to_owned(), Value::from(status));
    if let Some(body) = get(event, "body") {
        out.insert("body".to_owned(), body.clone());
        if let Some(error) = get(body, "error") {
            out.insert("error".to_owned(), error.clone());
            return Value::Object(out);
        }
    }
    if let Some(error) = get(event, "error") {
        out.insert("error".to_owned(), error.clone());
        return Value::Object(out);
    }
    let mut error = Map::new();
    error.insert("type".to_owned(), Value::from("server_error"));
    error.insert("message".to_owned(), Value::from(status_text(status)));
    out.insert("error".to_owned(), Value::Object(error));
    Value::Object(out)
}

/// Whether the event says too many connections are open
/// (`isCodexWebsocketConnectionLimitError`).
fn is_connection_limit(event: &Value) -> bool {
    [
        "error.code",
        "error.type",
        "body.error.code",
        "body.error.type",
        "code",
        "error",
    ]
    .iter()
    .any(|path| str_at(event, path).trim() == "websocket_connection_limit_reached")
}

/// The headers an error event carries (`parseCodexWebsocketErrorHeaders`):
/// string, number and boolean values that aren't blank.
fn error_headers(event: &Value) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let Some(Value::Object(fields)) = get(event, "headers") else {
        return headers;
    };
    for (key, value) in fields {
        let name = key.trim();
        if name.is_empty() {
            continue;
        }
        let text = match value {
            Value::String(text) => text.trim().to_owned(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&text),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}
