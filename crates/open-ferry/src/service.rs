// Ported from CLIProxyAPI sdk/cliproxy/service_lifecycle.go (Run and
// Shutdown), service_auth.go (prepareCoreAuthForModelRegistration,
// completeModelRegistrationForAuth and applyCoreAuthRemoval),
// service_config.go (applyConfigRuntime and registerConfigAPIKeyAuths),
// service_executors.go (registerAvailableExecutors,
// registerExecutorForAuth and registerOpenAICompatProviderExecutor),
// builder.go (runtimeAuthSyncHook), the
// order of internal/watcher/synthesizer/config.go (Synthesize), and the
// auth dispatch of internal/watcher's clients.go and config_reload.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Serving the proxy.
//!
//! At start the credentials in the auth directory, the config's API keys
//! (Gemini, Claude, Codex and Vertex AI) and its OpenAI-compatible
//! providers' keys are registered with the credential manager, and each
//! one's models with the model registry. Each
//! OpenAI-compatible provider gets an executor of its own, keyed by its
//! provider key (`openai-compatible-<name>`), beside the baseline
//! `openai-compatibility` one. Token refresh runs in the background every
//! fifteen minutes. Then the server listens, serving the management API
//! beside the proxy, and a watcher follows the config file and the auth
//! directory:
//! - A config that changes is applied to the manager, the server, the
//!   management API and the executors; the API-key and OpenAI-compatible
//!   credentials are made again from it, each file credential takes its
//!   provider's `oauth-excluded-models` from it again, and every
//!   credential's models are registered again, as aliases and exclusions
//!   may have changed.
//! - An auth file that is added or changes is registered from the contents
//!   the watcher read; one that is removed is unregistered. An event for a
//!   file that is gone by the time it is applied unregisters its
//!   credential, so a credential the management API deleted isn't brought
//!   back by an event from before.
//!
//! A credential the management API saves, changes or removes is applied
//! by the same loop at once, as upstream's `runtimeAuthSyncHook` applies
//! it, and the API waits until it is. Once the loop has stopped, the API's
//! changes fail, and it answers 503.
//!
//! Changes are ordered as upstream's service orders them. Each one carries
//! a revision from one counter: the watcher takes its event's before it
//! reads the file, and the API's change takes one once it is saved. A
//! change at or below the last revision applied to its credential is
//! skipped, so the watcher's report of a file read before the API saved it
//! doesn't undo the API's change. A credential the API changed carries its
//! generation (see [`Auth::generation`]), and is refused once the manager
//! has changed the credential since, as by a token refresh, keeping the
//! newer one. The API is told a skipped or refused change was applied, as
//! upstream tells it.
//!
//! Credentials read from files or the config aren't saved back; the manager
//! saves those it changes itself, as after a refresh.
//!
//! Deviations from upstream:
//! - Only the Codex, Claude, Gemini, Vertex AI and OpenAI-compatible
//!   executors are registered, the native ones at start rather than as
//!   their first credential comes. Upstream gives a credential of a provider
//!   it has no executor for (such as `gemini-cli`, `aistudio` or
//!   `gemini-interactions`) an OpenAI-compatible executor keyed by that
//!   provider; here such a credential has no executor, and isn't served.
//! - Executors are made again on a reload only when a setting they use
//!   changed (for the native ones `proxy-url` or
//!   `claude.model-level-cooling`, and for the OpenAI-compatible ones
//!   `proxy-url` or `openai-compatibility`); upstream makes them again on
//!   every reload, which ends their WebSocket sessions. Remaking the Vertex
//!   AI executor drops the access tokens it cached.
//! - An OpenAI-compatible executor that no credential uses any more after a
//!   reload is unregistered; upstream keeps it.
//! - With an empty `host` the server listens on every IPv6 and IPv4
//!   interface, as Go does; an IPv6 `host` is bracketed, where upstream's
//!   address fails to parse.
//! - Changing `host`, `port` or `tls` takes a restart, as upstream; a reload
//!   logs that it was ignored.
//! - The auth directory is made absolute, as the watcher's paths are, so a
//!   credential file has the same `path` and ID whether it was found at
//!   start or reported by the watcher. Upstream keeps a relative directory
//!   relative, and so do its watcher's paths.
//! - On a reload, each file credential's excluded models are worked out
//!   again from the credential as registered and the new config, keeping
//!   its tokens and state. Upstream synthesizes every auth file again and
//!   updates the credentials that differ, so a change to a file that the
//!   watcher has yet to report is applied then, rather than with the
//!   report.
//! - The logs, the usage statistics and the cooldown state store take the
//!   config through the `observability` hooks once the credentials are
//!   loaded, at start as on a reload; P3 ports what is behind them. pprof,
//!   the discovery advertiser, the WebSocket gateway, plugins and Home
//!   aren't ported.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use open_ferry_core::auth::compat::OPENAI_COMPATIBILITY;
use open_ferry_core::auth::synthesizer::file::{
    apply_config_attributes, synthesize_auth_file, synthesize_file_auths,
};
use open_ferry_core::auth::synthesizer::{
    StableIdGenerator, SynthesisContext, synthesize_config_auths,
};
use open_ferry_core::auth::{Auth, FileStore, Status};
use open_ferry_core::config::{AuthFile, Config, ConfigWatcher, WatchEvent, next_revision};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::observe::Observability;
use open_ferry_core::registry::{ModelRegistry, RegistrationRules};
use open_ferry_management::{
    CredentialSync, ManagementState, SyncError, SyncFuture, management_password_from_env,
};
use open_ferry_providers::claude::ClaudeExecutor;
use open_ferry_providers::codex::CodexExecutor;
use open_ferry_providers::gemini::{GeminiExecutor, VertexExecutor};
use open_ferry_providers::openai_compat::OpenAiCompatExecutor;
use open_ferry_server::{AppState, ServerConfig, router_with};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::logging::LogLevel;
use crate::observability;
use crate::tls::{self, TlsListener, TlsPeer};

/// How often background refresh looks for tokens to renew.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long shutdown waits for open requests.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How many credential changes from the management API may wait for the
/// service loop.
const SYNC_QUEUE: usize = 32;

/// Serves until a shutdown signal, or until the server fails.
pub async fn run(
    config: Config,
    config_path: PathBuf,
    auth_dir: PathBuf,
    log_level: LogLevel,
) -> ExitCode {
    if let Err(error) = ensure_auth_dir(&auth_dir) {
        tracing::error!(
            "failed to create auth directory {}: {error}",
            auth_dir.display()
        );
        return ExitCode::FAILURE;
    }
    if config.has_example_api_keys() {
        tracing::error!(
            api_keys = %config.example_api_keys().join(","),
            "unsafe example API key configured; proxy API endpoints disabled until api-keys is updated"
        );
    }
    let config = Arc::new(config);
    let mut service = Service::new(
        Arc::clone(&config),
        config_path.clone(),
        auth_dir,
        log_level,
    );
    service.register_executors();
    service.load_file_auths();
    service.sync_config_auths();
    service.reconfigure_observability(None);
    if let Err(error) = service.manager.start_auto_refresh(AUTO_REFRESH_INTERVAL) {
        tracing::warn!("failed to start core auth auto-refresh: {error}");
    } else {
        tracing::info!("core auth auto-refresh started (interval=15m0s)");
    }

    let listener = match bind(&config.host, config.port) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!("failed to start HTTP server: {error}");
            return ExitCode::FAILURE;
        }
    };
    let tls_config = if config.tls.enable {
        match tls::load(&config.tls.cert, &config.tls.key) {
            Ok(tls_config) => Some(tls_config),
            Err(error) => {
                tracing::error!("failed to start HTTPS server: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let (stop, stopped) = watch::channel(false);
    let mut server = tokio::spawn(serve(listener, tls_config, service.app(), stopped));
    println!(
        "API server started successfully on: {}:{}",
        config.host, config.port
    );

    let mut events = match ConfigWatcher::start(&config_path, &config) {
        Ok((watcher, events)) => {
            service.watcher = Some(watcher);
            Some(events)
        }
        Err(error) => {
            tracing::error!("failed to create watcher: {error}");
            service.close_sync();
            shut_down(&service, &stop, server).await;
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("file watcher started for config and auth directory changes");

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::debug!("shutdown signal received, shutting down...");
                break;
            }
            result = &mut server => {
                service.manager.stop_auto_refresh();
                service.management.shutdown().await;
                open_ferry_core::manager::cooldown_store::flush(&service.manager);
                return exit_code(result);
            }
            event = next_event(&mut events) => match event {
                Some(event) => match service.handle(event, &config_path) {
                    Watching::Same => {}
                    Watching::Restarted(next) => events = Some(next),
                    Watching::Stopped => events = None,
                },
                None => {
                    tracing::warn!("file watcher stopped; config and auth changes are no longer followed");
                    events = None;
                }
            },
            Some(request) = service.sync_requests.recv() => service.apply_sync(request),
        }
    }
    service.close_sync();
    shut_down(&service, &stop, server).await
}

type Server = tokio::task::JoinHandle<io::Result<()>>;

/// Stops refresh, the management API's OAuth logins and the server, giving
/// open requests up to [`SHUTDOWN_TIMEOUT`], then saves the cooldowns.
async fn shut_down(service: &Service, stop: &watch::Sender<bool>, mut server: Server) -> ExitCode {
    service.manager.stop_auto_refresh();
    service.management.shutdown().await;
    let _ = stop.send(true);
    let code = match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut server).await {
        Ok(result) => exit_code(result),
        Err(_) => {
            tracing::warn!("open requests didn't finish within 30s; closing them");
            server.abort();
            ExitCode::SUCCESS
        }
    };
    open_ferry_core::manager::cooldown_store::flush(&service.manager);
    code
}

fn exit_code(result: Result<io::Result<()>, tokio::task::JoinError>) -> ExitCode {
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            tracing::error!("API server failed: {error}");
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::error!("API server stopped: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The watcher's next event; never, once there is no watcher.
async fn next_event(events: &mut Option<mpsc::Receiver<WatchEvent>>) -> Option<WatchEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// What an event did to the watcher.
enum Watching {
    /// Nothing.
    Same,
    /// It restarted, and sends its events on a new channel.
    Restarted(mpsc::Receiver<WatchEvent>),
    /// It stopped.
    Stopped,
}

/// A credential change from the management API, and where to say it was
/// applied.
struct SyncRequest {
    change: SyncChange,
    /// Taken once the change was saved (see [`next_revision`]).
    revision: u64,
    applied: oneshot::Sender<()>,
}

/// A credential change from the management API.
enum SyncChange {
    /// A credential to register or update.
    Upsert(Box<Auth>),
    /// An auth file written, with its contents.
    FileWritten(AuthFile),
    /// An auth file removed.
    FileRemoved(PathBuf),
}

/// The service's [`CredentialSync`], which the management API holds: each
/// change waits in a bounded queue for the service loop, which applies it
/// and says so. The loop never waits for a change itself.
///
/// A change takes its revision when the call is made, before its future is
/// polled: the API calls once the change is saved (upstream's
/// `DispatchPersistedAuthUpdateWithRevision`).
struct SyncSender {
    requests: mpsc::Sender<SyncRequest>,
}

impl SyncSender {
    /// Queues `change` and waits until the loop has applied it; fails once
    /// the loop has stopped.
    async fn send(&self, change: SyncChange, revision: u64) -> Result<(), SyncError> {
        let (applied, done) = oneshot::channel();
        let request = SyncRequest {
            change,
            revision,
            applied,
        };
        self.requests
            .send(request)
            .await
            .map_err(|_| SyncError::Stopped)?;
        done.await.map_err(|_| SyncError::Stopped)
    }
}

impl CredentialSync for SyncSender {
    fn upsert(&self, auth: Auth) -> SyncFuture<'_> {
        let revision = next_revision();
        Box::pin(self.send(SyncChange::Upsert(Box::new(auth)), revision))
    }

    fn file_written(&self, file: AuthFile) -> SyncFuture<'_> {
        let revision = next_revision();
        Box::pin(self.send(SyncChange::FileWritten(file), revision))
    }

    fn file_removed(&self, path: PathBuf) -> SyncFuture<'_> {
        let revision = next_revision();
        Box::pin(self.send(SyncChange::FileRemoved(path), revision))
    }
}

/// The credential manager, the model registry, the server and management
/// state, and what was registered from the config and the auth directory.
struct Service {
    config: Arc<Config>,
    auth_dir: PathBuf,
    log_level: LogLevel,
    store: Arc<FileStore>,
    manager: Manager,
    registry: Arc<ModelRegistry>,
    state: AppState,
    management: ManagementState,
    /// The log directory, the request logger and the usage statistics the
    /// server and the management API share.
    observability: Observability,
    watcher: Option<ConfigWatcher>,
    /// The IDs of the credentials made from config API keys.
    config_auths: BTreeSet<String>,
    /// The credential ID registered for each auth file.
    file_auths: HashMap<PathBuf, String>,
    /// The revision of the last change applied to each credential ID, kept
    /// once it is unregistered so an older change can't bring it back
    /// (upstream's `authRevisions`).
    revisions: HashMap<String, u64>,
    /// The provider keys OpenAI-compatible executors are registered for.
    compat_executors: BTreeSet<String>,
    /// The management API's credential changes, waiting for the loop.
    sync_requests: mpsc::Receiver<SyncRequest>,
}

impl Service {
    fn new(
        config: Arc<Config>,
        config_path: PathBuf,
        auth_dir: PathBuf,
        log_level: LogLevel,
    ) -> Self {
        let auth_dir = absolute_dir(auth_dir);
        let registry = Arc::new(ModelRegistry::new());
        let store = Arc::new(FileStore::new(&auth_dir));
        let manager = Manager::new(
            Settings::from(&*config),
            Arc::clone(&registry) as _,
            Some(Arc::clone(&store) as _),
        );
        let observability = observability::build(&config, &config_path);
        manager.set_error_events(observability.usage.error_events());
        let state = AppState::new(
            ServerConfig::from(&*config),
            Arc::new(manager.clone()),
            Arc::clone(&registry) as _,
        )
        .with_observability(observability.clone());
        let (requests, sync_requests) = mpsc::channel(SYNC_QUEUE);
        let management = ManagementState::new(
            Arc::clone(&config),
            manager.clone(),
            Arc::clone(&registry),
            management_password_from_env(),
        )
        .with_store(Arc::clone(&store))
        .with_sync(Arc::new(SyncSender { requests }))
        .with_config_path(config_path)
        .with_observability(observability.clone());
        Self {
            config,
            auth_dir,
            log_level,
            store,
            manager,
            registry,
            state,
            management,
            observability,
            watcher: None,
            config_auths: BTreeSet::new(),
            file_auths: HashMap::new(),
            revisions: HashMap::new(),
            compat_executors: BTreeSet::new(),
            sync_requests,
        }
    }

    /// Applies the config to the logs, the usage statistics and the
    /// cooldown store (see [`observability::reconfigure`]). `previous` is
    /// the config before, `None` at start.
    fn reconfigure_observability(&self, previous: Option<&Config>) {
        observability::reconfigure(
            &self.observability,
            self.log_level.file_log(),
            &self.manager,
            self.management.available(),
            previous,
            &self.config,
        );
    }

    /// The proxy's routes, with the management API's beside them.
    fn app(&self) -> axum::Router {
        let management = open_ferry_management::router(self.management.clone());
        router_with(self.state.clone(), management)
    }

    /// Registers the executors for the current config: Codex, Claude,
    /// Gemini, Vertex AI, and the OpenAI-compatible ones (see
    /// [`Self::register_compat_executors`]).
    fn register_executors(&mut self) {
        self.register_native_executors();
        self.register_compat_executors();
    }

    /// Registers the Codex, Claude, Gemini and Vertex AI executors for the
    /// current config.
    fn register_native_executors(&self) {
        let proxy_url = self.config.proxy_url.clone();
        self.register_codex_executor();
        self.manager.register_executor(Arc::new(
            ClaudeExecutor::new(proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _)
                .with_model_level_cooling(self.config.claude.model_level_cooling),
        ));
        self.manager.register_executor(Arc::new(
            GeminiExecutor::new(proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
        self.manager.register_executor(Arc::new(
            VertexExecutor::new(proxy_url)
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
    }

    /// Registers the Codex executor for the current config.
    fn register_codex_executor(&self) {
        self.manager.register_executor(Arc::new(
            CodexExecutor::new(self.config.proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
    }

    /// Registers OpenAI-compatible executors made for the current config,
    /// replacing those registered: the baseline `openai-compatibility` one,
    /// and one for each provider an enabled credential belongs to. Those no
    /// credential uses any more are unregistered.
    fn register_compat_executors(&mut self) {
        let providers = self.compat_providers();
        for provider in &providers {
            self.register_compat_executor(provider);
        }
        self.compat_executors.extend(providers);
        self.prune_compat_executors();
    }

    /// Unregisters the OpenAI-compatible executors no enabled credential
    /// uses any more, keeping the baseline one.
    fn prune_compat_executors(&mut self) {
        let providers = self.compat_providers();
        let manager = &self.manager;
        self.compat_executors.retain(|provider| {
            let used = providers.contains(provider);
            if !used {
                manager.unregister_executor(provider);
            }
            used
        });
    }

    /// The provider keys OpenAI-compatible executors are wanted for: the
    /// baseline `openai-compatibility` one, and that of each provider an
    /// enabled credential belongs to.
    fn compat_providers(&self) -> BTreeSet<String> {
        let mut providers = BTreeSet::from([OPENAI_COMPATIBILITY.to_owned()]);
        let auths = self.manager.list();
        providers.extend(auths.iter().filter_map(|auth| compat_provider(auth)));
        providers
    }

    /// Registers an OpenAI-compatible executor for `provider`, made for the
    /// current config.
    fn register_compat_executor(&self, provider: &str) {
        self.manager.register_executor(Arc::new(
            OpenAiCompatExecutor::new(provider.to_owned(), Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
    }

    /// Registers an OpenAI-compatible executor for `auth`'s provider, unless
    /// one is registered already or `auth` isn't an enabled OpenAI-compatible
    /// credential (upstream's `ensureExecutorsForAuth`).
    fn ensure_executor(&mut self, auth: &Auth) {
        if let Some(provider) = compat_provider(auth)
            && !self.compat_executors.contains(&provider)
        {
            self.register_compat_executor(&provider);
            self.compat_executors.insert(provider);
        }
    }

    fn synthesis_context(&self) -> SynthesisContext {
        let mut ctx = SynthesisContext::new(&self.auth_dir, Utc::now());
        ctx.oauth_excluded_models = self.config.oauth_excluded_models.clone();
        ctx
    }

    fn rules(&self) -> RegistrationRules {
        RegistrationRules::from(&*self.config)
    }

    /// Registers every credential file in the auth directory.
    fn load_file_auths(&mut self) {
        let rules = self.rules();
        for auth in synthesize_file_auths(&self.synthesis_context()) {
            let path = PathBuf::from(auth.attribute("path").unwrap_or_default());
            let id = auth.id.clone();
            if self.upsert(auth, &rules) && !path.as_os_str().is_empty() {
                self.file_auths.insert(path, id);
            }
        }
    }

    /// Registers a credential for each config API key (Gemini, Claude,
    /// Codex and Vertex AI) and each key of an enabled OpenAI-compatible
    /// provider, and unregisters those whose key is gone (upstream's
    /// `registerConfigAPIKeyAuths` and the watcher's diff of config
    /// credentials). An invalid weight anywhere leaves every credential as
    /// it was, as upstream checks all weights first.
    fn sync_config_auths(&mut self) {
        let ctx = self.synthesis_context();
        let mut ids = StableIdGenerator::new();
        let auths = match synthesize_config_auths(&self.config, &ctx, &mut ids) {
            Ok(auths) => auths,
            Err(error) => {
                tracing::warn!("failed to synthesize config API key auths: {error}");
                return;
            }
        };
        let rules = self.rules();
        let mut ids = BTreeSet::new();
        for auth in auths {
            ids.insert(auth.id.clone());
            self.upsert(auth, &rules);
        }
        for stale in self.config_auths.difference(&ids) {
            self.remove(stale);
        }
        self.config_auths = ids;
    }

    /// Registers or updates `auth`, once its provider has an executor, then
    /// its models (upstream's `prepareCoreAuthForModelRegistration` and
    /// `completeModelRegistrationForAuth`). Returns whether it is
    /// registered.
    ///
    /// A stale `auth` (see [`is_stale`]) leaves the registered credential as
    /// it is, and registers its models again.
    fn upsert(&mut self, mut auth: Auth, rules: &RegistrationRules) -> bool {
        self.ensure_executor(&auth);
        let existing = self.manager.get(&auth.id);
        if let Some(existing) = existing.as_deref()
            && is_stale(existing, &auth)
        {
            tracing::debug!(
                "skipping stale auth update for {}: incoming gen={}, existing gen={}",
                auth.id,
                auth.generation,
                existing.generation
            );
            self.registry.register_auth(existing, rules);
            self.manager.reconcile_registry_model_states(&existing.id);
            return true;
        }
        let (op, result) = match existing {
            Some(existing) => {
                auth.created_at = existing.created_at;
                if !is_disabled(&existing) && !is_disabled(&auth) {
                    auth.last_refreshed_at = existing.last_refreshed_at;
                    auth.next_refresh_after = existing.next_refresh_after;
                    if auth.model_states.is_empty() && !existing.model_states.is_empty() {
                        auth.model_states = existing.model_states.clone();
                    }
                }
                (
                    "update",
                    self.manager.update_unsaved(auth.clone()).map(|_| ()),
                )
            }
            None => (
                "register",
                self.manager.register_unsaved(auth.clone()).map(|_| ()),
            ),
        };
        let auth = match result {
            Ok(()) => self.manager.get(&auth.id).unwrap_or_else(|| Arc::new(auth)),
            Err(error) => {
                tracing::error!("failed to {op} auth {}: {error}", auth.id);
                match self.manager.get(&auth.id) {
                    Some(current) if !current.disabled => current,
                    _ => {
                        self.registry.unregister_client(&auth.id);
                        return false;
                    }
                }
            }
        };
        self.registry.register_auth(&auth, rules);
        self.manager.reconcile_registry_model_states(&auth.id);
        true
    }

    /// Whether a change to credential `id` at `revision` is newer than every
    /// one applied to it, recording it if so (the revision check of
    /// upstream's `handleAuthUpdates`). Revision zero is always applied.
    fn claim_revision(&mut self, id: &str, revision: u64) -> bool {
        if revision == 0 {
            return true;
        }
        match self.revisions.get_mut(id) {
            Some(last) if revision <= *last => {
                tracing::debug!(
                    "skipping stale auth update for {id}: rev {revision} <= processed {last}"
                );
                false
            }
            Some(last) => {
                *last = revision;
                true
            }
            None => {
                self.revisions.insert(id.to_owned(), revision);
                true
            }
        }
    }

    /// Unregisters credential `id` (upstream's `applyCoreAuthRemoval`).
    fn remove(&self, id: &str) {
        self.registry.unregister_client(id);
        self.manager.remove(id);
    }

    /// Applies a watcher event.
    fn handle(&mut self, event: WatchEvent, config_path: &Path) -> Watching {
        match event {
            WatchEvent::ConfigChanged(config) => return self.apply_config(config, config_path),
            WatchEvent::ConfigInvalid(error) => {
                tracing::error!("failed to reload config: {error}; keeping the current one");
            }
            WatchEvent::AuthAdded(file, revision) | WatchEvent::AuthChanged(file, revision) => {
                self.load_auth_file(&file, revision);
            }
            WatchEvent::AuthRemoved(path, revision) => self.remove_auth_file(&path, revision),
            _ => {}
        }
        Watching::Same
    }

    /// Applies a credential change from the management API, then says so,
    /// even when it was skipped as stale.
    fn apply_sync(&mut self, request: SyncRequest) {
        let revision = request.revision;
        match request.change {
            SyncChange::Upsert(auth) => {
                let path = PathBuf::from(auth.attribute("path").unwrap_or_default());
                let id = auth.id.clone();
                let rules = self.rules();
                if self.claim_revision(&id, revision)
                    && self.upsert(*auth, &rules)
                    && !path.as_os_str().is_empty()
                    && let Some(previous) = self.file_auths.insert(path, id.clone())
                    && previous != id
                {
                    self.remove(&previous);
                }
            }
            SyncChange::FileWritten(file) => self.load_auth_file(&file, revision),
            SyncChange::FileRemoved(path) => self.remove_auth_file(&path, revision),
        }
        // The change stands even if its handler has gone.
        let _ = request.applied.send(());
    }

    /// Takes no more credential changes from the management API, and
    /// applies those already waiting.
    fn close_sync(&mut self) {
        self.sync_requests.close();
        while let Ok(request) = self.sync_requests.try_recv() {
            self.apply_sync(request);
        }
    }

    /// Unregisters the credential registered for the auth file at `path`,
    /// unless a change after `revision` was applied to it.
    fn remove_auth_file(&mut self, path: &Path, revision: u64) {
        let Some(id) = self.file_auths.get(path).cloned() else {
            return;
        };
        if self.claim_revision(&id, revision) {
            self.file_auths.remove(path);
            self.remove(&id);
        }
    }

    /// Registers the credential in an auth file, from the contents the
    /// watcher read, unless a change after `revision` was applied to it. A
    /// file that is gone unregisters its credential instead: the event is
    /// older than the removal.
    fn load_auth_file(&mut self, file: &AuthFile, revision: u64) {
        let path = file.path.as_path();
        if matches!(path.try_exists(), Ok(false)) {
            self.remove_auth_file(path, revision);
            return;
        }
        let auth = match synthesize_auth_file(&self.synthesis_context(), path, &file.data) {
            Ok(auth) => auth,
            Err(error) => {
                tracing::warn!("skipping auth file {}: {error}", path.display());
                None
            }
        };
        // The credential in the file, else the one it unregisters.
        let id = match &auth {
            Some(auth) => Some(auth.id.clone()),
            None => self.file_auths.get(path).cloned(),
        };
        if let Some(id) = id
            && !self.claim_revision(&id, revision)
        {
            return;
        }
        let previous = self.file_auths.remove(path);
        let Some(auth) = auth else {
            if let Some(id) = previous {
                self.remove(&id);
            }
            return;
        };
        if let Some(previous) = previous
            && previous != auth.id
        {
            self.remove(&previous);
        }
        let id = auth.id.clone();
        let rules = self.rules();
        if self.upsert(auth, &rules) {
            self.file_auths.insert(path.to_owned(), id);
        }
    }

    /// Sets the attributes each file credential takes from the config
    /// again, keeping the rest, and updates those that changed (the file
    /// part of upstream's `reloadClients` and `refreshAuthState`, which
    /// rebuild the credentials of each provider whose
    /// `oauth-excluded-models` changed).
    fn apply_config_to_file_auths(&self) {
        let ctx = self.synthesis_context();
        let ids: BTreeSet<&String> = self.file_auths.values().collect();
        for id in ids {
            let Some(current) = self.manager.get(id) else {
                continue;
            };
            let mut auth = Auth::clone(&current);
            if apply_config_attributes(&ctx, &mut auth)
                && let Err(error) = self.manager.update_unsaved(auth)
            {
                tracing::error!("failed to update auth {id}: {error}");
            }
        }
    }

    /// Applies a reloaded config (upstream's `applyConfigRuntime`, with the
    /// watcher's credential diff).
    fn apply_config(&mut self, config: Arc<Config>, config_path: &Path) -> Watching {
        let previous = std::mem::replace(&mut self.config, config);
        let config = Arc::clone(&self.config);
        self.log_level.set_debug(config.debug);
        if (&previous.host, previous.port, &previous.tls)
            != (&config.host, config.port, &config.tls)
        {
            tracing::warn!("host, port and tls changes take effect after a restart");
        }
        self.manager.set_settings(Settings::from(&*config));
        self.state.set_config(ServerConfig::from(&*config));
        self.management.set_config(Arc::clone(&config));
        // What the executors do to Codex clients' requests before
        // translating them.
        let codex_clients_changed = previous.client.codex.optimize_multi_agent_v2
            != config.client.codex.optimize_multi_agent_v2
            || previous.codex.orphan_delegation_compatibility
                != config.codex.orphan_delegation_compatibility;
        if previous.proxy_url != config.proxy_url
            || previous.claude.model_level_cooling != config.claude.model_level_cooling
            || codex_clients_changed
        {
            self.register_native_executors();
        } else if previous != config {
            // The Codex executor follows the whole config.
            self.register_codex_executor();
        }
        // Made again before the credentials change, as upstream does, so no
        // credential of the new config is served by an executor of the old.
        if previous.proxy_url != config.proxy_url
            || previous.openai_compatibility != config.openai_compatibility
            || codex_clients_changed
        {
            self.register_compat_executors();
        }

        let mut watching = Watching::Same;
        match config.resolve_auth_dir().map(absolute_dir) {
            Ok(auth_dir) if auth_dir != self.auth_dir => {
                tracing::info!("auth directory changed to {}", auth_dir.display());
                if let Err(error) = ensure_auth_dir(&auth_dir) {
                    tracing::error!(
                        "failed to create auth directory {}: {error}",
                        auth_dir.display()
                    );
                }
                self.auth_dir = auth_dir;
                self.store.set_base_dir(&self.auth_dir);
                for (_, id) in std::mem::take(&mut self.file_auths) {
                    self.remove(&id);
                }
                // The new watcher reports every file in the new directory.
                self.watcher = None;
                match ConfigWatcher::start(config_path, &config) {
                    Ok((watcher, events)) => {
                        self.watcher = Some(watcher);
                        watching = Watching::Restarted(events);
                    }
                    Err(error) => {
                        tracing::error!("failed to restart watcher: {error}");
                        self.load_file_auths();
                        watching = Watching::Stopped;
                    }
                }
            }
            Ok(_) => {}
            Err(error) => tracing::error!("failed to resolve auth directory: {error}"),
        }

        // New providers get executors as their credentials are registered.
        self.sync_config_auths();
        self.prune_compat_executors();
        self.apply_config_to_file_auths();
        let rules = self.rules();
        for auth in self.manager.list() {
            self.registry.register_auth(&auth, &rules);
            self.manager.reconcile_registry_model_states(&auth.id);
        }
        self.reconfigure_observability(Some(&previous));
        tracing::info!("config reloaded");
        watching
    }
}

fn is_disabled(auth: &Auth) -> bool {
    auth.disabled || auth.status == Status::Disabled
}

/// Whether `incoming` is older than the registered `existing`: from an
/// earlier registration of its ID, or an earlier generation of it
/// (upstream's `isStaleCoreAuth`). Zero is no version, as for a credential
/// read from its file.
fn is_stale(existing: &Auth, incoming: &Auth) -> bool {
    (incoming.registration_epoch > 0 && incoming.registration_epoch < existing.registration_epoch)
        || (incoming.generation > 0 && incoming.generation < existing.generation)
}

/// The provider key of `auth`'s OpenAI-compatible executor, unless `auth`
/// is disabled or isn't an OpenAI-compatible credential.
fn compat_provider(auth: &Auth) -> Option<String> {
    if auth.disabled {
        return None;
    }
    auth.openai_compat_info().map(|(provider, _)| provider)
}

/// `dir` made absolute against the current directory, as the watcher makes
/// it; unchanged when that fails, as for an empty path.
fn absolute_dir(dir: PathBuf) -> PathBuf {
    std::path::absolute(&dir).unwrap_or(dir)
}

/// Creates the auth directory if needed, readable by its owner only
/// (upstream's `ensureAuthDir`).
fn ensure_auth_dir(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Listens on `host`:`port`; every interface when `host` is empty.
fn bind(host: &str, port: i64) -> io::Result<TcpListener> {
    let port = u16::try_from(port)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid port {port}")))?;
    let host = host.trim();
    if host.is_empty() {
        return bind_any(port);
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let address = format!("{host}:{port}");
    let std_listener = std::net::TcpListener::bind(&address)?;
    std_listener.set_nonblocking(true)?;
    TcpListener::from_std(std_listener)
}

/// Listens on every interface: IPv6 with IPv4 mapped where the system has
/// IPv6, and IPv4 otherwise.
fn bind_any(port: u16) -> io::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};
    let dual = || -> io::Result<Socket> {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
        socket.set_only_v6(false)?;
        #[cfg(not(windows))]
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
        Ok(socket)
    };
    let socket = match dual() {
        Ok(socket) => socket,
        Err(_) => {
            let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
            #[cfg(not(windows))]
            socket.set_reuse_address(true)?;
            socket.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
            socket
        }
    };
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Serves `app` until `stopped` turns true.
async fn serve(
    listener: TcpListener,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    app: axum::Router,
    mut stopped: watch::Receiver<bool>,
) -> io::Result<()> {
    let stop = async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };
    match tls_config {
        Some(tls_config) => {
            let listener = TlsListener::new(listener, tls_config)?;
            axum::serve(
                listener,
                tls::with_peer_addr(app).into_make_service_with_connect_info::<TlsPeer>(),
            )
            .with_graceful_shutdown(stop)
            .await
        }
        None => {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stop)
            .await
        }
    }
}

/// Resolves on Ctrl-C, or on SIGTERM on Unix.
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use open_ferry_core::auth::weight::MAX_WEIGHT;
    use open_ferry_core::exec::{ErrorKind, ExecError, Options, Request, Response, StreamResponse};
    use open_ferry_core::executor::ProviderExecutor;

    use super::*;

    /// A service over `dir` with `extra` config, its executors registered.
    fn service(dir: &Path, extra: &str) -> Service {
        let config = Config::parse(format!("auth-dir: '{}'\n{extra}", dir.display())).unwrap();
        let mut service = Service::new(
            Arc::new(config),
            dir.join("config.yaml"),
            dir.to_owned(),
            LogLevel::detached(),
        );
        service.register_executors();
        service
    }

    /// What the watcher reports for `path` with its current contents.
    fn auth_file(path: &Path) -> AuthFile {
        AuthFile {
            path: path.to_owned(),
            data: std::fs::read(path).unwrap().into(),
        }
    }

    /// The watcher's report that `path` was added, with its current
    /// contents, taking the next revision.
    fn added(path: &Path) -> WatchEvent {
        WatchEvent::AuthAdded(auth_file(path), next_revision())
    }

    /// The watcher's report that `file` changed, taking the next revision.
    fn changed(file: AuthFile) -> WatchEvent {
        WatchEvent::AuthChanged(file, next_revision())
    }

    /// The watcher's report that `path` was removed, taking the next
    /// revision.
    fn removed(path: &Path) -> WatchEvent {
        WatchEvent::AuthRemoved(path.to_owned(), next_revision())
    }

    fn codex_file(dir: &Path, name: &str, extra: &str) -> PathBuf {
        let path = dir.join(name);
        let body =
            format!(r#"{{"type":"codex","email":"a@example.com","access_token":"fake"{extra}}}"#);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[tokio::test]
    async fn bind_checks_the_port_and_brackets_ipv6() {
        let error = bind("127.0.0.1", 70_000).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bind("127.0.0.1", -1).is_err());

        let listener = bind(" 127.0.0.1 ", 0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        let listener = bind("", 0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_unspecified());
        // Without brackets "::1:0" doesn't parse as an address; a machine
        // without IPv6 fails to bind instead.
        match bind("::1", 0) {
            Ok(listener) => assert!(listener.local_addr().unwrap().is_ipv6()),
            Err(error) => assert_ne!(error.kind(), io::ErrorKind::InvalidInput),
        }
    }

    #[tokio::test]
    async fn auth_files_are_registered_without_being_saved_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let written = std::fs::read(&path).unwrap();
        let mut service = service(dir.path(), "");

        service.load_file_auths();
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        let id = auths[0].id.clone();
        assert_eq!(auths[0].provider, "codex");
        assert!(!service.registry.models_for_client(&id).is_empty());
        assert_eq!(service.file_auths.get(&path), Some(&id));

        // The watcher reports the same file again, then a change to it.
        service.handle(added(&path), Path::new(""));
        codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let rewritten = std::fs::read(&path).unwrap();
        service.handle(changed(auth_file(&path)), Path::new(""));
        assert_eq!(service.manager.list().len(), 1);
        assert_eq!(service.manager.get(&id).unwrap().prefix, "team");
        assert_ne!(written, rewritten);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            rewritten,
            "the file was rewritten"
        );

        service.handle(removed(&path), Path::new(""));
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.file_auths.is_empty());
    }

    #[tokio::test]
    async fn an_auth_file_that_stops_parsing_is_unregistered() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        service.handle(added(&path), Path::new(""));
        let id = service.manager.list()[0].id.clone();

        let broken = AuthFile {
            path: path.clone(),
            data: Arc::from(&b"{not json"[..]),
        };
        service.handle(changed(broken), Path::new(""));
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.file_auths.is_empty());
    }

    #[tokio::test]
    async fn auth_events_load_the_contents_the_watcher_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let checked = auth_file(&path);
        // A write that doesn't parse lands while the event waits.
        std::fs::write(&path, "{not json").unwrap();
        let mut service = service(dir.path(), "");
        service.handle(
            WatchEvent::AuthAdded(checked, next_revision()),
            Path::new(""),
        );
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].prefix, "team");
        assert_eq!(service.file_auths.get(&path), Some(&auths[0].id));
    }

    #[tokio::test]
    async fn an_event_for_a_file_that_is_gone_unregisters_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        let stale = auth_file(&path);
        service.handle(
            WatchEvent::AuthAdded(stale.clone(), next_revision()),
            Path::new(""),
        );
        let id = service.manager.list()[0].id.clone();

        // The file is deleted before the watcher's report of a change to it
        // is applied.
        std::fs::remove_file(&path).unwrap();
        service.handle(changed(stale.clone()), Path::new(""));
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.file_auths.is_empty());
        service.handle(WatchEvent::AuthAdded(stale, next_revision()), Path::new(""));
        assert!(service.manager.list().is_empty());
    }

    /// The service's sync over a queue of one, which `service` now takes
    /// its requests from.
    fn sync_sender(service: &mut Service) -> Arc<SyncSender> {
        let (requests, receiver) = mpsc::channel(1);
        service.sync_requests = receiver;
        Arc::new(SyncSender { requests })
    }

    /// Sends a change through `sync` with `send`, applies it as the loop
    /// would, and returns what the sender was answered.
    async fn apply_next<F, Fut>(
        service: &mut Service,
        sync: &Arc<SyncSender>,
        send: F,
    ) -> Result<(), SyncError>
    where
        F: FnOnce(Arc<SyncSender>) -> Fut,
        Fut: Future<Output = Result<(), SyncError>> + Send + 'static,
    {
        let sent = tokio::spawn(send(Arc::clone(sync)));
        let request = service.sync_requests.recv().await.unwrap();
        service.apply_sync(request);
        sent.await.unwrap()
    }

    #[tokio::test]
    async fn management_changes_are_applied_by_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(dir.path(), "");
        let sync = sync_sender(&mut service);

        // A written file is registered from the contents sent.
        let path = codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let file = auth_file(&path);
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.file_written(file).await
        });
        sent.await.unwrap();
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        let id = auths[0].id.clone();
        assert_eq!(auths[0].prefix, "team");
        assert!(!service.registry.models_for_client(&id).is_empty());
        assert_eq!(service.file_auths.get(&path), Some(&id));

        // A credential upserted for the same file under another ID takes
        // its place.
        let mut renamed = Auth::clone(&service.manager.get(&id).unwrap());
        renamed.id = "codex-renamed".into();
        renamed.prefix = "other".into();
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.upsert(renamed).await
        });
        sent.await.unwrap();
        assert!(service.manager.get(&id).is_none());
        assert_eq!(
            service.manager.get("codex-renamed").unwrap().prefix,
            "other"
        );
        assert_eq!(
            service.file_auths.get(&path).map(String::as_str),
            Some("codex-renamed")
        );

        // A removed file's credential is unregistered.
        std::fs::remove_file(&path).unwrap();
        let removed = path.clone();
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.file_removed(removed).await
        });
        sent.await.unwrap();
        assert!(service.manager.list().is_empty());
        assert!(service.file_auths.is_empty());
    }

    #[tokio::test]
    async fn management_changes_fail_once_the_loop_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        let sync = sync_sender(&mut service);

        // A change taken but dropped unapplied.
        let sent = tokio::spawn({
            let sync = Arc::clone(&sync);
            async move { sync.file_removed(PathBuf::from("gone.json")).await }
        });
        drop(service.sync_requests.recv().await.unwrap());
        assert_eq!(sent.await.unwrap(), Err(SyncError::Stopped));

        // A change still waiting when the loop stops is applied; later ones
        // fail.
        let (applied, done) = oneshot::channel();
        let change = SyncChange::FileWritten(auth_file(&path));
        let revision = next_revision();
        sync.requests
            .try_send(SyncRequest {
                change,
                revision,
                applied,
            })
            .unwrap_or_else(|_| panic!("the queue is full"));
        service.close_sync();
        assert_eq!(done.await, Ok(()));
        assert_eq!(service.manager.list().len(), 1);
        let error = sync.file_removed(path).await.unwrap_err();
        assert_eq!(error, SyncError::Stopped);
        assert_eq!(error.status().as_u16(), 503);
        assert_eq!(service.manager.list().len(), 1);
    }

    /// The access token in credential `id`'s metadata.
    fn access_token(service: &Service, id: &str) -> String {
        let auth = service.manager.get(id).unwrap();
        let token = auth.metadata.get("access_token").and_then(|v| v.as_str());
        token.unwrap_or_default().to_owned()
    }

    /// Not upstream's test, for its revisions (sdk/cliproxy/service_auth.go,
    /// handleAuthUpdates): the watcher's report of a file it read before
    /// the management API disabled the credential, applied after the
    /// disable, is skipped, and the credential stays disabled with no
    /// models.
    #[tokio::test]
    async fn a_watcher_report_from_before_a_disable_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        let sync = sync_sender(&mut service);
        service.handle(added(&path), Path::new(""));
        let id = service.manager.list()[0].id.clone();
        assert!(!model_ids(&service, &id).is_empty());

        // The watcher reads the enabled file; its report waits.
        let queued = changed(auth_file(&path));

        // The API disables the credential, saving its file, as the status
        // route does.
        let mut auth = Auth::clone(&service.manager.get(&id).unwrap());
        auth.disabled = true;
        auth.status = Status::Disabled;
        let disabled = service.manager.update(auth).unwrap().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["disabled"], true);
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.upsert(Auth::clone(&disabled)).await
        });
        sent.await.unwrap();
        assert!(model_ids(&service, &id).is_empty());

        service.handle(queued, Path::new(""));
        assert!(service.manager.get(&id).unwrap().disabled);
        assert!(model_ids(&service, &id).is_empty());

        // A report read after the save is applied.
        service.handle(changed(auth_file(&path)), Path::new(""));
        assert!(service.manager.get(&id).unwrap().disabled);
        assert_eq!(service.revisions.len(), 1);
    }

    /// Not upstream's test, for its revisions: a removal the watcher saw
    /// before the management API wrote the file again doesn't unregister
    /// the credential written, and a management change older than one
    /// applied is skipped too, its handler still told it was applied.
    #[tokio::test]
    async fn stale_removals_and_management_changes_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        let sync = sync_sender(&mut service);
        service.handle(added(&path), Path::new(""));
        let id = service.manager.list()[0].id.clone();

        let queued = removed(&path);
        codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let file = auth_file(&path);
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.file_written(file).await
        });
        sent.await.unwrap();
        service.handle(queued, Path::new(""));
        assert_eq!(service.manager.get(&id).unwrap().prefix, "team");
        assert_eq!(service.file_auths.get(&path), Some(&id));

        // An upsert that took its revision before a newer report was
        // applied.
        let mut older = Auth::clone(&service.manager.get(&id).unwrap());
        older.prefix = "older".into();
        let (applied, done) = oneshot::channel();
        let request = SyncRequest {
            change: SyncChange::Upsert(Box::new(older)),
            revision: next_revision(),
            applied,
        };
        service.handle(changed(auth_file(&path)), Path::new(""));
        service.apply_sync(request);
        assert_eq!(done.await, Ok(()));
        assert_eq!(service.manager.get(&id).unwrap().prefix, "team");
    }

    type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    /// A Codex executor that serves nothing, and refreshes a credential to
    /// the access token `refreshed`.
    struct Refresher;

    impl ProviderExecutor for Refresher {
        fn id(&self) -> &str {
            "codex"
        }

        fn execute(
            &self,
            _: Arc<Auth>,
            _: Request,
            _: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not served")) })
        }

        fn execute_stream(
            &self,
            _: Arc<Auth>,
            _: Request,
            _: Options,
        ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
            Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not served")) })
        }

        fn count_tokens(
            &self,
            _: Arc<Auth>,
            _: Request,
            _: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not served")) })
        }

        fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
            Box::pin(async move {
                let mut auth = Auth::clone(&auth);
                auth.metadata
                    .insert("access_token".into(), "refreshed".into());
                Ok(auth)
            })
        }
    }

    /// Not upstream's test, for its generations (sdk/cliproxy/service_auth.go,
    /// prepareCoreAuthForModelRegistration and isStaleCoreAuth): a
    /// management field change whose sync waits while the credential's
    /// token is refreshed is refused when it comes, its handler still told
    /// it was applied, and the refreshed token is kept.
    #[tokio::test]
    async fn a_management_change_older_than_a_refresh_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", r#","refresh_token":"rt""#);
        let mut service = service(dir.path(), "");
        service.manager.register_executor(Arc::new(Refresher));
        let sync = sync_sender(&mut service);
        service.handle(added(&path), Path::new(""));
        let id = service.manager.list()[0].id.clone();

        // The API changes a field, saving the file; its sync waits.
        let mut auth = Auth::clone(&service.manager.get(&id).unwrap());
        auth.metadata.insert("note".into(), "changed".into());
        let changed = service.manager.update(auth).unwrap().unwrap();
        let held = tokio::spawn({
            let sync = Arc::clone(&sync);
            async move { sync.upsert(Auth::clone(&changed)).await }
        });
        let request = service.sync_requests.recv().await.unwrap();

        let refreshed = service.manager.force_refresh(&id).await.unwrap();
        assert_eq!(access_token(&service, &id), "refreshed");
        assert!(refreshed.generation > request_generation(&request));

        service.apply_sync(request);
        assert_eq!(held.await.unwrap(), Ok(()));
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(access_token(&service, &id), "refreshed");
        assert_eq!(
            auth.metadata.get("note").and_then(|v| v.as_str()),
            Some("changed")
        );
        assert!(!model_ids(&service, &id).is_empty());
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "refreshed");
    }

    /// The generation of the credential an upsert request carries.
    fn request_generation(request: &SyncRequest) -> u64 {
        match &request.change {
            SyncChange::Upsert(auth) => auth.generation,
            _ => panic!("expected an upsert"),
        }
    }

    /// Not upstream's: `is_stale` decides as upstream's `isStaleCoreAuth`
    /// (sdk/cliproxy/service_auth.go), which v8.0.10 doesn't test.
    #[test]
    fn stale_credentials_are_older_by_epoch_or_generation() {
        let existing = Auth {
            registration_epoch: 2,
            generation: 5,
            ..Auth::default()
        };
        let at = |registration_epoch, generation| Auth {
            registration_epoch,
            generation,
            ..Auth::default()
        };
        assert!(is_stale(&existing, &at(1, 9)));
        assert!(is_stale(&existing, &at(2, 4)));
        assert!(is_stale(&existing, &at(0, 4)));
        assert!(!is_stale(&existing, &at(2, 5)));
        // Generations are compared whatever the epoch, as upstream does.
        assert!(is_stale(&existing, &at(3, 1)));
        assert!(!is_stale(&existing, &at(3, 5)));
        assert!(!is_stale(&existing, &at(0, 0)));
        assert!(!is_stale(&existing, &at(2, 0)));
    }

    /// `path` relative to the current directory, when they share a root.
    fn relative_to_cwd(path: &Path) -> Option<PathBuf> {
        let cwd = std::env::current_dir().ok()?;
        let mut base = cwd.components().peekable();
        let mut target = path.components().peekable();
        let mut shared = 0;
        while let (Some(a), Some(b)) = (base.peek(), target.peek()) {
            if a != b {
                break;
            }
            base.next();
            target.next();
            shared += 1;
        }
        if shared == 0 {
            return None;
        }
        let mut out: PathBuf = base.map(|_| std::path::Component::ParentDir).collect();
        out.extend(target);
        Some(out)
    }

    #[tokio::test]
    async fn a_relative_auth_dir_gives_one_id_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let Some(relative) = relative_to_cwd(dir.path()) else {
            return;
        };
        assert!(relative.is_relative());
        codex_file(dir.path(), "codex-a.json", "");
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.yaml");
        let yaml = format!("auth-dir: '{}'\n", relative.display());
        std::fs::write(&config_path, &yaml).unwrap();
        let config = Arc::new(Config::parse(yaml).unwrap());

        let mut service = Service::new(
            Arc::clone(&config),
            config_path.clone(),
            relative,
            LogLevel::detached(),
        );
        service.register_executors();
        service.load_file_auths();
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        let id = auths[0].id.clone();

        // The watcher reports the file under its absolute directory.
        let (watcher, mut events) = ConfigWatcher::start(&config_path, &config).unwrap();
        assert_eq!(watcher.auth_dir(), service.auth_dir);
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let WatchEvent::AuthAdded(file, revision) = event else {
            panic!("expected an added auth file, got {event:?}");
        };
        let path = file.path.clone();
        service.handle(WatchEvent::AuthAdded(file, revision), &config_path);
        assert_eq!(service.manager.list().len(), 1);
        assert!(service.manager.get(&id).is_some());
        assert_eq!(service.file_auths.get(&path), Some(&id));

        service.handle(removed(&path), &config_path);
        assert!(service.manager.list().is_empty());
        assert!(service.file_auths.is_empty());

        // A reload naming the same directory keeps the watcher.
        let same = Arc::new(Config::parse(std::fs::read_to_string(&config_path).unwrap()).unwrap());
        service.watcher = Some(watcher);
        assert!(matches!(
            service.handle(WatchEvent::ConfigChanged(same), &config_path),
            Watching::Same
        ));
    }

    #[tokio::test]
    async fn config_api_keys_follow_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let keys =
            "codex-api-key:\n  - api-key: sk-test-1\n    base-url: https://codex.example.com\n";
        let mut service = service(dir.path(), keys);
        service.sync_config_auths();
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        let id = auths[0].id.clone();
        assert_eq!(auths[0].provider, "codex");
        assert!(!service.registry.models_for_client(&id).is_empty());

        // An unchanged reload keeps the credential.
        let same = Config::parse(format!("auth-dir: '{}'\n{keys}", dir.path().display())).unwrap();
        assert!(matches!(
            service.handle(WatchEvent::ConfigChanged(Arc::new(same)), Path::new("")),
            Watching::Same
        ));
        assert!(service.manager.get(&id).is_some());

        // A reload without the key removes it.
        let none = Config::parse(format!("auth-dir: '{}'\n", dir.path().display())).unwrap();
        service.handle(WatchEvent::ConfigChanged(Arc::new(none)), Path::new(""));
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.config_auths.is_empty());
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "a file was saved"
        );
    }

    /// Reloads `service` with a config over `dir` holding `extra`.
    fn reload(service: &mut Service, dir: &Path, extra: &str) {
        let config = Config::parse(format!("auth-dir: '{}'\n{extra}", dir.display())).unwrap();
        service.handle(WatchEvent::ConfigChanged(Arc::new(config)), Path::new(""));
    }

    /// The models a Codex file credential with no exclusions serves.
    fn codex_models() -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let models = model_ids(&service, &id);
        assert!(models.len() > 1, "{models:?}");
        models
    }

    /// Not upstream's test, for its reload (internal/watcher/config_reload.go,
    /// reloadConfig, and clients.go, reloadClients): a file credential whose
    /// provider's `oauth-excluded-models` a reload removes gets its models
    /// back, keeping its own exclusions, its token and its state, and the
    /// reverse.
    #[tokio::test]
    async fn file_credentials_follow_oauth_excluded_models_reloads() {
        let all = codex_models();
        let own = all[0].clone();
        let excluded = "oauth-excluded-models:\n  codex:\n    - \"*\"\n";
        let dir = tempfile::tempdir().unwrap();
        let extra = format!(r#","excluded_models":["{own}"]"#);
        let path = codex_file(dir.path(), "codex-a.json", &extra);
        let mut service = service(dir.path(), excluded);
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let auth = service.manager.get(&id).unwrap();
        let both = format!("*,{own}");
        assert_eq!(auth.attribute("excluded_models"), Some(both.as_str()));
        assert!(model_ids(&service, &id).is_empty());

        // Runtime state the file doesn't hold.
        let mut auth = Auth::clone(&auth);
        auth.metadata
            .insert("access_token".into(), "runtime".into());
        auth.status_message = "cooling".into();
        let retry = Utc::now() + chrono::Duration::hours(1);
        auth.next_retry_after = Some(retry);
        service.manager.update_unsaved(auth).unwrap();

        reload(&mut service, dir.path(), "");
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(auth.attribute("excluded_models"), Some(own.as_str()));
        let models = model_ids(&service, &id);
        assert_eq!(models.len(), all.len() - 1, "{models:?}");
        assert!(!models.contains(&own), "{models:?}");
        assert_eq!(access_token(&service, &id), "runtime");
        assert_eq!(auth.status_message, "cooling");
        assert_eq!(auth.next_retry_after, Some(retry));
        assert_eq!(service.file_auths.get(&path), Some(&id));

        reload(&mut service, dir.path(), excluded);
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(auth.attribute("excluded_models"), Some(both.as_str()));
        assert!(model_ids(&service, &id).is_empty());
        assert_eq!(access_token(&service, &id), "runtime");
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("runtime"),
            "the file was saved"
        );
    }

    /// Not upstream's test, for the same reload: a credential with no
    /// exclusions of its own loses its excluded-models attributes once the
    /// config has none for its provider, and one whose exclusions a reload
    /// doesn't change is left as it is.
    #[tokio::test]
    async fn a_reload_clears_excluded_models_from_the_config() {
        let own = codex_models().swap_remove(0);
        let dir = tempfile::tempdir().unwrap();
        codex_file(dir.path(), "codex-a.json", "");
        let config = format!("oauth-excluded-models:\n  codex:\n    - {own}\n");
        let mut service = service(dir.path(), &config);
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(auth.attribute("excluded_models"), Some(own.as_str()));
        assert!(!model_ids(&service, &id).contains(&own));

        let other = "oauth-excluded-models:\n  claude:\n    - x\n";
        reload(&mut service, dir.path(), other);
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(auth.attribute("excluded_models"), None);
        assert_eq!(auth.attribute("excluded_models_hash"), None);
        assert!(model_ids(&service, &id).contains(&own));

        let another = "oauth-excluded-models:\n  claude:\n    - y\n";
        reload(&mut service, dir.path(), another);
        let unchanged = service.manager.get(&id).unwrap();
        assert_eq!(unchanged.generation, auth.generation);
    }

    /// A config with a Gemini key at `gemini_url` and a Vertex AI key at
    /// `vertex_url`, serving `gemini-2.5-flash` as `g1` and `gemini-2.5-pro`
    /// as `v1`.
    fn google_keys(gemini_url: &str, vertex_url: &str) -> String {
        format!(
            "gemini-api-key:\n  - api-key: gm-test\n    base-url: {gemini_url}\n    models:\n      - name: gemini-2.5-flash\n        alias: g1\nvertex-api-key:\n  - api-key: vx-test\n    base-url: {vertex_url}\n    models:\n      - name: gemini-2.5-pro\n        alias: v1\n"
        )
    }

    /// A Vertex AI service-account file is served by the Vertex AI executor.
    /// Its account holds no key: registering it doesn't read one.
    #[tokio::test]
    async fn vertex_service_account_files_are_registered() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"type":"vertex","project_id":"proxy-test","location":"us-central1","email":"sa@proxy-test.iam.gserviceaccount.com","service_account":{"type":"service_account","client_email":"sa@proxy-test.iam.gserviceaccount.com"}}"#;
        std::fs::write(dir.path().join("vertex-proxy-test.json"), body).unwrap();
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].provider, "vertex");
        assert!(!model_ids(&service, &auths[0].id).is_empty());
        assert_eq!(executor_id(&service, "vertex").as_deref(), Some("vertex"));
    }

    #[tokio::test]
    async fn gemini_and_vertex_api_keys_follow_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let keys = google_keys("https://gemini.example.test", "https://vertex.example.test");
        let mut service = service(dir.path(), &keys);
        service.sync_config_auths();
        let mut auths = service.manager.list();
        auths.sort_by(|a, b| a.provider.cmp(&b.provider));
        let providers: Vec<&str> = auths.iter().map(|auth| auth.provider.as_str()).collect();
        assert_eq!(providers, ["gemini", "vertex"]);
        assert_eq!(auths[0].attribute("api_key"), Some("gm-test"));
        assert_eq!(model_ids(&service, &auths[0].id), ["g1"]);
        assert_eq!(model_ids(&service, &auths[1].id), ["v1"]);
        assert_eq!(service.config_auths.len(), 2);

        // A reload without them removes both.
        let none = Config::parse(format!("auth-dir: '{}'\n", dir.path().display())).unwrap();
        service.handle(WatchEvent::ConfigChanged(Arc::new(none)), Path::new(""));
        for auth in &auths {
            assert!(service.manager.get(&auth.id).is_none());
            assert!(model_ids(&service, &auth.id).is_empty());
        }
        assert!(service.config_auths.is_empty());
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "a file was saved"
        );
    }

    /// An `openai-compatibility` config with `entries`.
    fn compat(entries: &[String]) -> String {
        format!("openai-compatibility:\n{}", entries.concat())
    }

    /// A provider entry named `name` with one key, serving `model` as
    /// `alias`.
    fn compat_entry(name: &str, base_url: &str, model: &str, alias: &str) -> String {
        format!(
            "  - name: {name}\n    base-url: {base_url}\n    api-key-entries:\n      - api-key: sk-{name}\n    models:\n      - name: {model}\n        alias: {alias}\n"
        )
    }

    /// The IDs of the models registered for credential `id`.
    fn model_ids(service: &Service, id: &str) -> Vec<String> {
        let models = service.registry.models_for_client(id);
        models.into_iter().map(|model| model.id).collect()
    }

    /// The ID of the executor registered for `provider`.
    fn executor_id(service: &Service, provider: &str) -> Option<String> {
        let executor = service.manager.executor(provider)?;
        Some(executor.id().to_owned())
    }

    /// Ports `TestRegisterAvailableExecutors` of CLIProxyAPI
    /// sdk/cliproxy/service_executor_registration_test.go (v8.0.10, MIT)
    /// for the executors ported: Codex, Claude, Gemini, Vertex AI and the
    /// baseline OpenAI-compatible one. The plugin executor and the other
    /// providers' aren't ported.
    #[tokio::test]
    async fn registers_the_available_executors() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path(), "");
        for provider in [
            "codex",
            "claude",
            "gemini",
            "vertex",
            "openai-compatibility",
        ] {
            assert_eq!(executor_id(&service, provider).as_deref(), Some(provider));
        }
        assert_eq!(executor_id(&service, "openai-compatible-x"), None);
    }

    /// Ports `TestRegisterExecutorForAuth_OpenAICompatUsesNamespacedProviderKey`
    /// of the same file, with Codex as the native provider, as Kimi isn't
    /// ported: an OpenAI-compatible provider named after a native one gets
    /// an executor of its own, whichever credential comes first. Executors
    /// can't be told apart by type here; the native one is the one
    /// registered at start, and the other answers to the namespaced key.
    #[tokio::test]
    async fn compat_executor_uses_namespaced_provider_key() {
        for compat_first in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut service = service(dir.path(), "");
            let native_executor = service.manager.executor("codex").unwrap();
            let native = Auth {
                id: "native-codex".into(),
                provider: "codex".into(),
                ..Auth::default()
            };
            let mut compat = Auth {
                id: "compat-codex".into(),
                provider: "openai-compatibility".into(),
                label: "codex".into(),
                ..Auth::default()
            };
            compat
                .attributes
                .insert("compat_name".into(), "codex".into());
            compat
                .attributes
                .insert("provider_key".into(), "codex".into());
            let mut auths = vec![native, compat];
            if compat_first {
                auths.reverse();
            }
            let rules = service.rules();
            for auth in auths {
                assert!(service.upsert(auth, &rules));
            }

            let resolved = service.manager.executor("codex").unwrap();
            assert!(Arc::ptr_eq(&resolved, &native_executor), "{compat_first}");
            assert_eq!(
                executor_id(&service, "openai-compatible-codex").as_deref(),
                Some("openai-compatible-codex"),
                "{compat_first}"
            );
        }
    }

    #[tokio::test]
    async fn disabled_compat_credentials_get_no_executor() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(dir.path(), "");
        let mut auth = Auth {
            id: "compat-off".into(),
            provider: "openai-compatible-off".into(),
            disabled: true,
            ..Auth::default()
        };
        auth.attributes.insert("compat_name".into(), "off".into());
        service.upsert(auth, &service.rules());
        assert_eq!(executor_id(&service, "openai-compatible-off"), None);
    }

    #[tokio::test]
    async fn openai_compat_providers_follow_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let reload = |service: &mut Service, extra: &str| {
            let text = format!("auth-dir: '{}'\n{extra}", dir.path().display());
            let config = Arc::new(Config::parse(text).unwrap());
            service.handle(WatchEvent::ConfigChanged(config), Path::new(""))
        };
        let alpha = |model: &str, alias: &str| {
            compat_entry("alpha", "https://alpha.example.test/v1", model, alias)
        };
        let beta = compat_entry("beta", "https://beta.example.test/v1", "up-b", "b");

        let first = compat(&[alpha("up-1", "a1")]);
        let mut service = service(dir.path(), &first);
        service.sync_config_auths();
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        let alpha_id = auths[0].id.clone();
        assert!(
            alpha_id.starts_with("openai-compatibility:alpha:"),
            "{alpha_id}"
        );
        assert_eq!(auths[0].provider, "openai-compatible-alpha");
        assert_eq!(
            auths[0].attribute("base_url"),
            Some("https://alpha.example.test/v1")
        );
        assert_eq!(model_ids(&service, &alpha_id), ["a1"]);
        let alpha_executor = service.manager.executor("openai-compatible-alpha").unwrap();
        assert_eq!(alpha_executor.id(), "openai-compatible-alpha");

        // An unchanged reload keeps the credential and the executor.
        assert!(matches!(reload(&mut service, &first), Watching::Same));
        assert!(service.manager.get(&alpha_id).is_some());
        let same = service.manager.executor("openai-compatible-alpha").unwrap();
        assert!(Arc::ptr_eq(&same, &alpha_executor));

        // Changed models keep the credential, register the new models, and
        // make the executor again for the new config; a new entry gets a
        // credential and an executor.
        reload(&mut service, &compat(&[alpha("up-2", "a2"), beta.clone()]));
        assert!(service.manager.get(&alpha_id).is_some());
        assert_eq!(model_ids(&service, &alpha_id), ["a2"]);
        let remade = service.manager.executor("openai-compatible-alpha").unwrap();
        assert!(!Arc::ptr_eq(&remade, &alpha_executor));
        let beta_auth = service
            .manager
            .list()
            .into_iter()
            .find(|auth| auth.provider == "openai-compatible-beta")
            .unwrap();
        assert_eq!(model_ids(&service, &beta_auth.id), ["b"]);
        assert_eq!(
            executor_id(&service, "openai-compatible-beta").as_deref(),
            Some("openai-compatible-beta")
        );
        assert_eq!(service.config_auths.len(), 2);

        // A removed entry takes its credential, models and executor with it.
        reload(&mut service, &compat(std::slice::from_ref(&beta)));
        assert!(service.manager.get(&alpha_id).is_none());
        assert!(model_ids(&service, &alpha_id).is_empty());
        assert_eq!(executor_id(&service, "openai-compatible-alpha"), None);
        assert!(service.manager.get(&beta_auth.id).is_some());

        // So does a disabled one; the baseline executor stays.
        let disabled = beta.replace("  - name: beta\n", "  - name: beta\n    disabled: true\n");
        reload(&mut service, &compat(&[disabled]));
        assert!(service.manager.list().is_empty());
        assert!(service.config_auths.is_empty());
        assert_eq!(executor_id(&service, "openai-compatible-beta"), None);
        assert_eq!(
            executor_id(&service, "openai-compatibility").as_deref(),
            Some("openai-compatibility")
        );

        // An invalid weight anywhere leaves the credentials as they were, as
        // upstream checks every weight before making any. The config's
        // loader rejects one, so it is set after.
        reload(&mut service, &compat(std::slice::from_ref(&beta)));
        assert_eq!(service.config_auths.len(), 1);
        let entries = compat(&[alpha("up-1", "a1"), beta.clone()]);
        let text = format!("auth-dir: '{}'\n{entries}", dir.path().display());
        let mut invalid = Config::parse(text).unwrap();
        invalid.openai_compatibility[1].api_key_entries[0].weight = Some(MAX_WEIGHT + 1);
        service.handle(WatchEvent::ConfigChanged(Arc::new(invalid)), Path::new(""));
        assert_eq!(service.manager.list().len(), 1);
        assert!(service.manager.get(&beta_auth.id).is_some());
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "a file was saved"
        );
    }

    /// A client's request reaches an OpenAI-compatible provider through the
    /// server, under the model's alias, before and after a reload.
    mod compat_requests {
        use std::net::SocketAddr;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        use axum::body::Bytes;
        use axum::http::{HeaderMap, Uri};
        use open_ferry_core::config::{Config, WatchEvent};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::watch;

        use super::super::serve;
        use super::{compat, compat_entry, service};

        const ANSWER: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"up-1","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;

        /// What the provider was sent: the path, the `Authorization` header
        /// and the body.
        type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

        /// A provider on a 127.0.0.1 ephemeral port that answers every
        /// request with [`ANSWER`].
        async fn provider() -> (String, Seen) {
            let seen = Seen::default();
            let record = Arc::clone(&seen);
            let app =
                axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
                    let record = Arc::clone(&record);
                    async move {
                        let authorization = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let body = String::from_utf8_lossy(&body).into_owned();
                        record
                            .lock()
                            .unwrap()
                            .push((uri.path().to_owned(), authorization, body));
                        ([("content-type", "application/json")], ANSWER)
                    }
                });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            (format!("http://{addr}/v1"), seen)
        }

        /// Sends `method path` with the client key and `body`, and returns
        /// the status and body of the answer.
        pub(super) async fn send(
            addr: SocketAddr,
            method: &str,
            path: &str,
            body: &str,
        ) -> (u16, String) {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            let status = head.split(' ').nth(1).unwrap().parse().unwrap();
            (status, body.to_owned())
        }

        pub(super) fn chat(model: &str) -> String {
            format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"hello"}}]}}"#)
        }

        #[tokio::test]
        async fn requests_reach_the_provider() {
            let dir = tempfile::tempdir().unwrap();
            let (base_url, seen) = provider().await;
            let config = |model: &str, alias: &str| {
                let entry = compat_entry("alpha", &base_url, model, alias);
                format!("api-keys: ['client-key']\n{}", compat(&[entry]))
            };
            let mut service = service(dir.path(), &config("up-1", "a1"));
            service.sync_config_auths();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (_stop, stopped) = watch::channel(false);
            tokio::spawn(serve(listener, None, service.app(), stopped));

            let (status, models) = send(addr, "GET", "/v1/models", "").await;
            assert_eq!(status, 200, "{models}");
            assert!(models.contains(r#""id":"a1""#), "{models}");

            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("a1")).await;
            assert_eq!((status, body.as_str()), (200, ANSWER));
            let want = (
                "/v1/chat/completions".to_owned(),
                "Bearer sk-alpha".to_owned(),
                chat("up-1"),
            );
            assert_eq!(seen.lock().unwrap().as_slice(), [want]);

            // After a reload the new alias is served, and the old one isn't.
            let text = format!(
                "auth-dir: '{}'\n{}",
                dir.path().display(),
                config("up-2", "a2")
            );
            let reloaded = Arc::new(Config::parse(text).unwrap());
            service.handle(WatchEvent::ConfigChanged(reloaded), Path::new(""));
            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("a2")).await;
            assert_eq!((status, body.as_str()), (200, ANSWER));
            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("a1")).await;
            assert_ne!(status, 200, "{body}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[1].2, chat("up-2"));
        }
    }

    /// A client's request reaches Gemini and Vertex AI through the server,
    /// with the config's keys.
    mod google_requests {
        use std::sync::{Arc, Mutex};

        use axum::http::{HeaderMap, Uri};
        use tokio::net::TcpListener;
        use tokio::sync::watch;

        use super::super::serve;
        use super::compat_requests::{chat, send};
        use super::{google_keys, service};

        const ANSWER: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;

        /// What the provider was sent: the path and query, and the
        /// `x-goog-api-key` header.
        type Seen = Arc<Mutex<Vec<(String, String)>>>;

        /// A provider on a 127.0.0.1 ephemeral port that answers every
        /// request with [`ANSWER`].
        async fn provider() -> (String, Seen) {
            let seen = Seen::default();
            let record = Arc::clone(&seen);
            let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
                let record = Arc::clone(&record);
                async move {
                    let key = headers
                        .get("x-goog-api-key")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    record.lock().unwrap().push((uri.to_string(), key));
                    ([("content-type", "application/json")], ANSWER)
                }
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            (format!("http://{addr}"), seen)
        }

        #[tokio::test]
        async fn requests_reach_gemini_and_vertex() {
            let dir = tempfile::tempdir().unwrap();
            let (gemini_url, gemini) = provider().await;
            let (vertex_url, vertex) = provider().await;
            let config = format!(
                "api-keys: ['client-key']\n{}",
                google_keys(&gemini_url, &vertex_url)
            );
            let mut service = service(dir.path(), &config);
            service.sync_config_auths();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (_stop, stopped) = watch::channel(false);
            tokio::spawn(serve(listener, None, service.app(), stopped));

            let (status, models) = send(addr, "GET", "/v1/models", "").await;
            assert_eq!(status, 200, "{models}");
            assert!(models.contains(r#""id":"g1""#), "{models}");
            assert!(models.contains(r#""id":"v1""#), "{models}");

            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("g1")).await;
            assert_eq!(status, 200, "{body}");
            let want = (
                "/v1beta/models/gemini-2.5-flash:generateContent".to_owned(),
                "gm-test".to_owned(),
            );
            assert_eq!(gemini.lock().unwrap().as_slice(), [want]);

            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("v1")).await;
            assert_eq!(status, 200, "{body}");
            let want = (
                "/v1/publishers/google/models/gemini-2.5-pro:generateContent".to_owned(),
                "vx-test".to_owned(),
            );
            assert_eq!(vertex.lock().unwrap().as_slice(), [want]);
        }
    }

    /// A client's turn on the Responses WebSocket reaches Codex over Codex's
    /// own WebSocket, for a Codex API key with websockets on; the session's
    /// socket to Codex closes when the client's does.
    mod codex_websocket {
        use std::time::Duration;

        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio::net::TcpListener;
        use tokio::sync::{mpsc, watch};
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        use super::super::serve;
        use super::service;

        const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_up","status":"in_progress","output":[]}}"#;
        const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_up","status":"completed","output":[{"id":"msg_up","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi from codex"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
        const TURN: &str = r#"{"type":"response.create","model":"gpt-5.5","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}"#;

        /// What Codex saw.
        #[derive(Debug, PartialEq)]
        enum Seen {
            /// A handshake's path and `Authorization`.
            Handshake(String, String),
            /// A message.
            Message(String),
            /// The end of a connection.
            Closed,
        }

        /// A Codex Responses WebSocket on a 127.0.0.1 ephemeral port that
        /// answers each message with [`CREATED`] and [`COMPLETED`]. Returns
        /// its URL and what it sees.
        async fn codex() -> (String, mpsc::UnboundedReceiver<Seen>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (seen, events) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let handshake = seen.clone();
                        let callback = move |request: &Request, response: Response| {
                            let authorization = request
                                .headers()
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned();
                            let path = request.uri().path().to_owned();
                            let _ = handshake.send(Seen::Handshake(path, authorization));
                            Ok(response)
                        };
                        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, callback).await
                        else {
                            return;
                        };
                        while let Some(Ok(message)) = ws.next().await {
                            match message {
                                Message::Text(text) => {
                                    let _ = seen.send(Seen::Message(text.as_str().to_owned()));
                                    for event in [CREATED, COMPLETED] {
                                        let _ = ws.send(Message::text(event)).await;
                                    }
                                }
                                Message::Close(_) => break,
                                _ => {}
                            }
                        }
                        let _ = seen.send(Seen::Closed);
                    });
                }
            });
            (url, events)
        }

        /// The next thing Codex saw, or a failed test after a while.
        async fn next(events: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
            tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("Codex saw nothing")
                .expect("Codex stopped")
        }

        #[tokio::test]
        async fn a_turn_reaches_codex_over_its_websocket() {
            let dir = tempfile::tempdir().unwrap();
            let (base_url, mut seen) = codex().await;
            let config = format!(
                "api-keys: ['client-key']\ncodex-api-key:\n  - api-key: sk-codex\n    base-url: {base_url}\n    websockets: true\n    models: [{{name: gpt-5.5, alias: gpt-5.5}}]\n"
            );
            let mut service = service(dir.path(), &config);
            service.sync_config_auths();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (_stop, stopped) = watch::channel(false);
            tokio::spawn(serve(listener, None, service.app(), stopped));

            let mut request = format!("ws://{addr}/v1/responses")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("authorization", "Bearer client-key".parse().unwrap());
            let (mut client, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            client.send(Message::text(TURN)).await.unwrap();
            let mut answer = Vec::new();
            loop {
                let message = tokio::time::timeout(Duration::from_secs(10), client.next())
                    .await
                    .expect("no answer")
                    .expect("the client's socket ended")
                    .unwrap();
                if let Message::Text(text) = message {
                    let done = text.contains(r#""type":"response.completed""#);
                    answer.push(text.as_str().to_owned());
                    if done {
                        break;
                    }
                }
            }
            assert!(
                answer.iter().any(|event| event.contains("hi from codex")),
                "{answer:?}"
            );

            assert_eq!(
                next(&mut seen).await,
                Seen::Handshake("/responses".into(), "Bearer sk-codex".into())
            );
            let Seen::Message(sent) = next(&mut seen).await else {
                panic!("Codex wasn't sent the turn");
            };
            assert!(sent.contains(r#""type":"response.create""#), "{sent}");
            assert!(sent.contains("hello"), "{sent}");

            // The session keeps its socket to Codex until the client goes.
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                seen.try_recv().is_err(),
                "Codex saw more before the client went"
            );
            client.close(None).await.unwrap();
            assert_eq!(next(&mut seen).await, Seen::Closed);
            assert!(seen.try_recv().is_err(), "Codex saw more");
        }
    }

    /// The management API as the binary serves it, over TCP.
    ///
    /// Ports the management parts of CLIProxyAPI
    /// internal/api/server_test.go (v8.0.10, MIT):
    /// - `TestManagementResponseExposesPluginSupportHeaderForCORS`, without
    ///   its `X-CPA-SUPPORT-PLUGIN` check: the plugin host isn't ported and
    ///   the header isn't sent.
    /// - `TestExampleAPIKeySafeModeShowsWarningAndKeepsManagement`, without
    ///   its warning page and control panel checks, as neither is ported,
    ///   and with the credential list in place of the unported config
    ///   route.
    /// - `TestNewServerAppliesTrustedProxyConfiguration`, through what the
    ///   management API makes of a client's address.
    /// - `TestManagementUsageRequiresManagementAuthAndPopsArray`, with the
    ///   service's own usage queue, which it turns on as `run` does while
    ///   the management API is available.
    ///
    /// `TestHomeEnabledHidesManagementEndpointsAndControlPanel`,
    /// `TestManagementPluginsRouteRegistered` and
    /// `TestOAuthCallbackRouteSkipsManagementKeyMiddleware` are dropped:
    /// Home mode and plugins aren't ported, and the OAuth test completes a
    /// plugin's login session. The main server's callback pages are tested
    /// by `oauth_callback_pages_are_served_beside_the_proxy`.
    mod management {
        use std::fmt::Write as _;
        use std::net::SocketAddr;
        use std::path::Path;
        use std::sync::Arc;

        use axum::body::Bytes;
        use open_ferry_core::config::{Config, WatchEvent};
        use open_ferry_core::observe::usage;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::watch;

        use super::super::{Service, serve, shut_down};
        use super::service;

        const KEYED: &str = "remote-management:\n  secret-key: test-secret\n";
        const LIST: &str = "/v0/management/auth-files";

        /// A response, read.
        struct Answer {
            status: u16,
            head: String,
            body: String,
        }

        impl Answer {
            /// The value of header `name`, if there is one.
            fn header(&self, name: &str) -> Option<&str> {
                self.head.lines().skip(1).find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name).then(|| value.trim())
                })
            }
        }

        /// Serves `service` as `run` does, on a 127.0.0.1 ephemeral port,
        /// until the sender is dropped or sends true.
        async fn start(service: &Service) -> (SocketAddr, watch::Sender<bool>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (stop, stopped) = watch::channel(false);
            tokio::spawn(serve(listener, None, service.app(), stopped));
            (addr, stop)
        }

        /// Sends `method path` with `headers` and no body, and reads the
        /// answer.
        async fn fetch(
            addr: SocketAddr,
            method: &str,
            path: &str,
            headers: &[(&str, &str)],
        ) -> Answer {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
            for (name, value) in headers {
                let _ = write!(request, "{name}: {value}\r\n");
            }
            request.push_str("Connection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(
                !head.to_ascii_lowercase().contains("transfer-encoding"),
                "{head}"
            );
            Answer {
                status: head.split(' ').nth(1).unwrap().parse().unwrap(),
                head: head.to_owned(),
                body: body.to_owned(),
            }
        }

        /// The config in `dir` with `extra`, as the watcher reports it.
        fn config(dir: &Path, extra: &str) -> WatchEvent {
            let text = format!("auth-dir: '{}'\n{extra}", dir.display());
            WatchEvent::ConfigChanged(Arc::new(Config::parse(text).unwrap()))
        }

        #[tokio::test]
        async fn management_is_served_beside_the_proxy() {
            let dir = tempfile::tempdir().unwrap();
            let extra = format!("api-keys: ['client-key']\n{KEYED}");
            let service = service(dir.path(), &extra);
            let (addr, _stop) = start(&service).await;
            let origin = ("Origin", "http://127.0.0.1:5173");

            // A management answer carries CORS headers, and exposes the
            // build headers.
            let answer = fetch(addr, "GET", LIST, &[origin]).await;
            assert_eq!(answer.status, 401, "{}", answer.body);
            assert_eq!(answer.body, r#"{"error":"missing management key"}"#);
            assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
            let exposed = answer.header("access-control-expose-headers").unwrap();
            let exposed: Vec<_> = exposed.split(',').map(str::trim).collect();
            for name in ["X-CPA-VERSION", "X-CPA-COMMIT", "X-CPA-BUILD-DATE"] {
                assert!(exposed.contains(&name), "{name}: {exposed:?}");
                assert!(answer.header(name).is_some(), "{name}");
            }

            // The key is the management key; the client keys don't apply.
            let key = ("Authorization", "Bearer test-secret");
            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert!(answer.body.starts_with(r#"{"files":[],"observed_at":""#));
            assert_eq!(
                answer.header("content-type"),
                Some("application/json; charset=utf-8")
            );
            let client = ("Authorization", "Bearer client-key");
            let answer = fetch(addr, "GET", LIST, &[client]).await;
            assert_eq!(answer.status, 401);
            assert_eq!(answer.body, r#"{"error":"invalid management key"}"#);
            assert_eq!(fetch(addr, "GET", "/v1/models", &[key]).await.status, 401);
            let answer = fetch(addr, "GET", "/v1/models", &[client]).await;
            assert_eq!(answer.status, 200);

            // Without trusted proxies, a forwarded address is ignored.
            let forwarded = ("X-Forwarded-For", "203.0.113.5");
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);

            // Unported management routes answer an empty 404, other
            // unknown paths the server's 404; CORS answers OPTIONS.
            let unported = "/v0/management/usage-statistics-enabled";
            let answer = fetch(addr, "PUT", unported, &[key]).await;
            assert_eq!((answer.status, answer.body.as_str()), (404, ""));
            assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
            let answer = fetch(addr, "GET", "/v0/other", &[key]).await;
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (404, "404 page not found")
            );
            let answer = fetch(addr, "OPTIONS", LIST, &[origin]).await;
            assert_eq!((answer.status, answer.body.as_str()), (204, ""));
        }

        /// Ports TestManagementUsageRequiresManagementAuthAndPopsArray: the
        /// usage queue is behind the management key, takes records oldest
        /// first as a JSON array, and the old `usage` route is gone.
        #[tokio::test]
        async fn usage_queue_needs_the_management_key_and_pops_an_array() {
            let dir = tempfile::tempdir().unwrap();
            let service = service(dir.path(), KEYED);
            let queue = &service.observability.usage;
            usage::reconfigure(queue, None, &service.config, service.management.available());
            queue.enqueue(Bytes::from_static(br#"{"id":1}"#));
            queue.enqueue(Bytes::from_static(br#"{"id":2}"#));
            let (addr, _stop) = start(&service).await;
            let key = ("Authorization", "Bearer test-secret");
            let path = "/v0/management/usage-queue?count=2";

            let answer = fetch(addr, "GET", path, &[]).await;
            assert_eq!(answer.status, 401, "{}", answer.body);
            let answer = fetch(addr, "GET", "/v0/management/usage?count=2", &[key]).await;
            assert_eq!(answer.status, 404, "{}", answer.body);

            let answer = fetch(addr, "GET", path, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert_eq!(answer.body, r#"[{"id":1},{"id":2}]"#);
            assert!(queue.pop_oldest(1).is_empty());
        }

        /// The main server serves the OAuth callback pages to anyone, for
        /// `GET` only, even without a management key set, ahead of the
        /// proxy's client keys; the management API's callback route needs
        /// a key set.
        #[tokio::test]
        async fn oauth_callback_pages_are_served_beside_the_proxy() {
            let dir = tempfile::tempdir().unwrap();
            let service = service(
                dir.path(),
                "api-keys: ['client-key']
",
            );
            let (addr, _stop) = start(&service).await;
            let page = concat!(
                r#"<html><head><meta charset="utf-8"><title>Authentication successful</title>"#,
                "<script>setTimeout(function(){window.close();},5000);</script></head>",
                "<body><h1>Authentication successful!</h1><p>You can close this window.</p>",
                "<p>This window will close automatically in 5 seconds.</p></body></html>",
            );

            for path in ["/anthropic/callback", "/codex/callback"] {
                let query = format!("{path}?state=unknown-state&code=c");
                let answer = fetch(addr, "GET", &query, &[]).await;
                assert_eq!((answer.status, answer.body.as_str()), (200, page), "{path}");
                assert_eq!(
                    answer.header("content-type"),
                    Some("text/html; charset=utf-8")
                );
                let answer = fetch(addr, "POST", path, &[]).await;
                assert_eq!(
                    (answer.status, answer.body.as_str()),
                    (404, "404 page not found"),
                    "POST {path}"
                );
                assert_eq!(fetch(addr, "HEAD", path, &[]).await.status, 404, "{path}");
            }
            for path in ["/antigravity/callback", "/devin/callback", "/callback"] {
                let answer = fetch(addr, "GET", path, &[]).await;
                assert_eq!(
                    (answer.status, answer.body.as_str()),
                    (404, "404 page not found"),
                    "{path}"
                );
            }
            let path = "/v0/management/oauth-callback?state=s&code=c";
            let answer = fetch(addr, "GET", path, &[]).await;
            assert_eq!((answer.status, answer.body.as_str()), (404, ""));
        }

        /// Not upstream's: shutting the service down stops the OAuth logins
        /// in progress, dropping their sessions, and no login starts after
        /// that.
        #[tokio::test]
        async fn shutting_down_stops_oauth_logins() {
            let dir = tempfile::tempdir().unwrap();
            let service = service(dir.path(), KEYED);
            let (addr, _stop) = start(&service).await;
            let key = ("Authorization", "Bearer test-secret");
            let start_login = "/v0/management/codex-auth-url";
            let answer = fetch(addr, "GET", start_login, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            let started: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
            let state = started["state"].as_str().unwrap();
            let status = format!("/v0/management/get-auth-status?state={state}");
            let answer = fetch(addr, "GET", &status, &[key]).await;
            assert_eq!(answer.body, r#"{"status":"wait"}"#);

            // Shut down as `run` does, another server standing for the one
            // it stops; the first still answers.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (stop, stopped) = watch::channel(false);
            let server = tokio::spawn(serve(listener, None, service.app(), stopped));
            let _ = shut_down(&service, &stop, server).await;

            let answer = fetch(addr, "GET", &status, &[key]).await;
            assert_eq!(
                answer.body,
                r#"{"error":"unknown or expired state","status":"error"}"#
            );
            let answer = fetch(addr, "GET", start_login, &[key]).await;
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (503, r#"{"error":"server shutting down"}"#)
            );
        }

        #[tokio::test]
        async fn management_follows_config_reloads() {
            let dir = tempfile::tempdir().unwrap();
            let trusted = "trusted-proxies: ['127.0.0.1']\n";
            let mut service = service(dir.path(), trusted);
            let (addr, _stop) = start(&service).await;
            let key = ("Authorization", "Bearer test-secret");
            let forwarded = ("X-Forwarded-For", "203.0.113.5");

            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!((answer.status, answer.body.as_str()), (404, ""));
            assert_eq!(answer.header("x-cpa-version"), None);

            // A trusted proxy speaks for its client, which isn't local.
            service.handle(
                config(dir.path(), &format!("{KEYED}{trusted}")),
                Path::new(""),
            );
            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 403);
            assert_eq!(answer.body, r#"{"error":"remote management disabled"}"#);

            let remote = format!("{KEYED}  allow-remote: true\n{trusted}");
            service.handle(config(dir.path(), &remote), Path::new(""));
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);

            // The trusted proxies are read once, at start, as upstream
            // reads them.
            service.handle(config(dir.path(), KEYED), Path::new(""));
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 403, "{}", answer.body);

            service.handle(config(dir.path(), ""), Path::new(""));
            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!((answer.status, answer.body.as_str()), (404, ""));
        }

        #[tokio::test]
        async fn safe_mode_shuts_the_proxy_but_not_management() {
            let dir = tempfile::tempdir().unwrap();
            let example = format!("api-keys: ['your-api-key-1']\n{KEYED}");
            let mut service = service(dir.path(), &example);
            let (addr, _stop) = start(&service).await;
            let key = ("Authorization", "Bearer test-secret");

            let client = ("Authorization", "Bearer your-api-key-1");
            let answer = fetch(addr, "GET", "/v1/models", &[client]).await;
            assert_eq!(answer.status, 403);
            assert_eq!(answer.header("x-cpa-safe-mode"), Some("example-api-key"));
            assert!(answer.body.contains("unsafe_example_api_key"));

            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert_eq!(answer.header("x-cpa-safe-mode"), None);

            let real = format!("api-keys: ['real-key']\n{KEYED}");
            service.handle(config(dir.path(), &real), Path::new(""));
            let client = ("Authorization", "Bearer real-key");
            let answer = fetch(addr, "GET", "/v1/models", &[client]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
        }
    }
}
