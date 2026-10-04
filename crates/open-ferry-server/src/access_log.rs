// Ported from CLIProxyAPI internal/logging/gin_logger.go (GinLogrusLogger,
// isAIAPIPath, aiAPIPrefixes) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The access log: a line for each request once it is answered (upstream's
//! `GinLogrusLogger`):
//!
//! ```text
//! 200 |       23.559s |       127.0.0.1 | POST    "/v1/chat/completions?key=AIza...wxyz"
//! ```
//!
//! The status, the time taken, the client's address, the method, and the
//! path with its query, key-like values masked. The line is logged at info
//! level, at warn from 400 and at error from 500. A health probe answered
//! with a 2xx isn't logged.
//!
//! A request on the AI routes (`/v1`, `/v1beta`, `/openai/v1` and
//! `/backend-api/codex`) is handled in a span with its ID as
//! `request_id`, so its line and every line logged while it is handled show
//! the ID, including those logged while the answer's body is sent; the
//! others show `--------`. The layer runs inside the request context, so
//! every request has its [`RequestContext`], and outside the request log.
//!
//! Deviations from upstream:
//! - The line is logged once the response's body is sent or dropped, so
//!   the time taken covers a stream, as gin's does. A WebSocket's line
//!   comes once its handshake is answered, where gin's comes when the
//!   connection closes.
//! - The path is written as the client sent it, percent-encoded, where Go
//!   writes it decoded.
//! - Gin's handler errors and the antigravity `[credits]` marker aren't
//!   ported, nor is `SkipGinRequestLogging`, which upstream never calls.
//! - The ID is the request context's, which every request has; only the
//!   AI routes' lines show it, as upstream makes one for those only.
//!
//! [`RequestContext`]: open_ferry_core::observe::RequestContext

use std::fmt::Write as _;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body::{Frame, SizeHint};
use open_ferry_core::observe::mask::mask_sensitive_query;
use tracing::Instrument;

use crate::request_context;

#[cfg(test)]
mod tests;

/// The path prefixes of the AI routes, whose requests are logged with
/// their ID (upstream's `aiAPIPrefixes`).
const AI_API_PREFIXES: [&str; 4] = ["/v1", "/v1beta", "/openai/v1", "/backend-api/codex"];

/// The request ID shown for a request off the AI routes.
const NO_REQUEST_ID: &str = "--------";

/// Logs a request once it is answered.
pub(crate) async fn layer(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let Some(context) = request_context::of(request.extensions()).cloned() else {
        return next.run(request).await;
    };
    let query = request
        .uri()
        .query()
        .map(mask_sensitive_query)
        .unwrap_or_default();
    let request_id = is_ai_api_path(&context.path).then(|| context.id.to_string());
    let span = match &request_id {
        Some(id) => tracing::info_span!("request", request_id = %id),
        None => tracing::Span::none(),
    };
    let response = next.run(request).instrument(span.clone()).await;

    let method = context.method.clone();
    let status = response.status();
    if is_quiet_health_probe(&context.path, &method, status) {
        return response;
    }
    let mut path = context.path.clone();
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query);
    }
    let line = AccessLine {
        start,
        status,
        client_ip: context.resolved_client_ip.clone(),
        method,
        path,
        request_id,
    };
    let (parts, body) = response.into_parts();
    let body = Logged {
        inner: body,
        line: Some(line),
        span,
    };
    Response::from_parts(parts, Body::new(body))
}

/// Whether `path` is on the AI routes: one of [`AI_API_PREFIXES`] or under
/// one (upstream's `isAIAPIPath`).
fn is_ai_api_path(path: &str) -> bool {
    AI_API_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// Whether a request isn't logged: a `GET` or `HEAD` of `/healthz`
/// answered with a 2xx. A failed probe is.
fn is_quiet_health_probe(path: &str, method: &Method, status: StatusCode) -> bool {
    path == "/healthz" && (method == Method::GET || method == Method::HEAD) && status.is_success()
}

/// What a request's line says.
struct AccessLine {
    start: Instant,
    status: StatusCode,
    client_ip: String,
    method: Method,
    path: String,
    request_id: Option<String>,
}

impl AccessLine {
    /// Logs the line, at a level following the status.
    fn log(&self) {
        let latency = go_latency(self.start.elapsed());
        let text = format!(
            "{:3} | {latency:>13} | {:>15} | {:<7} \"{}\"",
            self.status.as_u16(),
            self.client_ip,
            self.method.as_str(),
            self.path,
        );
        let request_id = self.request_id.as_deref().unwrap_or(NO_REQUEST_ID);
        match self.status.as_u16() {
            500.. => tracing::error!(request_id, "{text}"),
            400.. => tracing::warn!(request_id, "{text}"),
            _ => tracing::info!(request_id, "{text}"),
        }
    }
}

/// `elapsed` as gin's line shows it: truncated to the second past a
/// minute and to the millisecond under, written as Go's
/// `Duration.String` writes it (`0s`, `23ms`, `23.559s`, `1m5s`,
/// `1h0m0s`).
fn go_latency(elapsed: Duration) -> String {
    let millis = if elapsed > Duration::from_secs(60) {
        u128::from(elapsed.as_secs()) * 1000
    } else {
        elapsed.as_millis()
    };
    if millis == 0 {
        return "0s".to_owned();
    }
    if millis < 1000 {
        return format!("{millis}ms");
    }
    let (seconds, fraction) = (millis / 1000, millis % 1000);
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut out = String::new();
    if hours > 0 {
        let _ = write!(out, "{hours}h");
    }
    if hours > 0 || minutes > 0 {
        let _ = write!(out, "{minutes}m");
    }
    let _ = write!(out, "{seconds}");
    if fraction > 0 {
        let digits = format!("{fraction:03}");
        let _ = write!(out, ".{}", digits.trim_end_matches('0'));
    }
    out.push('s');
    out
}

/// A response body that logs its request's line once it ends or is
/// dropped. The body is polled outside the handler, so each poll, and
/// dropping the body, enter the request's span again.
struct Logged {
    inner: Body,
    line: Option<AccessLine>,
    span: tracing::Span,
}

/// Logs `line`, if it hasn't been yet.
fn log_once(line: &mut Option<AccessLine>) {
    if let Some(line) = line.take() {
        line.log();
    }
}

impl http_body::Body for Logged {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        let _entered = this.span.enter();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(polled, Poll::Ready(None)) {
            log_once(&mut this.line);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        let _entered = self.span.enter();
        log_once(&mut self.line);
        // The body's own drop may log too.
        drop(std::mem::take(&mut self.inner));
    }
}
