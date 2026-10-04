// Ported from CLIProxyAPI internal/api/middleware/request_logging.go
// (RequestLoggingMiddleware, attachDeferredRequestBodyCapture,
// shouldSkipMethodForRequestLogging, isResponsesWebsocketUpgrade,
// shouldCaptureRequestBody, captureRequestInfo, shouldLogRequest),
// internal/api/middleware/response_writer.go (ResponseWriterWrapper's
// Write, WriteHeader and Finalize) and internal/logging/cpa_trace.go
// (CPATraceIDMiddleware) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The request log's capture layer: what the client sent and what it was
//! answered, for the request log and the error logs, and the
//! `X-CPA-TRACE-ID` header naming the credential that served the request.
//!
//! The layer runs inside the access log and outside CORS, so it sees every
//! request with its [`RequestContext`], and the answer CORS gave. For a
//! request it logs, it starts the request's capture (see
//! [`RequestLogger::start`]), keeps the client's request, and wraps the
//! answer's body to keep what is sent; once the body ends, or the client
//! leaves, it hands the request to the logger ([`request_log::finish`]),
//! whose writer thread writes it. Nothing waits on the log.
//!
//! What is logged:
//! - Not a `GET`, unless it is a Responses WebSocket upgrade, nor the
//!   management API (`/v0/management`, `/v8/management`, `/management`),
//!   nor an OAuth callback (a path ending `/callback`).
//! - With `request-log` on, the client's body is read before the handler
//!   runs, up to the server's body limit, and every answer is kept.
//! - With it off, what the handler reads of the client's body is kept as it
//!   reads it (not a multipart form), and only an error answer.
//! - A WebSocket session's log is written when the session ends, with the
//!   upstream attempts of all its turns.
//!
//! The trace header is set on every answer whose request picked a
//! credential before the answer's head was ready, unless the answer has
//! one.
//!
//! Deviations from upstream:
//! - Upstream installs the middleware only without `commercial-mode`; the
//!   layer is always installed and reads the setting for each request.
//! - OAuth callbacks aren't logged.
//! - With `request-log` off, a body of up to 1 MiB isn't read ahead of the
//!   handler: every body is kept as the handler reads it.
//! - With `request-log` on, the body read ahead is bounded by the server's
//!   body limit, which the handler then enforces; a body that fails to read
//!   is logged as far as it was read.
//! - A client that leaves before the answer's head is logged with status
//!   499; one that leaves during the body, with what was sent.
//! - The trace ID is the latest selection's, taken when the answer's head
//!   is ready.
//!
//! [`RequestContext`]: open_ferry_core::observe::RequestContext
//! [`RequestLogger::start`]: open_ferry_core::observe::request_log::RequestLogger::start

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use http_body::{Frame, SizeHint};
use open_ferry_core::observe::RequestContext;
use open_ferry_core::observe::request_log::{
    self, Answer, CPA_TRACE_ID_HEADER, DeferredCapture, Downstream, Mode, RequestBody,
    STATUS_CLIENT_CLOSED_REQUEST,
};

use crate::request_context;
use crate::state::AppState;

#[cfg(test)]
mod tests;

/// The path prefixes of the management API, which is never logged
/// (upstream's `shouldLogRequest`).
const MANAGEMENT_PREFIXES: [&str; 3] = ["/v0/management", "/v8/management", "/management"];

/// Captures a request and its answer for the request log, and sets the
/// trace header.
pub(crate) async fn layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(context) = request_context::of(request.extensions()).cloned() else {
        return next.run(request).await;
    };
    let mode = if should_log(request.method(), request.uri().path(), request.headers()) {
        state.observability().request_log.start(&context)
    } else {
        None
    };
    let Some(mode) = mode else {
        let mut response = next.run(request).await;
        set_trace_header(&context, response.headers_mut());
        return response;
    };

    let (parts, body) = request.into_parts();
    let content_length = http_body::Body::size_hint(&body).exact();
    let (body, kept) = match capture_plan(mode, &parts.headers, content_length) {
        Capture::Eager => {
            let limit = state.settings().config.body_limit;
            let (raw, truncated, body) = read_ahead(body, limit).await;
            (body, RequestBody::Captured { raw, truncated })
        }
        Capture::Deferred => {
            let capture = DeferredCapture::new(content_length);
            let body = Body::new(TeeBody {
                inner: body,
                capture: capture.clone(),
            });
            (body, RequestBody::Deferred(capture))
        }
        Capture::None => (body, RequestBody::None),
    };
    let downstream = Downstream {
        url: Downstream::url(parts.uri.path(), parts.uri.query()),
        method: parts.method.as_str().to_owned(),
        headers: parts.headers.clone(),
        body: kept,
    };
    let mut pending = Pending {
        context: Arc::clone(&context),
        downstream: Some(downstream),
    };

    let mut response = next.run(Request::from_parts(parts, body)).await;
    set_trace_header(&context, response.headers_mut());
    let Some(downstream) = pending.downstream.take() else {
        return response;
    };
    respond(context, mode, downstream, response)
}

/// Whether a request with `method`, `path` and `headers` is logged.
pub(crate) fn should_log(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    !skips_method(method, path, headers) && should_log_path(path)
}

/// Whether a request's method keeps it out of the log (upstream's
/// `shouldSkipMethodForRequestLogging`): a `GET` that isn't a Responses
/// WebSocket upgrade.
pub(crate) fn skips_method(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    method == Method::GET && !is_responses_websocket_upgrade(path, headers)
}

/// Whether a request is a Responses WebSocket upgrade (upstream's
/// `isResponsesWebsocketUpgrade`).
pub(crate) fn is_responses_websocket_upgrade(path: &str, headers: &HeaderMap) -> bool {
    (path == "/v1/responses" || path == "/backend-api/codex/responses")
        && headers.get(header::UPGRADE).is_some_and(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .trim()
                .eq_ignore_ascii_case("websocket")
        })
}

/// Whether a request to `path` may be logged (upstream's
/// `shouldLogRequest`, and not an OAuth callback).
pub(crate) fn should_log_path(path: &str) -> bool {
    !MANAGEMENT_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        && !path.ends_with("/callback")
}

/// How the client's body is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Capture {
    /// Read before the handler runs.
    Eager,
    /// Kept as the handler reads it.
    Deferred,
    /// Not kept.
    None,
}

/// How the body of a request logged in `mode`, with `headers` and
/// `content_length`, is kept (upstream's `shouldCaptureRequestBody` and
/// `attachDeferredRequestBodyCapture`): read ahead with `request-log` on;
/// else kept as it is read, unless it is empty or a multipart form.
pub(crate) fn capture_plan(
    mode: Mode,
    headers: &HeaderMap,
    content_length: Option<u64>,
) -> Capture {
    match mode {
        Mode::Full => Capture::Eager,
        Mode::ErrorsOnly => {
            let multipart = headers.get(header::CONTENT_TYPE).is_some_and(|value| {
                String::from_utf8_lossy(value.as_bytes())
                    .trim()
                    .to_ascii_lowercase()
                    .starts_with("multipart/form-data")
            });
            if content_length == Some(0) || multipart {
                Capture::None
            } else {
                Capture::Deferred
            }
        }
    }
}

/// Reads `body` up to `limit` bytes (upstream's `captureRequestInfo`):
/// what was read, cut at the limit, whether the body went past it or
/// failed, and the body for the handler, the same bytes from the start.
async fn read_ahead(body: Body, limit: usize) -> (Bytes, bool, Body) {
    if http_body::Body::is_end_stream(&body) {
        return (Bytes::new(), false, body);
    }
    let mut data = body.into_data_stream();
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut len = 0usize;
    let mut failed = None;
    let mut ended = false;
    while len <= limit {
        match data.next().await {
            Some(Ok(chunk)) => {
                len = len.saturating_add(chunk.len());
                chunks.push(chunk);
            }
            Some(Err(error)) => {
                failed = Some(error);
                break;
            }
            None => {
                ended = true;
                break;
            }
        }
    }

    let mut raw = Vec::with_capacity(len.min(limit));
    for chunk in &chunks {
        let room = limit.saturating_sub(raw.len());
        raw.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
    }
    let raw = Bytes::from(raw);
    let truncated = !ended;
    if ended && len <= limit {
        return (raw.clone(), false, Body::from(raw));
    }
    let read = stream::iter(chunks.into_iter().map(Ok::<_, axum::Error>));
    let body = match failed {
        Some(error) => Body::from_stream(read.chain(stream::once(async move { Err(error) }))),
        None => Body::from_stream(read.chain(data)),
    };
    (raw, truncated, body)
}

/// Sets the trace header from the request's latest selection, unless the
/// answer has one (upstream's `applyTraceHeader`).
fn set_trace_header(context: &RequestContext, headers: &mut HeaderMap) {
    if headers.contains_key(CPA_TRACE_ID_HEADER) {
        return;
    }
    if let Some(trace) = request_log::trace_id(context)
        && let Ok(value) = HeaderValue::from_str(&trace)
    {
        headers.insert(CPA_TRACE_ID_HEADER, value);
    }
}

/// Wraps the answer's body so that what is sent is kept, and the request
/// is finished when it ends; or finishes the request now when nothing of
/// the answer is to be kept.
fn respond(
    context: Arc<RequestContext>,
    mode: Mode,
    downstream: Downstream,
    response: Response,
) -> Response {
    let status = response.status().as_u16();
    let answer = Answer::new(status, response.headers().clone());
    if status == 101 {
        request_log::finish_later(&context, downstream, answer);
        return response;
    }
    if !mode.keeps_answer(status) {
        request_log::finish(&context, downstream, answer);
        return response;
    }
    let (parts, body) = response.into_parts();
    let body = Body::new(LoggedBody {
        inner: body,
        finish: Some(Finish {
            context,
            downstream,
            answer,
        }),
    });
    Response::from_parts(parts, body)
}

/// What finishes a request's log.
struct Finish {
    context: Arc<RequestContext>,
    downstream: Downstream,
    answer: Answer,
}

impl Finish {
    fn done(mut self, canceled: bool) {
        self.answer.canceled = canceled;
        request_log::finish(&self.context, self.downstream, self.answer);
    }
}

/// The request of a handler still running: it is finished with status 499
/// if the client leaves first (the handler's future is dropped).
struct Pending {
    context: Arc<RequestContext>,
    downstream: Option<Downstream>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(downstream) = self.downstream.take() {
            request_log::finish(
                &self.context,
                downstream,
                Answer {
                    canceled: true,
                    ..Answer::new(STATUS_CLIENT_CLOSED_REQUEST, HeaderMap::new())
                },
            );
        }
    }
}

/// The answer's body, kept as it is sent (upstream's
/// `ResponseWriterWrapper.Write`). The request is finished when the body
/// ends, or, if the client leaves first, when it is dropped.
struct LoggedBody {
    inner: Body,
    finish: Option<Finish>,
}

impl LoggedBody {
    /// Drops the inner body, so that the taps it holds flush, then
    /// finishes the request.
    fn end(&mut self, canceled: bool) {
        if let Some(finish) = self.finish.take() {
            drop(std::mem::take(&mut self.inner));
            finish.done(canceled);
        }
    }
}

impl http_body::Body for LoggedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(data), Some(finish)) = (frame.data_ref(), this.finish.as_mut()) {
                    finish.answer.body.push(data);
                }
            }
            Poll::Ready(Some(Err(_)) | None) => this.end(false),
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        let canceled = !http_body::Body::is_end_stream(&self.inner);
        self.end(canceled);
    }
}

/// The client's body, kept as the handler reads it (upstream's
/// `deferredRequestBodyCapture`).
pub(crate) struct TeeBody {
    pub(crate) inner: Body,
    pub(crate) capture: DeferredCapture,
}

impl http_body::Body for TeeBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.capture.record(data);
                }
            }
            Poll::Ready(None) => this.capture.end(),
            Poll::Ready(Some(Err(_))) | Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
