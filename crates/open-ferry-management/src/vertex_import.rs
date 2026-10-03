// To be ported from CLIProxyAPI internal/api/handlers/management/
// vertex_import.go (ImportVertexCredential), auth_files_v8.go
// (ImportOAuthV8) and internal/auth/vertex/keyutil.go
// (NormalizeServiceAccountMap) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Importing a Vertex AI service account as a credential file.
//!
//! Not ported yet: `POST /v0/management/vertex/import` (also `POST
//! /v8/management/oauth/import?provider=vertex`).

use crate::Route;

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
