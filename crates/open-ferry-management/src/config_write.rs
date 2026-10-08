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
//!   `config/undo` route, with the checks an [`UndoCheck`] asks for.
//!   Upstream has no undo.
//! - The state keeps the SHA-256 of the file's contents the config the
//!   handlers read was loaded from, or last written as (see
//!   [`ManagementState::with_config_sha256`](crate::ManagementState::with_config_sha256)).
//!   A change made to that config is saved only while the file still
//!   holds them, which the writer checks under the file's lock: else it
//!   answers 409 `{"error":"config_changed","message":"..."}`, writes
//!   nothing, and has the service load the file again, forced (see
//!   [`ConfigReload::force_reload`]), so the change can be made again. The
//!   config the handlers read is only ever one the service applied, or one
//!   written here: when the file is empty or doesn't load, the service keeps
//!   its config and so do the handlers, and each such save answers 409,
//!   saying the file needs fixing, until it is. Upstream saves its config
//!   over the file whatever the file holds, so a change made there since,
//!   as by `open-ferry config`, is undone. The v8 writes, `PUT config.yaml`
//!   and the undo work from what the file holds under its lock, so they
//!   don't check it.

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
pub use open_ferry_core::config::save::UndoCheck;
pub use open_ferry_core::config::v8_edit::{V8Edit, V8EditError, V8Method};
use open_ferry_core::config::{save, v8_edit};

use crate::auth_files::run_blocking;
use crate::bind;
use crate::json::{self, Json};
use crate::state::ManagementState;

/// What a route answers when the state has no [`ConfigWriter`].
const WRITER_UNAVAILABLE: &str = "config writer unavailable";

/// What a save answers when the file changed since the config the handlers
/// read was loaded or written, once the service has loaded it again.
const CONFIG_CHANGED: &str = "the config file changed on disk since the server loaded it; it has been loaded again, so try again";

/// What a save answers when the file changed since, and the service didn't
/// load it again: it is empty or doesn't load.
const CANT_LOAD: &str =
    "the config file changed on disk and can't be loaded: fix it, then try again";

/// Saves the config file the proxy was started with. The service gives one
/// to the management state with
/// [`ManagementState::with_config_writer`](crate::ManagementState::with_config_writer);
/// the handlers call it on the blocking pool, one call at a time.
///
/// A write returns the SHA-256 of what it wrote, in lowercase hex, which
/// the state keeps to check the file against before the next save, or
/// `None` for a writer that doesn't keep one; nothing is checked then.
pub trait ConfigWriter: Send + Sync {
    /// Saves `config` over the file, keeping the file's comments and the
    /// order of its keys (upstream's `SaveConfigPreserveComments`). With
    /// `migrate_v8`, as for a change made through a v8 route, a file in the
    /// legacy layout is saved in the v8 layout. With `expected_sha256`, the
    /// SHA-256 of the contents `config` was loaded from or last written as,
    /// it saves only while the file still holds them, checked under the
    /// file's lock, and otherwise writes nothing and answers
    /// [`WriteError::config_changed`] (not upstream's).
    fn save_preserving_comments(
        &self,
        config: &Config,
        migrate_v8: bool,
        expected_sha256: Option<&str>,
    ) -> Result<Option<String>, WriteError>;

    /// Replaces the file with `data`, already checked to load (upstream's
    /// `WriteConfig`).
    fn write_file(&self, data: &[u8]) -> Result<Option<String>, WriteError>;

    /// Reads the file, makes `edit` to it in the v8 layout, checks the
    /// result and saves it, and returns the config the file now holds
    /// (upstream's `ConfigV8` for `PUT`, `PATCH` and `DELETE`).
    fn edit_v8(&self, edit: &V8Edit) -> Result<(Config, Option<String>), V8EditError>;

    /// Puts the backup the last write kept in place of the file, keeping
    /// the file it replaces as the new backup so the undo can itself be
    /// undone, and returns the config the file now holds, after the checks
    /// `check` asks for (see [`open_ferry_core::config::save::undo`]). Not
    /// upstream's: the dashboard API's `config/undo` route uses it, through
    /// [`undo_config`]. A writer that keeps no backup has nothing to undo,
    /// which is what this answers unless the writer says otherwise.
    fn undo(&self, check: &UndoCheck) -> Result<(Config, Option<String>), UndoError> {
        let _ = check;
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
    /// The file was changed since the last write that kept a backup, as by
    /// a hand edit, so the undo would lose that change too; it wasn't
    /// forced.
    ChangedSince,
    /// The file or the backup isn't the one the caller expected: it
    /// changed since the caller read it.
    Stale,
    /// The backup couldn't be put back. The text says why, and holds no
    /// secret from the config.
    Failed(String),
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str(WRITER_UNAVAILABLE),
            Self::NoBackup => f.write_str("there is no backup to undo to"),
            Self::ChangedSince => {
                f.write_str("the config file was changed since the last change that kept a backup")
            }
            Self::Stale => f.write_str("the config file or its backup changed since it was read"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for UndoError {}

/// Why a [`ConfigWriter`] didn't save. Its text follows `failed to save
/// config: ` in the answer, so it must not hold a secret from the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError {
    message: String,
    config_changed: bool,
}

impl WriteError {
    /// An error with `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            config_changed: false,
        }
    }

    /// The file doesn't hold the contents the save expected: it changed
    /// since the config was loaded or last written. Nothing was written.
    /// Not upstream's.
    pub fn config_changed() -> Self {
        Self {
            message: "the config file changed since it was loaded".to_owned(),
            config_changed: true,
        }
    }

    /// Whether this is [`WriteError::config_changed`].
    pub fn is_config_changed(&self) -> bool {
        self.config_changed
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
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
        expected_sha256: Option<&str>,
    ) -> Result<Option<String>, WriteError> {
        save::save_preserving_comments_expecting(&self.path, config, migrate_v8, expected_sha256)
            .map(Some)
            .map_err(|error| match error.kind() {
                SaveErrorKind::Stale => WriteError::config_changed(),
                _ => WriteError::new(error.to_string()),
            })
    }

    fn write_file(&self, data: &[u8]) -> Result<Option<String>, WriteError> {
        save::write_file_with_sha256(&self.path, data)
            .map(Some)
            .map_err(|error| WriteError::new(error.to_string()))
    }

    fn edit_v8(&self, edit: &V8Edit) -> Result<(Config, Option<String>), V8EditError> {
        v8_edit::edit_v8_with_sha256(&self.path, edit)
            .map(|(config, sha256)| (config, Some(sha256)))
    }

    fn undo(&self, check: &UndoCheck) -> Result<(Config, Option<String>), UndoError> {
        save::undo_with_sha256(&self.path, check)
            .map(|(config, sha256)| (config, Some(sha256)))
            .map_err(|error| match error.kind() {
                SaveErrorKind::NoBackup => UndoError::NoBackup,
                SaveErrorKind::ChangedSince => UndoError::ChangedSince,
                SaveErrorKind::Stale => UndoError::Stale,
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

    /// [`reload`](Self::reload), loading the file even when it holds what
    /// the service loaded last (not upstream's). A save that found the file
    /// changed has it reloaded so, as the file may hold again what the
    /// service loaded before the handlers' last write, which a plain reload
    /// would skip. An empty file is still skipped, and one that doesn't load
    /// left; the service then keeps its config, and the handlers theirs.
    /// Unless the service says otherwise, this is `reload`.
    fn force_reload(&self) -> ReloadFuture<'_> {
        self.reload()
    }
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

/// `{"error":"config_changed","message":"..."}` with 409, what a save
/// answers when the file changed since the config the handlers read was
/// loaded or written; the message says whether the service `loaded` it
/// again, or it needs fixing first.
pub(crate) fn config_changed(loaded: bool) -> Response {
    let message = if loaded { CONFIG_CHANGED } else { CANT_LOAD };
    json::response(
        StatusCode::CONFLICT,
        &Json::map([
            ("error", Json::Str("config_changed".into())),
            ("message", Json::Str(message.into())),
        ]),
    )
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
/// the saved config is the one the handlers read. The file must still
/// hold what that config was loaded from or last written as: else the
/// answer is 409 `config_changed`, nothing is written, and the service
/// loads the file again (not upstream's).
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
        let guard = state.config_write_lock().lock().await;
        let (current, expected) = state.loaded_config();
        let stale = expected.clone();
        let mut config = Config::clone(&current);
        change(&mut config)?;
        let config = Arc::new(config);
        let saved = Arc::clone(&config);
        let written = run_blocking(move || {
            writer.save_preserving_comments(&saved, migrate_v8, expected.as_deref())
        })
        .await;
        match written {
            Ok(sha256) => {
                state.set_loaded_config(config, sha256);
                Ok(())
            }
            Err(error) if error.is_config_changed() => {
                drop(guard);
                let loaded = catch_up(&state, stale).await;
                Err(config_changed(loaded))
            }
            Err(error) => Err(json::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to save config: {error}"),
            )),
        }
    })
    .await
}

/// Has the service load the config file again, forced, after a save found
/// that the file changed since the config the handlers read was loaded or
/// written as `stale`, the SHA-256 the save expected. That config changes
/// only as the service applies what it loaded, never here, so it is always
/// one the proxy runs. Whether it changed: it doesn't when the service
/// skipped or refused the file, as one that is empty or doesn't load, nor
/// without a service to reload.
async fn catch_up(state: &ManagementState, stale: Option<String>) -> bool {
    reload_with(state, true).await;
    state.config_sha256() != stale
}

/// Puts the config file's backup in its place with the state's writer
/// ([`ConfigWriter::undo`]), under the write lock every change takes, makes
/// the config it holds the one the handlers read, and has the service load
/// the file again. Not upstream's: the dashboard API's `config/undo` route
/// calls it. On an error nothing was changed.
pub async fn undo_config(state: &ManagementState, check: UndoCheck) -> Result<(), UndoError> {
    let writer = state
        .config_writer()
        .cloned()
        .ok_or(UndoError::Unavailable)?;
    let state = state.clone();
    run_task(async move {
        {
            let _guard = state.config_write_lock().lock().await;
            let (config, sha256) = run_blocking(move || writer.undo(&check)).await?;
            state.set_loaded_config(Arc::new(config), sha256);
        }
        reload(&state).await;
        Ok(())
    })
    .await
}

/// Has the service load the config file again, if it gave a way to.
pub(crate) async fn reload(state: &ManagementState) {
    reload_with(state, false).await;
}

/// [`reload`], forced with `force` (see [`ConfigReload::force_reload`]).
async fn reload_with(state: &ManagementState, force: bool) {
    if let Some(reload) = state.config_reload().cloned() {
        run_task(async move {
            if force {
                reload.force_reload().await;
            } else {
                reload.reload().await;
            }
        })
        .await;
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
