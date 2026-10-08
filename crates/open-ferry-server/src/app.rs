// Ported from the routes in CLIProxyAPI internal/api/server_routes.go and the
// middleware in internal/api/server_middleware.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The router, and the middleware every request passes through: from the
//! outside in, the request context, which makes the request's
//! `RequestContext`, the access log, the request log, CORS, panic handling
//! and safe mode.

use std::any::Any;

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

use crate::auth::require_key;
use crate::errors::{JSON_UTF8, error_response};
use crate::handlers::{
    alpha_search, claude, gemini, health, images, interactions, models, openai, openai_speech,
    openai_videos, responses, responses_ws,
};
use crate::state::AppState;
use crate::{access_log, request_context, request_log};

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
/// through the same request context, logging, CORS, panic handling and
/// safe mode, but not the client-key check, and must not set a fallback.
pub fn router_with(state: AppState, extra: Router) -> Router {
    let auth = middleware::from_fn_with_state(state.clone(), require_key);
    let responses_routes = || {
        get(responses_ws::websocket.layer(auth.clone()))
            .head(not_found)
            .post(responses::responses.layer(auth.clone()))
    };
    let compact_route = || post(responses::compact.layer(auth.clone()));
    let alpha_search_route = || post(alpha_search::search.layer(auth.clone()));
    let model_detail_route = || get(models::detail.layer(auth.clone())).head(not_found);
    let gemini_action_routes = || {
        get(gemini::model.layer(auth.clone()))
            .head(not_found)
            .post(gemini::action.layer(auth.clone()))
    };
    let native_video_create = || post(openai_videos::native_create.layer(auth.clone()));
    Router::new()
        .route("/", get(health::root).head(not_found))
        .route("/healthz", get(health::healthz).head(health::healthz_head))
        .route(
            "/v1/models",
            get(models::unified.layer(auth.clone())).head(not_found),
        )
        // As for the Gemini actions, gin's catch-all matches an empty rest.
        .route("/v1/models/", model_detail_route())
        .route("/v1/models/{*model}", model_detail_route())
        .route(
            "/v1/chat/completions",
            post(openai::chat_completions.layer(auth.clone())),
        )
        .route(
            "/v1/completions",
            post(openai::completions.layer(auth.clone())),
        )
        .route(
            "/v1/images/generations",
            post(images::generations.layer(auth.clone())),
        )
        .route("/v1/images/edits", post(images::edits.layer(auth.clone())))
        .route(
            "/v1/audio/speech",
            post(openai_speech::speech.layer(auth.clone())),
        )
        .route("/v1/tts", post(openai_speech::speech.layer(auth.clone())))
        .route("/v1/messages", post(claude::messages.layer(auth.clone())))
        .route(
            "/v1/messages/count_tokens",
            post(claude::count_tokens.layer(auth.clone())),
        )
        .route("/v1/responses", responses_routes())
        .route("/v1/responses/compact", compact_route())
        .route("/backend-api/codex/responses", responses_routes())
        .route("/backend-api/codex/responses/compact", compact_route())
        .route("/v1/alpha/search", alpha_search_route())
        .route("/backend-api/codex/alpha/search", alpha_search_route())
        .route(
            "/v1beta/models",
            get(gemini::models.layer(auth.clone())).head(not_found),
        )
        // A catch-all doesn't match an empty action, which gin's does.
        .route("/v1beta/models/", gemini_action_routes())
        .route("/v1beta/models/{*action}", gemini_action_routes())
        .route(
            "/v1beta/interactions",
            post(interactions::interactions.layer(auth.clone())),
        )
        .route("/v1/videos", native_video_create())
        // gin keeps a tree of routes for each method, so a `GET` of these
        // is the retrieve of a video by that ID.
        .route(
            "/v1/videos/generations",
            native_video_create()
                .get(openai_videos::native_retrieve_generations.layer(auth.clone()))
                .head(not_found),
        )
        .route(
            "/v1/videos/edits",
            native_video_create()
                .get(openai_videos::native_retrieve_edits.layer(auth.clone()))
                .head(not_found),
        )
        .route(
            "/v1/videos/extensions",
            native_video_create()
                .get(openai_videos::native_retrieve_extensions.layer(auth.clone()))
                .head(not_found),
        )
        .route(
            "/v1/videos/{request_id}",
            get(openai_videos::native_retrieve.layer(auth.clone())).head(not_found),
        )
        .route(
            "/openai/v1/videos",
            post(openai_videos::create.layer(auth.clone())),
        )
        .route(
            "/openai/v1/videos/{video_id}",
            get(openai_videos::retrieve.layer(auth.clone())).head(not_found),
        )
        .route(
            "/openai/v1/videos/{video_id}/content",
            get(openai_videos::content.layer(auth.clone())).head(not_found),
        )
        .method_not_allowed_fallback(not_found)
        .with_state(state.clone())
        .merge(extra)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), safe_mode))
        .layer(CatchPanicLayer::custom(panicked))
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_log::layer,
        ))
        .layer(middleware::from_fn(access_log::layer))
        .layer(middleware::from_fn_with_state(
            state,
            request_context::layer,
        ))
}

/// Gin's 404.
async fn not_found() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "404 page not found").into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
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
/// at `/` and `/management.html`; here `/management.html` is the
/// dashboard's, which opens its API-key setup for `?safe-mode=configure`.
async fn safe_mode(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !state.settings().safe_mode || !is_safe_mode_path(request.uri().path()) {
        return next.run(request).await;
    }
    let body = json!({
        "error": "unsafe_example_api_key",
        "message": "Proxy API endpoints are disabled because api-keys contains template \
                    values. Open /management.html?safe-mode=configure, update api-keys in \
                    Management, then retry.",
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
