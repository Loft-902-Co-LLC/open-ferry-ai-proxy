// Ported from CLIProxyAPI internal/api/server_management.go
// (registerManagementRoutes, managementAvailabilityMiddleware,
// pluginManagementNoRoute) and internal/api/server_management_v8.go
// (registerManagementV8Routes) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! CLIProxyAPI-compatible management API, under `/v0/management` and its
//! v8 names under `/v8/management`.
//!
//! [`router`] serves the routes every module gives with its `routes()`, for
//! a [`ManagementState`]. Each module documents the routes it serves, with
//! their v0 and v8 paths.
//!
//! Most routes answer only while a management key is set, and only to a
//! client that offers it, as the `access` module describes. A few, such as
//! a login's OAuth callback, need only the key to be set, or nothing at
//! all: each route names its `Access`. The server mounts the router
//! beside the proxy's routes, inside its CORS, logging and panic handling;
//! the proxy's client keys don't apply here.
//!
//! Every other path under `/v0/management` or `/v8/management`, and every
//! other method on a management path, `HEAD` included, answers an empty
//! 404, as upstream answers a request it has no route for. Another method
//! on a route outside those paths answers the server's `404 page not
//! found`, as its own routes do.
//!
//! Errors are `{"error":"..."}` with the status upstream uses, and bodies
//! are written as gin writes them, byte for byte.
//!
//! Deviations from upstream, besides those noted on each module:
//! - Upstream's management routes that aren't listed by a module, the
//!   management control panel, the plugin host and its routes, the local
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
mod config_read;
mod credential_files;
mod credential_state;
mod credential_sync;
mod go;
mod go_url;
mod json;
mod latest_version;
mod log_dir;
mod logs;
mod model_definitions;
mod oauth;
mod observability_settings;
mod proxy;
mod query;
mod quota;
mod quota_fetch;
mod quota_types;
mod request_logs;
mod state;
#[cfg(test)]
mod tests;
mod token_record;
mod usage;
mod vertex_import;

use open_ferry_core::observe::client_ip;

pub use client_ip::TrustedProxies;
pub use credential_sync::{CredentialSync, SyncError, SyncFuture};
pub use state::{ManagementState, management_password_from_env};

use std::borrow::Cow;
use std::collections::BTreeMap;

use axum::Router;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any};
use http::{HeaderValue, StatusCode, header};

/// The path prefixes of the management API.
const PREFIXES: [&str; 2] = ["/v0/management", "/v8/management"];

/// The management routes, for merging into the server's router.
pub fn router(state: ManagementState) -> Router {
    router_from(state, routes())
}

/// Every module's routes.
fn routes() -> Vec<Route> {
    [
        auth_files::routes(),
        api_call::routes(),
        quota::routes(),
        credential_files::routes(),
        vertex_import::routes(),
        credential_state::routes(),
        oauth::routes(),
        config_read::routes(),
        model_definitions::routes(),
        latest_version::routes(),
        observability_settings::routes(),
        logs::routes(),
        request_logs::routes(),
        usage::routes(),
        quota_fetch::routes(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// One route: the methods a module serves on a path, and who may call
/// them.
pub(crate) struct Route {
    /// An axum path, such as `/v0/management/auth-files` or
    /// `/v8/management/config/{*path}`.
    pub(crate) path: Cow<'static, str>,
    /// The methods served. Never `HEAD`, nor a fallback: the router answers
    /// every other method itself, and `GET` routes don't answer `HEAD`.
    pub(crate) handler: MethodRouter<ManagementState>,
    /// Who may call them.
    pub(crate) access: Access,
}

/// Who may call a [`Route`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Access {
    /// Only a client that offers the management key, while one is set
    /// (upstream's management group: `managementAvailabilityMiddleware`,
    /// then the handler's `Middleware`).
    Key,
    /// Anyone, while a management key is set (upstream's
    /// `managementAvailabilityMiddleware` alone, as on `oauth-callback`).
    Availability,
    /// Anyone, always, as the OAuth callback pages of the main server.
    Open,
}

impl Route {
    /// A route for clients with the management key.
    pub(crate) fn key(
        path: impl Into<Cow<'static, str>>,
        handler: MethodRouter<ManagementState>,
    ) -> Self {
        Self::new(path, handler, Access::Key)
    }

    /// A route for anyone while a management key is set.
    #[cfg_attr(not(test), allow(dead_code, reason = "for the OAuth callback routes"))]
    pub(crate) fn availability(
        path: impl Into<Cow<'static, str>>,
        handler: MethodRouter<ManagementState>,
    ) -> Self {
        Self::new(path, handler, Access::Availability)
    }

    /// A route for anyone, always.
    #[cfg_attr(not(test), allow(dead_code, reason = "for the OAuth callback pages"))]
    pub(crate) fn open(
        path: impl Into<Cow<'static, str>>,
        handler: MethodRouter<ManagementState>,
    ) -> Self {
        Self::new(path, handler, Access::Open)
    }

    fn new(
        path: impl Into<Cow<'static, str>>,
        handler: MethodRouter<ManagementState>,
        access: Access,
    ) -> Self {
        Self {
            path: path.into(),
            handler,
            access,
        }
    }
}

/// The router serving `routes`, and the empty 404 for every other path
/// under the management prefixes.
///
/// The routes on one path are merged, each with the checks of its access,
/// and the path then answers every other method, `HEAD` included, with
/// the empty 404 under the prefixes and with the server's 404 elsewhere.
/// Panics, as the router is made, if two routes on a path serve the same
/// method, or one serves `HEAD`.
pub(crate) fn router_from(state: ManagementState, routes: Vec<Route>) -> Router {
    let mut paths: BTreeMap<Cow<'static, str>, BTreeMap<Access, MethodRouter<ManagementState>>> =
        BTreeMap::new();
    for route in routes {
        let methods = paths.entry(route.path).or_default();
        let handler = match methods.remove(&route.access) {
            Some(existing) => existing.merge(route.handler),
            None => route.handler,
        };
        methods.insert(route.access, handler);
    }

    let mut router = Router::new();
    for (path, methods) in paths {
        let mut merged = MethodRouter::new();
        for (access, handler) in methods {
            merged = merged.merge(guard(&state, access, handler));
        }
        merged = if is_management_path(&path) {
            merged.head(unported).fallback(unported)
        } else {
            merged.head(not_found).fallback(not_found)
        };
        router = router.route(&path, merged);
    }
    for prefix in PREFIXES {
        router = router
            .route(prefix, any(unported))
            .route(&format!("{prefix}/"), any(unported))
            .route(&format!("{prefix}/{{*rest}}"), any(unported));
    }
    router.with_state(state)
}

/// `handler` with the checks of `access`: for [`Access::Key`] the
/// availability check, then the key; for [`Access::Availability`] the
/// availability check alone.
fn guard(
    state: &ManagementState,
    access: Access,
    handler: MethodRouter<ManagementState>,
) -> MethodRouter<ManagementState> {
    let handler = match access {
        Access::Key => handler.route_layer(middleware::from_fn_with_state(
            state.clone(),
            access::authenticate,
        )),
        Access::Availability | Access::Open => handler,
    };
    match access {
        Access::Key | Access::Availability => handler.route_layer(middleware::from_fn_with_state(
            state.clone(),
            access::availability,
        )),
        Access::Open => handler,
    }
}

/// Whether `path` is under a management prefix.
fn is_management_path(path: &str) -> bool {
    PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// What upstream answers a management path it has no route for: an empty
/// 404 (`pluginManagementNoRoute` without a plugin host).
async fn unported() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// The server's answer to a path or method it has no route for: gin's 404.
async fn not_found() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "404 page not found").into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}
