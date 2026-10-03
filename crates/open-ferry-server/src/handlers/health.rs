// Ported from the health and root handlers in CLIProxyAPI setupRoutes,
// internal/api/server_routes.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `/healthz` and `/`.

use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde_json::json;

use super::json_utf8;

/// `GET /healthz`.
pub(crate) async fn healthz() -> Response {
    json_utf8(json!({"status": "ok"}).to_string())
}

/// `HEAD /healthz`: 200 with no body.
pub(crate) async fn healthz_head() -> Response {
    StatusCode::OK.into_response()
}

/// `GET /`: what the server is, and its main endpoints. Upstream names
/// itself `CLI Proxy API Server`.
pub(crate) async fn root() -> Response {
    json_utf8(
        json!({
            "endpoints": [
                "POST /v1/chat/completions",
                "POST /v1/completions",
                "GET /v1/models",
            ],
            "message": "open-ferry-ai-proxy",
        })
        .to_string(),
    )
}
