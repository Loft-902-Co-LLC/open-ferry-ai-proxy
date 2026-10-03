//! `POST /v1/responses` and `POST /v1/responses/compact`.
//!
//! Placeholder: answers 501 until the Responses handlers are ported.

use axum::response::Response;

use crate::errors::local_error;

/// `POST /v1/responses`.
pub(crate) async fn responses() -> Response {
    local_error(
        501,
        "the Responses API is not implemented yet",
        "server_error",
    )
}

/// `POST /v1/responses/compact`.
pub(crate) async fn compact() -> Response {
    local_error(
        501,
        "the Responses API is not implemented yet",
        "server_error",
    )
}
