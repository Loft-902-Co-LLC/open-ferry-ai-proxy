// Ported from the ProviderExecutor and ExecutionSessionCloser interfaces in
// CLIProxyAPI sdk/cliproxy/auth/conductor.go (v8.0.10, MIT).
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

use std::sync::Arc;
use std::time::Duration;

use futures_core::future::BoxFuture;

use crate::auth::Auth;
use crate::exec::{
    ErrorKind, ExecError, HttpCall, HttpReply, Options, Request, Response, StreamResponse,
};

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

    /// How long before its tokens expire to refresh a credential, or `None`
    /// to not refresh ahead (upstream's `RefreshLead`).
    fn refresh_lead(&self) -> Option<Duration> {
        None
    }

    /// Ends a Responses WebSocket session's state, when the socket closes
    /// (upstream's `ExecutionSessionCloser`). The session ID
    /// [`CLOSE_ALL_EXECUTION_SESSIONS`] asks for all of them to end.
    fn close_execution_session(&self, _session_id: &str) {}

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
}

/// The session ID that asks an executor to close all its sessions
/// (upstream's `CloseAllExecutionSessionsID`).
pub const CLOSE_ALL_EXECUTION_SESSIONS: &str = "__all_execution_sessions__";
