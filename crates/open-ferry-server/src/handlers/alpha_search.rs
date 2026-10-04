// Ported from CLIProxyAPI internal/api/server_routes.go (codexAlphaSearch,
// and its /v1/alpha/search and /backend-api/codex/alpha/search routes)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex Alpha Search: `POST /v1/alpha/search` and
//! `POST /backend-api/codex/alpha/search`, behind the client keys.
//!
//! The payload, up to 16 MiB, goes to the dispatcher's
//! [`Dispatcher::codex_alpha_search`], which picks a Codex credential that
//! may search and sends the payload on untranslated (see the core
//! manager's `alpha_search` module). The answer comes back with Codex's
//! status, `Content-Type` and body, up to 32 MiB. A failure answers
//! `{"error":"<message>"}`: 503 when no credential could be picked, or when
//! the picked API key has no base URL; 502 when Codex couldn't be reached,
//! its answer couldn't be read, or the URL holds an ASCII control
//! character; and a `Retry-After` when every credential is cooling down.
//!
//! Deviations from upstream:
//! - A payload over 16 MiB (or the configured body limit, when lower) gets
//!   the proxy's usual 413, and one that fails to arrive its usual 400.
//!   Upstream sends the first 16 MiB on, and answers a failed read with
//!   `{"error":"Failed to read search request"}`.
//! - An answer without a `Content-Type` goes out without one, where Go's
//!   server sniffs one from the body.
//! - `X-CPA-TRACE-ID` isn't set and nothing is logged about the call:
//!   request logging isn't ported.
//!
//! [`Dispatcher::codex_alpha_search`]: open_ferry_core::exec::Dispatcher::codex_alpha_search

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, StatusCode, header};
use open_ferry_core::exec::{AlphaSearch, ExecError, HttpReply};
use open_ferry_core::observe::Observation;

use crate::body;
use crate::errors::{JSON_UTF8, error_response};
use crate::exec::ClientRequest;
use crate::json::json_string;
use crate::state::AppState;

/// The largest payload sent on (16 MiB).
const MAX_SEARCH_BODY: usize = 16 << 20;

/// `POST /v1/alpha/search` (`codexAlphaSearch`).
pub(crate) async fn search(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let limit = MAX_SEARCH_BODY.min(state.settings().config.body_limit);
    let raw = match body::read_raw(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    // Upstream records the search in the request log, and no usage.
    let observation = client.context.as_ref().map(|context| {
        let taps = state.observability().request_log.tap(context);
        Arc::new(Observation::new(
            Arc::clone(context),
            taps.into_iter().collect(),
        ))
    });
    let request = AlphaSearch {
        body: raw,
        headers: client.headers,
        observation,
    };
    match state.dispatcher_arc().codex_alpha_search(request).await {
        Ok(reply) => answer(reply),
        Err(error) => failure(&error),
    }
}

/// Codex's answer, as it came: status, `Content-Type` and body.
fn answer(reply: HttpReply) -> Response {
    if let Some(error) = &reply.read_error {
        tracing::warn!("codex alpha search: reading the response failed: {error}");
        return error_json(
            502,
            "Failed to read Codex search response",
            HeaderMap::new(),
        );
    }
    let mut response = Response::new(Body::from(reply.body));
    *response.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(content_type) = reply
        .headers
        .get(header::CONTENT_TYPE)
        .filter(|value| !value.is_empty())
    {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.clone());
    }
    response
}

/// A failed call: its status, else 503, and the `Retry-After` upstream
/// trusts it to carry.
fn failure(error: &ExecError) -> Response {
    let status = match error.http_status() {
        0 => 503,
        status => status,
    };
    let mut headers = HeaderMap::new();
    if let Some(retry_after) = error.retry_after_header() {
        headers.insert(header::RETRY_AFTER, retry_after);
    }
    error_json(status, &error.to_string(), headers)
}

/// `{"error":"<message>"}`, as gin's `c.JSON` writes it.
fn error_json(status: u16, message: &str, headers: HeaderMap) -> Response {
    let body = format!("{{\"error\":{}}}", json_string(message));
    error_response(status, headers, Bytes::from(body), JSON_UTF8)
}

#[cfg(test)]
mod tests;
