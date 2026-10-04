// Ported from CLIProxyAPI internal/api/handlers/management/handler.go
// (Handler, NewHandler, SetConfig, SetTokenStore, SetPostAuthPersistHook)
// and internal/api/server.go (NewServer's MANAGEMENT_PASSWORD lookup)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the management handlers share: the current config, the credential
//! manager, the model registry, the `MANAGEMENT_PASSWORD` secret, the
//! trusted proxies, the failed-attempt record and the HTTP clients for
//! `api-call`; and, as the service sets them, the credential store, the
//! [`CredentialSync`] that reaches the service, the config file's path,
//! the OAuth login sessions and the credential lock.
//!
//! A handler that writes credentials takes the store and the sync together
//! with `credential_store`; without them, as in a state made only with
//! [`ManagementState::new`], it answers 503
//! `{"error":"credential store unavailable"}` and changes nothing.
//!
//! Deviations from upstream:
//! - The config file's path is only ever read: open-ferry never writes the
//!   config.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use axum::response::{IntoResponse, Response};
use http::StatusCode;
use open_ferry_core::auth::FileStore;
use open_ferry_core::config::Config;
use open_ferry_core::manager::Manager;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::go::trim_space;

use crate::access::Attempts;
use crate::client_ip::TrustedProxies;
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
    config: RwLock<Arc<Config>>,
    manager: Manager,
    registry: Arc<ModelRegistry>,
    env_secret: Vec<u8>,
    trusted_proxies: TrustedProxies,
    attempts: Mutex<Attempts>,
    clients: Clients,
    oauth_sessions: oauth::Sessions,
    /// Upstream's `authStatusMu`.
    credential_lock: tokio::sync::Mutex<()>,
}

/// What the builder methods set.
#[derive(Clone, Default)]
struct Parts {
    store: Option<Arc<FileStore>>,
    sync: Option<Arc<dyn CredentialSync>>,
    config_path: Option<PathBuf>,
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
                config: RwLock::new(config),
                manager,
                registry,
                env_secret,
                trusted_proxies,
                attempts: Mutex::new(Attempts::default()),
                clients: Clients::default(),
                oauth_sessions: oauth::Sessions::default(),
                credential_lock: tokio::sync::Mutex::new(()),
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
    /// handlers read but never write (upstream's `configFilePath`).
    #[must_use]
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        Arc::make_mut(&mut self.parts).config_path = Some(path);
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
    /// `trusted-proxies` takes a restart, as upstream.
    pub fn set_config(&self, config: Arc<Config>) {
        *self
            .inner
            .config
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// Stops the OAuth logins in progress and waits for them to end; no
    /// login starts after this. The service calls it as it shuts down.
    pub async fn shutdown(&self) {
        self.inner.oauth_sessions.shutdown().await;
    }

    /// The current config.
    pub(crate) fn config(&self) -> Arc<Config> {
        Arc::clone(
            &self
                .inner
                .config
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    pub(crate) fn manager(&self) -> &Manager {
        &self.inner.manager
    }

    pub(crate) fn registry(&self) -> &ModelRegistry {
        &self.inner.registry
    }

    /// The trimmed `MANAGEMENT_PASSWORD`, or empty.
    pub(crate) fn env_secret(&self) -> &[u8] {
        &self.inner.env_secret
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
    /// management key or `MANAGEMENT_PASSWORD` is set (upstream's
    /// `managementRoutesEnabled`).
    pub(crate) fn available(&self) -> bool {
        !self.env_secret().is_empty() || !self.config().remote_management.secret_key.is_empty()
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

    /// The config file's path, if the service set it. Only ever read.
    pub(crate) fn config_path(&self) -> Option<&Path> {
        self.parts.config_path.as_deref()
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
