//! open-ferry's dashboard: the web app built into the binary and served at
//! `/dashboard/`, the dashboard API under `/open-ferry/api/v1/`, and the
//! usage ledger behind the API's usage routes. `docs/dashboard-api.md` is
//! the contract between the server and the app.
//!
//! [`router`] serves all of it, for merging beside the proxy's routes and
//! the management API's, inside the server's CORS, logging and panic
//! handling. The app is embedded at build time from `dashboard/dist` when
//! it was built, else a page says it wasn't (see `build.rs`).
//!
//! The dashboard API checks access with the management API's own code
//! ([`check_key`](open_ferry_management::check_key)): the key in the same
//! header forms, the local management password from 127.0.0.1 and ::1,
//! the local and remote rule, and one record of failed attempts for both
//! APIs. The app's own paths check only the address
//! ([`check_address`](open_ferry_management::check_address)), so the app
//! loads before the user has typed a key, or set one.
//!
//! The [`Ledger`] is a SQLite file in the log directory, written on a
//! thread of its own from the usage records the server makes (see
//! [`Usage::observe`](open_ferry_core::observe::usage::Usage::observe)).
//!
//! Every answer from these routes carries the dashboard's security headers:
//! a Content Security Policy that allows nothing from elsewhere, nothing
//! inline and no framing, `nosniff`, no referrer and `X-Frame-Options:
//! DENY`.
//!
//! Upstream has none of this but `/management.html`, its control panel's
//! path, which the `serve` module ports.

mod api;
mod assets;
mod hmac;
mod ledger;
mod serve;
#[cfg(test)]
mod tests;

use axum::Router;
use axum::middleware;
use axum::response::Response;
use http::{HeaderName, HeaderValue};
use open_ferry_management::ManagementState;

pub use ledger::{LEDGER_FILE, Ledger};

use crate::assets::Assets;

/// The dashboard's Content Security Policy: everything from the server's
/// own origin, images also as `data:` URLs, and nothing inline, from
/// elsewhere, or framing it.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
     object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// The headers every dashboard answer carries.
const SECURITY_HEADERS: [(&str, &str); 4] = [
    ("content-security-policy", CONTENT_SECURITY_POLICY),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
];

/// The dashboard's routes: the app at `/dashboard/`, `/management.html`,
/// and the dashboard API, for `management`'s config and access checks and
/// `ledger`'s usage. Merge them into the server's router; they set no
/// fallback.
pub fn router(management: ManagementState, ledger: Ledger) -> Router {
    router_from(DashboardState {
        management,
        ledger,
        assets: Assets::embedded(),
    })
}

/// What the dashboard's handlers share.
#[derive(Clone)]
pub(crate) struct DashboardState {
    /// The management API's state: the config, the access checks, the
    /// model registry and the log directory.
    pub(crate) management: ManagementState,
    /// The usage ledger.
    pub(crate) ledger: Ledger,
    /// The app's files.
    pub(crate) assets: Assets,
}

/// The router for `state`.
pub(crate) fn router_from(state: DashboardState) -> Router {
    serve::routes()
        .merge(api::routes(&state))
        .with_state(state)
        .layer(middleware::map_response(security_headers))
}

/// Adds the dashboard's security headers to `response`.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}
