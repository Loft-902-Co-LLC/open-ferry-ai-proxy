// Ported from the routes in CLIProxyAPI internal/api/server_routes.go and the
// middleware in internal/api/server_middleware.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The router, and the middleware every request passes through.

use std::any::Any;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::handler::Handler;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use serde_json::json;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::Instrument;

use crate::auth::require_key;
use crate::errors::{JSON_UTF8, error_response};
use crate::handlers::{claude, gemini, health, models, openai, responses, responses_ws};
use crate::state::AppState;

/// The response headers browsers may read (`corsExposedResponseHeaders`).
const CORS_EXPOSED_HEADERS: &str = "X-CPA-TRACE-ID, X-CPA-VERSION, X-CPA-COMMIT, \
    X-CPA-BUILD-DATE, X-CPA-SUPPORT-PLUGIN, X-CPA-HOME-VERSION, X-CPA-HOME-BUILD-DATE, \
    X-SERVER-VERSION, X-SERVER-BUILD-DATE, Location, Retry-After, X-Request-Id, \
    OpenAI-Request-Id";

/// The path prefixes safe mode shuts.
const SAFE_MODE_PREFIXES: [&str; 4] = ["/v1", "/v1beta", "/openai/v1", "/backend-api/codex"];

/// The proxy's routes. Every response gets CORS headers, an `OPTIONS`
/// request gets 204, and a path or method with no route gets 404.
pub fn router(state: AppState) -> Router {
    router_with(state, Router::new())
}

/// The proxy's routes, as [`router`] serves them, with `extra` routes
/// beside them, such as the management API's. The extra routes pass
/// through the same logging, CORS, panic handling and safe mode, but not the
/// client-key check, and must not set a fallback.
pub fn router_with(state: AppState, extra: Router) -> Router {
    let auth = middleware::from_fn_with_state(state.clone(), require_key);
    let responses_routes = || {
        get(responses_ws::websocket.layer(auth.clone()))
            .head(not_found)
            .post(responses::responses.layer(auth.clone()))
    };
    let compact_route = || post(responses::compact.layer(auth.clone()));
    let gemini_action_routes = || {
        get(gemini::model.layer(auth.clone()))
            .head(not_found)
            .post(gemini::action.layer(auth.clone()))
    };
    Router::new()
        .route("/", get(health::root).head(not_found))
        .route("/healthz", get(health::healthz).head(health::healthz_head))
        .route(
            "/v1/models",
            get(models::unified.layer(auth.clone())).head(not_found),
        )
        .route(
            "/v1/chat/completions",
            post(openai::chat_completions.layer(auth.clone())),
        )
        .route(
            "/v1/completions",
            post(openai::completions.layer(auth.clone())),
        )
        .route("/v1/messages", post(claude::messages.layer(auth.clone())))
        .route(
            "/v1/messages/count_tokens",
            post(claude::count_tokens.layer(auth.clone())),
        )
        .route("/v1/responses", responses_routes())
        .route("/v1/responses/compact", compact_route())
        .route("/backend-api/codex/responses", responses_routes())
        .route("/backend-api/codex/responses/compact", compact_route())
        .route(
            "/v1beta/models",
            get(gemini::models.layer(auth.clone())).head(not_found),
        )
        // A catch-all doesn't match an empty action, which gin's does.
        .route("/v1beta/models/", gemini_action_routes())
        .route("/v1beta/models/{*action}", gemini_action_routes())
        .method_not_allowed_fallback(not_found)
        .with_state(state.clone())
        .merge(extra)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state, safe_mode))
        .layer(CatchPanicLayer::custom(panicked))
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn(log_request))
}

/// Gin's 404.
async fn not_found() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "404 page not found").into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

/// Logs each request, without its query string, which may hold a key.
async fn log_request(request: Request, next: Next) -> Response {
    let id = uuid::Uuid::now_v7();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let span = tracing::info_span!("request", %id, %method, %path);
    let started = Instant::now();
    let response = next.run(request).instrument(span.clone()).await;
    span.in_scope(|| {
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "handled"
        );
    });
    response
}

/// Adds CORS headers to every response, and answers `OPTIONS` with 204
/// (`corsMiddleware`).
async fn cors(request: Request, next: Next) -> Response {
    let mut response = if request.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    for (name, value) in [
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, PUT, PATCH, DELETE, OPTIONS",
        ),
        (header::ACCESS_CONTROL_ALLOW_HEADERS, "*"),
        (header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSED_HEADERS),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// A handler's panic, as an empty 500.
fn panicked(panic: Box<dyn Any + Send + 'static>) -> Response {
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic");
    tracing::error!("handler panicked: {message}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Shuts the proxy routes while a client key is still a template value
/// (`exampleAPIKeySafeModeMiddleware`). Upstream also serves a warning page
/// at `/`, and points to its management page; this port points to the
/// config file.
async fn safe_mode(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !state.settings().safe_mode || !is_safe_mode_path(request.uri().path()) {
        return next.run(request).await;
    }
    let body = json!({
        "error": "unsafe_example_api_key",
        "message": "Proxy API endpoints are disabled because api-keys contains template \
                    values. Replace them in the config file, then retry.",
    })
    .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-cpa-safe-mode"),
        HeaderValue::from_static("example-api-key"),
    );
    error_response(403, headers, Bytes::from(body), JSON_UTF8)
}

/// Whether safe mode shuts `path`.
fn is_safe_mode_path(path: &str) -> bool {
    SAFE_MODE_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_mode_shuts_proxy_paths() {
        for path in [
            "/v1",
            "/v1/models",
            "/v1beta/models",
            "/openai/v1/x",
            "/backend-api/codex",
        ] {
            assert!(is_safe_mode_path(path), "{path}");
        }
        for path in [
            "/",
            "/v10",
            "/healthz",
            "/v1beta2",
            "/openai",
            "/backend-api",
        ] {
            assert!(!is_safe_mode_path(path), "{path}");
        }
    }
}
