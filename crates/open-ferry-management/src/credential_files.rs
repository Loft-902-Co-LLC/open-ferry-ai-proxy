// To be ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_crud.go (DownloadAuthFile, UploadAuthFile, writeAuthFile,
// DeleteAuthFile), auth_files.go (isUnsafeAuthFileName) and
// auth_files_fields.go (removeAuth, deleteTokenRecord) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credential files: downloading, uploading and deleting them.
//!
//! Not ported yet: `GET /v0/management/auth-files/download` (also
//! `/v8/management/credentials/download`), and `POST` and `DELETE
//! /v0/management/auth-files` (also `/v8/management/credentials`). They
//! will change the auth directory only, never the config.

use crate::Route;

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
