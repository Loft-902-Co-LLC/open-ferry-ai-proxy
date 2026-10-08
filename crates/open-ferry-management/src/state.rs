// Ported from CLIProxyAPI internal/api/handlers/management/handler.go
// (Handler, NewHandler, SetConfig, SetTokenStore, SetPostAuthPersistHook,
// SetLocalPassword), internal/api/server.go (NewServer's
// MANAGEMENT_PASSWORD lookup and hasManagementSecret) and
// internal/api/server_reload.go (UpdateClients' managementRoutesEnabled)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the management handlers share: the current config, the credential
//! manager, the model registry, the `MANAGEMENT_PASSWORD` secret, the
//! local management password, the trusted proxies, the failed-attempt
//! record and the HTTP clients for `api-call`; and, as the service sets
//! them, the credential store, the
//! [`CredentialSync`] that reaches the service, the config file's path,
//! the OAuth login sessions, the credential lock, the [`Observability`]
//! handles the log and usage routes read, and the [`ConfigWriter`] and
//! [`ConfigReload`] the routes that change the config use, and
//! open-ferry's own [`UpdateService`], which the dashboard API's update
//! routes read.
//!
//! A handler that writes credentials takes the store and the sync together
//! with `credential_store`; without them, as in a state made only with
//! [`ManagementState::new`], it answers 503
//! `{"error":"credential store unavailable"}` and changes nothing.
//!
//! A handler that changes the config saves it with the writer, as the
//! `config_write` module describes; without one it answers 503
//! `{"error":"config writer unavailable"}` and changes nothing.
//!
//! Deviations from upstream:
//! - The handlers read the config file at its path, and write it only
//!   through the writer, which the service makes for that path.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use axum::response::{IntoResponse, Response};
use http::StatusCode;
use open_ferry_core::auth::FileStore;
use open_ferry_core::config::Config;
use open_ferry_core::manager::Manager;
use open_ferry_core::observe::Observability;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::go::trim_space;
use open_ferry_update::UpdateService;

use crate::access::Attempts;
use crate::client_ip::TrustedProxies;
use crate::config_write::{ConfigReload, ConfigWriter};
use crate::credential_sync::CredentialSync;
use crate::proxy::Clients;
use crate::{json, oauth};

/// The environment variable that holds a management secret.
const PASSWORD_VAR: &str = "MANAGEMENT_PASSWORD";

/// The management API's state. Cloning it gives another handle to the same
/// state.
#[derive(Clone)]
pub struct ManagementState {
    inner: Arc<Inner>,
    parts: Arc<Parts>,
}

struct Inner {
    config: RwLock<Loaded>,
    manager: Manager,
    registry: Arc<ModelRegistry>,
    env_secret: Vec<u8>,
    trusted_proxies: TrustedProxies,
    attempts: Mutex<Attempts>,
    clients: Clients,
    oauth_sessions: oauth::Sessions,
    /// Upstream's `authStatusMu`.
    credential_lock: tokio::sync::Mutex<()>,
    /// Upstream's `mu`, as the handlers that change the config take it.
    config_write_lock: tokio::sync::Mutex<()>,
}

/// The config the handlers read, and the SHA-256 of the config file's
/// contents it was loaded from or last written as, in lowercase hex, when
/// known: a save of it is made only while the file still holds them.
struct Loaded {
    config: Arc<Config>,
    sha256: Option<String>,
}

/// What the builder methods set.
#[derive(Clone, Default)]
struct Parts {
    store: Option<Arc<FileStore>>,
    sync: Option<Arc<dyn CredentialSync>>,
    config_path: Option<PathBuf>,
    config_writer: Option<Arc<dyn ConfigWriter>>,
    config_reload: Option<Arc<dyn ConfigReload>>,
    observability: Observability,
    /// The local management password, or empty.
    local_password: Vec<u8>,
    /// Whether the local management password turns the API on, as it does
    /// until the first config reload.
    local_enables: Arc<AtomicBool>,
    /// open-ferry's own update checks, when the server runs them.
    updates: Option<UpdateService>,
    #[cfg(test)]
    latest_release_url: Option<String>,
}

impl ManagementState {
    /// State serving `config`'s settings for `manager`'s credentials and
    /// `registry`'s models. `management_password` is the value of
    /// `MANAGEMENT_PASSWORD`, if set: see [`management_password_from_env`].
    /// Leading and trailing white space is ignored; a password left empty
    /// counts as unset.
    pub fn new(
        config: Arc<Config>,
        manager: Manager,
        registry: Arc<ModelRegistry>,
        management_password: Option<OsString>,
    ) -> Self {
        let env_secret = management_password
            .map(|password| trim_space(&os_bytes(password)).to_vec())
            .unwrap_or_default();
        let trusted_proxies = TrustedProxies::new(&config.trusted_proxies);
        Self {
            inner: Arc::new(Inner {
                config: RwLock::new(Loaded {
                    config,
                    sha256: None,
                }),
                manager,
                registry,
                env_secret,
                trusted_proxies,
                attempts: Mutex::new(Attempts::default()),
                clients: Clients::default(),
                oauth_sessions: oauth::Sessions::default(),
                credential_lock: tokio::sync::Mutex::new(()),
                config_write_lock: tokio::sync::Mutex::new(()),
            }),
            parts: Arc::default(),
        }
    }

    /// Keeps credential files in `store`'s auth directory (upstream's
    /// `SetTokenStore`). Pass the store the service saves with, so the
    /// handlers follow it when a reload moves the auth directory.
    #[must_use]
    pub fn with_store(mut self, store: Arc<FileStore>) -> Self {
        Arc::make_mut(&mut self.parts).store = Some(store);
        self
    }

    /// Tells the running service about the credentials the handlers change
    /// through `sync` (upstream's `SetPostAuthPersistHook`).
    #[must_use]
    pub fn with_sync(mut self, sync: Arc<dyn CredentialSync>) -> Self {
        Arc::make_mut(&mut self.parts).sync = Some(sync);
        self
    }

    /// The path of the config file the proxy was started with, which the
    /// handlers read (upstream's `configFilePath`). They write it only
    /// through the [`ConfigWriter`].
    #[must_use]
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        Arc::make_mut(&mut self.parts).config_path = Some(path);
        self
    }

    /// Saves the config file through `writer` when a route changes the
    /// config. Give it the file at the path given to
    /// [`with_config_path`](Self::with_config_path).
    #[must_use]
    pub fn with_config_writer(mut self, writer: Arc<dyn ConfigWriter>) -> Self {
        Arc::make_mut(&mut self.parts).config_writer = Some(writer);
        self
    }

    /// Records that the config the state was made with was loaded from
    /// contents with the SHA-256 `sha256`, in hex, as
    /// [`Config::load_with_sha256`] gives it. A route that saves the config
    /// the handlers read then saves it only while the file still holds
    /// those contents, or the ones the last write here wrote, and answers
    /// 409 `{"error":"config_changed"}` otherwise, writing nothing (see the
    /// `config_write` module). Without it nothing is checked until a config
    /// is applied with [`set_loaded_config`](Self::set_loaded_config) or
    /// written here. Not upstream's.
    #[must_use]
    pub fn with_config_sha256(self, sha256: impl Into<String>) -> Self {
        self.loaded_mut().sha256 = Some(sha256.into());
        self
    }

    /// Has the running service load the config file again through
    /// `reload` once a route has saved it (upstream's
    /// `SetConfigReloadHook`).
    #[must_use]
    pub fn with_config_reload(mut self, reload: Arc<dyn ConfigReload>) -> Self {
        Arc::make_mut(&mut self.parts).config_reload = Some(reload);
        self
    }

    /// Gives the dashboard API's update routes `updates`, the server's
    /// update checks. Not upstream's: open-ferry's own updates.
    #[must_use]
    pub fn with_updates(mut self, updates: UpdateService) -> Self {
        Arc::make_mut(&mut self.parts).updates = Some(updates);
        self
    }

    /// The server's update checks, if it runs them.
    pub fn updates(&self) -> Option<&UpdateService> {
        self.parts.updates.as_ref()
    }

    /// Serves the logs, request logs and usage statistics of
    /// `observability` (upstream's `SetLogDirectory`, and the request logger
    /// and usage queue upstream reaches through globals).
    #[must_use]
    pub fn with_observability(mut self, observability: Observability) -> Self {
        Arc::make_mut(&mut self.parts).observability = observability;
        self
    }

    /// Accepts `password` as a management key from 127.0.0.1 and ::1
    /// (upstream's `SetLocalPassword`, which the command line's `-password`
    /// and the TUI's standalone mode set). A password that isn't empty also
    /// turns the API on, until the first config reload, as upstream's
    /// server does; a request still needs a management key in the config or
    /// `MANAGEMENT_PASSWORD`, without which it is refused with "remote
    /// management key not set", as upstream's is.
    #[must_use]
    pub fn with_local_password(mut self, password: &str) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.local_password = password.as_bytes().to_vec();
        parts.local_enables = Arc::new(AtomicBool::new(!password.is_empty()));
        self
    }

    /// Asks a test's server for the latest release instead of GitHub.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_latest_release_url(mut self, url: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.parts).latest_release_url = Some(url.into());
        self
    }

    /// Applies a reloaded config (upstream's `SetConfig`). The management
    /// key, `allow-remote` and the proxies take effect on the next request;
    /// `trusted-proxies` takes a restart, as upstream. The local management
    /// password no longer turns the API on. The SHA-256 a save checks the
    /// file for is kept as it was: apply a config loaded from the file with
    /// [`set_loaded_config`](Self::set_loaded_config).
    pub fn set_config(&self, config: Arc<Config>) {
        self.parts.local_enables.store(false, Ordering::Relaxed);
        self.loaded_mut().config = config;
    }

    /// [`set_config`](Self::set_config) for a config loaded from the config
    /// file, as the service applies one after a reload, with `sha256`, the
    /// SHA-256 of the contents it was loaded from, in hex: a route saves
    /// the config only while the file still holds them (see
    /// [`with_config_sha256`](Self::with_config_sha256)). `None` checks
    /// nothing. The two change together, so a save never pairs a config
    /// with the SHA-256 of other contents. Not upstream's.
    pub fn set_loaded_config(&self, config: Arc<Config>, sha256: Option<String>) {
        self.parts.local_enables.store(false, Ordering::Relaxed);
        *self.loaded_mut() = Loaded { config, sha256 };
    }

    /// The SHA-256, in lowercase hex, the config file must have for a
    /// route to save the config the handlers read, if known: that of the
    /// contents the config was loaded from or last written as.
    pub fn config_sha256(&self) -> Option<String> {
        self.loaded().sha256.clone()
    }

    /// The config the handlers read, with [`config_sha256`](Self::config_sha256).
    pub(crate) fn loaded_config(&self) -> (Arc<Config>, Option<String>) {
        let loaded = self.loaded();
        (Arc::clone(&loaded.config), loaded.sha256.clone())
    }

    fn loaded(&self) -> RwLockReadGuard<'_, Loaded> {
        self.inner
            .config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn loaded_mut(&self) -> RwLockWriteGuard<'_, Loaded> {
        self.inner
            .config
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Stops the OAuth logins in progress and waits for them to end; no
    /// login starts after this. The service calls it as it shuts down.
    pub async fn shutdown(&self) {
        self.inner.oauth_sessions.shutdown().await;
    }

    /// The current config.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.loaded().config)
    }

    pub(crate) fn manager(&self) -> &Manager {
        &self.inner.manager
    }

    /// The model registry.
    pub fn registry(&self) -> &ModelRegistry {
        &self.inner.registry
    }

    /// The directory the logs are in: the one the binary resolved at
    /// start, or else the config's, as the log routes read it.
    pub fn log_directory(&self) -> PathBuf {
        crate::log_dir::log_directory(self)
    }

    /// The trimmed `MANAGEMENT_PASSWORD`, or empty.
    pub(crate) fn env_secret(&self) -> &[u8] {
        &self.inner.env_secret
    }

    /// The local management password, or empty.
    pub(crate) fn local_password(&self) -> &[u8] {
        &self.parts.local_password
    }

    pub(crate) fn trusted_proxies(&self) -> &TrustedProxies {
        &self.inner.trusted_proxies
    }

    pub(crate) fn attempts(&self) -> MutexGuard<'_, Attempts> {
        self.inner
            .attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn clients(&self) -> &Clients {
        &self.inner.clients
    }

    /// Whether the management API serves requests: when the config has a
    /// management key or `MANAGEMENT_PASSWORD` is set, or until the first
    /// config reload when a local management password is (upstream's
    /// `managementRoutesEnabled`).
    pub fn available(&self) -> bool {
        !self.env_secret().is_empty()
            || !self.config().remote_management.secret_key.is_empty()
            || self.parts.local_enables.load(Ordering::Relaxed)
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "for the management routes that aren't ported yet")
)]
impl ManagementState {
    /// The credential store, if the service set one.
    pub(crate) fn store(&self) -> Option<&Arc<FileStore>> {
        self.parts.store.as_ref()
    }

    /// The way to the running service, if it set one.
    pub(crate) fn sync(&self) -> Option<&Arc<dyn CredentialSync>> {
        self.parts.sync.as_ref()
    }

    /// The store and the sync, which every handler that writes credentials
    /// needs; [`StoreUnavailable`] when either wasn't set.
    pub(crate) fn credential_store(&self) -> Result<CredentialStore, StoreUnavailable> {
        match (self.store(), self.sync()) {
            (Some(files), Some(sync)) => Ok(CredentialStore {
                files: Arc::clone(files),
                sync: Arc::clone(sync),
            }),
            _ => Err(StoreUnavailable),
        }
    }

    /// The config file's path, if the service set it. The handlers read
    /// it; they write it only through the writer.
    pub(crate) fn config_path(&self) -> Option<&Path> {
        self.parts.config_path.as_deref()
    }

    /// The config writer, if the service set one.
    pub(crate) fn config_writer(&self) -> Option<&Arc<dyn ConfigWriter>> {
        self.parts.config_writer.as_ref()
    }

    /// The way to reload the running service's config, if it set one.
    pub(crate) fn config_reload(&self) -> Option<&Arc<dyn ConfigReload>> {
        self.parts.config_reload.as_ref()
    }

    /// The lock that orders the handlers changing the config, as upstream's
    /// `mu` does: held from copying the config until the copy is saved and
    /// in place. Take it with `.lock().await`.
    pub(crate) fn config_write_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.inner.config_write_lock
    }

    /// The OAuth login sessions.
    pub(crate) fn oauth_sessions(&self) -> &oauth::Sessions {
        &self.inner.oauth_sessions
    }

    /// The lock that orders the handlers changing a credential, as
    /// upstream's `authStatusMu`: held from finding the credential until it
    /// is saved. It is an async lock, as the save runs on the blocking pool
    /// while it is held; take it with `.lock().await`. Only handlers take
    /// it, never the service, and they let it go before they wait for a
    /// [`CredentialSync`] call, as upstream lets `authStatusMu` go before
    /// its post-auth persist hook.
    pub(crate) fn credential_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.inner.credential_lock
    }

    pub(crate) fn observability(&self) -> &Observability {
        &self.parts.observability
    }

    /// Where to ask for the latest release: open-ferry's releases on
    /// GitHub, or a test's server.
    pub(crate) fn latest_release_url(&self) -> &str {
        #[cfg(test)]
        if let Some(url) = &self.parts.latest_release_url {
            return url;
        }
        crate::latest_version::LATEST_RELEASE_URL
    }
}

/// The credential store and the way to the running service, as
/// `credential_store` gives them.
#[derive(Clone)]
pub(crate) struct CredentialStore {
    /// Where the credential files are.
    pub(crate) files: Arc<FileStore>,
    /// The running service.
    pub(crate) sync: Arc<dyn CredentialSync>,
}

/// No credential store or sync was set: answers 503
/// `{"error":"credential store unavailable"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoreUnavailable;

impl std::fmt::Display for StoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("credential store unavailable")
    }
}

impl IntoResponse for StoreUnavailable {
    fn into_response(self) -> Response {
        json::error(StatusCode::SERVICE_UNAVAILABLE, &self.to_string())
    }
}

impl std::fmt::Debug for ManagementState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementState").finish_non_exhaustive()
    }
}

/// The value of `MANAGEMENT_PASSWORD`, if set, for
/// [`ManagementState::new`]. A set password turns the management API on
/// and allows remote clients, whatever the config says.
pub fn management_password_from_env() -> Option<OsString> {
    std::env::var_os(PASSWORD_VAR)
}

/// The bytes of an environment value, as Go reads them: as they are on
/// Unix, and as UTF-8 elsewhere, each unpaired surrogate replaced.
fn os_bytes(value: OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        value.into_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().into_owned().into_bytes()
    }
}
