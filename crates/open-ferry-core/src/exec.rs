// Ported from CLIProxyAPI sdk/cliproxy/executor/types.go and the methods of
// sdk/cliproxy/auth's Manager that the HTTP handlers call (v8.0.15, MIT).
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
//!   session-affinity keys, which aren't ported. What the request log and
//!   the usage statistics read of the request, upstream's context values
//!   and usage keys, is the call's [`Options::observation`].
//! - Errors are one type, [`ExecError`], where upstream checks an error for
//!   optional methods (`StatusCode`, `Headers`, `IsTerminalAuth` and so on).
//!   An executor's error carries no code of its own (upstream's `Error.Code`),
//!   so a credential's recorded error has a code only for the manager's own
//!   errors; no upstream executor sets one.
//! - The Responses WebSocket learns what it needs about credentials from one
//!   query, [`Dispatcher::websocket_support`], where upstream's handler reads
//!   the auth manager's credentials and the model registry itself.
//! - Codex Alpha Search is one call, [`Dispatcher::codex_alpha_search`],
//!   where upstream's handler picks the credential and sends the request
//!   through the auth manager itself.
//! - A finished video is fetched with [`Dispatcher::download`], where
//!   upstream's handler looks the credential up and fetches it itself.
//! - The client's frames for response steering travel in
//!   [`Options::websocket_input`], where upstream puts them in the call's
//!   context (see [`WebsocketInput`]).

mod download;
mod error;
mod http_call;
mod websocket_input;

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use http::HeaderMap;

use crate::observe::Observation;

pub use download::{Download, Downloaded};
pub use error::{ErrorKind, ExecError, TransportFault, WsClose};
pub use http_call::{AlphaSearch, HttpCall, HttpReply, HttpTarget};
pub use open_ferry_translate::registry::Format;
pub use websocket_input::{AuthCheck, InputFrame, WebsocketInput};

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
    /// What the call's observers see: the request's context, and the taps
    /// that see its upstream traffic. `None` for a call no client request
    /// made.
    pub observation: Option<Arc<Observation>>,
    /// The client's further frames on a Responses WebSocket with response
    /// steering on, which a Codex WebSocket stream reads while it runs
    /// (upstream's `WithWebsocketInput`). `None` everywhere else.
    pub websocket_input: Option<WebsocketInput>,
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
            observation: None,
            websocket_input: None,
        }
    }

    /// The call's observation when any tap sees it.
    pub fn tapped(&self) -> Option<&Arc<Observation>> {
        self.observation
            .as_ref()
            .filter(|observation| observation.is_tapped())
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
    /// The only provider the call may use, whatever providers it is given
    /// (upstream's `ForcedProvider`, which an Interactions `agent` request
    /// sets).
    pub forced_provider: Option<ProviderId>,
    /// Leave out Codex credentials on the free plan (`disallow_free_auth`,
    /// which the images endpoints set for Codex calls).
    pub disallow_free_auth: bool,
    /// The hashed namespace of the client's API key
    /// ([`caller_scope`](crate::session::caller_scope)), which keeps
    /// different callers' derived sessions apart (`caller_scope`).
    pub caller_scope: String,
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
            .field("forced_provider", &self.forced_provider)
            .field("disallow_free_auth", &self.disallow_free_auth)
            .field("caller_scope", &!self.caller_scope.is_empty())
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

    /// What the Responses WebSocket may rely on for `model` among
    /// `providers`, and, with `auth_id`, what it needs to know about that
    /// credential (the credential lookups in upstream's
    /// `openai_responses_websocket_session.go`).
    ///
    /// `model` is the routed model without its thinking suffix, and
    /// `providers` the providers that serve it, which may be none. The
    /// WebSocket asks after a call too, about each credential the call's
    /// [`Metadata::selected_auth`] was given. The default supports nothing,
    /// so every turn goes over HTTP.
    fn websocket_support(
        &self,
        _providers: &[ProviderId],
        _model: &str,
        _auth_id: Option<&str>,
    ) -> WebsocketSupport {
        WebsocketSupport::default()
    }

    /// A Codex Alpha Search call: picks a credential the
    /// `codex_alpha_search_v1` policy allows and sends it the client's
    /// payload (upstream's `codexAlphaSearch` handler, past reading the
    /// body). The reply comes back whatever its status; an error carries the
    /// status to answer with. The default has no credentials: 503.
    fn codex_alpha_search(
        &self,
        _request: AlphaSearch,
    ) -> BoxFuture<'_, Result<HttpReply, ExecError>> {
        Box::pin(async {
            Err(
                ExecError::new(ErrorKind::Upstream, "Codex auth manager unavailable")
                    .with_status(503),
            )
        })
    }

    /// Fetches a file a provider made from the URL it gave, through the
    /// proxy of the credential that made it and without its token (the
    /// fetch in upstream's `writeVideoContentFromURL`). The answer comes
    /// back whatever its status; an error means none came, and carries the
    /// status to answer with. Nothing is recorded on the credential. The
    /// default fetches nothing: 502.
    fn download(&self, _download: Download) -> BoxFuture<'_, Result<Downloaded, ExecError>> {
        Box::pin(async {
            Err(ExecError::new(ErrorKind::Upstream, "downloads aren't supported").with_status(502))
        })
    }
}

/// What the Responses WebSocket may rely on for a model, and what it knows
/// about one credential ([`Dispatcher::websocket_support`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebsocketSupport {
    /// The model's available credentials are all of one provider, `codex` or
    /// `xai`, and all have websockets on, so the WebSocket may hand a
    /// session's requests on as they are
    /// (`responsesWebsocketUsesUpstreamWebsocketPassthrough`).
    pub upstream_passthrough: bool,
    /// The model has an available credential and all of them are `codex`,
    /// whose upstream reads a compacted transcript as it is
    /// (`websocketUpstreamSupportsCompactionReplayForModel`).
    pub compaction_replay: bool,
    /// The credential asked about, unless the dispatcher doesn't know it.
    pub auth: Option<WebsocketAuth>,
}

/// A credential, as the Responses WebSocket sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebsocketAuth {
    /// Its provider, in lower case.
    pub provider: ProviderId,
    /// Whether it may serve the model now: its provider is one of those
    /// asked about, it is registered for the model, and it is neither
    /// disabled nor cooling down for it
    /// (`responsesWebsocketPinnedAuthMatchesModel`).
    pub serves_model: bool,
    /// Whether its `websockets` attribute is on
    /// (`websocketUpstreamSupportsIncrementalInput`).
    pub websockets: bool,
    /// Whether it is disabled, by its flag or its status, so a call holding
    /// its connection may send nothing more.
    pub disabled: bool,
}
