// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (XAIWebsocketsExecutor, XAIAutoExecutor, xaiWebsocketsEnabled,
// buildXAIResponsesWebsocketURL, CloseExecutionSession) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses WebSocket upstream for xAI: streaming calls over a
//! WebSocket to `<base>/responses`, for clients on the Responses WebSocket.
//!
//! [`XaiExecutor`](super::XaiExecutor) takes this route for a streaming
//! call when the client is on the Responses WebSocket and the credential's
//! `websockets` attribute (or metadata) is on; any other call goes over
//! HTTP. The request is prepared as for HTTP, then goes out as a
//! `response.create` message, and xAI's events come back as messages,
//! undone and bridged as the HTTP stream's are, then handed to the client
//! as events.
//!
//! The URL is the credential's `base_url` (or xAI's API) with `http`
//! becoming `ws` and `https` becoming `wss`; Grok's CLI chat proxy, which
//! takes no WebSocket, is never dialled. The handshake sends the bearer
//! token, the credential's custom headers (see [`crate::custom_headers`])
//! and `x-grok-conv-id`, which is only ever the `prompt_cache_key` the
//! client sent.
//!
//! The connections are Codex's (see [`crate::codex::websocket`]): the calls
//! of one Responses WebSocket session (its `execution_session_id`) share a
//! connection, one call at a time, which
//! [`close_execution_session`](open_ferry_core::executor::ProviderExecutor::close_execution_session)
//! closes; a call outside a session gets a connection of its own. Each
//! session also keeps the response IDs and transcript of [`ids`].
//!
//! - [`stream`] makes the call;
//! - [`ids`] keeps what a session remembers between its calls;
//! - [`message`] builds the message and the events made up for the client;
//! - [`errors`] reads xAI's error events;
//! - [`compaction`] answers a `compaction_trigger` over HTTP.
//!
//! Deviations from upstream (each module lists its own):
//! - Upstream's `XAIAutoExecutor` wraps an HTTP and a WebSocket executor;
//!   here [`XaiExecutor`](super::XaiExecutor) picks the route itself, and
//!   the sessions are the executor's, not global.
//! - A credential whose `base_url` is Grok's CLI chat proxy is dialled at
//!   xAI's API, as a compact call is sent there; upstream dials the proxy,
//!   which refuses the upgrade.
//! - The execution lifecycle binding, `RequiredUpstreamWebsocket`,
//!   `UpstreamDisconnectChan` and `CloseXAIWebsocketSessionsForAuthID`
//!   aren't ported, as the server uses none of them (as for Codex).
//! - Every message xAI sends, a success or a failure, reaches the client
//!   with the secrets its handshake sent redacted if they are of eight
//!   bytes or more, as every client error is (see `Policy::Client` in the
//!   crate's `redact` module); upstream passes them on. The redaction comes
//!   before anything else reads the message, so the session's transcript
//!   keeps the redacted text too; the call's taps see each message as it
//!   came. A `compaction_trigger`'s events are made from the compact answer
//!   redacted whole, while the transcript keeps the compaction as xAI sent
//!   it, since it goes back to xAI (see the `compaction` module). A call
//!   keeping a connection redacts the secrets that connection's handshake
//!   sent as well as its own, as a custom header may have changed since
//!   the connection opened, and xAI may quote either.
//! - The log lines hide those secrets, however short, from every field
//!   they fill in; upstream logs them as they are.
//! - Usage reporting and request logging are left to the call's taps, as
//!   for Codex's WebSocket (see [`crate::codex::websocket`]).

mod compaction;
mod errors;
mod ids;
mod message;
mod stream;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Options};
use open_ferry_core::executor::CLOSE_ALL_EXECUTION_SESSIONS;

use super::request::{CLI_CHAT_PROXY_BASE_URL, DEFAULT_BASE_URL, base_url};
use crate::codex::request::refuse_control_characters;
use crate::codex::websocket::request::split_scheme;
use crate::codex::websocket::session::Store;
use crate::codex::websocket::websockets_enabled;

pub(super) use stream::execute_stream;

/// Whether a streaming call goes over the WebSocket: the client is on the
/// Responses WebSocket and the credential has websockets on
/// (`XAIAutoExecutor.ExecuteStream`).
pub(super) fn routes(auth: &Auth, options: &Options) -> bool {
    options.downstream_websocket && websockets_enabled(auth)
}

/// Where a session's connection last went: its credential, URL and proxy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Target {
    auth_id: String,
    url: String,
    proxy: String,
}

/// The executor's WebSocket sessions (`globalXAIWebsocketSessionStore` and
/// `globalXAIWebsocketIDStates`).
pub(super) struct Sessions {
    /// The sessions and their connections.
    store: Store,
    /// What each session remembers between calls, by the session's ID or
    /// the client's `prompt_cache_key`.
    ids: ids::Store,
    /// Where each session's connection last went, kept when it closes
    /// (upstream's session `authID`, `wsURL` and `proxyURL`).
    targets: Mutex<HashMap<String, Target>>,
}

impl Sessions {
    pub(super) fn new() -> Self {
        Self {
            store: Store::new(),
            ids: ids::Store::default(),
            targets: Mutex::new(HashMap::new()),
        }
    }

    /// Sessions whose connections close after `idle` without a message.
    #[cfg(test)]
    fn with_idle(idle: std::time::Duration) -> Self {
        Self {
            store: Store::with_idle(idle),
            ..Self::new()
        }
    }

    fn targets(&self) -> MutexGuard<'_, HashMap<String, Target>> {
        self.targets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes the session `id` names and forgets what it remembered, or
    /// closes all of them for [`CLOSE_ALL_EXECUTION_SESSIONS`]
    /// (`CloseExecutionSession`). Closing all keeps what the sessions
    /// remembered, as upstream does.
    pub(super) fn close(&self, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        if id == CLOSE_ALL_EXECUTION_SESSIONS {
            self.store.close(id);
            self.targets().clear();
            return;
        }
        self.store.close(id);
        self.ids.delete(id);
        self.targets().remove(id);
    }

    /// Whether the session `id` names last connected elsewhere: to another
    /// credential, URL or proxy (`websocketSessionTargetChanged`). A session
    /// that never connected hasn't changed.
    fn target_changed(&self, id: &str, target: &Target) -> bool {
        let targets = self.targets();
        let Some(last) = targets.get(id) else {
            return false;
        };
        if last.auth_id.trim().is_empty() && last.url.trim().is_empty() {
            return false;
        }
        last.auth_id.trim() != target.auth_id.trim()
            || last.url.trim() != target.url.trim()
            || last.proxy.trim() != target.proxy.trim()
    }

    /// Notes where the session `id` names is connected.
    fn record_target(&self, id: &str, target: &Target) {
        self.targets().insert(id.to_owned(), target.clone());
    }
}

/// The WebSocket URL of `auth`'s `/responses`
/// (`buildXAIResponsesWebsocketURL`): `http` becomes `ws` and `https`
/// becomes `wss`; `ws` and `wss` stay. Grok's CLI chat proxy becomes xAI's
/// API.
fn websocket_url(auth: &Auth) -> Result<String, ExecError> {
    let mut base = base_url(auth);
    if base.trim_end_matches('/') == CLI_CHAT_PROXY_BASE_URL {
        base = DEFAULT_BASE_URL;
    }
    let base = base.strip_suffix('/').unwrap_or(base);
    let http_url = format!("{base}/responses");
    let trimmed = http_url.trim();
    refuse_control_characters(trimmed)?;
    let (scheme, rest) = split_scheme(trimmed);
    let ws_scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => {
            return Err(ExecError::new(
                ErrorKind::Upstream,
                format!(
                    "xai websockets executor: unsupported responses websocket URL scheme {scheme:?}"
                ),
            ));
        }
    };
    let host = rest
        .strip_prefix("//")
        .map(|authority| {
            let end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
            let authority = authority.get(..end).unwrap_or_default();
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host)
        })
        .unwrap_or_default();
    if host.trim().is_empty() {
        return Err(ExecError::new(
            ErrorKind::Upstream,
            "xai websockets executor: responses websocket URL host is empty",
        ));
    }
    Ok(format!("{ws_scheme}:{rest}"))
}
