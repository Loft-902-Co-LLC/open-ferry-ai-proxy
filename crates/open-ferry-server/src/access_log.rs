//! The access log: a line for each request once it is answered (upstream's
//! internal/logging/gin_logger.go `GinLogrusLogger`). Not ported yet (P3
//! WP-B).
//!
//! What is here is the layer the router installs, with the signature the
//! port keeps: it runs inside the request context, so every request has
//! its [`RequestContext`], and outside the request log. For now it logs
//! what open-ferry logged before: a span with the request's ID, method and
//! path, and the status and duration once the request is handled. The
//! query string is left out, as it may hold a key.
//!
//! Deviations from upstream: the line isn't upstream's yet.
//!
//! [`RequestContext`]: open_ferry_core::observe::RequestContext

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

use crate::request_context;

#[cfg(test)]
mod tests;

/// Logs a request once it is answered.
pub(crate) async fn layer(request: Request, next: Next) -> Response {
    let Some(context) = request_context::of(request.extensions()).cloned() else {
        return next.run(request).await;
    };
    let span = tracing::info_span!(
        "request",
        id = %context.id,
        method = %context.method,
        path = %context.path,
    );
    let response = next.run(request).instrument(span.clone()).await;
    span.in_scope(|| {
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = context.started.elapsed().as_millis() as u64,
            "handled"
        );
    });
    response
}
