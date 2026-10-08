// Ported from CLIProxyAPI internal/api/handlers/management/handler.go
// (persist, persistLocked, saveConfigAndSnapshotLocked,
// reloadConfigAfterManagementSave, reloadConfigAfterManagementSaveAsync,
// SetConfigReloadHook), config_basic.go (WriteConfig, as PutConfigYAML
// uses it) and config_v8.go (ConfigV8's writes, as the writer makes them),
// and sdk/cliproxy/builder.go (the reload hook the service sets, which
// reloads the file) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Writing the config: the [`ConfigWriter`] that saves it, the
//! [`ConfigReload`] that has the running service load it again, and the
//! way every handler that changes the config uses them.
//!
//! A change is made under one lock, so changes never interleave: the
//! current config is copied, the copy changed and saved, and the saved copy
//! becomes the config the handlers read. The service is then asked to load
//! the file again, so the change takes effect without waiting for the file
//! watcher, and the handler answers once it has. A save the writer refuses
//! answers 500 `{"error":"failed to save config: ..."}` and changes
//! nothing. The work runs on its own task, so a client that goes away
//! doesn't stop it halfway.
//!
//! Only the management routes write the config, and only through the
//! writer the service gives the state; nothing is written when the config
//! is loaded. The binary's service always gives one, a [`FileConfigWriter`]
//! over the file it was started with, and has its file watcher load the
//! file again after each save.
//!
//! Deviations from upstream:
//! - Without a writer, as in a state made only with
//!   [`ManagementState::new`](crate::ManagementState::new), every route
//!   that changes the config answers 503 `{"error":"config writer
//!   unavailable"}` before reading its body, and changes nothing. Upstream
//!   always has a path to save to.
//! - A save that fails leaves the config the handlers read as it was.
//!   Upstream changes its config before saving and keeps the change when
//!   the save fails, so later reads and saves show a change the file never
//!   got.
//! - The answer is sent once the service has reloaded; upstream answers
//!   first and reloads in the background, except for a status change on a
//!   config API key, which it reloads first too.
//! - Upstream numbers its reloads and skips one older than a reload
//!   already applied. The service's reload goes through its file watcher,
//!   as upstream's hook goes through `ReloadConfigIfChanged`, and the
//!   watcher's changes are applied in the order it read them, so there is
//!   nothing to skip.
//! - [`undo_config`] puts back the backup the last write kept, keeping the
//!   file it replaces as the new backup, for the dashboard API's
//!   `config/undo` route. Upstream has no undo.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::response::Response;
use http::StatusCode;
use open_ferry_core::config::Config;
use open_ferry_core::config::save::SaveErrorKind;
pub use open_ferry_core::config::v8_edit::{V8Edit, V8EditError, V8Method};
use open_ferry_core::config::{save, v8_edit};

use crate::auth_files::run_blocking;
use crate::bind;
use crate::json::{self, Json};
use crate::state::ManagementState;

/// What a route answers when the state has no [`ConfigWriter`].
const WRITER_UNAVAILABLE: &str = "config writer unavailable";

/// Saves the config file the proxy was started with. The service gives one
/// to the management state with
/// [`ManagementState::with_config_writer`](crate::ManagementState::with_config_writer);
/// the handlers call it on the blocking pool, one call at a time.
pub trait ConfigWriter: Send + Sync {
    /// Saves `config` over the file, keeping the file's comments and the
    /// order of its keys (upstream's `SaveConfigPreserveComments`). With
    /// `migrate_v8`, as for a change made through a v8 route, a file in the
    /// legacy layout is saved in the v8 layout.
    fn save_preserving_comments(&self, config: &Config, migrate_v8: bool)
    -> Result<(), WriteError>;

    /// Replaces the file with `data`, already checked to load (upstream's
    /// `WriteConfig`).
    fn write_file(&self, data: &[u8]) -> Result<(), WriteError>;

    /// Reads the file, makes `edit` to it in the v8 layout, checks the
    /// result and saves it, and returns the config the file now holds
    /// (upstream's `ConfigV8` for `PUT`, `PATCH` and `DELETE`).
    fn edit_v8(&self, edit: &V8Edit) -> Result<Config, V8EditError>;

    /// Puts the backup the last write kept in place of the file, keeping
    /// the file it replaces as the new backup so the undo can itself be
    /// undone, and returns the config the file now holds. Not upstream's:
    /// the dashboard API's `config/undo` route uses it, through
    /// [`undo_config`]. A writer that keeps no backup has nothing to undo,
    /// which is what this answers unless the writer says otherwise.
    fn undo(&self) -> Result<Config, UndoError> {
        Err(UndoError::NoBackup)
    }
}

/// Why [`undo_config`] or [`ConfigWriter::undo`] didn't put the backup
/// back. Nothing was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UndoError {
    /// The state has no [`ConfigWriter`].
    Unavailable,
    /// There is no backup to put back.
    NoBackup,
    /// The backup couldn't be put back. The text says why, and holds no
    /// secret from the config.
    Failed(String),
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str(WRITER_UNAVAILABLE),
            Self::NoBackup => f.write_str("there is no backup to undo to"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for UndoError {}

/// Why a [`ConfigWriter`] didn't save. Its text follows `failed to save
/// config: ` in the answer, so it must not hold a secret from the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError(String);

impl WriteError {
    /// An error with `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WriteError {}

/// The [`ConfigWriter`] the service uses: it writes the config file at a
/// path with open-ferry-core's writer
/// ([`save`](open_ferry_core::config::save) and
/// [`v8_edit`](open_ferry_core::config::v8_edit)). Each write is checked to
/// load, written atomically beside a backup of the file it replaces, and
/// refused when the path is a symbolic link.
#[derive(Clone, Debug)]
pub struct FileConfigWriter {
    path: PathBuf,
}

impl FileConfigWriter {
    /// A writer for the config file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file it writes.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ConfigWriter for FileConfigWriter {
    fn save_preserving_comments(
        &self,
        config: &Config,
        migrate_v8: bool,
    ) -> Result<(), WriteError> {
        save::save_preserving_comments(&self.path, config, migrate_v8)
            .map_err(|error| WriteError::new(error.to_string()))
    }

    fn write_file(&self, data: &[u8]) -> Result<(), WriteError> {
        save::write_file(&self.path, data).map_err(|error| WriteError::new(error.to_string()))
    }

    fn edit_v8(&self, edit: &V8Edit) -> Result<Config, V8EditError> {
        v8_edit::edit_v8(&self.path, edit)
    }

    fn undo(&self) -> Result<Config, UndoError> {
        save::undo(&self.path).map_err(|error| match error.kind() {
            SaveErrorKind::NoBackup => UndoError::NoBackup,
            // The loader's message may quote the file; say only what failed.
            SaveErrorKind::Check => {
                UndoError::Failed("the backup doesn't load as a config".to_owned())
            }
            _ => UndoError::Failed(error.to_string()),
        })
    }
}

/// What a v8 route answers when [`ConfigWriter::edit_v8`] made no change
/// (upstream's `ConfigV8`).
pub(crate) fn v8_error_response(error: &V8EditError) -> Response {
    use V8EditError as E;
    let (status, error, detail) = match error {
        E::ReadFailed => (StatusCode::INTERNAL_SERVER_ERROR, "read_failed", None),
        E::StoredInvalid(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_config",
            Some(("message", message)),
        ),
        E::CannotDeleteConfig => (StatusCode::BAD_REQUEST, "cannot_delete_config", None),
        E::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
        E::InvalidBody => (StatusCode::BAD_REQUEST, "invalid_body", None),
        E::InvalidJson => (StatusCode::BAD_REQUEST, "invalid_json", None),
        E::ConfigMustBeObject => (StatusCode::BAD_REQUEST, "config_must_be_object", None),
        E::InvalidPath => (StatusCode::BAD_REQUEST, "invalid_path", None),
        E::InvalidConfig(message) => (
            StatusCode::BAD_REQUEST,
            "invalid_config",
            Some(("message", message)),
        ),
        E::ReadOnlyField(field) => (
            StatusCode::BAD_REQUEST,
            "read_only_field",
            Some(("field", field)),
        ),
        E::Unprocessable(message) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            Some(("message", message)),
        ),
        E::WriteFailed(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "write_failed",
            Some(("message", message)),
        ),
        // A refusal core adds later, until it has its own answer.
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "write_failed", None),
    };
    let body = match detail {
        None => Json::map([("error", Json::Str(error.to_owned()))]),
        Some((name, value)) => Json::map([
            ("error", Json::Str(error.to_owned())),
            (name, Json::Str(value.clone())),
        ]),
    };
    json::response(status, &body)
}

/// The future a [`ConfigReload`] call returns: ready once the service has
/// loaded the file and applied it.
pub type ReloadFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// The running service, as the management handlers reach it once they
/// have saved the config (upstream's config reload hook).
pub trait ConfigReload: Send + Sync {
    /// Loads the config file again and applies it, as when the file
    /// watcher reports a change (upstream's `ReloadConfigIfChanged`). A
    /// file that doesn't load is logged and left, as the watcher leaves it.
    fn reload(&self) -> ReloadFuture<'_>;
}

/// `{"error":"config writer unavailable"}` with 503, when the state has no
/// writer.
pub(crate) fn writer_unavailable() -> Response {
    json::error(StatusCode::SERVICE_UNAVAILABLE, WRITER_UNAVAILABLE)
}

/// `{"status":"ok"}`, what a saved change answers.
pub(crate) fn ok() -> Response {
    json::response(
        StatusCode::OK,
        &Json::map([("status", Json::Str("ok".into()))]),
    )
}

/// The state's writer, or the 503 answer.
pub(crate) fn writer(state: &ManagementState) -> Result<Arc<dyn ConfigWriter>, Response> {
    state
        .config_writer()
        .cloned()
        .ok_or_else(writer_unavailable)
}

/// The body of a request that changes the config: the 503 answer when the
/// state has no writer, else the body, or the 413 answer when it is too
/// large.
pub(crate) async fn request_body(state: &ManagementState, body: Body) -> Result<Bytes, Response> {
    writer(state)?;
    bind::read_body(body).await
}

/// `{"error":"<message>"}` with 400.
pub(crate) fn bad_request(message: &str) -> Response {
    json::error(StatusCode::BAD_REQUEST, message)
}

/// `{"error":"<message>"}` with 404.
pub(crate) fn not_found(message: &str) -> Response {
    json::error(StatusCode::NOT_FOUND, message)
}

/// Changes the config with `change`, saves it, has the service reload it,
/// and answers `{"status":"ok"}` (upstream's `persist` after a handler's
/// change). `change` answers instead when it can't make the change; the
/// config is then left as it was.
pub(crate) async fn update<F>(state: &ManagementState, migrate_v8: bool, change: F) -> Response
where
    F: FnOnce(&mut Config) -> Result<(), Response> + Send + 'static,
{
    let state = state.clone();
    run_task(async move {
        match save(&state, migrate_v8, change).await {
            Ok(()) => {
                reload(&state).await;
                ok()
            }
            Err(response) => response,
        }
    })
    .await
}

/// Changes the config with `change` and saves it, under the write lock
/// (upstream's change and `persistLocked`), without reloading. On success
/// the saved config is the one the handlers read.
pub(crate) async fn save<F>(
    state: &ManagementState,
    migrate_v8: bool,
    change: F,
) -> Result<(), Response>
where
    F: FnOnce(&mut Config) -> Result<(), Response> + Send + 'static,
{
    let writer = writer(state)?;
    let state = state.clone();
    run_task(async move {
        let _guard = state.config_write_lock().lock().await;
        let mut config = Config::clone(&state.config());
        change(&mut config)?;
        let config = Arc::new(config);
        let saved = Arc::clone(&config);
        run_blocking(move || writer.save_preserving_comments(&saved, migrate_v8))
            .await
            .map_err(|error| {
                json::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("failed to save config: {error}"),
                )
            })?;
        state.set_config(config);
        Ok(())
    })
    .await
}

/// Puts the config file's backup in its place with the state's writer
/// ([`ConfigWriter::undo`]), under the write lock every change takes, makes
/// the config it holds the one the handlers read, and has the service load
/// the file again. Not upstream's: the dashboard API's `config/undo` route
/// calls it. On an error nothing was changed.
pub async fn undo_config(state: &ManagementState) -> Result<(), UndoError> {
    let writer = state
        .config_writer()
        .cloned()
        .ok_or(UndoError::Unavailable)?;
    let state = state.clone();
    run_task(async move {
        {
            let _guard = state.config_write_lock().lock().await;
            let config = run_blocking(move || writer.undo()).await?;
            state.set_config(Arc::new(config));
        }
        reload(&state).await;
        Ok(())
    })
    .await
}

/// Has the service load the config file again, if it gave a way to.
pub(crate) async fn reload(state: &ManagementState) {
    if let Some(reload) = state.config_reload().cloned() {
        run_task(async move { reload.reload().await }).await;
    }
}

/// Runs `future` on its own task, so it finishes even when the request is
/// dropped, and passes a panic on.
pub(crate) async fn run_task<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> T {
    match tokio::spawn(future).await {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(error) => panic!("config write task failed: {error}"),
        },
    }
}
