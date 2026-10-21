// Ported from CLIProxyAPI internal/api/middleware/request_logging.go
// (RequestLoggingMiddleware, attachDeferredRequestBodyCapture,
// shouldSkipMethodForRequestLogging, isResponsesWebsocketUpgrade,
// shouldCaptureRequestBody, captureRequestInfo, shouldLogRequest),
// internal/api/middleware/response_writer.go (ResponseWriterWrapper's
// Write, WriteHeader and Finalize) and internal/logging/cpa_trace.go
// (CPATraceIDMiddleware) (v8.0.32, MIT).
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
//!   nor the dashboard's API (`/open-ferry/`), nor an OAuth callback (a
//!   path ending `/callback`).
//! - With `request-log` on, a bounded prefix of the client's body is kept
//!   as authenticated admission reads it, and every answer is kept.
//! - With it off, the same capture is made (not a multipart form), and
//!   only an error answer is kept.
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
//! - OAuth callbacks aren't logged, nor is the dashboard's API, which is
//!   open-ferry's own and, like the management API, holds the management
//!   key.
//! - Capture never reads ahead of authentication. In either mode, the raw
//!   prefix and its decoded rendering are each limited to 64 KiB, with a
//!   size note when truncated. Smaller bodies retain upstream's output.
//! - A client that leaves before the answer's head is logged with status
//!   499; one that leaves during the body, with what was sent.
//! - The trace ID is the latest selection's, taken when the answer's head
//!   is ready.
//! - Authenticated admission buffers bodies before the handler. Logging
//!   copies only its bounded prefix; multipart error-only capture is disabled.
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
use http::{HeaderMap, HeaderValue, Method, header};
use http_body::{Frame, SizeHint};
use open_ferry_core::exec::ErrorKind;
use open_ferry_core::observe::RequestContext;
use open_ferry_core::observe::request_log::{
    self, Answer, CPA_TRACE_ID_HEADER, DeferredCapture, Downstream, Mode, RequestBody,
    STATUS_CLIENT_CLOSED_REQUEST,
};

use crate::errors::ErrorMessage;
use crate::request_context;
use crate::state::AppState;

#[cfg(test)]
mod tests;

/// The path prefixes of the management API, which is never logged
/// (upstream's `shouldLogRequest`), and of the dashboard's API.
const MANAGEMENT_PREFIXES: [&str; 4] = [
    "/v0/management",
    "/v8/management",
    "/management",
    "/open-ferry/",
];

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
        let mut response = crate::ingress::budget_response(next.run(request).await);
        set_trace_header(&context, response.headers_mut());
        return response;
    };

    let (parts, body) = request.into_parts();
    let content_length = http_body::Body::size_hint(&body).exact();
    let plan = capture_plan(mode, &parts.headers, content_length);
    // Authentication runs inside this layer. Capture lazily so a refused
    // request is never polled by logging, including in Full mode.
    let (body, kept) = match plan {
        Capture::Deferred => {
            let capture = DeferredCapture::with_limit(
                content_length,
                open_ferry_core::observe::request_log::REQUEST_CAPTURE_LIMIT,
            );
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
        secrets: Downstream::url_secrets(parts.uri.path(), parts.uri.query()),
        method: parts.method.as_str().to_owned(),
        headers: parts.headers.clone(),
        body: kept,
    };
    let mut pending = Pending {
        context: Arc::clone(&context),
        downstream: Some(downstream),
    };

    let mut response =
        crate::ingress::budget_response(next.run(Request::from_parts(parts, body)).await);
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
    /// Kept as the handler reads it.
    Deferred,
    /// Not kept.
    None,
}

/// How the body of a request logged in `mode`, with `headers` and
/// `content_length`, is kept (upstream's `shouldCaptureRequestBody` and
/// `attachDeferredRequestBodyCapture`): a bounded lazy prefix in either mode;
/// error-only capture skips empty and multipart bodies.
pub(crate) fn capture_plan(
    mode: Mode,
    headers: &HeaderMap,
    content_length: Option<u64>,
) -> Capture {
    match mode {
        Mode::Full => Capture::Deferred,
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

/// Records `error`, as a handler gave it to the client, for the log's
/// `=== API ERROR RESPONSE ===` sections (upstream's
/// `LoggingAPIResponseError`), which are kept only with `request-log` on.
/// Upstream's Responses handlers, over HTTP and WebSocket, are the ones
/// that record their errors.
pub(crate) fn record_api_error(context: Option<&RequestContext>, error: &ErrorMessage) {
    let Some(context) = context else {
        return;
    };
    let canceled = error
        .source
        .as_ref()
        .is_some_and(|source| source.kind == ErrorKind::Canceled);
    context
        .request_log()
        .record_api_error(error.status, &error.text, canceled);
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
