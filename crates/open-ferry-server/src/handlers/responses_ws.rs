//! `GET /v1/responses`: the Responses WebSocket.
//!
//! Placeholder: answers 501 until the WebSocket is ported.

use axum::response::Response;

use crate::errors::local_error;

/// `GET /v1/responses`.
pub(crate) async fn websocket() -> Response {
    local_error(
        501,
        "the Responses WebSocket is not implemented yet",
        "server_error",
    )
}
