//! The config file's undo.
//!
//! - `POST /config/undo`: puts back the config file the last change
//!   replaced, the backup the config writer keeps beside it, and keeps the
//!   file it replaces as the new backup, so the undo can itself be undone.
//!   It takes the lock every config change takes, so it never interleaves
//!   with a management write, and the server loads the file again before
//!   it answers. `open-ferry config undo` uses it while a server is running
//!   for the config.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;
use open_ferry_management::{UndoError, undo_config};
use serde_json::json;

use super::{ApiError, ok};
use crate::DashboardState;

/// `POST /config/undo`.
pub(super) async fn undo(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    match undo_config(&state.management).await {
        Ok(()) => Ok(ok(&json!({"status": "ok"}))),
        Err(UndoError::NoBackup) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "no_backup",
            "there is no backup of the config file to undo to",
        )),
        Err(UndoError::Unavailable) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "config_writer_unavailable",
            "this server can't write its config file",
        )),
        Err(error) => Err(ApiError::internal(format!("failed to undo: {error}"))),
    }
}
