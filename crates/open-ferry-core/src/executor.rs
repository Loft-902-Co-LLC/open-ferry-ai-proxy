// Ported from the ProviderExecutor and ExecutionSessionCloser interfaces in
// CLIProxyAPI sdk/cliproxy/auth/conductor.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What calls a provider: a [`ProviderExecutor`] for each provider, which
//! the credential manager hands a credential and a call.
//!
//! An executor translates the payload from [`Options::source_format`] to
//! its provider's format, sends it with the credential, and translates the
//! answer to [`Options::response_format`], chunk by chunk when streaming
//! (see [`crate::exec`] for each format's chunks). It reports a provider's
//! failure as an [`ExecError`] with the provider's status, body and headers,
//! and leaves retries and cooldowns to the manager.
//!
//! Deviations from upstream:
//! - `HttpRequest` is [`ProviderExecutor::http_request`], which takes an
//!   [`HttpCall`] and reads the answer's body; only Codex Alpha Search uses
//!   it, and executors that don't need it keep the default, which refuses.
//!   The management API's `api-call` doesn't go through it.
//! - Upstream's optional interfaces (`ExecutionSessionCloser` and others) are
//!   methods with defaults.
//! - [`ProviderExecutor::download`] fetches a file a provider made, which
//!   upstream's video handler fetches with an HTTP client of its own; only
//!   xAI's executor does, and the default refuses.

use std::sync::Arc;
use std::time::Duration;

use futures_core::future::BoxFuture;

use crate::auth::Auth;
use crate::exec::{
    Downloaded, ErrorKind, ExecError, HttpCall, HttpReply, Options, Request, Response,
    StreamResponse,
};

/// Options for refreshing independent credential partitions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshOptions {
    /// Refresh every refreshable partition even if it is not due.
    pub force: bool,
}

impl RefreshOptions {
    /// Options that set `force` and leave every other option at its default.
    pub fn forced(force: bool) -> Self {
        Self { force }
    }
}

/// Calls one provider with a credential (upstream's `ProviderExecutor`).
pub trait ProviderExecutor: Send + Sync + 'static {
    /// The provider served, such as `codex`; [`Auth::provider`] of the
    /// credentials it takes.
    fn id(&self) -> &str;

    /// A non-streaming call.
    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>>;

    /// A streaming call. It returns once the provider has answered with a
    /// success status, before reading the body.
    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>>;

    /// Counts the request's input tokens.
    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>>;

    /// Refreshes the credential's tokens, and returns the record with the new
    /// ones in its metadata. A credential with nothing to refresh, such as an
    /// API key, comes back unchanged.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>>;

    /// Refreshes with the manager's force flag. Existing executors retain
    /// their `refresh` implementation. Codex executors attach successful
    /// partition patches and any partial failure to the returned record.
    /// They clear `Auth::codex_refresh_failure` on complete success. The due
    /// helpers mark a failed partition due again after the manager's backoff.
    fn refresh_with_options(
        &self,
        auth: Arc<Auth>,
        _options: RefreshOptions,
    ) -> BoxFuture<'_, Result<Auth, ExecError>> {
        self.refresh(auth)
    }

    /// How long before its tokens expire to refresh a credential, or `None`
    /// to not refresh ahead (upstream's `RefreshLead`).
    fn refresh_lead(&self) -> Option<Duration> {
        None
    }

    /// Ends a Responses WebSocket session's state, when the socket closes
    /// (upstream's `ExecutionSessionCloser`). The session ID
    /// [`CLOSE_ALL_EXECUTION_SESSIONS`] asks for all of them to end.
    fn close_execution_session(&self, _session_id: &str) {}

    /// The shared session store, when sessions survive executor replacement.
    fn execution_session_store(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        None
    }

    /// Whether replacing this executor with `other` keeps the same sessions.
    fn shares_execution_sessions_with(&self, other: &dyn ProviderExecutor) -> bool {
        match (
            self.execution_session_store(),
            other.execution_session_store(),
        ) {
            (Some(store), Some(next)) => Arc::ptr_eq(&store, &next),
            _ => false,
        }
    }

    /// Closes one credential's sessions; executors without that capability
    /// retain the upstream fallback of closing all sessions.
    fn close_auth_execution_sessions(&self, _auth_id: &str) {
        self.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS);
    }

    /// Forwards the original interrupt to the current upstream, without dialing.
    /// `false` means there is no active upstream WebSocket.
    fn interrupt_execution_session<'a>(
        &'a self,
        _session_id: &'a str,
        _payload: &'a [u8],
        _auth_check: crate::exec::AuthCheck,
    ) -> BoxFuture<'a, Result<bool, ExecError>> {
        Box::pin(async {
            Err(ExecError::upstream(
                400,
                "upstream executor does not support response.interrupt",
            ))
        })
    }

    /// Sends `call` with the credential's token and custom headers, and
    /// returns the answer whatever its status (upstream's `HttpRequest`). An
    /// error means no answer came. The default sends nothing.
    fn http_request(
        &self,
        _auth: Arc<Auth>,
        _call: HttpCall,
    ) -> BoxFuture<'_, Result<HttpReply, ExecError>> {
        let message = format!(
            "{} executor: plain HTTP requests aren't supported",
            self.id()
        );
        Box::pin(async move { Err(ExecError::new(ErrorKind::Upstream, message)) })
    }

    /// Fetches `url`, a file the provider made, with no credential headers,
    /// through `auth`'s proxy, or the global proxy without a credential
    /// (see [`crate::exec::Download`]). The answer comes back whatever its
    /// status; an error means none came. The default fetches nothing.
    fn download(
        &self,
        _auth: Option<Arc<Auth>>,
        _url: String,
    ) -> BoxFuture<'_, Result<Downloaded, ExecError>> {
        let message = format!("{} executor: downloads aren't supported", self.id());
        Box::pin(async move { Err(ExecError::new(ErrorKind::Upstream, message).with_status(502)) })
    }
}

/// The session ID that asks an executor to close all its sessions
/// (upstream's `CloseAllExecutionSessionsID`).
pub const CLOSE_ALL_EXECUTION_SESSIONS: &str = "__all_execution_sessions__";
