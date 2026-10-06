//! What every request handler shares.

use std::sync::{Arc, PoisonError, RwLock};

use open_ferry_core::exec::Dispatcher;
use open_ferry_core::models::ModelCatalog;
use open_ferry_core::observe::Observability;
use open_ferry_core::observe::client_ip::TrustedProxies;

use crate::auth::PrincipalTags;
use crate::config::ServerConfig;
use crate::handlers::responses_ws::ServerToolCaches;

/// The template keys that put the server in safe mode
/// (internal/safemode/example_api_keys.go).
const EXAMPLE_API_KEYS: [&str; 3] = ["your-api-key-1", "your-api-key-2", "your-api-key-3"];

/// The settings, the [`Dispatcher`] that makes provider calls, the
/// [`ModelCatalog`] that says which providers serve a model, the
/// [`Observability`] handles, and what the server keeps for its clients.
/// Cloning is cheap.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
    observability: Arc<Observability>,
}

struct Inner {
    settings: RwLock<Arc<Settings>>,
    dispatcher: Arc<dyn Dispatcher>,
    catalog: Arc<dyn ModelCatalog>,
    principal_tags: PrincipalTags,
    tool_caches: ServerToolCaches,
    /// The proxies whose forwarded-address headers are believed, read once
    /// at start.
    trusted_proxies: TrustedProxies,
}

/// The config, with what is worked out from it.
#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) config: ServerConfig,
    /// The client keys, trimmed, without empty or repeated ones.
    pub(crate) keys: Vec<String>,
    /// Whether a client key is still a template value, which shuts the proxy
    /// routes.
    pub(crate) safe_mode: bool,
}

impl Settings {
    fn new(config: ServerConfig) -> Self {
        let mut keys: Vec<String> = Vec::new();
        for key in &config.api_keys {
            let key = key.trim();
            if !key.is_empty() && !keys.iter().any(|k| k == key) {
                keys.push(key.to_owned());
            }
        }
        let safe_mode = keys.iter().any(|k| EXAMPLE_API_KEYS.contains(&k.as_str()));
        Self {
            config,
            keys,
            safe_mode,
        }
    }
}

impl AppState {
    /// State for a server with `config`, making calls through `dispatcher`.
    pub fn new(
        config: ServerConfig,
        dispatcher: Arc<dyn Dispatcher>,
        catalog: Arc<dyn ModelCatalog>,
    ) -> Self {
        let trusted_proxies = TrustedProxies::new(&config.trusted_proxies);
        Self {
            inner: Arc::new(Inner {
                settings: RwLock::new(Arc::new(Settings::new(config))),
                dispatcher,
                catalog,
                principal_tags: PrincipalTags::default(),
                tool_caches: ServerToolCaches::default(),
                trusted_proxies,
            }),
            observability: Arc::default(),
        }
    }

    /// Gives each call the server makes the taps of `observability`'s
    /// request logger and usage statistics. Without it, no call is tapped.
    #[must_use]
    pub fn with_observability(mut self, observability: Observability) -> Self {
        self.observability = Arc::new(observability);
        self
    }

    /// Replaces the config. Requests that have started keep the old one.
    pub fn set_config(&self, config: ServerConfig) {
        let settings = Arc::new(Settings::new(config));
        *self
            .inner
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = settings;
    }

    pub(crate) fn settings(&self) -> Arc<Settings> {
        Arc::clone(
            &self
                .inner
                .settings
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Whether a client key is still a template value, so safe mode shuts
    /// the proxy routes.
    pub fn safe_mode(&self) -> bool {
        self.settings().safe_mode
    }

    /// The dispatcher, for a future that outlives the request.
    pub(crate) fn dispatcher_arc(&self) -> Arc<dyn Dispatcher> {
        Arc::clone(&self.inner.dispatcher)
    }

    pub(crate) fn catalog(&self) -> &dyn ModelCatalog {
        &*self.inner.catalog
    }

    /// What tags client keys as principals. It outlives config reloads.
    pub(crate) fn principal_tags(&self) -> &PrincipalTags {
        &self.inner.principal_tags
    }

    /// The tool calls and outputs the Responses WebSocket has seen, by
    /// principal and session.
    pub(crate) fn tool_caches(&self) -> &ServerToolCaches {
        &self.inner.tool_caches
    }

    /// The request logger and the usage statistics.
    pub(crate) fn observability(&self) -> &Observability {
        &self.observability
    }

    /// The proxies whose forwarded-address headers are believed.
    pub(crate) fn trusted_proxies(&self) -> &TrustedProxies {
        &self.inner.trusted_proxies
    }
}
