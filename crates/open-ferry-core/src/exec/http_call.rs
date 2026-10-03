// Ported from the HttpRequest methods of CLIProxyAPI sdk/cliproxy/auth's
// ProviderExecutor and Manager (conductor.go, conductor_execution.go)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Plain HTTP calls made with a credential, outside the translated calls:
//! what [`ProviderExecutor::http_request`] sends and gets back, and the
//! Codex Alpha Search call a [`Dispatcher`](super::Dispatcher) makes.
//!
//! Deviations from upstream:
//! - A call is an [`HttpCall`] rather than an `http.Request`, and the
//!   executor reads the answer's body, up to the call's limit, where
//!   upstream hands the caller the open response.
//! - A call may name a path under the executor's own base URL
//!   ([`HttpTarget::Path`]), which tests point at a local server; upstream's
//!   caller always gives the whole URL.
//!
//! [`ProviderExecutor::http_request`]: crate::executor::ProviderExecutor::http_request

use std::fmt;

use bytes::Bytes;
use http::{HeaderMap, Method};

/// Where an [`HttpCall`] goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpTarget {
    /// This URL.
    Url(String),
    /// This path under the executor's default base URL, such as
    /// `/alpha/search` under Codex's `https://chatgpt.com/backend-api/codex`.
    Path(String),
}

/// A request an executor sends with a credential, after adding the
/// credential's token and custom headers (upstream's `HttpRequest`, which
/// runs the executor's `PrepareRequest`).
#[derive(Clone)]
pub struct HttpCall {
    /// The method.
    pub method: Method,
    /// Where it goes.
    pub target: HttpTarget,
    /// The headers, which the executor adds the credential's own to.
    pub headers: HeaderMap,
    /// The body.
    pub body: Bytes,
    /// The client's headers, without its key, which a credential's custom
    /// header values may name (`$Name`).
    pub client_headers: HeaderMap,
    /// The most bytes of the answer's body to read; the rest is dropped.
    pub response_limit: usize,
}

impl fmt::Debug for HttpCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpCall")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("body_len", &self.body.len())
            .field("response_limit", &self.response_limit)
            .finish_non_exhaustive()
    }
}

/// The answer to an [`HttpCall`], whatever its status.
#[derive(Clone, Debug, Default)]
pub struct HttpReply {
    /// The HTTP status.
    pub status: u16,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, up to the call's limit.
    pub body: Bytes,
    /// Why the body ended early, when reading it failed; [`Self::body`] holds
    /// what was read.
    pub read_error: Option<String>,
}

/// A Codex Alpha Search call (`POST /v1/alpha/search`), as the client made
/// it.
#[derive(Clone, Default)]
pub struct AlphaSearch {
    /// The client's payload.
    pub body: Bytes,
    /// The client's headers, without its key.
    pub headers: HeaderMap,
}

impl fmt::Debug for AlphaSearch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlphaSearch")
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}
