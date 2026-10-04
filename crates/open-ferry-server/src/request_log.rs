//! The request log's capture layer: what the client sent and what it was
//! answered, for the request log and the error logs, and the
//! `X-CPA-TRACE-ID` header naming the credential that served the request
//! (upstream's internal/api/middleware/request_logging.go and
//! response_writer.go, and internal/logging/cpa_trace.go). Not ported yet
//! (P3 WP-A).
//!
//! What is here is the layer the router installs, with the signature the
//! port keeps: it runs inside the access log and outside CORS, so it sees
//! every request, the management API's included, with its
//! [`RequestContext`], and the answer CORS gave. Upstream installs its
//! middleware only without `commercial-mode`; this layer is always
//! installed, and is to read the setting. The request logger is the
//! state's observability handle's, which the binary reconfigures on every
//! config load. For now the layer passes requests through.
//!
//! Deviations from upstream: nothing is captured yet.
//!
//! [`RequestContext`]: open_ferry_core::observe::RequestContext

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

#[cfg(test)]
mod tests;

/// Captures a request and its answer for the request log. For now it
/// passes the request through.
pub(crate) async fn layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let _ = state;
    next.run(request).await
}
