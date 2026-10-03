// To be ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_fields.go (PatchAuthFileStatus, PatchAuthFileFields) and
// auth_files_refresh.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's state: turning it on and off, changing its settings and
//! refreshing its tokens.
//!
//! Not ported yet: `PATCH /v0/management/auth-files/status` and
//! `/v0/management/auth-files/fields`, and `POST
//! /v0/management/auth-files/refresh` (also under
//! `/v8/management/credentials`). They will change credentials only,
//! never the config.

use crate::Route;

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
