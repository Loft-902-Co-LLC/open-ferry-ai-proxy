// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_executor.go
// (CodexAutoExecutor, codexWebsocketsEnabled) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses WebSocket upstream: Codex calls over a WebSocket to
//! `<base>/responses`, for clients on the Responses WebSocket.
//!
//! [`CodexExecutor`](super::CodexExecutor) takes this route when the client
//! is on the Responses WebSocket, the credential's `websockets` attribute
//! (or metadata) is on, and the call isn't `responses/compact`; any other
//! call goes over HTTP. Each request goes out as a `response.create`
//! message and Codex's events come back as messages, which reach the client
//! as they are but for the secrets the call sent, redacted from each (see
//! [`stream`] and [`execute`]).
//!
//! The calls of one Responses WebSocket session (its
//! `execution_session_id`) share a connection, one call at a time, which
//! [`close_execution_session`](open_ferry_core::executor::ProviderExecutor::close_execution_session)
//! closes when the client's socket ends; a call outside a session gets a
//! connection of its own, closed when it ends. A connection idle for five
//! minutes is closed, as is one whose credential, URL, proxy or token
//! changes. The token goes only in the handshake, so a refreshed token, or
//! an error event or close that ends the connection, has the next call
//! connect again.
//!
//! - [`request`] prepares the body, URL and handshake headers;
//! - [`dial`] connects, directly or through an HTTP proxy, and shakes hands;
//! - [`session`] keeps sessions, their connection and its reader;
//! - [`execute`] and [`stream`] make the calls;
//! - [`duplex`] keeps a streaming call's connection for the client's socket
//!   with `codex.response-steering` on;
//! - [`errors`] reads failures as upstream's errors.
//!
//! Deviations from upstream (each module lists its own):
//! - Upstream's `CodexAutoExecutor` wraps an HTTP and a WebSocket executor;
//!   here [`CodexExecutor`](super::CodexExecutor) picks the route itself.
//!   `responses/compact` goes over HTTP before the WebSocket route is
//!   considered, where upstream's WebSocket executor hands it to HTTP; the
//!   outcome is the same.
//! - The execution lifecycle binding, `RequiredUpstreamWebsocket`,
//!   `UpstreamDisconnectChan` and `CloseCodexWebsocketSessionsForAuthID`
//!   aren't ported, as the server uses none of them.
//! - Usage reporting and request logging are left to the call's taps: they
//!   are told of the `response.create` message before the connection is
//!   made, with no answer head, then of each message read (see the crate's
//!   `observe_send` module). A send tried again on a new connection isn't
//!   told again, and connection errors are only the call's; upstream
//!   records each.
//! - Each message Codex sends has the secrets the call sent redacted before
//!   it is read, if they are of eight bytes or more, as every client error
//!   is (see `Policy::Client` in the crate's `redact` module), what a model
//!   says in a successful answer as well as a failure; upstream passes each
//!   on as it came. The taps read each message as it came. A call keeping
//!   a connection redacts the secrets its handshake sent as well, as a
//!   custom header may have changed since.
//! - The connection log lines hide every secret its handshake sent, however
//!   short, from each field; upstream logs them as they are.

pub(crate) mod dial;
mod duplex;
pub(crate) mod errors;
mod execute;
#[cfg(test)]
pub(crate) mod mock;
pub(crate) mod request;
pub(crate) mod session;
mod stream;
#[cfg(test)]
mod tests;

use open_ferry_core::auth::Auth;
use open_ferry_core::exec::Options;
use serde_json::Value;

pub(super) use execute::execute;
pub(super) use session::Store;
pub(super) use stream::execute_stream;

/// The `alt` of a `/responses/compact` call, which never goes over the
/// WebSocket.
const COMPACT_ALT: &str = "responses/compact";

/// Whether a call goes over the WebSocket: the client is on the Responses
/// WebSocket, the credential has websockets on, and the call isn't
/// `responses/compact` (`CodexAutoExecutor`).
pub(super) fn routes(auth: &Auth, options: &Options) -> bool {
    options.downstream_websocket && options.alt != COMPACT_ALT && websockets_enabled(auth)
}

/// Whether the credential's `websockets` attribute, or else its metadata,
/// turns the WebSocket on (`codexWebsocketsEnabled`).
pub(crate) fn websockets_enabled(auth: &Auth) -> bool {
    if let Some(raw) = auth.attribute("websockets").map(str::trim)
        && !raw.is_empty()
        && let Some(parsed) = parse_bool(raw)
    {
        return parsed;
    }
    match auth.metadata.get("websockets") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => parse_bool(text.trim()).unwrap_or(false),
        _ => false,
    }
}

/// Go's `strconv.ParseBool`.
fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}
