// Ported from CLIProxyAPI sdk/cliproxy/service_lifecycle.go (Run and
// Shutdown), service_auth.go (prepareCoreAuthForModelRegistration,
// completeModelRegistrationForAuth and applyCoreAuthRemoval),
// service_config.go (applyConfigRuntime and registerConfigAPIKeyAuths),
// service_executors.go, and the auth dispatch of internal/watcher's
// clients.go and config_reload.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Serving the proxy.
//!
//! At start the credentials in the auth directory and the config's API keys
//! are registered with the credential manager, and each one's models with
//! the model registry. Token refresh runs in the background every fifteen
//! minutes. Then the server listens, serving the management API beside the
//! proxy, and a watcher follows the config file and the auth directory:
//! - A config that changes is applied to the manager, the server, the
//!   management API and the executors; the API-key credentials are made
//!   again from it, and every credential's models registered again, as
//!   aliases and exclusions may have changed.
//! - An auth file that is added or changes is registered from the contents
//!   the watcher read; one that is removed is unregistered.
//!
//! Credentials read from files or the config aren't saved back; the manager
//! saves those it changes itself, as after a refresh.
//!
//! Deviations from upstream:
//! - Only the Codex and Claude executors are registered.
//! - Executors are made again on a reload only when a setting they use
//!   (`proxy-url`, `claude.model-level-cooling`) changed; upstream makes
//!   them again on every reload, which ends their WebSocket sessions.
//! - With an empty `host` the server listens on every IPv6 and IPv4
//!   interface, as Go does; an IPv6 `host` is bracketed, where upstream's
//!   address fails to parse.
//! - Changing `host`, `port` or `tls` takes a restart, as upstream; a reload
//!   logs that it was ignored.
//! - The auth directory is made absolute, as the watcher's paths are, so a
//!   credential file has the same `path` and ID whether it was found at
//!   start or reported by the watcher. Upstream keeps a relative directory
//!   relative, and so do its watcher's paths.
//! - The cooldown state store, usage statistics, pprof, the discovery
//!   advertiser, the WebSocket gateway, plugins and Home aren't ported.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use open_ferry_core::auth::synthesizer::api_key::{ApiKeyEntry, synthesize_api_key_auths};
use open_ferry_core::auth::synthesizer::file::{synthesize_auth_file, synthesize_file_auths};
use open_ferry_core::auth::synthesizer::{StableIdGenerator, SynthesisContext};
use open_ferry_core::auth::{Auth, FileStore, Status};
use open_ferry_core::config::{AuthFile, Config, ConfigWatcher, WatchEvent};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::registry::{ModelRegistry, RegistrationRules};
use open_ferry_management::{ManagementState, management_password_from_env};
use open_ferry_providers::claude::ClaudeExecutor;
use open_ferry_providers::codex::CodexExecutor;
use open_ferry_server::{AppState, ServerConfig, router_with};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::logging::LogLevel;
use crate::tls::{self, TlsListener, TlsPeer};

/// How often background refresh looks for tokens to renew.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long shutdown waits for open requests.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

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
    let mut service = Service::new(Arc::clone(&config), auth_dir, log_level);
    service.register_executors();
    service.load_file_auths();
    service.sync_config_auths();
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
        }
    }
    shut_down(&service, &stop, server).await
}

type Server = tokio::task::JoinHandle<io::Result<()>>;

/// Stops refresh and the server, giving open requests up to
/// [`SHUTDOWN_TIMEOUT`].
async fn shut_down(service: &Service, stop: &watch::Sender<bool>, mut server: Server) -> ExitCode {
    service.manager.stop_auto_refresh();
    let _ = stop.send(true);
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut server).await {
        Ok(result) => exit_code(result),
        Err(_) => {
            tracing::warn!("open requests didn't finish within 30s; closing them");
            server.abort();
            ExitCode::SUCCESS
        }
    }
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
    watcher: Option<ConfigWatcher>,
    /// The IDs of the credentials made from config API keys.
    config_auths: BTreeSet<String>,
    /// The credential ID registered for each auth file.
    file_auths: HashMap<PathBuf, String>,
}

impl Service {
    fn new(config: Arc<Config>, auth_dir: PathBuf, log_level: LogLevel) -> Self {
        let auth_dir = absolute_dir(auth_dir);
        let registry = Arc::new(ModelRegistry::new());
        let store = Arc::new(FileStore::new(&auth_dir));
        let manager = Manager::new(
            Settings::from(&*config),
            Arc::clone(&registry) as _,
            Some(Arc::clone(&store) as _),
        );
        let state = AppState::new(
            ServerConfig::from(&*config),
            Arc::new(manager.clone()),
            Arc::clone(&registry) as _,
        );
        let management = ManagementState::new(
            Arc::clone(&config),
            manager.clone(),
            Arc::clone(&registry),
            management_password_from_env(),
        );
        Self {
            config,
            auth_dir,
            log_level,
            store,
            manager,
            registry,
            state,
            management,
            watcher: None,
            config_auths: BTreeSet::new(),
            file_auths: HashMap::new(),
        }
    }

    /// The proxy's routes, with the management API's beside them.
    fn app(&self) -> axum::Router {
        let management = open_ferry_management::router(self.management.clone());
        router_with(self.state.clone(), management)
    }

    /// Registers the Codex and Claude executors for the current config.
    fn register_executors(&self) {
        let proxy_url = self.config.proxy_url.clone();
        self.manager
            .register_executor(Arc::new(CodexExecutor::new(proxy_url.clone())));
        self.manager.register_executor(Arc::new(
            ClaudeExecutor::new(proxy_url)
                .with_models(Arc::clone(&self.registry) as _)
                .with_model_level_cooling(self.config.claude.model_level_cooling),
        ));
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

    /// Registers a credential for each config API key, and unregisters
    /// those whose key is gone (upstream's `registerConfigAPIKeyAuths` and
    /// the watcher's diff of config credentials).
    fn sync_config_auths(&mut self) {
        let claude: Vec<ApiKeyEntry> = self.config.claude_api_key.iter().map(Into::into).collect();
        let codex: Vec<ApiKeyEntry> = self.config.codex_api_key.iter().map(Into::into).collect();
        let ctx = self.synthesis_context();
        let auths =
            match synthesize_api_key_auths(&claude, &codex, &ctx, &mut StableIdGenerator::new()) {
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

    /// Registers or updates `auth`, then its models (upstream's
    /// `prepareCoreAuthForModelRegistration` and
    /// `completeModelRegistrationForAuth`). Returns whether it is
    /// registered.
    fn upsert(&self, mut auth: Auth, rules: &RegistrationRules) -> bool {
        let (op, result) = match self.manager.get(&auth.id) {
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
            WatchEvent::AuthAdded(file) | WatchEvent::AuthChanged(file) => {
                self.load_auth_file(&file)
            }
            WatchEvent::AuthRemoved(path) => {
                if let Some(id) = self.file_auths.remove(&path) {
                    self.remove(&id);
                }
            }
            _ => {}
        }
        Watching::Same
    }

    /// Registers the credential in an auth file, from the contents the
    /// watcher read.
    fn load_auth_file(&mut self, file: &AuthFile) {
        let path = file.path.as_path();
        let auth = match synthesize_auth_file(&self.synthesis_context(), path, &file.data) {
            Ok(auth) => auth,
            Err(error) => {
                tracing::warn!("skipping auth file {}: {error}", path.display());
                None
            }
        };
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
        if self.upsert(auth, &self.rules()) {
            self.file_auths.insert(path.to_owned(), id);
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
        if previous.proxy_url != config.proxy_url
            || previous.claude.model_level_cooling != config.claude.model_level_cooling
        {
            self.register_executors();
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

        self.sync_config_auths();
        let rules = self.rules();
        for auth in self.manager.list() {
            self.registry.register_auth(&auth, &rules);
            self.manager.reconcile_registry_model_states(&auth.id);
        }
        tracing::info!("config reloaded");
        watching
    }
}

fn is_disabled(auth: &Auth) -> bool {
    auth.disabled || auth.status == Status::Disabled
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
    use super::*;

    /// A service over `dir` with `extra` config, its executors registered.
    fn service(dir: &Path, extra: &str) -> Service {
        let config = Config::parse(format!("auth-dir: '{}'\n{extra}", dir.display())).unwrap();
        let service = Service::new(Arc::new(config), dir.to_owned(), LogLevel::detached());
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
        service.handle(WatchEvent::AuthAdded(auth_file(&path)), Path::new(""));
        codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let changed = std::fs::read(&path).unwrap();
        service.handle(WatchEvent::AuthChanged(auth_file(&path)), Path::new(""));
        assert_eq!(service.manager.list().len(), 1);
        assert_eq!(service.manager.get(&id).unwrap().prefix, "team");
        assert_ne!(written, changed);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            changed,
            "the file was rewritten"
        );

        service.handle(WatchEvent::AuthRemoved(path.clone()), Path::new(""));
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.file_auths.is_empty());
    }

    #[tokio::test]
    async fn an_auth_file_that_stops_parsing_is_unregistered() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        service.handle(WatchEvent::AuthAdded(auth_file(&path)), Path::new(""));
        let id = service.manager.list()[0].id.clone();

        let broken = AuthFile {
            path: path.clone(),
            data: Arc::from(&b"{not json"[..]),
        };
        service.handle(WatchEvent::AuthChanged(broken), Path::new(""));
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
        service.handle(WatchEvent::AuthAdded(checked), Path::new(""));
        let auths = service.manager.list();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].prefix, "team");
        assert_eq!(service.file_auths.get(&path), Some(&auths[0].id));
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

        let mut service = Service::new(Arc::clone(&config), relative, LogLevel::detached());
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
        let WatchEvent::AuthAdded(file) = event else {
            panic!("expected an added auth file, got {event:?}");
        };
        let path = file.path.clone();
        service.handle(WatchEvent::AuthAdded(file), &config_path);
        assert_eq!(service.manager.list().len(), 1);
        assert!(service.manager.get(&id).is_some());
        assert_eq!(service.file_auths.get(&path), Some(&id));

        service.handle(WatchEvent::AuthRemoved(path), &config_path);
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
    ///
    /// `TestHomeEnabledHidesManagementEndpointsAndControlPanel`,
    /// `TestManagementPluginsRouteRegistered`,
    /// `TestManagementUsageRequiresManagementAuthAndPopsArray` and
    /// `TestOAuthCallbackRouteSkipsManagementKeyMiddleware` are dropped:
    /// Home mode, plugins, usage and the OAuth callbacks aren't ported.
    mod management {
        use std::fmt::Write as _;
        use std::net::SocketAddr;
        use std::path::Path;
        use std::sync::Arc;

        use open_ferry_core::config::{Config, WatchEvent};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::watch;

        use super::super::{Service, serve};
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
            let answer = fetch(addr, "GET", "/v0/management/config", &[key]).await;
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
