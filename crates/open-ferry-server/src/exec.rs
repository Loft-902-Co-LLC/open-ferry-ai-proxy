// Ported from CLIProxyAPI executeWithAuthManagerFormats and
// executeCountWithAuthManager in sdk/api/handlers/handlers_execution.go,
// executeStreamWithAuthManagerFormats in sdk/api/handlers/handlers_stream.go,
// enrichAuthSelectionError in sdk/api/handlers/handlers_errors.go, and
// requestExecutionMetadata and GetAlt in sdk/api/handlers/handlers.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Calls to providers, as the handlers make them.
//!
//! A [`Call`] owns everything it needs, so its futures and streams are
//! `'static` and can outlive the handler that made them. Each call carries
//! the request's observation: its context, and the taps the request log and
//! the usage statistics give the call.
//!
//! Deviations from upstream:
//! - [`Call::new`] refuses a payload with 128 or more arrays and objects
//!   inside one another, with a 400 ([`body::check_depth`]); upstream calls
//!   with a payload of any depth.

use std::sync::Arc;

use axum::extract::{FromRequestParts, MatchedPath};
use bytes::Bytes;
use futures_util::stream::{self, BoxStream, StreamExt};
use http::HeaderMap;
use http::request::Parts;
use open_ferry_core::exec::{
    ChunkStream, ExecError, Format, Metadata, Options, ProviderId, Request, StreamResponse,
};
use open_ferry_core::observe::{Observation, RequestContext};

use crate::auth::{Principal, strip_credentials};
use crate::body;
use crate::entry_protocol;
use crate::errors::ErrorMessage;
use crate::headers::filter_upstream_headers;
use crate::query;
use crate::request_context;
use crate::routing;
use crate::sse_check::SseCheck;
use crate::state::AppState;

/// What a request carries besides its body, as the handlers pass it to
/// calls. The client's key is already gone.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientRequest {
    /// The request headers, without the client's key.
    pub(crate) headers: HeaderMap,
    /// The query parameters, without the client's key.
    pub(crate) query: Vec<(String, String)>,
    /// The route that matched, such as `/v1/chat/completions`, or the path
    /// when none did.
    pub(crate) path: String,
    /// The trimmed `Idempotency-Key` header, if it isn't empty.
    pub(crate) idempotency_key: Option<String>,
    /// The `alt` query parameter, or `$alt`, with `sse` made empty
    /// (upstream's `GetAlt`).
    pub(crate) alt: String,
    /// Who the client authenticated as. It is not sent upstream.
    pub(crate) principal: Principal,
    /// The request's context, which each call's observation carries.
    /// `None` only for a request that didn't pass through the router, as
    /// in tests.
    pub(crate) context: Option<Arc<RequestContext>>,
}

impl ClientRequest {
    /// Reads what a call needs from a request's parts.
    pub(crate) fn from_parts(parts: &Parts) -> Self {
        let mut headers = parts.headers.clone();
        let mut query = query::parse(parts.uri.query().unwrap_or(""));
        let alt = match query::first(&query, "alt").or_else(|| query::first(&query, "$alt")) {
            Some("sse") | None => String::new(),
            Some(alt) => alt.to_owned(),
        };
        strip_credentials(&mut headers, &mut query);
        let path = match parts.extensions.get::<MatchedPath>() {
            Some(matched) => matched.as_str().trim().to_owned(),
            None => parts.uri.path().trim().to_owned(),
        };
        let idempotency_key = parts
            .headers
            .get("idempotency-key")
            .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
            .filter(|key| !key.is_empty());
        let principal = parts
            .extensions
            .get::<Principal>()
            .copied()
            .unwrap_or_default();
        Self {
            headers,
            query,
            path,
            idempotency_key,
            alt,
            principal,
            context: request_context::of(&parts.extensions).cloned(),
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientRequest {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(parts))
    }
}

/// A non-streaming result.
#[derive(Debug)]
pub(crate) struct Reply {
    /// The body, in the response format.
    pub(crate) body: Bytes,
    /// The provider's headers to send on: empty unless
    /// `passthrough-headers` is on.
    pub(crate) headers: HeaderMap,
}

/// A stream's items. An error ends it.
pub(crate) type HandlerStream = BoxStream<'static, Result<Bytes, ErrorMessage>>;

/// A stream that has started: its first item is the first payload, or the
/// error that stopped it before one. A stream that ends with no items closed
/// before it gave anything.
pub(crate) struct Started {
    /// The provider's headers to send on: empty unless
    /// `passthrough-headers` is on.
    pub(crate) headers: HeaderMap,
    /// The payloads, in the response format.
    pub(crate) items: HandlerStream,
}

impl Started {
    /// A stream whose only item is `error`.
    pub(crate) fn failed(error: ErrorMessage) -> Self {
        Self {
            headers: HeaderMap::new(),
            items: stream::iter([Err(error)]).boxed(),
        }
    }
}

/// A call, routed and ready to make.
pub(crate) struct Call {
    pub(crate) state: AppState,
    pub(crate) providers: Vec<ProviderId>,
    pub(crate) request: Request,
    pub(crate) options: Options,
}

impl Call {
    /// Routes a call for `model` with `payload` in `format`. `alt` is the
    /// endpoint variant, and `stream` whether the client asked for a stream.
    ///
    /// A `payload` nested deeper than [`body::MAX_DEPTH`] is refused with a
    /// 400 before anything else, even the routing: a handler that couldn't
    /// read such a body has no model to route, and a translator or an
    /// executor would read it as empty.
    pub(crate) fn new(
        state: &AppState,
        client: &ClientRequest,
        format: Format,
        model: &str,
        payload: Bytes,
        alt: &str,
        stream: bool,
    ) -> Result<Self, ErrorMessage> {
        body::check_depth(&payload)?;
        let mut route = routing::route(state.catalog(), model)?;
        route.providers = entry_protocol::adjust_execution_providers(&format, route.providers);
        Ok(Self::routed(
            state, client, format, model, route, payload, alt, stream,
        ))
    }

    /// A call for `model` that goes where `route` says, as [`Call::new`]
    /// makes once it has routed the model.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn routed(
        state: &AppState,
        client: &ClientRequest,
        format: Format,
        model: &str,
        route: routing::Route,
        payload: Bytes,
        alt: &str,
        stream: bool,
    ) -> Self {
        let mut options = Options::new(format);
        options.stream = stream;
        options.alt = alt.to_owned();
        options.headers = client.headers.clone();
        options.query = client.query.clone();
        options.original_request = payload.clone();
        options.metadata = Metadata {
            requested_model: model.to_owned(),
            request_path: client.path.clone(),
            idempotency_key: client.idempotency_key.clone(),
            ..Metadata::default()
        };
        let request = Request {
            model: route.model,
            payload,
        };
        options.observation = client
            .context
            .as_ref()
            .map(|context| observe(state, context, &request, &options));
        Self {
            state: state.clone(),
            providers: route.providers,
            request,
            options,
        }
    }

    /// Makes a non-streaming call.
    pub(crate) async fn execute(self) -> Result<Reply, ErrorMessage> {
        let passthrough = self.state.settings().config.passthrough_headers;
        let dispatcher = self.state.dispatcher_arc();
        let model = self.request.model.clone();
        match dispatcher
            .execute(&self.providers, self.request, self.options)
            .await
        {
            Ok(response) => Ok(Reply {
                body: response.payload,
                headers: passed_on(&response.headers, passthrough),
            }),
            Err(err) => Err(ErrorMessage::from_exec(enrich(
                err,
                &self.providers,
                &model,
            ))),
        }
    }

    /// Counts tokens. Upstream sets no response format for a count, so
    /// neither does this.
    pub(crate) async fn count_tokens(mut self) -> Result<Reply, ErrorMessage> {
        self.options.response_format = Format::from_static("");
        let passthrough = self.state.settings().config.passthrough_headers;
        let dispatcher = self.state.dispatcher_arc();
        let model = self.request.model.clone();
        match dispatcher
            .count_tokens(&self.providers, self.request, self.options)
            .await
        {
            Ok(response) => Ok(Reply {
                body: response.payload,
                headers: passed_on(&response.headers, passthrough),
            }),
            Err(err) => Err(ErrorMessage::from_exec(enrich(
                err,
                &self.providers,
                &model,
            ))),
        }
    }

    /// Starts a stream, restarting it as configured while it fails before
    /// its first payload. A Responses stream's events are checked as they
    /// pass.
    pub(crate) async fn stream(self) -> Started {
        let settings = self.state.settings();
        let passthrough = settings.config.passthrough_headers;
        let max_retries = settings.config.streaming.bootstrap_retries;
        drop(settings);
        let dispatcher = self.state.dispatcher_arc();
        let Call {
            providers,
            request,
            options,
            ..
        } = self;
        let check = options.response_format == Format::OPENAI_RESPONSE;
        let model = request.model.clone();

        let start = |request: Request, options: Options| {
            let dispatcher = Arc::clone(&dispatcher);
            let providers = providers.clone();
            async move {
                dispatcher
                    .execute_stream(&providers, request, options)
                    .await
            }
        };

        let mut response: StreamResponse = match start(request.clone(), options.clone()).await {
            Ok(response) => response,
            Err(err) => {
                return Started::failed(ErrorMessage::from_exec(enrich(err, &providers, &model)));
            }
        };
        let mut retries = 0;
        let outcome = loop {
            let mut checker = check.then(SseCheck::default);
            match read_first(&mut response.chunks, &mut checker).await {
                First::Payload(payload) => break Ok((Some(payload), Source::Open, checker)),
                First::Closed => break Ok((None, Source::Closed, checker)),
                First::Failed(error) => break Err(error),
                First::StreamError(err) => {
                    if retries >= max_retries || !restartable(&err) {
                        break Err(ErrorMessage::from_exec(err));
                    }
                    retries += 1;
                    match start(request.clone(), options.clone()).await {
                        Ok(next) => response = next,
                        Err(retry_err) => {
                            let original = ErrorMessage::from_exec(err);
                            break Err(
                                if retry_err.kind.is_auth_selection() && original.status >= 500 {
                                    original
                                } else {
                                    ErrorMessage::from_exec(enrich(retry_err, &providers, &model))
                                },
                            );
                        }
                    }
                }
            }
        };

        let headers = passed_on(&response.headers, passthrough);
        let items = match outcome {
            Err(error) => stream::iter([Err(error)]).boxed(),
            Ok((first, source, checker)) => {
                let producer = Producer {
                    first,
                    chunks: response.chunks,
                    source,
                    checker,
                };
                stream::unfold(producer, |mut producer| async move {
                    let item = producer.next().await?;
                    Some((item, producer))
                })
                .boxed()
            }
        };
        Started { headers, items }
    }
}

/// The observation of a call made with `request` and `options` for the
/// request of `context`: the context, and the taps the request log and the
/// usage statistics give the call. With both off, it has none.
fn observe(
    state: &AppState,
    context: &Arc<RequestContext>,
    request: &Request,
    options: &Options,
) -> Arc<Observation> {
    let observability = state.observability();
    let taps = [
        observability.request_log.tap(context),
        observability.usage.tap(context, request, options),
    ];
    Arc::new(Observation::new(
        Arc::clone(context),
        taps.into_iter().flatten().collect(),
    ))
}

/// What a stream gave first.
enum First {
    /// A payload to send.
    Payload(Bytes),
    /// Nothing: it ended.
    Closed,
    /// The provider's error, which a restart may get past.
    StreamError(ExecError),
    /// An error of the proxy's own, such as a Responses event that isn't
    /// JSON.
    Failed(ErrorMessage),
}

/// Reads a stream up to its first payload.
async fn read_first(chunks: &mut ChunkStream, checker: &mut Option<SseCheck>) -> First {
    while let Some(item) = chunks.next().await {
        let chunk = match item {
            Ok(chunk) if chunk.is_empty() => continue,
            Ok(chunk) => chunk,
            Err(err) => return First::StreamError(err),
        };
        match pass(checker, chunk) {
            Ok(Some(payload)) => return First::Payload(payload),
            Ok(None) => {}
            Err(error) => return First::Failed(error),
        }
    }
    First::Closed
}

/// Whether a stream that failed before its first payload may be restarted:
/// a failure with no status, an auth or rate-limit status, a timeout, or a
/// server error.
fn restartable(err: &ExecError) -> bool {
    match err.http_status() {
        0 | 401 | 402 | 403 | 408 | 429 => true,
        status => status >= 500,
    }
}

/// Checks a chunk when the stream is checked, giving what may be sent now.
fn pass(checker: &mut Option<SseCheck>, chunk: Bytes) -> Result<Option<Bytes>, ErrorMessage> {
    let Some(checker) = checker else {
        return Ok(Some(chunk));
    };
    match checker.add_chunk(&chunk) {
        Ok(checked) if checked.is_empty() => Ok(None),
        Ok(checked) => Ok(Some(Bytes::from(checked))),
        Err(err) => Err(ErrorMessage::new(502, err)),
    }
}

/// Where a started stream's remaining chunks come from.
enum Source {
    /// The provider's stream.
    Open,
    /// Nowhere: the stream ended before its first payload, and only the
    /// final check is left.
    Closed,
    /// Nowhere: the stream is over.
    Done,
}

/// The rest of a started stream.
struct Producer {
    first: Option<Bytes>,
    chunks: ChunkStream,
    source: Source,
    checker: Option<SseCheck>,
}

impl Producer {
    async fn next(&mut self) -> Option<Result<Bytes, ErrorMessage>> {
        if let Some(first) = self.first.take() {
            return Some(Ok(first));
        }
        loop {
            match self.source {
                Source::Done => return None,
                Source::Closed => return self.close(),
                Source::Open => {}
            }
            if let Some(err) = self.checker.as_mut().and_then(SseCheck::take_overflow) {
                self.stop();
                return Some(Err(ErrorMessage::new(502, err)));
            }
            let chunk = match self.chunks.next().await {
                None => return self.close(),
                Some(Err(err)) => {
                    self.stop();
                    return Some(Err(ErrorMessage::from_exec(err)));
                }
                Some(Ok(chunk)) if chunk.is_empty() => continue,
                Some(Ok(chunk)) => chunk,
            };
            match pass(&mut self.checker, chunk) {
                Ok(Some(payload)) => return Some(Ok(payload)),
                Ok(None) => {}
                Err(error) => {
                    self.stop();
                    return Some(Err(error));
                }
            }
        }
    }

    /// Ends the stream, with an error if what the checker held back is bad.
    fn close(&mut self) -> Option<Result<Bytes, ErrorMessage>> {
        self.stop();
        let err = self.checker.as_mut()?.finish().err()?;
        Some(Err(ErrorMessage::new(502, err)))
    }

    /// Ends the stream, dropping the provider's chunks now, which cancels the
    /// call, rather than when the client's response is done with this.
    fn stop(&mut self) {
        self.source = Source::Done;
        self.chunks = stream::empty().boxed();
    }
}

/// The provider's headers to send on: the filtered headers with
/// `passthrough-headers` on, otherwise none.
fn passed_on(headers: &HeaderMap, passthrough: bool) -> HeaderMap {
    if passthrough {
        filter_upstream_headers(headers)
    } else {
        HeaderMap::new()
    }
}

/// Says which providers and model an auth selection error was for
/// (upstream's `enrichAuthSelectionError`).
pub(crate) fn enrich(mut err: ExecError, providers: &[ProviderId], model: &str) -> ExecError {
    if !err.kind.is_auth_selection() {
        return err;
    }
    let providers = match providers.join(",") {
        joined if joined.is_empty() => "unknown".to_owned(),
        joined => joined,
    };
    let model = match model.trim() {
        "" => "unknown",
        model => model,
    };
    let base = match err.message.trim() {
        "" => "no auth available",
        base => base,
    };
    let mut detail = match err.cause.as_deref() {
        Some(summary) if !summary.is_empty() && !base.contains(summary) => {
            format!("{base} (providers={providers}, model={model}; last upstream error: {summary})")
        }
        _ => format!("{base} (providers={providers}, model={model})"),
    };
    if format!(",{providers},").contains(",claude,") {
        detail.push_str(
            "; check Claude auth/key session and cooldown state via /v0/management/auth-files",
        );
    }
    err.message = detail;
    if err.status == 0 {
        err.status = 503;
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn enriches_auth_selection_errors() {
        let providers = ["codex".to_owned(), "claude".to_owned()];
        let err = enrich(ExecError::auth_not_found(), &providers, " gpt-5 ");
        assert_eq!(err.status, 503);
        assert_eq!(
            err.to_string(),
            "auth_not_found: no auth available (providers=codex,claude, model=gpt-5); \
             check Claude auth/key session and cooldown state via /v0/management/auth-files"
        );

        let caused =
            ExecError::auth_unavailable(Duration::from_secs(3)).with_cause("rate_limit: slow");
        let err = enrich(caused, &[], "");
        assert_eq!(err.status, 503);
        assert_eq!(
            err.to_string(),
            "auth_unavailable: no auth available (providers=unknown, model=unknown; \
             last upstream error: rate_limit: slow)"
        );

        let other = enrich(ExecError::upstream(400, "bad"), &providers, "m");
        assert_eq!(other.to_string(), "bad");
        let cooldown = ExecError::model_cooldown("m", None, Duration::from_secs(1), None);
        let text = cooldown.to_string();
        assert_eq!(enrich(cooldown, &providers, "m").to_string(), text);
    }

    #[test]
    fn restarts_only_retryable_failures() {
        assert!(restartable(&ExecError::new(
            open_ferry_core::exec::ErrorKind::Upstream,
            "x"
        )));
        for status in [401, 402, 403, 408, 429, 500, 503, 529] {
            assert!(restartable(&ExecError::upstream(status, "x")), "{status}");
        }
        for status in [400, 404, 413, 422] {
            assert!(!restartable(&ExecError::upstream(status, "x")), "{status}");
        }
        assert!(!restartable(&ExecError::canceled()));
    }

    #[test]
    fn reads_client_requests() {
        let request = http::Request::builder()
            .uri("/v1/messages?alt=sse&key=secret&beta=true")
            .header("authorization", "Bearer secret")
            .header("idempotency-key", "  k-1 ")
            .header("anthropic-version", "2023-06-01")
            .body(())
            .unwrap();
        let (parts, ()) = request.into_parts();
        let client = ClientRequest::from_parts(&parts);
        assert_eq!(client.alt, "");
        assert_eq!(client.path, "/v1/messages");
        assert_eq!(client.idempotency_key.as_deref(), Some("k-1"));
        assert!(client.headers.get("authorization").is_none());
        assert_eq!(client.headers.len(), 2);
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        assert_eq!(client.query, [pair("alt", "sse"), pair("beta", "true")]);

        let request = http::Request::builder()
            .uri("/x?%24alt=media&idempotency=1")
            .header("idempotency-key", "  ")
            .body(())
            .unwrap();
        let client = ClientRequest::from_parts(&request.into_parts().0);
        assert_eq!(client.alt, "media");
        assert_eq!(client.idempotency_key, None);
        // An empty `alt` still counts, so `$alt` isn't read.
        let request = http::Request::builder()
            .uri("/x?alt=&$alt=media")
            .body(())
            .unwrap();
        assert_eq!(ClientRequest::from_parts(&request.into_parts().0).alt, "");
    }
}
