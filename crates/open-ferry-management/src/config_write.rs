// Ported from CLIProxyAPI internal/api/handlers/management/handler.go
// (persist, persistLocked, saveConfigAndSnapshotLocked,
// reloadConfigAfterManagementSave, reloadConfigAfterManagementSaveAsync,
// SetConfigReloadHook), config_basic.go (WriteConfig, as PutConfigYAML
// uses it) and config_v8.go (ConfigV8's writes, as the writer makes them),
// and sdk/cliproxy/builder.go (the reload hook the service sets, which
// reloads the file) (v8.0.15, MIT).
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
//! is loaded.
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
//!   already applied. The reload here reads the file, which holds the
//!   latest save, so there is nothing to skip.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::response::Response;
use http::StatusCode;
use open_ferry_core::config::Config;

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

    /// Sets the scalar at `keys`, a path of mapping keys, to `value`,
    /// leaving the rest of the file as it is (upstream's
    /// `SaveConfigPreserveCommentsUpdateNestedScalar`).
    fn update_nested_scalar(&self, keys: &[&str], value: &str) -> Result<(), WriteError>;

    /// Replaces the file with `data`, already checked to load (upstream's
    /// `WriteConfig`).
    fn write_file(&self, data: &[u8]) -> Result<(), WriteError>;

    /// Reads the file, makes `edit` to it in the v8 layout, checks the
    /// result and saves it, and returns the config the file now holds
    /// (upstream's `ConfigV8` for `PUT`, `PATCH` and `DELETE`).
    fn edit_v8(&self, edit: &V8Edit) -> Result<Config, V8EditError>;
}

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

/// The method of a v8 config write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V8Method {
    /// Replaces the value at the path, or the whole config.
    Put,
    /// Merges the body into the value at the path, or into the whole
    /// config: mappings key by key, anything else replaced. `null` is kept,
    /// not deleted.
    Patch,
    /// Removes the value at the path, and the mappings it leaves empty.
    Delete,
}

/// A v8 config write, as the route received it.
#[derive(Clone, PartialEq, Eq)]
pub struct V8Edit {
    /// What to do.
    pub method: V8Method,
    /// The keys leading to the value, from the request path split on `/`;
    /// empty for the whole config. A part may be empty, which the writer
    /// refuses.
    pub path: Vec<String>,
    /// The request body, for `PUT` and `PATCH`.
    pub body: Vec<u8>,
    /// Whether the route was `PUT /v8/management/config.yaml`: the body is
    /// YAML, and need not be JSON.
    pub yaml: bool,
}

/// The body's length only: it may hold secrets.
impl fmt::Debug for V8Edit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V8Edit")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("body_len", &self.body.len())
            .field("yaml", &self.yaml)
            .finish()
    }
}

/// Why [`ConfigWriter::edit_v8`] made no change, and so what the route
/// answers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum V8EditError {
    /// The file couldn't be read: 500 `read_failed`.
    ReadFailed,
    /// The file as it is doesn't read in the v8 layout: 500
    /// `invalid_config` with the message.
    StoredInvalid(String),
    /// `DELETE` of the whole config: 400 `cannot_delete_config`.
    CannotDeleteConfig,
    /// `DELETE` of a path that doesn't exist: 404 `not_found`.
    NotFound,
    /// The body is empty or isn't YAML: 400 `invalid_body`.
    InvalidBody,
    /// A body sent to a JSON route isn't JSON: 400 `invalid_json`.
    InvalidJson,
    /// A body for the whole config isn't a mapping: 400
    /// `config_must_be_object`.
    ConfigMustBeObject,
    /// A part of the path is empty, or passes through a value that isn't a
    /// mapping: 400 `invalid_path`.
    InvalidPath,
    /// The result isn't a valid v8 config: 400 `invalid_config` with the
    /// message.
    InvalidConfig(String),
    /// The write changes a field Home owns: 400 `read_only_field` naming
    /// it.
    ReadOnlyField(String),
    /// The result doesn't load as a config: 422 `invalid_config` with the
    /// message.
    Unprocessable(String),
    /// The file couldn't be written: 500 `write_failed` with the message.
    WriteFailed(String),
}

impl V8EditError {
    /// What a v8 route answers with (upstream's `ConfigV8`).
    pub(crate) fn response(&self) -> Response {
        let (status, error, detail) = match self {
            Self::ReadFailed => (StatusCode::INTERNAL_SERVER_ERROR, "read_failed", None),
            Self::StoredInvalid(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid_config",
                Some(("message", message)),
            ),
            Self::CannotDeleteConfig => (StatusCode::BAD_REQUEST, "cannot_delete_config", None),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
            Self::InvalidBody => (StatusCode::BAD_REQUEST, "invalid_body", None),
            Self::InvalidJson => (StatusCode::BAD_REQUEST, "invalid_json", None),
            Self::ConfigMustBeObject => (StatusCode::BAD_REQUEST, "config_must_be_object", None),
            Self::InvalidPath => (StatusCode::BAD_REQUEST, "invalid_path", None),
            Self::InvalidConfig(message) => (
                StatusCode::BAD_REQUEST,
                "invalid_config",
                Some(("message", message)),
            ),
            Self::ReadOnlyField(field) => (
                StatusCode::BAD_REQUEST,
                "read_only_field",
                Some(("field", field)),
            ),
            Self::Unprocessable(message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_config",
                Some(("message", message)),
            ),
            Self::WriteFailed(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "write_failed",
                Some(("message", message)),
            ),
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
}

impl fmt::Display for V8EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadFailed => f.write_str("read_failed"),
            Self::StoredInvalid(message) => write!(f, "invalid_config: {message}"),
            Self::CannotDeleteConfig => f.write_str("cannot_delete_config"),
            Self::NotFound => f.write_str("not_found"),
            Self::InvalidBody => f.write_str("invalid_body"),
            Self::InvalidJson => f.write_str("invalid_json"),
            Self::ConfigMustBeObject => f.write_str("config_must_be_object"),
            Self::InvalidPath => f.write_str("invalid_path"),
            Self::InvalidConfig(message) | Self::Unprocessable(message) => {
                write!(f, "invalid_config: {message}")
            }
            Self::ReadOnlyField(field) => write!(f, "read_only_field: {field}"),
            Self::WriteFailed(message) => write!(f, "write_failed: {message}"),
        }
    }
}

impl std::error::Error for V8EditError {}

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
