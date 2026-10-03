// Ported from CLIProxyAPI sdk/cliproxy/executor/types.go and the methods of
// sdk/cliproxy/auth's Manager that the HTTP handlers call (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A call to a provider, as the HTTP layer makes it: a [`Request`] and its
//! [`Options`], handed to a [`Dispatcher`] with the providers that serve the
//! model. The dispatcher picks a credential, calls the provider's executor,
//! and retries or rotates credentials as it sees fit.
//!
//! Executors translate: the payload is in the client's format
//! ([`Options::source_format`]), and the response comes back in
//! [`Options::response_format`]. Stream chunks per response format:
//!
//! | Format | Chunk |
//! |---|---|
//! | `openai` | a bare `chat.completion.chunk` JSON object, with no `data:` |
//! | `claude` | complete SSE text, possibly several events |
//! | `openai-response` | SSE text, which the HTTP layer checks and reframes |
//!
//! Dropping a returned future or stream cancels the call; there is no
//! separate cancellation token, where upstream passes a context.
//!
//! Deviations from upstream:
//! - The metadata upstream keeps in a map is a typed [`Metadata`], without the
//!   session-affinity and usage-logging keys, which aren't ported.
//! - Errors are one type, [`ExecError`], where upstream checks an error for
//!   optional methods (`StatusCode`, `Headers`, `IsTerminalAuth` and so on).

mod error;

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use http::HeaderMap;

pub use error::{ErrorKind, ExecError, WsClose};
pub use open_ferry_translate::registry::Format;

/// A provider's identifier, such as `codex` or `claude`.
pub type ProviderId = String;

/// The model and body of a call.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The model, with any thinking suffix such as `(high)`.
    pub model: String,
    /// The client's body, in [`Options::source_format`]. Empty when the client
    /// sent none.
    pub payload: Bytes,
}

/// How to make a call, and what the client asked for.
#[derive(Clone, Debug)]
pub struct Options {
    /// Whether the client asked for a stream.
    pub stream: bool,
    /// A variant of the endpoint, such as `responses/compact`, or empty.
    pub alt: String,
    /// The format the payload is in.
    pub source_format: Format,
    /// The format to answer in: the source format, for every route so far.
    pub response_format: Format,
    /// The client's request headers, without its proxy credentials.
    pub headers: HeaderMap,
    /// The client's query parameters, without its proxy credentials.
    pub query: Vec<(String, String)>,
    /// The client's body as it arrived, before the HTTP layer changed it.
    pub original_request: Bytes,
    /// Whether the client is on the Responses WebSocket.
    pub downstream_websocket: bool,
    /// Hints for credential selection.
    pub metadata: Metadata,
}

impl Options {
    /// Options for a call in `format`, which is both the source and the
    /// response format.
    pub fn new(format: Format) -> Self {
        Self {
            stream: false,
            alt: String::new(),
            source_format: format.clone(),
            response_format: format,
            headers: HeaderMap::new(),
            query: Vec::new(),
            original_request: Bytes::new(),
            downstream_websocket: false,
            metadata: Metadata::default(),
        }
    }
}

/// Called with the ID of the credential a call was given.
pub type SelectedAuthCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Hints for credential selection, from upstream's metadata keys.
#[derive(Clone, Default)]
pub struct Metadata {
    /// The model as the client named it (`requested_model`).
    pub requested_model: String,
    /// The route, such as `/v1/responses/compact` (`request_path`).
    pub request_path: String,
    /// The client's `Idempotency-Key` header (`idempotency_key`).
    pub idempotency_key: Option<String>,
    /// A model to pick the credential by, when it differs from the request's
    /// (`auth_selection_model`).
    pub auth_selection_model: Option<String>,
    /// The credential to use (`pinned_auth_id`).
    pub pinned_auth_id: Option<String>,
    /// The WebSocket session the call belongs to (`execution_session_id`).
    pub execution_session_id: Option<String>,
    /// Called with the credential picked (`selected_auth_callback`).
    pub selected_auth: Option<SelectedAuthCallback>,
}

impl fmt::Debug for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metadata")
            .field("requested_model", &self.requested_model)
            .field("request_path", &self.request_path)
            .field("idempotency_key", &self.idempotency_key)
            .field("auth_selection_model", &self.auth_selection_model)
            .field("pinned_auth_id", &self.pinned_auth_id)
            .field("execution_session_id", &self.execution_session_id)
            .field("selected_auth", &self.selected_auth.is_some())
            .finish()
    }
}

/// A non-streaming result.
#[derive(Clone, Debug, Default)]
pub struct Response {
    /// The body, in the response format.
    pub payload: Bytes,
    /// The provider's response headers.
    pub headers: HeaderMap,
}

/// A stream's chunks. An empty chunk carries nothing and is skipped.
pub type ChunkStream = BoxStream<'static, Result<Bytes, ExecError>>;

/// A streaming result.
pub struct StreamResponse {
    /// The provider's response headers.
    pub headers: HeaderMap,
    /// The chunks, in the response format.
    pub chunks: ChunkStream,
}

impl fmt::Debug for StreamResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamResponse")
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// Makes calls on the client's behalf: picks a credential among `providers`,
/// calls its executor, and retries as configured (upstream's auth manager).
pub trait Dispatcher: Send + Sync + 'static {
    /// A non-streaming call (`Manager.Execute`).
    fn execute<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>>;

    /// A token count (`Manager.ExecuteCount`).
    fn count_tokens<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>>;

    /// A streaming call (`Manager.ExecuteStream`).
    ///
    /// It returns once a credential's stream has given a non-empty chunk,
    /// which is the stream's first. When every credential failed before
    /// giving one, it returns either an error or a stream whose only item is
    /// the last error, as upstream does.
    fn execute_stream<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>>;

    /// Ends a WebSocket session's executor state, when the socket closes.
    fn close_execution_session(&self, _session_id: &str) {}
}
