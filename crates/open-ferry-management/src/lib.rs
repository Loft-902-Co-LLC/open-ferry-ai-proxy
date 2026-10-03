// Ported from CLIProxyAPI internal/api/server_management.go
// (registerManagementRoutes, managementAvailabilityMiddleware,
// pluginManagementNoRoute) and internal/api/server_management_v8.go
// (registerManagementV8Routes) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! CLIProxyAPI-compatible management API, under `/v0/management` and its
//! v8 names under `/v8/management`.
//!
//! [`router`] serves these routes for a [`ManagementState`]:
//!
//! | Route | v8 route | Module |
//! | --- | --- | --- |
//! | `GET /v0/management/auth-files` | `GET /v8/management/credentials` | `auth_files` |
//! | `GET /v0/management/auth-files/models` | `GET /v8/management/credentials/models` | `auth_files` |
//! | `POST /v0/management/api-call` | `POST /v8/management/requests/api-call` | `api_call` |
//! | `POST /v0/management/reset-quota` | `POST /v8/management/routing/cooldown/reset` | `quota` |
//!
//! Each answers only while a management key is set, and only to a client
//! that offers it, as the `access` module describes. The server mounts the
//! router beside the proxy's routes, inside its CORS, logging and panic
//! handling; the proxy's client keys don't apply here.
//!
//! Every other path under `/v0/management` or `/v8/management`, and every
//! other method on these paths, `HEAD` included, answers an empty 404, as
//! upstream answers a request it has no route for. Upstream serves many
//! more management routes; none of them is ported yet.
//!
//! Errors are `{"error":"..."}` with the status upstream uses, and bodies
//! are written as gin writes them, byte for byte.
//!
//! Deviations from upstream, besides those noted on each module:
//! - Upstream's other management routes, the management control panel,
//!   the OAuth callback routes, the plugin host and its routes, the local
//!   management password and Home mode aren't ported.
//! - Upstream registers the management routes only once a key is set, and
//!   until then answers them as unknown paths; here they are always
//!   registered and answer the same empty 404 while no key is set.
//! - Paths match exactly, as elsewhere in the server. While a key is set,
//!   gin redirects a ported path with a trailing slash to the path without
//!   it (301 for `GET`, else 307), before any key check, and matches a
//!   percent-encoded path decoded; both get the empty 404 here.

mod access;
mod api_call;
mod auth_files;
mod bind;
mod client_ip;
mod go;
mod go_url;
mod json;
mod proxy;
mod query;
mod quota;
mod state;
#[cfg(test)]
mod tests;

pub use client_ip::TrustedProxies;
pub use state::{ManagementState, management_password_from_env};

use axum::Router;
use axum::middleware;
use axum::routing::{MethodRouter, any, get, post};
use http::StatusCode;

/// The management routes, for merging into the server's router.
pub fn router(state: ManagementState) -> Router {
    let guarded = |route: MethodRouter<ManagementState>| {
        route
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                access::authenticate,
            ))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                access::availability,
            ))
            .head(unported)
            .fallback(unported)
    };
    let list = || guarded(get(auth_files::list));
    let models = || guarded(get(auth_files::models));
    let api_call = || guarded(post(api_call::api_call));
    let reset = || guarded(post(quota::reset));
    let mut router = Router::new()
        .route("/v0/management/auth-files", list())
        .route("/v0/management/auth-files/models", models())
        .route("/v0/management/api-call", api_call())
        .route("/v0/management/reset-quota", reset())
        .route("/v8/management/credentials", list())
        .route("/v8/management/credentials/models", models())
        .route("/v8/management/requests/api-call", api_call())
        .route("/v8/management/routing/cooldown/reset", reset());
    for prefix in ["/v0/management", "/v8/management"] {
        router = router
            .route(prefix, any(unported))
            .route(&format!("{prefix}/"), any(unported))
            .route(&format!("{prefix}/{{*rest}}"), any(unported));
    }
    router.with_state(state)
}

/// What upstream answers a management path it has no route for: an empty
/// 404 (`pluginManagementNoRoute` without a plugin host).
async fn unported() -> StatusCode {
    StatusCode::NOT_FOUND
}
