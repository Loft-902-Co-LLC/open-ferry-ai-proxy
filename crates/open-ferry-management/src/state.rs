// Ported from CLIProxyAPI internal/api/handlers/management/handler.go
// (Handler, NewHandler, SetConfig) and internal/api/server.go (NewServer's
// MANAGEMENT_PASSWORD lookup) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What the management handlers share: the current config, the credential
//! manager, the model registry, the `MANAGEMENT_PASSWORD` secret, the
//! trusted proxies, the failed-attempt record and the HTTP clients for
//! `api-call`.
//!
//! Deviations from upstream: none.

use std::ffi::OsString;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use open_ferry_core::config::Config;
use open_ferry_core::manager::Manager;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::go::trim_space;

use crate::access::Attempts;
use crate::client_ip::TrustedProxies;
use crate::proxy::Clients;

/// The environment variable that holds a management secret.
const PASSWORD_VAR: &str = "MANAGEMENT_PASSWORD";

/// The management API's state. Cloning it gives another handle to the same
/// state.
#[derive(Clone)]
pub struct ManagementState {
    inner: Arc<Inner>,
}

struct Inner {
    config: RwLock<Arc<Config>>,
    manager: Manager,
    registry: Arc<ModelRegistry>,
    env_secret: Vec<u8>,
    trusted_proxies: TrustedProxies,
    attempts: Mutex<Attempts>,
    clients: Clients,
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
            }),
        }
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
