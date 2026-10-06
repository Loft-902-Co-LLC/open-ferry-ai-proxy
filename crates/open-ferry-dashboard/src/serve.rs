// Ported from CLIProxyAPI internal/api/server_management.go
// (serveManagementControlPanel) and internal/api/server_routes.go (its
// route) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Serves the app at `/dashboard/`, and answers `/management.html`,
//! upstream's control panel, by sending the browser there.
//!
//! | Path | |
//! |---|---|
//! | `GET /dashboard/` and below | the app's files; a path that isn't one answers `index.html`, except below `/dashboard/assets/`, where it is a 404 |
//! | `GET /dashboard` | 302 to `/dashboard/` |
//! | `GET /management.html` | 302 to `/dashboard/`, with the same query |
//!
//! Each first answers an empty 404 while
//! `remote-management.disable-control-panel` is set, as upstream's
//! `/management.html` does, then refuses an address as the management API
//! does (see [`check_address`]). No key is asked for: the app asks for it,
//! and its API calls carry it.
//!
//! The files under `assets/` are named by their content, so they may be
//! cached for good; every other file, `index.html` first, is checked with
//! the server each time. A binary built without the app answers every path
//! below `/dashboard/` with a page saying so and how to build it.
//!
//! Deviations from upstream:
//! - `/management.html` redirects to the dashboard. Upstream serves the
//!   control panel's single page from its static directory, downloading
//!   it from GitHub when it's missing; nothing is downloaded here.
//! - Home isn't ported, so it hides nothing here.
//! - A client the management API would refuse for its address gets the
//!   same refusal; upstream serves the panel to anyone.
//! - `/dashboard` and the paths below it are open-ferry's own, and answer
//!   `HEAD` as well. `HEAD /management.html` is a 404, as upstream's is
//!   outside safe mode; in safe mode upstream answers it, and
//!   `GET /management.html` without `?safe-mode=configure`, with its
//!   warning page, which isn't ported.

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use open_ferry_management::check_address;

use crate::DashboardState;
use crate::assets::{Assets, content_type};

/// The client's address, when the server knows it.
pub(crate) struct Peer(pub(crate) Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

/// Where the app is served.
pub(crate) const APP_PATH: &str = "/dashboard/";

/// The cache rule of the files under `assets/`, named by their content.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The cache rule of every other answer: check with the server first.
const NO_CACHE: &str = "no-cache";

/// The app's routes. They set no fallback; another method answers the
/// server's 404, as gin does.
pub(crate) fn routes() -> Router<DashboardState> {
    Router::new()
        .route(
            "/management.html",
            get(management_html).head(not_found).fallback(not_found),
        )
        .route("/dashboard", get(to_app).fallback(not_found))
        .route("/dashboard/", get(app).fallback(not_found))
        .route("/dashboard/{*path}", get(app).fallback(not_found))
}

/// `GET /management.html`: upstream's control panel, which is the
/// dashboard here.
async fn management_html(
    State(state): State<DashboardState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(refusal) = refuse(&state, peer, &headers) {
        return refusal;
    }
    redirect(uri.query())
}

/// `GET /dashboard`: the app is below it.
async fn to_app(
    State(state): State<DashboardState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(refusal) = refuse(&state, peer, &headers) {
        return refusal;
    }
    redirect(uri.query())
}

/// `GET /dashboard/` and the paths below it: the app's files.
async fn app(
    State(state): State<DashboardState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(refusal) = refuse(&state, peer, &headers) {
        return refusal;
    }
    let path = uri.path().strip_prefix(APP_PATH).unwrap_or_default();
    serve_file(state.assets, path)
}

/// The answer that stops a request before the app: an empty 404 while the
/// control panel is disabled, then the management API's refusal of the
/// client's address.
fn refuse(
    state: &DashboardState,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
) -> Option<Response> {
    if state
        .management
        .config()
        .remote_management
        .disable_control_panel
    {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    check_address(&state.management, peer, headers)
        .err()
        .map(|refusal| refusal.management_response())
}

/// A 302 to the app, with `query` if there is one.
fn redirect(query: Option<&str>) -> Response {
    let with_query = query
        .filter(|query| !query.is_empty())
        .and_then(|query| HeaderValue::try_from(format!("{APP_PATH}?{query}")).ok());
    let location = with_query.unwrap_or(HeaderValue::from_static(APP_PATH));
    let mut response = StatusCode::FOUND.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
    response
}

/// The app's file at `path` below `/dashboard/`: the file, else a 404
/// below `assets/`, else `index.html`; or, without an app, the
/// placeholder.
fn serve_file(assets: Assets, path: &str) -> Response {
    if !assets.built() {
        return file(PLACEHOLDER.as_bytes(), "index.html", NO_CACHE);
    }
    let path = if path.is_empty() { "index.html" } else { path };
    let hashed = path.starts_with("assets/");
    if let Some(bytes) = assets.find(path) {
        return file(bytes, path, if hashed { IMMUTABLE } else { NO_CACHE });
    }
    if hashed {
        return not_found_now();
    }
    match assets.find("index.html") {
        Some(bytes) => file(bytes, "index.html", NO_CACHE),
        None => not_found_now(),
    }
}

/// A 200 with `bytes`, typed as `name` is, and cached by `cache`.
fn file(bytes: &'static [u8], name: &str, cache: &'static str) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(name)),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        bytes,
    )
        .into_response()
}

/// The server's answer to a path or method it has no route for: gin's 404.
async fn not_found() -> Response {
    not_found_now()
}

/// Gin's 404, to answer now.
fn not_found_now() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "404 page not found").into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

/// The page served in place of the app when it wasn't built into the
/// binary. It has no inline style or script, which the dashboard's
/// Content Security Policy refuses.
const PLACEHOLDER: &str = "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>open-ferry dashboard</title>
</head>
<body>
<h1>The dashboard isn't built into this binary</h1>
<p>This open-ferry was built without the dashboard app, so there is nothing to show here.</p>
<p>To build it in, build the app first, with Node.js 22.12 or newer, then build open-ferry again:</p>
<pre>cd dashboard
npm ci
npm run build
cd ..
cargo build --release</pre>
<p>The release binaries have the dashboard built in. The management API and the dashboard API work either way.</p>
</body>
</html>
";
