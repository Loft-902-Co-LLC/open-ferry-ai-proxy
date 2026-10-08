//! The config file's undo.
//!
//! - `POST /config/undo`: puts back the config file the last change
//!   replaced, the backup the config writer keeps beside it, and keeps the
//!   file it replaces as the new backup, so the undo can itself be undone.
//!   It takes the lock every config change takes, so it never interleaves
//!   with a management write, and the server loads the file again before
//!   it answers. `open-ferry config undo` uses it while a server is running
//!   for the config.
//!
//!   An optional body says what the caller saw: `config_sha256` and
//!   `backup_sha256`, the SHA-256 the file and its backup must have, and
//!   `force`, to go ahead though the file was changed since the last write
//!   that kept a backup (as by a hand edit, which the undo would lose).
//!   Without `force` such an undo is refused with `409 changed_since`; a
//!   file or backup that isn't the one the caller saw is refused with
//!   `409 config_changed`.

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use http::StatusCode;
use open_ferry_management::{UndoCheck, UndoError, undo_config};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, ok, read_body_or_default};
use crate::DashboardState;

/// What `POST /config/undo` may be told.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UndoInput {
    #[serde(default)]
    config_sha256: Option<String>,
    #[serde(default)]
    backup_sha256: Option<String>,
    #[serde(default)]
    force: bool,
}

/// `hash`, when it is a SHA-256 in hex, else a `400`.
fn sha256(name: &str, hash: Option<String>) -> Result<Option<String>, ApiError> {
    match hash {
        Some(hash) if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Ok(Some(hash.to_ascii_lowercase()))
        }
        Some(_) => Err(ApiError::invalid(format!(
            "{name} must be a SHA-256 in hex, 64 characters"
        ))),
        None => Ok(None),
    }
}

/// `POST /config/undo`.
pub(super) async fn undo(
    State(state): State<DashboardState>,
    body: Body,
) -> Result<Response, ApiError> {
    let input: UndoInput = read_body_or_default(body).await?;
    let check = UndoCheck {
        config_sha256: sha256("config_sha256", input.config_sha256)?,
        backup_sha256: sha256("backup_sha256", input.backup_sha256)?,
        force: input.force,
    };
    match undo_config(&state.management, check).await {
        Ok(()) => Ok(ok(&json!({"status": "ok"}))),
        Err(UndoError::NoBackup) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "no_backup",
            "there is no backup of the config file to undo to",
        )),
        Err(UndoError::ChangedSince) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "changed_since",
            "the config file was changed since the last change that kept a backup, as by a hand edit, and undoing would lose that change too; send force to go ahead",
        )),
        Err(UndoError::Stale) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "config_changed",
            "the config file or its backup isn't the one expected: it changed since it was read",
        )),
        Err(UndoError::Unavailable) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "config_writer_unavailable",
            "this server can't write its config file",
        )),
        Err(error) => Err(ApiError::internal(format!("failed to undo: {error}"))),
    }
}
