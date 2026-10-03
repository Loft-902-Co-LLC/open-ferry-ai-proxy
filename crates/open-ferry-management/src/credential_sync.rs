// Stands in for CLIProxyAPI's postAuthPersistHook
// (internal/api/handlers/management/handler.go, SetPostAuthPersistHook;
// internal/api/server_options.go, WithPostAuthPersistHook) and the hook the
// service sets there, sdk/cliproxy/builder.go (runtimeAuthSyncHook)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`CredentialSync`]: how the management handlers tell the running
//! service about a credential they changed, so it is served at once rather
//! than once the auth-directory watcher notices.
//!
//! The service owns the credential manager's registrations and the model
//! registry's, and changes them from one loop; a handler sends it the
//! change and waits until the loop has applied it. A handler that writes or
//! removes a credential file calls [`CredentialSync::file_written`] or
//! [`CredentialSync::file_removed`] once the file is written or removed;
//! one that changes a credential without writing its file itself, such as
//! through [`Manager::update`](open_ferry_core::manager::Manager), calls
//! [`CredentialSync::upsert`]. The watcher reports the same change again a
//! moment later, which changes nothing more.
//!
//! When the service has stopped, every call fails with
//! [`SyncError::Stopped`], which answers 503. Handlers don't hold the
//! state's credential lock while they wait (see
//! [`ManagementState::credential_lock`](crate::ManagementState)).
//!
//! Deviations from upstream:
//! - Upstream's hook takes the credential and registers it, adding or
//!   updating it with its models; it never fails. Here a written file is
//!   sent instead of the credential made from it, the service making the
//!   credential as the watcher would, and a removed file is sent too, where
//!   upstream leaves removals to the watcher.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use axum::response::{IntoResponse, Response};
use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::AuthFile;

use crate::json;

/// The future a [`CredentialSync`] call returns: ready once the service has
/// applied the change.
pub type SyncFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SyncError>> + Send + 'a>>;

/// The running service, as the management handlers reach it to apply a
/// credential change (upstream's post-auth persist hook).
pub trait CredentialSync: Send + Sync {
    /// Registers `auth`, or updates the credential registered with its ID,
    /// and registers its models, without saving it again (upstream's
    /// `runtimeAuthSyncHook`).
    fn upsert(&self, auth: Auth) -> SyncFuture<'_>;

    /// Registers the credential in the auth file just written, from these
    /// contents, as the watcher does for a file that is added or changes.
    /// Contents that hold no credential unregister the one registered for
    /// the file.
    fn file_written(&self, file: AuthFile) -> SyncFuture<'_>;

    /// Unregisters the credential registered for the auth file at `path`,
    /// just removed, as the watcher does for a file that is removed.
    fn file_removed(&self, path: PathBuf) -> SyncFuture<'_>;
}

/// Why a [`CredentialSync`] call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncError {
    /// The service has stopped, or is stopping, and applies no more
    /// changes.
    Stopped,
}

impl SyncError {
    /// The status a handler answers with: 503.
    pub fn status(self) -> StatusCode {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("credential sync unavailable: the service has stopped"),
        }
    }
}

impl std::error::Error for SyncError {}

/// `503 {"error":"credential sync unavailable: the service has stopped"}`.
impl IntoResponse for SyncError {
    fn into_response(self) -> Response {
        json::error(self.status(), &self.to_string())
    }
}
