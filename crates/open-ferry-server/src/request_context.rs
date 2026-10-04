// Ported from CLIProxyAPI sdk/api/handlers/handlers.go (the endpoint and
// client metadata of GetContextWithCancel) and the request ID of
// internal/logging/gin_logger.go (GinLogrusLogger) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The request context: the [`RequestContext`] the outermost layer makes
//! for each request as it arrives, with its ID, its endpoint, and what the
//! connection and the headers say of the client. It goes in the request's
//! extensions, where the layers inside and the handlers find it ([`of`]):
//! the client-key check records the key in it, and each call a handler
//! makes carries it in its observation (see [`open_ferry_core::observe`]).
//!
//! Deviations from upstream:
//! - The context is made once, as the request arrives, for every request.
//!   Upstream's logger makes an ID for its AI routes only, and each
//!   provider handler gathers the endpoint and the client's metadata
//!   (`GetContextWithCancel`).
//! - A route is written as axum writes it (`/v1beta/models/{*action}`),
//!   where gin writes `/v1beta/models/*action`; a path that matched no
//!   route is percent-encoded as the client sent it, where Go's is
//!   decoded.
//! - An IPv6 zone is the scope's number, where Go writes the interface's
//!   name.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{Extensions, HeaderMap, HeaderName, header};
use open_ferry_core::observe::RequestContext;
use open_ferry_core::observe::client_ip::{TrustedProxies, client_ip, remote_ip};

use crate::state::AppState;

/// Makes the request's context, and puts it in its extensions.
pub(crate) async fn layer(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let context = context(&request, state.trusted_proxies());
    request.extensions_mut().insert(Arc::new(context));
    next.run(request).await
}

/// The context of the request with `extensions`, once [`layer`] has made
/// it.
pub(crate) fn of(extensions: &Extensions) -> Option<&Arc<RequestContext>> {
    extensions.get::<Arc<RequestContext>>()
}

/// The context of `request`, arriving now, reading forwarded addresses
/// from `trusted` proxies only.
fn context(request: &Request, trusted: &TrustedProxies) -> RequestContext {
    let mut context =
        RequestContext::new(request.method().clone(), request.uri().path().to_owned());
    if let Some(route) = request.extensions().get::<MatchedPath>() {
        context.endpoint = format!("{} {}", context.method, route.as_str().trim());
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let headers = request.headers();
    context.client_ip = remote_ip(peer);
    context.resolved_client_ip = client_ip(peer, headers, trusted);
    context.forwarded_for = joined(headers, &HeaderName::from_static("x-forwarded-for"));
    context.user_agent = headers
        .get(header::USER_AGENT)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    context
}

/// Every value of the header `name`, joined with `, ` and trimmed, as Go
/// joins `Header.Values`.
fn joined(headers: &HeaderMap, name: &HeaderName) -> String {
    let values: Vec<_> = headers
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()))
        .collect();
    values.join(", ").trim().to_owned()
}
