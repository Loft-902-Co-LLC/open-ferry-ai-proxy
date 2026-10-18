// Ported from CLIProxyAPI sdk/cliproxy/service_lifecycle.go (Run and
// Shutdown), internal/cmd/run.go (StartService and StartServiceBackground,
// with their local management password), service_auth.go (prepareCoreAuthForModelRegistration,
// completeModelRegistrationForAuth and applyCoreAuthRemoval),
// service_config.go (applyConfigRuntime, registerConfigAPIKeyAuths and the
// catalog sources of commitConfig), service_plugins.go
// (registerModelRefreshCallback), service_lifecycle.go
// (startModelCatalogUpdaters),
// service_executors.go (registerAvailableExecutors,
// registerExecutorForAuth and registerOpenAICompatProviderExecutor),
// builder.go (runtimeAuthSyncHook), the
// order of internal/watcher/synthesizer/config.go (Synthesize), and the
// auth dispatch of internal/watcher's clients.go and config_reload.go
// (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Serving the proxy.
//!
//! At start the credentials in the auth directory, the config's API keys
//! (Gemini, Claude, Codex and Vertex AI), its OpenAI-compatible
//! providers' keys and its `claude-cli` entries are registered with the
//! credential manager, and each one's models with the model registry. The
//! version of each `claude-cli` entry's Claude Code is checked in the
//! background, at start and when a reload changes the entries. Each
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
//! - Auth events queue a fresh, save-serialized read. Supported credentials
//!   never adopt captured watcher tokens. Failed reads keep live work and retry
//!   with a capped-rate dirty tail; only an accepted removal unregisters them.
//!
//! The model catalogs are read from the files the config's `models`
//! section names before the credentials are registered, and followed while
//! the service runs (see [`CatalogRuntime`]): a reload that changes a
//! source reads it, and when a catalog changes a provider's models, the
//! models of that provider's enabled credentials are registered again.
//!
//! A credential the management API saves, changes or removes is applied
//! by the same loop at once, as upstream's `runtimeAuthSyncHook` applies
//! it, and the API waits until it is. Once the loop has stopped, the API's
//! changes fail, and it answers 503.
//!
//! The management API saves the config file through a
//! [`FileConfigWriter`] over the file the service was started with, and
//! once it has saved, the loop has the watcher reload the file at once
//! (upstream's reload hook, which calls the watcher's
//! `ReloadConfigIfChanged`): the config is applied as for a change the
//! watcher sees, before the API answers, and the reload the save's own
//! write events bring finds nothing new. Without a watcher the loop loads
//! the file itself. Once the loop has stopped, a save still goes to the
//! file, and is loaded at the next start.
//!
//! Changes carry revisions from one counter: the watcher takes its event's
//! before reading, and the API's change takes one after saving. Same-key
//! observations and removals wait behind an in-flight read. Its successful
//! fresh revision, assigned under save serialization just before reading,
//! is accounted before that parked work drains. Changes at or below this
//! watermark are skipped; failures claim no revision. Registration and
//! origin checks fence replacement credentials, while a config-generation
//! fence immediately retries synthesis held across a config change. The API
//! is told a skipped or refused change was applied, as upstream tells it.
//!
//! Credentials read from files or the config aren't saved back; the manager
//! saves those it changes itself, as after a refresh.
//!
//! The dashboard (see [`open_ferry_dashboard`]) is served beside the
//! management API: the app at `/dashboard/`, `/management.html` sending
//! browsers there, and the dashboard API under `/open-ferry/api/v1/`. Its
//! usage routes read the usage ledger, which is opened in the log directory
//! at start, on a thread of its own while the server starts serving, and
//! records the usage records from then on; at shutdown it writes what it
//! was sent once the server has stopped.
//!
//! With open-ferry's `management.separate-address`, the management API,
//! the dashboard and the dashboard API are served on that address alone,
//! on a listener of their own, and the proxy's address answers their
//! paths as while management is off (see [`management_listener`]).
//!
//! The command line's `-password` is a local management password (see
//! [`Options`]): the management API accepts it from loopback clients, and
//! the server serves the [keep-alive endpoint](crate::keep_alive), and
//! stops when it isn't called. The TUI's standalone mode runs the service
//! with a password of its own, without the endpoint and without printing
//! to the terminal, until the TUI ends.
//!
//! Deviations from upstream:
//! - Only the Codex, Meta, Claude, Gemini, Gemini Interactions, Vertex AI,
//!   xAI and OpenAI-compatible executors are registered, the native ones at
//!   start rather than as their first credential comes. Upstream gives a
//!   credential of a provider it has no executor for (such as `gemini-cli`
//!   or `aistudio`) an OpenAI-compatible executor keyed by that provider;
//!   here such a credential has no executor, and isn't served.
//! - Executors are made again on a reload only when the config changed:
//!   the Codex, Meta and xAI ones on any change; Codex and xAI keep their
//!   shared WebSocket stores. The other native ones when `proxy-url`,
//!   `claude.model-level-cooling`, `codex.orphan-delegation-compatibility`
//!   or `client.codex.optimize-multi-agent-v2` changes; and the
//!   OpenAI-compatible ones when `proxy-url`, `openai-compatibility` or
//!   either of those Codex settings changes. Upstream makes them all again
//!   on every reload, retaining shared WebSocket sessions. Remaking the
//!   Vertex AI executor drops the access tokens it cached.
//! - An OpenAI-compatible executor that no credential uses any more after a
//!   reload is unregistered; upstream keeps it.
//! - With an empty `host` the server listens on every IPv6 and IPv4
//!   interface, as Go does; an IPv6 `host` is bracketed, where upstream's
//!   address fails to parse.
//! - Changing `host`, `port` or `tls` takes a restart, as upstream; a reload
//!   logs that it was ignored. So does changing open-ferry's
//!   `management.separate-address`.
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
//! - The dashboard's usage ledger is opened in the log directory at start,
//!   which makes the directory whether or not anything else is logged
//!   there; upstream makes it only to write a log.
//! - The logs, the usage statistics and the cooldown state store take the
//!   config through the `observability` hooks once the credentials are
//!   loaded, at start as on a reload; P3 ports what is behind them. pprof,
//!   the discovery advertiser, the WebSocket gateway, plugins and Home
//!   aren't ported.
//! - open-ferry's own update checks (see `docs/updates.md`) run beside
//!   the server when [`Options::self_update`] is set, and follow the
//!   config's `self-update` on a reload; upstream has none.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use open_ferry_core::auth::compat::OPENAI_COMPATIBILITY;
use open_ferry_core::auth::synthesizer::file::{
    apply_config_attributes, synthesize_auth_file, synthesize_file_auths, try_synthesize_file_auths,
};
use open_ferry_core::auth::synthesizer::{
    StableIdGenerator, SynthesisContext, synthesize_config_auths,
};
use open_ferry_core::auth::{AdoptionRead, Auth, AuthStore, FileStore, Status};
use open_ferry_core::config::{
    AuthFile, Config, ConfigWatcher, ReadGuard, WatchEvent, next_revision,
};
use open_ferry_core::gate::{ActivationKey, ActiveGate, Operation};
use open_ferry_core::manager::{AdoptionResult, Manager, ManagerError, Settings};
use open_ferry_core::models::{ModelCatalog, ModelInfo};
use open_ferry_core::observe::Observability;
use open_ferry_core::registry::{CatalogRuntime, ModelRegistry, RegistrationRules};
use open_ferry_dashboard::Ledger;
use open_ferry_management::{
    ConfigReload, CredentialSync, FileConfigWriter, ManagementState, ReloadFuture, SyncError,
    SyncFuture, management_password_from_env,
};
use open_ferry_providers::claude::ClaudeExecutor;
use open_ferry_providers::claude_cli::{self, ClaudeCliExecutor};
use open_ferry_providers::codex::CodexExecutor;
use open_ferry_providers::gemini::{GeminiExecutor, InteractionsExecutor, VertexExecutor};
use open_ferry_providers::meta::MetaExecutor;
use open_ferry_providers::openai_compat::OpenAiCompatExecutor;
use open_ferry_providers::xai::XaiExecutor;
use open_ferry_server::{AppState, ServerConfig, router_with};
use open_ferry_translate::go;
use open_ferry_update::background::{self as update_checks, FIRST_CHECK};
use open_ferry_update::{DataDir, UpdateService, Updater, settings as update_settings};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::keep_alive::{self, KeepAlive};
use crate::logging::LogLevel;
use crate::observability;
use crate::tls;

mod management_listener;
pub(crate) mod serve_loop;
pub(crate) mod worker;

/// How often background refresh looks for tokens to renew.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long shutdown waits for open requests.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How many credential changes from the management API may wait for the
/// service loop.
const SYNC_QUEUE: usize = 32;

/// How many config reloads the management API asks for may wait for the
/// service loop.
const RELOAD_QUEUE: usize = 32;

/// How long shutdown waits for the usage ledger to write the records it
/// was sent.
const LEDGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What the command line adds to the config.
#[derive(Clone, Default)]
pub struct Options {
    /// The local management password, or empty (`-password`).
    pub local_password: String,
    /// Whether, with a local management password, the server serves the
    /// keep-alive endpoint and stops when it isn't called, as upstream's
    /// `StartService` does and its `StartServiceBackground` doesn't.
    pub keep_alive: bool,
    /// Whether to print that the server started; not while the TUI has the
    /// terminal.
    pub announce: bool,
    /// The SHA-256 of the config file's contents the config was loaded
    /// from, in lowercase hex, as [`Config::load_with_sha256`] gives it:
    /// the management API saves the config only while the file still holds
    /// them, or what it last wrote, so it never writes over a change made
    /// there since. Without it, nothing is checked until the config is
    /// loaded again or written.
    pub config_sha256: Option<String>,
    /// Whether to run open-ferry's own update checks beside the server,
    /// as `self-update` in the config says. Not upstream's.
    pub self_update: bool,
}

/// Serves until `stop` resolves, or until the server fails.
pub async fn run(
    config: Config,
    config_path: PathBuf,
    auth_dir: PathBuf,
    log_level: LogLevel,
    options: Options,
    stop: impl Future<Output = ()>,
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
    // Construction is not a write: the service is built on a closed gate,
    // and the first activation opens it (review 3 T2).
    let (gate, key) = ActiveGate::standby();
    let mut service = Service::prepared(
        Arc::clone(&config),
        config_path.clone(),
        auth_dir,
        log_level,
        &options,
        gate,
    );
    if options.self_update {
        service.start_update_checks();
    }
    claude_cli::warn_outdated_guarded(&config.claude_cli, service.claude_guard.clone());
    // An unsupervised run activates at once and never closes the gate.
    if let Err(error) = service.first_activation(&key) {
        tracing::error!("failed to activate the service: {error}");
        return ExitCode::FAILURE;
    }
    service.gate.reopen();
    service.start_auto_refresh();

    let listener = match bind(&config.host, config.port) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!("failed to start HTTP server: {error}");
            return ExitCode::FAILURE;
        }
    };
    let separate = match management_listener::bind_separate(&config) {
        Ok(separate) => separate,
        Err(error) => {
            tracing::error!("{error}");
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
    let keep_alive = (options.keep_alive && !options.local_password.is_empty())
        .then(|| KeepAlive::new(&options.local_password, keep_alive::TIMEOUT));
    let extra = keep_alive
        .as_ref()
        .map_or_else(axum::Router::new, KeepAlive::router);
    let (stop_server, stopped) = watch::channel(false);
    let management_address = separate
        .as_ref()
        .map(|separate| separate.address.to_string());
    // One control for the run's listeners.
    let control = serve_loop::ServeControl::new();
    let mut server = match separate {
        None => tokio::spawn(serve_with(
            control.clone(),
            listener,
            tls_config,
            service.app_with(extra),
            stopped,
        )),
        Some(separate) => management_listener::spawn(
            &service, control, listener, separate, tls_config, extra, stopped,
        ),
    };
    if options.announce {
        println!(
            "API server started successfully on: {}:{}",
            config.host, config.port
        );
        if let Some(address) = management_address {
            println!("Management API and dashboard started on: {address}");
        }
    }

    let mut events = match start_watcher(
        &config_path,
        &config,
        Arc::clone(&service.probe),
        service.store.file_guard(),
    ) {
        Ok((watcher, events)) => {
            service.watcher = Some(watcher);
            Some(events)
        }
        Err(error) => {
            tracing::error!("failed to create watcher: {error}");
            service.close_sync();
            service.close_reloads();
            shut_down(&service, &stop_server, server).await;
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("file watcher started for config and auth directory changes");

    let shutdown = async {
        match &keep_alive {
            Some(keep_alive) => tokio::select! {
                () = stop => {}
                () = keep_alive.idle() => {}
            },
            None => stop.await,
        }
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::debug!("shutdown signal received, shutting down...");
                break;
            }
            result = &mut server => {
                stop_update_checks(&service.management);
                service.stop_catalogs();
                service.manager.stop_auto_refresh();
                service.management.shutdown().await;
                stop_ledger(&service.ledger).await;
                open_ferry_core::manager::cooldown_store::flush(&service.manager);
                return exit_code(result);
            }
            () = service.work(&config_path, &mut events) => {}
        }
    }
    service.close_sync();
    service.close_reloads();
    shut_down(&service, &stop_server, server).await
}

type Server = tokio::task::JoinHandle<io::Result<()>>;

/// Runs `work` as `operation`, when the request came from one, so what it
/// writes is admitted by the request's gate.
async fn in_scope<T>(operation: Option<Operation>, work: impl FnOnce() -> T) -> T {
    match operation {
        Some(operation) => operation.scope(async { work() }).await,
        None => work(),
    }
}

/// Builds open-ferry's own updater without starting its timer, or warns
/// and returns `None` when checks can't run. The
/// checks follow `config`'s `self-update`; while it is `off` they make no
/// request.
///
/// A supervised worker passes the data directory its supervisor named
/// (`welcome.installation.data_dir`); an ordinary run takes the user's from
/// the environment.
fn prepare_update_checks(
    probe: &dyn crate::scope::Probe,
    config: &Config,
    data_dir: Option<&Path>,
) -> Option<UpdateService> {
    let data = match update_check_data(probe, data_dir) {
        Ok(data) => data,
        Err(error) => {
            tracing::warn!("update checks don't run: {error}");
            return None;
        }
    };
    let make_fetch = update_checks::http_fetch();
    // Each check makes its own downloader from the config in force, so a
    // `proxy-url` that can't be used yet only fails the checks it is
    // in force for.
    let fetch = match make_fetch(&config.proxy_url).or_else(|_| make_fetch("")) {
        Ok(fetch) => fetch,
        Err(error) => {
            tracing::warn!("update checks don't run: {error}");
            return None;
        }
    };
    match Updater::for_this_binary(fetch, data) {
        Ok(updater) => {
            let updates = UpdateService::new(updater, make_fetch, config);
            Some(updates)
        }
        Err(error) => {
            tracing::warn!("update checks don't run: {error}");
            None
        }
    }
}

/// Authorizes the worker's records, or the ordinary server's account records.
fn update_check_data(
    probe: &dyn crate::scope::Probe,
    data_dir: Option<&Path>,
) -> Result<DataDir, String> {
    let root = match data_dir {
        Some(root) => root
            .to_str()
            .ok_or_else(|| format!("{} isn't valid Unicode", root.display()))?
            .to_owned(),
        None => probe.account_dir()?,
    };
    crate::scope::authorize(probe, &root).map_err(|error| error.to_string())?;
    Ok(DataDir::at(root))
}

#[cfg(test)]
mod update_check_scope_tests {
    use super::update_check_data;
    use crate::os_service::Platform;
    use crate::update::scope_probe::Fake;
    use std::path::Path;

    #[test]
    fn privileged_checks_refuse_untrusted_account_and_worker_records() {
        let mut probe = Fake::new(Platform::Windows, true);
        let account = probe.account.clone().unwrap();
        probe.dir(&account).untrust(&account);
        for root in [None, Some(Path::new(account.as_str()))] {
            let refusal = update_check_data(&probe, root).unwrap_err();
            assert!(refusal.contains("not opened as administrator"), "{refusal}");
        }
    }

    #[test]
    fn unprivileged_checks_use_account_and_worker_records() {
        let mut probe = Fake::new(Platform::Windows, false);
        let account = probe.account.clone().unwrap();
        probe.dir(&account).untrust(&account);
        assert_eq!(
            update_check_data(&probe, None).unwrap().root(),
            Path::new(&account)
        );
        assert_eq!(
            update_check_data(&probe, Some(Path::new(&account)))
                .unwrap()
                .root(),
            Path::new(&account)
        );
    }

    #[test]
    fn trusted_privileged_records_run_but_root_on_macos_does_not() {
        let mut probe = Fake::new(Platform::Linux, true);
        let account = probe.account.clone().unwrap();
        probe.dir(&account);
        assert!(update_check_data(&probe, None).is_ok());
        probe.platform = Platform::MacOs;
        assert!(
            update_check_data(&probe, None)
                .unwrap_err()
                .contains("macOS")
        );
    }
}

/// Stops the update checks, if they run.
fn stop_update_checks(management: &ManagementState) {
    if let Some(updates) = management.updates() {
        updates.stop();
    }
}

/// Stops the update checks, following the catalog files, refresh, the
/// management API's OAuth logins and the server, giving open requests up
/// to [`SHUTDOWN_TIMEOUT`], then the usage ledger, and saves the
/// cooldowns.
async fn shut_down(service: &Service, stop: &watch::Sender<bool>, server: Server) -> ExitCode {
    shut_down_within(service, stop, Some(server), SHUTDOWN_TIMEOUT).await
}

/// [`shut_down`], giving open requests up to `timeout`, then closing them
/// by dropping `server`, which cuts its connections. A worker passes its
/// stop budget; `None` when the server has ended already.
async fn shut_down_within(
    service: &Service,
    stop: &watch::Sender<bool>,
    server: Option<Server>,
    timeout: Duration,
) -> ExitCode {
    stop_update_checks(&service.management);
    service.stop_catalogs();
    service.manager.stop_auto_refresh();
    service.management.shutdown().await;
    let _ = stop.send(true);
    let code = match server {
        None => ExitCode::SUCCESS,
        Some(mut server) => match tokio::time::timeout(timeout, &mut server).await {
            Ok(result) => exit_code(result),
            Err(_) => {
                tracing::warn!("open requests didn't finish within {timeout:?}; closing them");
                server.abort();
                ExitCode::SUCCESS
            }
        },
    };
    stop_ledger(&service.ledger).await;
    open_ferry_core::manager::cooldown_store::flush(&service.manager);
    code
}

/// A supervised stop has one deadline, including management, requests,
/// ledger and cooldown flush. Blocking flushes may outlive it; the worker
/// retains its lease and main must exit the process immediately afterward.
async fn shut_down_by(
    service: &Service,
    stop: &watch::Sender<bool>,
    server: Option<Server>,
    control: &serve_loop::ServeControl,
    deadline: tokio::time::Instant,
    reserve: Duration,
) -> bool {
    stop_update_checks(&service.management);
    service.stop_catalogs();
    service.manager.stop_auto_refresh();
    let requests_deadline = deadline
        .checked_sub(reserve)
        .unwrap_or_else(tokio::time::Instant::now);
    let management = async {
        #[cfg(test)]
        if let Some((arrived, _)) = &service.shutdown_hooks {
            arrived.notify_one();
            std::future::pending::<()>().await;
        }
        service.management.shutdown().await;
    };
    let _ = tokio::time::timeout_at(requests_deadline, management).await;
    let _ = stop.send(true);
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    let requests = left.saturating_sub(reserve);
    let mut failed = false;
    if let Some(mut server) = server {
        match tokio::time::timeout(requests, &mut server).await {
            Ok(result) => failed = !matches!(result, Ok(Ok(()))),
            Err(_) => {
                server.abort();
                if let Ok(Err(error)) = tokio::time::timeout_at(deadline, &mut server).await {
                    failed = error.is_panic();
                }
            }
        }
    }
    control.cut();
    let ledger = async {
        #[cfg(test)]
        if let Some((_, finished)) = &service.shutdown_hooks {
            tokio::time::sleep(Duration::from_millis(10)).await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        stop_ledger(&service.ledger).await;
    };
    let _ = tokio::time::timeout_at(deadline, ledger).await;
    let manager = service.manager.clone();
    let flush = tokio::task::spawn_blocking(move || {
        open_ferry_core::manager::cooldown_store::flush(&manager);
    });
    let _ = tokio::time::timeout_at(deadline, flush).await;
    failed
}

/// Stops `ledger` recording, giving it up to [`LEDGER_SHUTDOWN_TIMEOUT`] to
/// write the records it was sent (see [`Ledger::shutdown`]).
async fn stop_ledger(ledger: &Ledger) {
    let ledger = ledger.clone();
    let stopped = tokio::task::spawn_blocking(move || ledger.shutdown());
    if tokio::time::timeout(LEDGER_SHUTDOWN_TIMEOUT, stopped)
        .await
        .is_err()
    {
        tracing::warn!("the usage ledger didn't finish writing within 10s");
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

/// A credential change from the management API, and where to say it was
/// applied.
struct SyncRequest {
    change: SyncChange,
    /// Taken once the change was saved (see [`next_revision`]).
    revision: u64,
    applied: oneshot::Sender<()>,
    /// The operation the call ran in, which the loop applies the change as.
    operation: Option<Operation>,
}

/// A credential change from the management API.
#[derive(Clone)]
enum SyncChange {
    /// A credential to register or update.
    Upsert(Box<Auth>),
    /// An auth file written, with its contents.
    FileWritten(AuthFile),
    /// An auth file removed.
    FileRemoved(PathBuf),
    /// Successful write authority is separate from its compatibility payload.
    Completed {
        payload: Box<SyncChange>,
        command: CommandSlot,
    },
}

// Shared only to let an already-delivered completion be a no-op. The context
// itself is neither Clone nor Copy and is transferred by value at publication.
#[derive(Clone)]
struct CommandSlot {
    arrival: u64,
    consumed: Arc<std::sync::atomic::AtomicBool>,
    authority: Arc<std::sync::Mutex<Option<open_ferry_core::manager::CompletedTokenCommand>>>,
}

impl CommandSlot {
    fn new(command: open_ferry_core::manager::CompletedTokenCommand) -> Self {
        Self {
            arrival: 0,
            consumed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            authority: Arc::new(std::sync::Mutex::new(Some(command))),
        }
    }

    fn is_live(command: &Option<Self>) -> bool {
        command
            .as_ref()
            .is_some_and(|slot| !slot.consumed.load(Ordering::Acquire))
    }

    fn provenance_only(self) -> Self {
        // A stale identity removes authority, never provenance.
        if self.consumed.load(Ordering::Acquire) {
            self
        } else {
            Self {
                arrival: self.arrival,
                consumed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                authority: Arc::new(std::sync::Mutex::new(None)),
            }
        }
    }

    fn newest(current: Option<Self>, incoming: Option<Self>) -> Option<Self> {
        // Consumption removes authority, never the fresh-only provenance.
        match (Self::is_live(&current), Self::is_live(&incoming)) {
            (true, false) => return current,
            (false, true) => return incoming,
            _ => {}
        }
        match (current, incoming) {
            (Some(current), Some(incoming)) if current.arrival > incoming.arrival => Some(current),
            (current, incoming) => incoming.or(current),
        }
    }
}

impl SyncChange {
    fn payload(&self) -> &Self {
        match self {
            Self::Completed { payload, .. } => payload,
            other => other,
        }
    }

    fn take_command(&mut self) -> Option<CommandSlot> {
        if !matches!(self, Self::Completed { .. }) {
            return None;
        }
        let Self::Completed { payload, command } =
            std::mem::replace(self, Self::FileRemoved(PathBuf::new()))
        else {
            unreachable!("completed payload checked above");
        };
        *self = *payload;
        Some(command)
    }
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
            operation: Operation::current(),
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

    fn completed_token_command(
        &self,
        auth: Auth,
        file: Option<AuthFile>,
        command: open_ferry_core::manager::CompletedTokenCommand,
    ) -> SyncFuture<'_> {
        let revision = next_revision();
        let payload = file.map_or_else(
            || SyncChange::Upsert(Box::new(auth)),
            SyncChange::FileWritten,
        );
        Box::pin(self.send(
            SyncChange::Completed {
                payload: Box::new(payload),
                command: CommandSlot::new(command),
            },
            revision,
        ))
    }

    fn file_removed(&self, path: PathBuf) -> SyncFuture<'_> {
        let revision = next_revision();
        Box::pin(self.send(SyncChange::FileRemoved(path), revision))
    }
}

/// The service's [`ConfigReload`], which the management API calls once it
/// has saved the config (upstream's reload hook, which calls the watcher's
/// `ReloadConfigIfChanged`): each call waits in a bounded queue for the
/// service loop, which has the watcher reload the file and says when the
/// config it found is applied. Once the loop has stopped, a call returns at
/// once; the file holds the change, which the next start loads.
struct ReloadSender {
    requests: mpsc::Sender<ReloadRequest>,
}

impl ReloadSender {
    /// Asks the loop for a reload, forced with `force`, and waits until it
    /// is applied.
    async fn ask(&self, force: bool) {
        let (applied, done) = oneshot::channel();
        if self
            .requests
            .send(ReloadRequest {
                force,
                applied,
                operation: Operation::current(),
            })
            .await
            .is_ok()
        {
            let _ = done.await;
        }
    }
}

impl ConfigReload for ReloadSender {
    fn reload(&self) -> ReloadFuture<'_> {
        Box::pin(self.ask(false))
    }

    fn force_reload(&self) -> ReloadFuture<'_> {
        Box::pin(self.ask(true))
    }
}

/// A reload the management API asked for, and what to tell once the config
/// it found is applied. A forced one, as a save that found the file changed
/// asks for, loads the file even when it holds what the watcher loaded last
/// (see [`ConfigWatcher::force_reload_config`]).
struct ReloadRequest {
    force: bool,
    applied: oneshot::Sender<()>,
    /// The operation the call ran in, which the loop applies the config as.
    operation: Option<Operation>,
}

// No credentials or event bytes survive in retry state.
#[derive(Clone)]
struct ReadIdentity {
    id: Option<String>,
    provider: String,
    path: Option<PathBuf>,
    file_name: String,
    registration: Option<u64>,
    tombstone: u64,
    binding: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum ReadKey {
    Id(String),
    Path(PathBuf),
}

struct AuthChange {
    change: SyncChange,
    revision: u64,
    applied: Option<oneshot::Sender<()>>,
    operation: Option<Operation>,
}

#[derive(Clone)]
struct RecoveryFloor {
    origin: Option<PathBuf>,
    revision: u64,
}

struct ReadJob {
    // Only the active job retains the compatibility payload for Retry-now.
    source: Option<SyncChange>,
    trigger_revision: u64,
    recovery_floor: Option<RecoveryFloor>,
    identity: ReadIdentity,
    parked: VecDeque<AuthChange>,
    covered: Vec<AuthChange>,
    applied: Option<oneshot::Sender<()>>,
    operation: Option<Operation>,
    attempt: usize,
    command: Option<CommandSlot>,
    pending_command: Option<CommandSlot>,
}

struct DirtyRead {
    identity: ReadIdentity,
    recovery_floor: Option<RecoveryFloor>,
    deadline: tokio::time::Instant,
    attempt: usize,
    command: Option<CommandSlot>,
}

fn observation_path(auth_dir: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        auth_dir.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(part.as_os_str()),
        }
    }
    normalized
}

struct ReadCompletion {
    key: ReadKey,
    identity: ReadIdentity,
    config_generation: u64,
    result: Result<AdoptionResult, ManagerError>,
    unsupported_empty: bool,
    unsupported_sourceless: bool,
    command_consumed: bool,
}

fn identity_hint(identity: &ReadIdentity, ctx: &SynthesisContext) -> Auth {
    let provider = if identity.provider.is_empty() {
        "codex"
    } else {
        &identity.provider
    };
    let mut auth = identity
        .path
        .as_ref()
        .and_then(|path| {
            let data = serde_json::to_vec(&serde_json::json!({ "type": provider })).ok()?;
            synthesize_auth_file(ctx, path, &data).ok().flatten()
        })
        .unwrap_or_default();
    if let Some(id) = &identity.id {
        auth.id = id.clone();
    }
    auth.provider = provider.to_owned();
    auth.registration_epoch = identity.registration.unwrap_or(0);
    if !identity.file_name.is_empty() {
        auth.file_name = identity.file_name.clone();
    }
    auth
}

fn retry_delay(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::from_millis(50),
        1 => Duration::from_millis(200),
        2 => Duration::from_secs(1),
        3 => Duration::from_secs(5),
        _ => Duration::from_secs(30),
    }
}

async fn read_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
type ObservationHook = Arc<dyn Fn(&ReadIdentity) + Send + Sync>;

/// The credential manager, the model registry, the server and management
/// state, and what was registered from the config and the auth directory.
struct Service {
    config: Arc<Config>,
    probe: crate::scope::RuntimeProbe,
    claude_guard: Option<claude_cli::SpawnGuard>,
    auth_dir: PathBuf,
    log_level: LogLevel,
    /// The gate the store, the manager, the config writer and the
    /// management state share. Closed until the first activation, then
    /// open for good for a run with no supervisor.
    gate: ActiveGate,
    /// Whether the cooldown store is held off ([`Service::hold_cooldown_store`])
    /// until the first activation.
    cooldown_held: bool,
    store: Arc<FileStore>,
    manager: Manager,
    registry: Arc<ModelRegistry>,
    executor_catalog: Arc<ExecutorCatalog>,
    state: AppState,
    management: ManagementState,
    /// The log directory, the request logger and the usage statistics the
    /// server and the management API share.
    observability: Observability,
    /// The usage ledger the dashboard API reads; unavailable until
    /// [`Service::start_ledger`].
    ledger: Ledger,
    watcher: Option<ConfigWatcher>,
    /// The IDs of the credentials made from config API keys.
    config_auths: BTreeSet<String>,
    /// The credential ID registered for each auth file.
    file_auths: HashMap<PathBuf, String>,
    /// The revision of the last change applied to each credential ID, kept
    /// once it is unregistered so an older change can't bring it back
    /// (upstream's `authRevisions`).
    revisions: HashMap<String, u64>,
    config_generation: Arc<AtomicU64>,
    reads: HashMap<ReadKey, ReadJob>,
    dirty_reads: HashMap<ReadKey, DirtyRead>,
    read_sender: mpsc::UnboundedSender<ReadCompletion>,
    read_completions: mpsc::UnboundedReceiver<ReadCompletion>,
    #[cfg(test)]
    read_hook: Option<ObservationHook>,
    #[cfg(test)]
    command_arrival_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    completion_hook: Option<ObservationHook>,
    #[cfg(test)]
    removal_hook: Option<Arc<dyn Fn() + Send + Sync>>,

    /// The provider keys OpenAI-compatible executors are registered for.
    compat_executors: BTreeSet<String>,
    /// The management API's credential changes, waiting for the loop.
    sync_requests: mpsc::Receiver<SyncRequest>,
    /// The management API's config reloads, waiting for the loop.
    reload_requests: mpsc::Receiver<ReloadRequest>,
    /// The reloads the watcher was asked for, by ticket, oldest first.
    pending_reloads: VecDeque<(u64, ReloadRequest)>,
    /// The ticket of the last reload asked for.
    last_reload: u64,
    /// Ignore queued config snapshots until the watcher records a direct apply.
    /// The fence rechecks disk afterward. A valid state overwritten by an
    /// invalid file before that recheck is never applied: the direct reload's
    /// config stays current, just as an invalid file keeps the current config.
    applied_reload: Option<u64>,
    /// A directory-changing snapshot refused by the gate, retried on reopen.
    deferred_config: Option<(Arc<Config>, Option<String>)>,
    #[cfg(test)]
    config_applies: usize,
    /// Reads the model catalogs from their sources.
    catalogs: CatalogRuntime,
    /// Where the catalogs say which providers' models changed.
    catalog_sender: mpsc::UnboundedSender<Vec<String>>,
    /// The providers whose catalog models changed, waiting for the loop.
    catalog_changes: mpsc::UnboundedReceiver<Vec<String>>,
    /// Runs where `upsert` decides between a stale credential and a
    /// replacement.
    #[cfg(test)]
    upsert_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    file_config_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    directory_apply_hook: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    #[cfg(test)]
    shutdown_hooks: Option<(Arc<tokio::sync::Notify>, Arc<std::sync::atomic::AtomicBool>)>,
}

// Credential-backed model capabilities use the same immutable Claude
// executor snapshot that the service registers at startup and on reload.
struct ExecutorCatalog {
    registry: Arc<ModelRegistry>,
    manager: Manager,
    claude: std::sync::RwLock<Option<Arc<ClaudeExecutor>>>,
}

impl ModelCatalog for ExecutorCatalog {
    fn model_providers(&self, model: &str) -> Vec<String> {
        self.registry.model_providers(model)
    }

    fn first_available_model(&self) -> Option<String> {
        self.registry.first_available_model()
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        self.registry.available_models()
    }

    fn model_info(&self, model: &str, provider: &str) -> Option<ModelInfo> {
        self.registry.model_info(model, provider)
    }

    fn supports_native_tool_search(&self, model: &str) -> bool {
        let executor = self
            .claude
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        executor.is_some_and(|executor| {
            self.manager
                .supports_native_tool_search_for_model(model, &|auth, upstream| {
                    go::equal_fold(auth.provider.trim(), "claude")
                        && executor.supports_native_tool_search(auth, upstream)
                })
        })
    }
}

impl Service {
    /// A service on an open gate, for the tests that don't go through a
    /// first activation.
    #[cfg(test)]
    fn new(
        config: Arc<Config>,
        config_path: PathBuf,
        auth_dir: PathBuf,
        log_level: LogLevel,
    ) -> Self {
        Self::with_gate(config, config_path, auth_dir, log_level, ActiveGate::open())
    }

    /// As `Service::new` (test-only), with every writer sharing `gate`.
    #[cfg(test)]
    fn with_gate(
        config: Arc<Config>,
        config_path: PathBuf,
        auth_dir: PathBuf,
        log_level: LogLevel,
        gate: ActiveGate,
    ) -> Self {
        Self::with_probe(
            config,
            config_path,
            auth_dir,
            log_level,
            gate,
            crate::scope::runtime_probe(),
        )
    }

    fn with_probe(
        config: Arc<Config>,
        config_path: PathBuf,
        auth_dir: PathBuf,
        log_level: LogLevel,
        gate: ActiveGate,
        probe: crate::scope::RuntimeProbe,
    ) -> Self {
        let read_probe = Arc::clone(&probe);
        let directory_probe = Arc::clone(&probe);
        let auth_dir = absolute_dir(auth_dir);
        let registry = Arc::new(ModelRegistry::new());
        let store = Arc::new(
            FileStore::new(&auth_dir)
                .with_file_guard(crate::scope::credential_file_guard(Arc::clone(&probe))),
        );
        store.set_gate(gate.clone());
        let manager = Manager::new(
            Settings::from(&*config),
            Arc::clone(&registry) as _,
            Some(Arc::clone(&store) as _),
        );
        let observability = observability::build(&config, &config_path, log_level.file_log());
        manager.set_gate(gate.clone());
        manager.set_error_events(observability.usage.error_events());
        let executor_catalog = Arc::new(ExecutorCatalog {
            registry: Arc::clone(&registry),
            manager: manager.clone(),
            claude: std::sync::RwLock::new(None),
        });
        let state = AppState::new(
            ServerConfig::from(&*config),
            Arc::new(manager.clone()),
            Arc::clone(&executor_catalog) as _,
        )
        .with_observability(observability.clone());
        let (requests, sync_requests) = mpsc::channel(SYNC_QUEUE);
        let (reloads, reload_requests) = mpsc::channel(RELOAD_QUEUE);
        let (catalog_sender, catalog_changes) = mpsc::unbounded_channel();
        let config_writer = Arc::new(FileConfigWriter::new(config_path).with_gate(gate.clone()));
        let checked_config_path = config_writer.path().to_path_buf();
        let (read_sender, read_completions) = mpsc::unbounded_channel();
        let management = ManagementState::new(
            Arc::clone(&config),
            manager.clone(),
            Arc::clone(&registry),
            management_password_from_env(),
        )
        .with_store(Arc::clone(&store))
        .with_sync(Arc::new(SyncSender { requests }))
        .with_config_writer(config_writer)
        .with_config_reload(Arc::new(ReloadSender { requests: reloads }))
        .with_config_path(checked_config_path)
        .with_config_read_check(move |path| {
            crate::scope::authorize_file(read_probe.as_ref(), path)
                .map_err(|error| error.to_string())
        })
        .with_config_directory_check(move |path| {
            crate::scope::authorize_dir(directory_probe.as_ref(), path)
                .map_err(|error| error.to_string())
        })
        .with_observability(observability.clone())
        .with_gate(gate.clone());
        Self {
            config,
            claude_guard: crate::scope::claude_spawn_guard(Arc::clone(&probe)),
            probe,
            auth_dir,
            log_level,
            gate,
            cooldown_held: false,
            store,
            manager,
            registry,
            executor_catalog,
            state,
            management,
            observability,
            ledger: Ledger::unavailable("the usage ledger wasn't opened"),
            watcher: None,
            config_auths: BTreeSet::new(),
            file_auths: HashMap::new(),
            revisions: HashMap::new(),
            config_generation: Arc::new(AtomicU64::new(0)),
            reads: HashMap::new(),
            dirty_reads: HashMap::new(),
            read_sender,
            read_completions,
            #[cfg(test)]
            read_hook: None,
            #[cfg(test)]
            command_arrival_hook: None,
            #[cfg(test)]
            completion_hook: None,
            #[cfg(test)]
            removal_hook: None,
            compat_executors: BTreeSet::new(),
            sync_requests,
            reload_requests,
            pending_reloads: VecDeque::new(),
            last_reload: 0,
            applied_reload: None,
            deferred_config: None,
            #[cfg(test)]
            config_applies: 0,
            catalogs: CatalogRuntime::global().clone(),
            catalog_sender,
            catalog_changes,
            #[cfg(test)]
            upsert_hook: None,
            #[cfg(test)]
            file_config_hook: None,
            #[cfg(test)]
            directory_apply_hook: None,
            #[cfg(test)]
            shutdown_hooks: None,
        }
    }

    /// A service on `gate` with the run's options applied and the executors
    /// registered, as an unsupervised run and a worker both start it.
    fn prepared(
        config: Arc<Config>,
        config_path: PathBuf,
        auth_dir: PathBuf,
        log_level: LogLevel,
        options: &Options,
        gate: ActiveGate,
    ) -> Self {
        Self::prepared_with_probe(
            config,
            config_path,
            auth_dir,
            log_level,
            options,
            gate,
            crate::scope::runtime_probe(),
        )
    }

    fn prepared_with_probe(
        config: Arc<Config>,
        config_path: PathBuf,
        auth_dir: PathBuf,
        log_level: LogLevel,
        options: &Options,
        gate: ActiveGate,
        probe: crate::scope::RuntimeProbe,
    ) -> Self {
        let mut service = Self::with_probe(config, config_path, auth_dir, log_level, gate, probe);
        service.management = service
            .management
            .clone()
            .with_local_password(&options.local_password);
        if let Some(sha256) = &options.config_sha256 {
            service.management = service.management.clone().with_config_sha256(sha256);
        }
        service.register_executors();
        service
    }

    /// Starts open-ferry's own update checks, when they can run.
    fn start_update_checks(&mut self) {
        self.prepare_update_checks(None);
        if let Some(updates) = self.management.updates() {
            updates.start(update_settings::between(FIRST_CHECK.0, FIRST_CHECK.1));
        }
    }

    /// Attaches the updater before any router clones the management state.
    /// `data_dir` is the supervised worker's; `None` is the user's.
    fn prepare_update_checks(&mut self, data_dir: Option<&Path>) {
        if let Some(updates) = prepare_update_checks(self.probe.as_ref(), &self.config, data_dir) {
            self.management = self.management.clone().with_updates(updates);
        }
    }

    /// Starts the credential auto-refresh.
    fn start_auto_refresh(&self) {
        if let Err(error) = self.manager.start_auto_refresh(AUTO_REFRESH_INTERVAL) {
            tracing::warn!("failed to start core auth auto-refresh: {error}");
        } else {
            tracing::info!("core auth auto-refresh started (interval=15m0s)");
        }
    }

    /// Not upstream's: reads the config and the credentials again, for a
    /// worker that retook the lease after a hand over (L9.5): the other
    /// worker may have changed both while this one held nothing. An empty or
    /// unreadable config is logged and the current one kept, as for the
    /// watcher's reloads. The gate is still closed, so a change of the auth
    /// directory waits for it to reopen.
    async fn reload_from_disk(
        &mut self,
        config_path: &Path,
        events: &mut Option<mpsc::Receiver<WatchEvent>>,
    ) {
        let loaded = load_config_with_probe(self.probe.as_ref(), config_path);
        self.reload_from_loaded(config_path, events, loaded).await;
    }

    async fn reload_from_loaded(
        &mut self,
        config_path: &Path,
        events: &mut Option<mpsc::Receiver<WatchEvent>>,
        loaded: Result<Option<(Config, String, Vec<u8>)>, String>,
    ) {
        match loaded {
            Ok(Some((config, sha256, _))) => {
                let watching = self
                    .apply_config(Arc::new(config), Some(sha256), config_path)
                    .await;
                self.follow(watching, events);
            }
            Ok(None) => tracing::debug!("ignoring empty config file"),
            Err(error) => {
                tracing::error!("failed to reload config: {error}; keeping the current one");
            }
        }
        self.load_file_auths();
    }

    /// Not upstream's: applies the config that [`reload_from_disk`] had to
    /// defer because the gate was closed, now that it is open, and reads the
    /// credentials of the auth directory it names before returning. With
    /// nothing deferred it only reads them again. Unlike a watcher's reload,
    /// it says whether the directory the config names is the one the service
    /// and the store now use: `apply_config` only logs a change it couldn't
    /// make, and a resume must not serve the old directory's credentials as
    /// the new config's. The credentials whose file is gone (deleted while
    /// this worker was handed over) are unregistered before the rest is read.
    ///
    /// [`reload_from_disk`]: Self::reload_from_disk
    async fn apply_deferred(
        &mut self,
        config_path: &Path,
        events: &mut Option<mpsc::Receiver<WatchEvent>>,
    ) -> Result<(), String> {
        if let Some((config, sha256)) = self.deferred_config.take() {
            let watching = self.apply_config(config, sha256, config_path).await;
            self.follow(watching, events);
        }
        let named = self
            .config
            .resolve_auth_dir()
            .map(absolute_dir)
            .map_err(|error| format!("the config's auth directory: {error}"))?;
        if named != self.auth_dir || self.store.base_dir() != named {
            return Err(format!(
                "the auth directory {} couldn't be taken into use",
                named.display()
            ));
        }
        // Listed before anything is unregistered or acknowledged: a
        // directory that can't be listed (replaced by a file, no
        // permission) must fail the resume, not look like an empty one.
        let listed = try_synthesize_file_auths(&self.synthesis_context()).map_err(|error| {
            format!(
                "the auth directory {} couldn't be read: {error}",
                named.display()
            )
        })?;
        self.unregister_vanished_file_auths();
        self.register_file_auths(listed);
        Ok(())
    }

    /// Unregisters the file credentials whose file no longer exists.
    fn unregister_vanished_file_auths(&mut self) {
        let gone: Vec<(PathBuf, String)> = self
            .file_auths
            .iter()
            .filter(|(path, _)| matches!(path.try_exists(), Ok(false)))
            .map(|(path, id)| (path.clone(), id.clone()))
            .collect();
        for (path, _) in gone {
            self.remove_auth_file(&path, next_revision());
        }
    }

    /// Not upstream's: a standby's config, read again at take over (L9.1),
    /// before the first activation reads the credentials and binds the auth
    /// directory the config now names.
    ///
    /// # Errors
    /// The config file can't be read or doesn't load. An empty file keeps
    /// the current config.
    fn reload_standby(&mut self, config_path: &Path) -> Result<(), String> {
        let Some((config, sha256, _)) = load_config_with_probe(self.probe.as_ref(), config_path)?
        else {
            return Ok(());
        };
        if let Ok(dir) = config.resolve_auth_dir() {
            // Nothing is bound to the old directory yet.
            self.auth_dir = absolute_dir(dir);
        }
        self.apply_config_authorized(Arc::new(config), Some(sha256), config_path);
        Ok(())
    }

    /// Holds the cooldown store off while the service is a standby: it
    /// neither reads nor writes the file. The first activation thaws it.
    // W3 takes over and hands over through the same hold.
    fn hold_cooldown_store(&mut self) {
        open_ferry_core::manager::cooldown_store::standby(&self.manager);
        self.cooldown_held = true;
    }

    /// One piece of the loop's work: a watcher event, a credential change,
    /// a catalog change or a config reload, whichever comes first.
    async fn work(&mut self, config_path: &Path, events: &mut Option<mpsc::Receiver<WatchEvent>>) {
        let gate = self.gate.clone();
        let deadline = self.dirty_reads.values().map(|d| d.deadline).min();
        tokio::select! {
            Some(completion) = self.read_completions.recv() => self.complete_read(completion),
            _ = read_deadline(deadline) => self.retry_dirty_reads(),
            event = next_event(events) => match event {
                Some(event) => {
                    // A ticket's snapshot belongs to the work admitted when
                    // its management request arrived, even after quiescing.
                    let operation = self.pending_reloads.iter()
                        .find_map(|(_, request)| request.operation.clone());
                    let watching = match operation {
                        Some(operation) => operation.scope(self.handle(event, config_path)).await,
                        None => self.handle(event, config_path).await,
                    };
                    self.follow(watching, events);
                }
                None => {
                    tracing::warn!("file watcher stopped; config and auth changes are no longer followed");
                    self.follow(Watching::Stopped, events);
                }
            },
            _ = gate.wait_open(), if self.deferred_config.is_some()
                && events.as_ref().is_none_or(mpsc::Receiver::is_empty) => {
                let (config, sha256) = self.deferred_config.take().unwrap();
                let watching = self.apply_config(config, sha256, config_path).await;
                if self.watcher.is_none()
                    && self.deferred_config.is_none()
                    && matches!(watching, Watching::Same)
                {
                    // Defensive: unreachable with pending tickets today.
                    // follow(Stopped) settles them, and request_reload queues
                    // a ticket only when a watcher exists. Without one, no
                    // Reloaded can answer; a live watcher settles after its snapshot.
                    self.settle_reloads();
                }
                self.follow(watching, events);
            }
            Some(request) = self.sync_requests.recv() => self.apply_sync(request),
            Some(changed) = self.catalog_changes.recv() => self.refresh_catalog_models(&changed),
            Some(request) = self.reload_requests.recv() => {
                let operation = request.operation.clone();
                let watching = match operation {
                    Some(operation) => operation.scope(self.request_reload(request, config_path)).await,
                    None => self.request_reload(request, config_path).await,
                };
                self.follow(watching, events);
            }
        }
    }

    /// The first activation, shared by every run: an unsupervised run calls
    /// it at start, and a supervised active worker after taking the lease.
    /// While the gate is `Closed` it binds the auth directory under the
    /// activation, applies the config to the logs, the usage statistics and
    /// the cooldown store, reads the credentials and starts the catalogs
    /// and the ledger. It leaves the gate Closed: the caller finishes the
    /// fallible listener/watcher setup, then explicitly reopens it. W3 can
    /// reopen, accept and send ready as its final take-over step.
    ///
    /// # Errors
    /// A key for another gate, a gate that is not Closed, failure to create
    /// the auth directory, or failure to bind the store. Directory IO errors
    /// retain their own cause rather than being reported as a lease error.
    // W3 reuses it for take over.
    fn first_activation(&mut self, key: &ActivationKey) -> Result<(), String> {
        let activation = key
            .activate(&self.gate)
            .map_err(|error| error.to_string())?;
        ensure_auth_dir(&self.auth_dir).map_err(|error| {
            format!(
                "failed to create auth directory {}: {error}",
                self.auth_dir.display()
            )
        })?;
        self.store
            .set_base_dir_under(&activation, &self.auth_dir)
            .map_err(|error| error.to_string())?;
        self.start_catalogs();
        self.load_file_auths();
        self.sync_config_auths();
        self.reconfigure_observability(None);
        if std::mem::take(&mut self.cooldown_held) {
            // A held store restores from the file and takes writes again.
            open_ferry_core::manager::cooldown_store::thaw(
                &self.manager,
                open_ferry_core::manager::cooldown_store::Thaw::Restore,
            );
        }
        self.start_ledger();
        Ok(())
    }

    /// Reads the model catalogs from the sources the config names, and
    /// follows them, telling the loop when a catalog changes a provider's
    /// models (upstream's `startModelCatalogUpdaters` and
    /// `registerModelRefreshCallback`).
    fn start_catalogs(&self) {
        let changes = self.catalog_sender.clone();
        self.catalogs.set_listener(Some(Arc::new(move |providers| {
            let _ = changes.send(providers);
        })));
        self.catalogs.start(&self.config.models);
    }

    /// Stops following the catalog sources.
    fn stop_catalogs(&self) {
        self.catalogs.stop();
        self.catalogs.set_listener(None);
    }

    /// Registers again the models of each enabled credential of a provider
    /// in `changed`, whose catalog models changed (the callback of
    /// upstream's `registerModelRefreshCallback`).
    fn refresh_catalog_models(&self, changed: &[String]) {
        let mut providers = HashSet::new();
        for provider in changed {
            let name = go::to_lower(provider.trim());
            if name.is_empty() {
                continue;
            }
            if matches!(name.as_str(), "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com") {
                providers.extend(["kimi", "kimi-ai", "kimi.ai", "kimi.com"].map(str::to_owned));
            }
            providers.insert(name);
        }
        let affected: Vec<Arc<Auth>> = self
            .manager
            .list()
            .into_iter()
            .filter(|auth| {
                !auth.id.is_empty()
                    && !auth.disabled
                    && providers.contains(&go::to_lower(auth.provider.trim()))
            })
            .collect();
        if affected.is_empty() {
            return;
        }
        let rules = self.rules();
        let catalog = self.catalogs.general();
        for auth in &affected {
            self.registry.register_auth_with(auth, &rules, &catalog);
            self.manager.reconcile_registry_model_states(&auth.id);
        }
        tracing::info!(
            "re-registered models for {} auth(s) due to model catalog changes: [{}]",
            affected.len(),
            changed.join(" ")
        );
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

    /// Opens the usage ledger in the log directory, and records the usage
    /// records into it from now on. It makes the directory and starts
    /// observing the records before the server serves, but opens the file
    /// on a thread of its own, so the server doesn't wait on the disk to
    /// start; the dashboard's usage routes wait for it instead. A ledger
    /// that can't be opened is unavailable, and the server runs on without
    /// it.
    fn start_ledger(&mut self) {
        self.ledger = match &self.observability.log_dir {
            Some(dir) => Ledger::start(dir, &self.observability.usage),
            None => Ledger::unavailable("there is no log directory"),
        };
    }

    /// The proxy's routes, with the management API's and the dashboard's
    /// beside them.
    #[cfg(test)]
    fn app(&self) -> axum::Router {
        self.app_with(axum::Router::new())
    }

    /// The proxy's routes, with the management API's, the dashboard's and
    /// `extra` beside them.
    fn app_with(&self, extra: axum::Router) -> axum::Router {
        let management = open_ferry_management::router(self.management.clone());
        let dashboard = open_ferry_dashboard::router_for_guarded(
            self.management.clone(),
            self.ledger.clone(),
            open_ferry_dashboard::Listener::Shared,
            self.claude_guard.clone(),
        );
        router_with(self.state.clone(), management.merge(dashboard).merge(extra))
    }

    /// Registers the executors for the current config: Codex, Meta, Claude,
    /// claude-cli, Gemini, Vertex AI, and the OpenAI-compatible ones (see
    /// [`Self::register_compat_executors`]).
    fn register_executors(&mut self) {
        self.register_native_executors();
        self.register_compat_executors();
    }

    /// Registers the Codex, Meta, Claude, claude-cli, Gemini, Gemini
    /// Interactions, Vertex AI and xAI executors for the current config.
    fn register_native_executors(&self) {
        let proxy_url = self.config.proxy_url.clone();
        self.register_codex_executor();
        self.register_meta_executor();
        self.register_claude_executor();
        // It sends nothing itself, so takes no proxy.
        self.manager.register_executor(Arc::new(
            ClaudeCliExecutor::new()
                .with_config(Arc::clone(&self.config))
                .with_spawn_guard(self.claude_guard.clone()),
        ));
        self.manager.register_executor(Arc::new(
            GeminiExecutor::new(proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
        self.manager.register_executor(Arc::new(
            InteractionsExecutor::new(proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
        self.register_xai_executor();
        self.manager.register_executor(Arc::new(
            VertexExecutor::new(proxy_url)
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
    }

    /// Registers Claude and publishes the same snapshot to its model catalog.
    fn register_claude_executor(&self) {
        let executor = Arc::new(
            ClaudeExecutor::new(self.config.proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _)
                .with_model_level_cooling(self.config.claude.model_level_cooling),
        );
        self.manager.register_executor(Arc::clone(&executor) as _);
        *self
            .executor_catalog
            .claude
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(executor);
    }

    /// Registers the Codex executor for the current config.
    fn register_codex_executor(&self) {
        let mut executor = CodexExecutor::new(self.config.proxy_url.clone())
            .with_config(Arc::clone(&self.config))
            .with_models(Arc::clone(&self.registry) as _);
        if let Some(previous) = self.manager.executor("codex") {
            executor = executor.with_execution_sessions_from(&*previous);
        }
        self.manager.register_executor(Arc::new(executor));
    }

    /// Registers the Meta executor for the current config.
    fn register_meta_executor(&self) {
        self.manager.register_executor(Arc::new(
            MetaExecutor::new(self.config.proxy_url.clone())
                .with_config(Arc::clone(&self.config))
                .with_models(Arc::clone(&self.registry) as _),
        ));
    }

    /// Registers the xAI executor for the current config.
    fn register_xai_executor(&self) {
        let mut executor = XaiExecutor::new(self.config.proxy_url.clone())
            .with_config(Arc::clone(&self.config))
            .with_models(Arc::clone(&self.registry) as _);
        if let Some(previous) = self.manager.executor("xai") {
            executor = executor.with_execution_sessions_from(&*previous);
        }
        self.manager.register_executor(Arc::new(executor));
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
        ctx.file_guard = self.store.file_guard();
        ctx.oauth_excluded_models = self.config.oauth_excluded_models.clone();
        ctx
    }

    fn rules(&self) -> RegistrationRules {
        RegistrationRules::from(&*self.config)
    }

    /// Registers every credential file in the auth directory.
    fn load_file_auths(&mut self) {
        self.register_file_auths(synthesize_file_auths(&self.synthesis_context()));
    }

    /// Registers `auths`, the records of the files in the auth directory.
    fn register_file_auths(&mut self, auths: Vec<Auth>) {
        let rules = self.rules();
        for auth in auths {
            let path = PathBuf::from(auth.attribute("path").unwrap_or_default());
            let id = auth.id.clone();
            if self.manager.get(&id).is_some()
                || self
                    .reads
                    .values()
                    .any(|job| job.identity.id.as_ref() == Some(&id))
            {
                self.queue_auth_change(AuthChange {
                    change: SyncChange::Upsert(Box::new(auth)),
                    revision: next_revision(),
                    applied: None,
                    operation: Operation::current(),
                });
            } else if self.upsert(auth, &rules) && !path.as_os_str().is_empty() {
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
    fn upsert(&mut self, auth: Auth, rules: &RegistrationRules) -> bool {
        self.ensure_executor(&auth);
        // The stale check and the replacement are one step under the
        // manager's state lock: a refresh that publishes after a separate
        // read would be overwritten with this older credential.
        let mut stale = false;
        let incoming = auth.clone();
        let edited = self
            .manager
            .edit_unsaved::<std::convert::Infallible>(&auth.id, |live| {
                #[cfg(test)]
                if let Some(hook) = &self.upsert_hook {
                    hook();
                }
                if is_stale(live, &incoming) {
                    stale = true;
                    return Ok(false);
                }
                let mut auth = incoming;
                auth.rebase_pending_refreshes_from(live);
                auth.created_at = live.created_at;
                if !is_disabled(live) && !is_disabled(&auth) {
                    auth.last_refreshed_at = live.last_refreshed_at;
                    auth.next_refresh_after = live.next_refresh_after;
                    if auth.model_states.is_empty() && !live.model_states.is_empty() {
                        auth.model_states = live.model_states.clone();
                    }
                }
                *live = auth;
                Ok(true)
            });
        let (op, result) = match edited {
            Ok(Some(_)) if stale => {
                if let Some(existing) = self.manager.get(&auth.id) {
                    tracing::debug!(
                        "skipping stale auth update for {}: incoming gen={}, existing gen={}",
                        auth.id,
                        auth.generation,
                        existing.generation
                    );
                    self.registry
                        .register_auth_with(&existing, rules, &self.catalogs.general());
                    self.manager.reconcile_registry_model_states(&existing.id);
                }
                return true;
            }
            Ok(Some(_)) => ("update", Ok(())),
            Ok(None) => (
                "register",
                self.manager.register_unsaved(auth.clone()).map(|_| ()),
            ),
            Err(open_ferry_core::manager::EditError::Manager(error)) => ("update", Err(error)),
            Err(open_ferry_core::manager::EditError::Rejected(never)) => match never {},
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
        self.registry
            .register_auth_with(&auth, rules, &self.catalogs.general());
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
    async fn handle(&mut self, event: WatchEvent, config_path: &Path) -> Watching {
        match event {
            WatchEvent::ConfigChanged(config, sha256) => {
                if self.applied_reload.is_none() {
                    return self.apply_config(config, Some(sha256), config_path).await;
                }
            }
            WatchEvent::ConfigInvalid(error) => {
                tracing::error!("failed to reload config: {error}; keeping the current one");
            }
            WatchEvent::AuthAdded(file, revision) | WatchEvent::AuthChanged(file, revision) => {
                self.load_auth_file(&file, revision);
            }
            WatchEvent::AuthRemoved(path, revision) => self.remove_auth_file(&path, revision),
            WatchEvent::Reloaded(ticket) => self.reloaded(ticket),
            _ => {}
        }
        Watching::Same
    }

    /// Follows what applying an event or a reload did to the watcher. A new
    /// watcher is asked again for the reloads the old one hadn't answered;
    /// once there is none, they are answered, as the config the watcher
    /// last found is applied.
    fn follow(&mut self, watching: Watching, events: &mut Option<mpsc::Receiver<WatchEvent>>) {
        match watching {
            Watching::Same => {}
            Watching::Restarted(next) => {
                *events = Some(next);
                if let Some(watcher) = &self.watcher {
                    // Reconcile disk even when replacing a receiver discarded
                    // a newer snapshot and no management ticket is waiting.
                    watcher.reload_config(0);
                    for (ticket, request) in &self.pending_reloads {
                        ask_reload(watcher, *ticket, request.force);
                    }
                }
            }
            Watching::Stopped => {
                self.applied_reload = None;
                *events = None;
                self.watcher = None;
                self.settle_reloads();
            }
        }
    }

    /// Has the config file reloaded for the management API, as `request`
    /// asks, and tells it once the config is applied: by the watcher, so
    /// that the reload its own events for the save bring finds nothing new,
    /// or here when there is no watcher, forced or not. An empty file is
    /// skipped and one that doesn't load logged, and the current config
    /// kept, as for the watcher's reloads.
    async fn request_reload(&mut self, request: ReloadRequest, config_path: &Path) -> Watching {
        // Keep main's ticket path whenever the gate is Open. Only work
        // already admitted before quiescing needs the direct operation scope.
        if (self.gate.state() == open_ferry_core::gate::GateState::Open
            || request.operation.is_none())
            && let Some(watcher) = &self.watcher
        {
            self.last_reload += 1;
            ask_reload(watcher, self.last_reload, request.force);
            self.pending_reloads.push_back((self.last_reload, request));
            return Watching::Same;
        }
        let watching = match load_config_with_probe(self.probe.as_ref(), config_path) {
            Ok(Some((config, sha256, contents))) => {
                let watching = self
                    .apply_config(Arc::new(config), Some(sha256), config_path)
                    .await;
                if self.deferred_config.is_none()
                    && let Some(watcher) = &self.watcher
                {
                    self.last_reload += 1;
                    self.applied_reload = Some(self.last_reload);
                    watcher.record_applied_config(&contents, self.last_reload);
                }
                watching
            }
            Ok(None) => {
                tracing::debug!("ignoring empty config file");
                Watching::Same
            }
            Err(error) => {
                tracing::error!("failed to reload config: {error}; keeping the current one");
                Watching::Same
            }
        };
        // The write-then-watch caller returns when its ticket is released,
        // whether the snapshot applied or the sender was dropped as deferred.
        if self.deferred_config.is_none() {
            let _ = request.applied.send(());
        }
        watching
    }

    /// Answers the reloads up to `ticket`, which the watcher has made.
    fn reloaded(&mut self, ticket: u64) {
        if self.applied_reload.is_some_and(|applied| ticket >= applied) {
            self.applied_reload = None;
        }
        while let Some((next, _)) = self.pending_reloads.front()
            && *next <= ticket
        {
            if let Some((_, request)) = self.pending_reloads.pop_front()
                && self.deferred_config.is_none()
            {
                let _ = request.applied.send(());
            }
            // Dropping a deferred ticket releases its sender and operation.
        }
    }

    /// Answers every reload the watcher was asked for.
    fn settle_reloads(&mut self) {
        for (_, request) in self.pending_reloads.drain(..) {
            if self.deferred_config.is_none() {
                let _ = request.applied.send(());
            }
        }
    }

    /// Takes no more reloads from the management API, and answers those
    /// asked for, so no handler waits on a loop that has stopped.
    fn close_reloads(&mut self) {
        self.reload_requests.close();
        while let Ok(request) = self.reload_requests.try_recv() {
            let _ = request.applied.send(());
        }
        self.settle_reloads();
    }

    /// Applies a credential change from the management API, then says so,
    /// even when it was skipped as stale.
    fn apply_sync(&mut self, request: SyncRequest) {
        self.queue_auth_change(AuthChange {
            change: request.change,
            revision: request.revision,
            applied: Some(request.applied),
            operation: request.operation,
        });
    }

    /// Refuse queued changes and cancel observation acknowledgements at shutdown.
    /// Running workers keep their operation until they finish; no reads are started.
    fn close_sync(&mut self) {
        self.sync_requests.close();
        self.config_generation.fetch_add(1, Ordering::SeqCst);
        self.dirty_reads.clear();
        self.reads.clear();
        while self.sync_requests.try_recv().is_ok() {}
    }

    fn observation_path(&self, path: &Path) -> PathBuf {
        observation_path(&self.auth_dir, path)
    }

    fn change_identity(&self, change: &SyncChange) -> (ReadKey, ReadIdentity) {
        let (id, path) = match change.payload() {
            SyncChange::Upsert(auth) => (
                Some(auth.id.clone()),
                auth.attribute("path").map(PathBuf::from).or_else(|| {
                    (!auth.file_name.is_empty()).then(|| PathBuf::from(&auth.file_name))
                }),
            ),
            SyncChange::FileWritten(file) => (None, Some(file.path.clone())),
            SyncChange::FileRemoved(path) => (None, Some(path.clone())),
            SyncChange::Completed { .. } => unreachable!("payload is unwrapped"),
        };
        let path = path.map(|p| self.observation_path(&p));
        let path_id = path.as_ref().map(|path| {
            open_ferry_core::auth::file_store::id_for(path, &path.to_string_lossy(), &self.auth_dir)
        });
        // Resolve known id/path aliases before looking for in-flight work. A
        // path-only job remains the same key even when its read registers it.
        for (key, identity) in self
            .reads
            .iter()
            .map(|(k, j)| (k, &j.identity))
            .chain(self.dirty_reads.iter().map(|(k, d)| (k, &d.identity)))
        {
            if id
                .as_ref()
                .is_some_and(|id| identity.id.as_ref() == Some(id))
                || path_id
                    .as_ref()
                    .is_some_and(|id| identity.id.as_ref() == Some(id))
                || path
                    .as_ref()
                    .is_some_and(|path| identity.path.as_ref() == Some(path))
            {
                return (key.clone(), identity.clone());
            }
        }
        let live = id.as_ref().and_then(|id| self.manager.get(id)).or_else(|| {
            path.as_ref().and_then(|path| {
                self.file_auths
                    .iter()
                    .find_map(|(known, id)| {
                        (self.observation_path(known) == *path)
                            .then(|| self.manager.get(id))
                            .flatten()
                    })
                    .or_else(|| {
                        self.manager.list().into_iter().find(|auth| {
                            auth.attribute("path")
                                .map(|p| self.observation_path(Path::new(p)) == *path)
                                .unwrap_or_else(|| {
                                    !auth.file_name.is_empty()
                                        && self.observation_path(Path::new(&auth.file_name))
                                            == *path
                                })
                        })
                    })
            })
        });
        let requested_id = id
            .clone()
            .or_else(|| live.as_ref().map(|a| a.id.clone()))
            .or_else(|| {
                path.as_ref().map(|p| {
                    open_ferry_core::auth::file_store::id_for(
                        p,
                        &p.to_string_lossy(),
                        &self.auth_dir,
                    )
                })
            });
        let registration = requested_id
            .as_ref()
            .map(|id| self.manager.observation_registration(id))
            .unwrap_or((None, 0));
        let path = path.or_else(|| {
            live.as_ref()
                .and_then(|auth| {
                    auth.attribute("path").map(PathBuf::from).or_else(|| {
                        (!auth.file_name.is_empty()).then(|| PathBuf::from(&auth.file_name))
                    })
                })
                .map(|path| self.observation_path(&path))
        });
        let identity = ReadIdentity {
            id: id
                .or_else(|| live.as_ref().map(|a| a.id.clone()))
                .or_else(|| {
                    path.as_ref().map(|p| {
                        open_ferry_core::auth::file_store::id_for(
                            p,
                            &p.to_string_lossy(),
                            &self.auth_dir,
                        )
                    })
                }),
            provider: live
                .as_ref()
                .map_or_else(String::new, |a| a.provider.clone()),
            path,
            file_name: live
                .as_ref()
                .map_or_else(String::new, |a| a.file_name.clone()),
            registration: registration.0,
            tombstone: registration.1,
            binding: self.store.binding_generation(),
        };
        let key = identity
            .id
            .as_ref()
            .filter(|_| live.is_some() || identity.path.is_none())
            .map_or_else(
                || ReadKey::Path(identity.path.clone().expect("file observation path")),
                |id| ReadKey::Id(id.clone()),
            );
        (key, identity)
    }

    // True only when this call applies a removal, not when it parks or rejects work.
    fn queue_auth_change(&mut self, mut change: AuthChange) -> bool {
        if let SyncChange::Completed { command, .. } = &mut change.change {
            // Release/Acquire publishes consumption without waiting on store I/O.
            if command.consumed.load(Ordering::Acquire) {
                Self::acknowledge_change(change);
                return false;
            }
            // Arrival order, not the potentially stale trigger revision, chooses
            // the newest authority when aliases and parked work coalesce.
            command.arrival = next_revision();
        }
        #[cfg(test)]
        if let Some(hook) = self.command_arrival_hook.take() {
            hook();
        }
        // An event after a registration/origin change starts a new observation,
        // rather than inheriting the canceled dirty observation's context.
        let stale: Vec<_> = self
            .dirty_reads
            .iter()
            .filter(|(_, dirty)| !self.identity_is_current(&dirty.identity))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.dirty_reads.remove(&key);
        }
        let (key, identity) = self.change_identity(&change.change);
        let origin = self.change_origin(&change.change);
        let recovery_floor = self
            .reads
            .get(&key)
            .filter(|job| self.identity_is_current(&job.identity))
            .and_then(|job| job.recovery_floor.as_ref())
            .or_else(|| self.dirty_reads.get(&key)?.recovery_floor.as_ref());
        if change.revision != 0
            && recovery_floor
                .is_some_and(|floor| floor.origin == origin && change.revision <= floor.revision)
        {
            // Reject the covered payload, never its command; neither the floor
            // nor the deadline is changed by attaching token-free authority.
            if let Some(command) = change.change.take_command() {
                if let Some(job) = self.reads.get_mut(&key) {
                    job.pending_command =
                        CommandSlot::newest(job.pending_command.take(), Some(command));
                } else if let Some(dirty) = self.dirty_reads.get_mut(&key) {
                    dirty.command = CommandSlot::newest(dirty.command.take(), Some(command));
                }
            }
            Self::acknowledge_change(change);
            return false;
        }
        if let Some(job) = self.reads.get_mut(&key) {
            job.parked.push_back(change);
            return false;
        }
        let watermark_key = identity.id.clone().unwrap_or_else(|| {
            identity
                .path
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        });
        let watermark = self
            .revisions
            .get(&watermark_key)
            .copied()
            .unwrap_or(0)
            .max(
                identity
                    .path
                    .as_ref()
                    .and_then(|p| self.revisions.get(p.to_string_lossy().as_ref()))
                    .copied()
                    .unwrap_or(0),
            );
        if change.revision != 0 && change.revision <= watermark {
            if let Some(command) = change.change.take_command() {
                // Like a consumed arrival (E2), a consumed stale command is
                // a no-op: it attaches neither new work nor provenance.
                if command.consumed.load(Ordering::Acquire) {
                    Self::acknowledge_change(change);
                    return false;
                }
                // Stale payloads supply no hint; retain the current recovery.
                let dirty = self.dirty_reads.remove(&key);
                let floor = dirty
                    .as_ref()
                    .and_then(|dirty| dirty.recovery_floor.clone());
                let command =
                    CommandSlot::newest(dirty.and_then(|dirty| dirty.command), Some(command));
                self.start_read_with_command(
                    key.clone(),
                    identity,
                    None,
                    0,
                    change.operation,
                    command,
                );
                let job = self.reads.get_mut(&key).unwrap();
                job.recovery_floor = floor;
                job.applied = change.applied;
            } else {
                Self::acknowledge_change(change);
            }
            return false;
        }
        if let SyncChange::FileRemoved(path) = &change.change {
            let live = identity.id.as_ref().and_then(|id| self.manager.get(id));
            let live_origin = live.as_ref().and_then(|auth| {
                auth.attribute("path")
                    .map(PathBuf::from)
                    .or_else(|| {
                        (!auth.file_name.is_empty()).then(|| PathBuf::from(&auth.file_name))
                    })
                    .map(|path| self.observation_path(&path))
            });
            if live_origin.is_some_and(|origin| origin != self.observation_path(path)) {
                let path = self.observation_path(path);
                if self.file_auths.get(&path) == identity.id.as_ref() {
                    self.file_auths.remove(&path);
                }
                Self::acknowledge_change(change);
                return false;
            }
            self.dirty_reads.remove(&key);
            let claimed = self.claim_revision(&watermark_key, change.revision);
            debug_assert!(claimed, "removal revision passed the watermark check");
            if let Some(path) = &identity.path {
                self.claim_revision(&path.to_string_lossy(), change.revision);
            }
            let applied = identity.id.as_ref().is_none_or(|id| {
                let removed = live
                    .as_ref()
                    .is_none_or(|auth| self.manager.remove_observed(auth));
                if removed {
                    self.file_auths.retain(|_, known| known != id);
                    self.registry.unregister_client(id);
                }
                removed
            });
            if let Some(applied) = change.applied {
                let _ = applied.send(());
            }
            return applied;
        }
        let command = self
            .dirty_reads
            .remove(&key)
            .and_then(|dirty| dirty.command);
        // A new event resets backoff, but only a newer command replaces authority.
        self.start_read_with_command(key, identity, Some(change), 0, None, command);
        false
    }

    #[cfg(test)]
    fn start_read(
        &mut self,
        key: ReadKey,
        identity: ReadIdentity,
        change: Option<AuthChange>,
        attempt: usize,
        operation: Option<Operation>,
    ) {
        self.start_read_with_command(key, identity, change, attempt, operation, None);
    }

    fn start_read_with_command(
        &mut self,
        key: ReadKey,
        identity: ReadIdentity,
        mut change: Option<AuthChange>,
        attempt: usize,
        operation: Option<Operation>,
        command: Option<CommandSlot>,
    ) {
        debug_assert!(
            !self.dirty_reads.contains_key(&key),
            "active read has no dirty timer"
        );
        let command = CommandSlot::newest(
            command,
            change
                .as_mut()
                .and_then(|change| change.change.take_command()),
        );
        debug_assert!(
            !self.reads.contains_key(&key),
            "observation job must not be replaced"
        );
        let manager = self.manager.clone();
        let store = self.store.clone();
        let ctx = self.synthesis_context();
        let auth_dir = self.auth_dir.clone();
        let generation = self.config_generation.load(Ordering::SeqCst);
        let config_generation = self.config_generation.clone();
        let sender = self.read_sender.clone();
        let trigger_revision = change.as_ref().map_or(0, |change| change.revision);
        let (source, applied, operation) = match change {
            Some(change) => (Some(change.change), change.applied, change.operation),
            None => (None, None, operation.or_else(Operation::current)),
        };
        self.reads.insert(
            key.clone(),
            ReadJob {
                source: source.clone(),
                trigger_revision,
                recovery_floor: None,
                identity: identity.clone(),
                parked: VecDeque::new(),
                covered: Vec::new(),
                applied,
                operation: operation.clone(),
                attempt,
                command: command.clone(),
                pending_command: None,
            },
        );
        #[cfg(test)]
        let hook = self.read_hook.clone();
        #[cfg(test)]
        let completion_hook = self.completion_hook.clone();
        tokio::spawn(async move {
            let failed_key = key.clone();
            let failed_identity = identity.clone();
            let work = move || {
                let mut identity = identity;
                let mut payload_empty = false;
                let mut unsupported_empty = false;
                let mut unsupported_sourceless = false;
                let has_source = source.is_some();
                let tagged = command.is_some();
                let mut hint = match source {
                    Some(SyncChange::Upsert(auth)) => *auth,
                    Some(SyncChange::FileWritten(file)) => {
                        // Payloads identify the requested record only. Supported
                        // records obtain their credentials inside the transaction.
                        synthesize_auth_file(&ctx, &file.path, &file.data)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| {
                                payload_empty = true;
                                identity_hint(&identity, &ctx)
                            })
                    }
                    _ => identity_hint(&identity, &ctx),
                };
                hint.registration_epoch = identity.registration.unwrap_or(0);
                if !identity.provider.is_empty() {
                    hint.provider.clone_from(&identity.provider);
                }
                identity.id = Some(hint.id.clone());
                if identity.provider.is_empty() {
                    identity.provider = hint.provider.clone();
                }
                if identity.file_name.is_empty() {
                    identity.file_name = hint.file_name.clone();
                }
                let read = |live: &Auth| {
                    if live.registration_epoch != identity.registration.unwrap_or(0)
                        || store.binding_generation() != identity.binding
                        || (identity.registration.is_some()
                            && (!go::equal_fold(live.provider.trim(), identity.provider.trim())
                                || live.file_name != identity.file_name
                                || (identity.path.is_none() && live.attribute("path").is_some())
                                || identity.path.as_ref().is_some_and(|path| {
                                    observation_path(
                                        &auth_dir,
                                        Path::new(
                                            live.attribute("path").unwrap_or(&live.file_name),
                                        ),
                                    ) != *path
                                })))
                    {
                        return Ok(AdoptionRead::Invalid);
                    }
                    let status = store.read_for_adoption(live).map_err(ManagerError::Store)?;
                    // Tagged writes never publish a compatibility payload, even
                    // for providers outside the partitioned Codex contract.
                    let status = if tagged && matches!(status, AdoptionRead::Unsupported) {
                        store
                            .read_back(live)
                            .map_err(ManagerError::Store)?
                            .map_or(AdoptionRead::Missing, AdoptionRead::Record)
                    } else {
                        status
                    };
                    let status = match status {
                        AdoptionRead::Record(record) => {
                            let path = identity
                                .path
                                .as_deref()
                                .or_else(|| record.attribute("path").map(Path::new))
                                .ok_or_else(|| {
                                    ManagerError::Other("missing observation origin".into())
                                })?;
                            let bytes = serde_json::to_vec(&record.metadata).map_err(|_| {
                                ManagerError::Other("invalid observation record".into())
                            })?;
                            match synthesize_auth_file(&ctx, path, &bytes).ok().flatten() {
                                Some(mut auth) => {
                                    auth.id = live.id.clone();
                                    AdoptionRead::Record(auth)
                                }
                                None => AdoptionRead::Invalid,
                            }
                        }
                        AdoptionRead::Unsupported if payload_empty => {
                            unsupported_empty = true;
                            AdoptionRead::Invalid
                        }
                        AdoptionRead::Unsupported if !has_source => {
                            // Unchanged-generation Unsupported failures are payload errors; sourceless retries cannot repair them.
                            unsupported_sourceless = true;
                            AdoptionRead::Invalid
                        }
                        other => other,
                    };
                    #[cfg(test)]
                    if let Some(hook) = &hook {
                        hook(&identity);
                    }
                    Ok(status)
                };
                let validate = || config_generation.load(Ordering::SeqCst) == generation;
                let mut command_consumed = false;
                let result = if let Some(slot) = command {
                    // I3: command slot -> refresh mutex (when used) -> ReloadGuard
                    // -> per-id save mutex -> short state/store I/O. Blocking only;
                    // the loop never takes this mutex and no guard crosses await.
                    let mut command = slot
                        .authority
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let had_command = command.is_some();
                    let result = if had_command {
                        manager.adopt_completed_token_command_validated(
                            &hint,
                            (identity.registration, identity.tombstone),
                            &mut command,
                            read,
                            validate,
                        )
                    } else {
                        manager.adopt_observed_validated(
                            &hint,
                            (identity.registration, identity.tombstone),
                            read,
                            validate,
                        )
                    };
                    command_consumed = had_command && command.is_none();
                    if command.is_none() {
                        // Publish while the authority mutex still excludes siblings.
                        slot.consumed.store(true, Ordering::Release);
                    }
                    result
                } else {
                    manager.adopt_observed_validated(
                        &hint,
                        (identity.registration, identity.tombstone),
                        read,
                        validate,
                    )
                };
                #[cfg(test)]
                if let Some(hook) = &completion_hook {
                    hook(&identity);
                }
                ReadCompletion {
                    key,
                    identity,
                    config_generation: generation,
                    result,
                    unsupported_empty,
                    unsupported_sourceless,
                    command_consumed,
                }
            };
            let completion = match operation {
                Some(operation) => operation.run_blocking(work).await,
                None => tokio::task::spawn_blocking(work).await,
            };
            let completion = completion.unwrap_or_else(|_| ReadCompletion {
                key: failed_key,
                identity: failed_identity,
                config_generation: generation,
                result: Err(ManagerError::Other("observation worker stopped".into())),
                unsupported_empty: false,
                unsupported_sourceless: false,
                command_consumed: false,
            });
            let _ = sender.send(completion);
        });
    }

    fn identity_is_current(&self, identity: &ReadIdentity) -> bool {
        if self.store.binding_generation() != identity.binding {
            return false;
        }
        if identity.id.as_ref().is_some_and(|id| {
            self.manager.observation_registration(id) != (identity.registration, identity.tombstone)
        }) {
            return false;
        }
        let live = identity.id.as_ref().and_then(|id| self.manager.get(id));
        match (identity.registration, live) {
            (None, None) => true,
            (Some(epoch), Some(auth)) => {
                auth.registration_epoch == epoch
                    && go::equal_fold(auth.provider.trim(), identity.provider.trim())
                    && auth.file_name == identity.file_name
                    && identity.path.as_ref().map_or_else(
                        || auth.attribute("path").is_none(),
                        |path| {
                            auth.attribute("path")
                                .map(|p| self.observation_path(Path::new(p)))
                                .unwrap_or_else(|| {
                                    self.observation_path(Path::new(&auth.file_name))
                                })
                                == *path
                        },
                    )
            }
            _ => false,
        }
    }

    fn complete_read(&mut self, completion: ReadCompletion) {
        let Some(mut job) = self.reads.remove(&completion.key) else {
            return;
        };
        let taken = if completion.command_consumed {
            job.command.take()
        } else {
            None
        };
        job.command = CommandSlot::newest(job.command, job.pending_command.take());
        if !self.identity_is_current(&completion.identity) {
            job.command = job.command.map(CommandSlot::provenance_only);
        }
        let mut retry_now = false;
        if completion.result.is_err() {
            tracing::debug!(
                id = completion.identity.id.as_deref().unwrap_or_default(),
                "credential observation read failed"
            );
        }
        if completion.unsupported_empty && self.identity_is_current(&completion.identity) {
            if let Some(SyncChange::FileWritten(file)) = &job.source {
                self.queue_auth_change(AuthChange {
                    change: SyncChange::FileRemoved(file.path.clone()),
                    revision: job.trigger_revision,
                    applied: None,
                    operation: job.operation.clone(),
                });
            }
        } else {
            match completion.result {
                Ok(AdoptionResult::Published { auth, revision }) => {
                    if let Some(current) = self.manager.get(&auth.id)
                        && current.registration_epoch == auth.registration_epoch
                        && go::equal_fold(current.provider.trim(), auth.provider.trim())
                        && current.file_name == auth.file_name
                        && current.attribute("path") == auth.attribute("path")
                        && self.store.binding_generation() == completion.identity.binding
                        && (job.trigger_revision == 0 || revision > job.trigger_revision)
                        && self.claim_revision(&auth.id, revision)
                    {
                        self.ensure_executor(&current);
                        if let Some(path) = completion.identity.path.clone().or_else(|| {
                            current
                                .attribute("path")
                                .map(|path| self.observation_path(Path::new(path)))
                        }) {
                            self.claim_revision(&path.to_string_lossy(), revision);
                            if let Some(previous) =
                                self.file_auths.insert(path.clone(), auth.id.clone())
                                && previous != auth.id
                            {
                                self.remove(&previous);
                            }
                        }
                        self.registry.register_auth_with(
                            &current,
                            &self.rules(),
                            &self.catalogs.general(),
                        );
                        self.manager.reconcile_registry_model_states(&auth.id);
                    } else if let Some(current) = self.manager.get(&auth.id)
                        && current.registration_epoch == auth.registration_epoch
                    {
                        if self.store.binding_generation() != completion.identity.binding {
                            #[cfg(test)]
                            if let Some(hook) = &self.removal_hook {
                                hook();
                            }
                            if self.manager.remove_observed(&auth) {
                                self.file_auths.retain(|_, id| id != &auth.id);
                                self.registry.unregister_client(&auth.id);
                            }
                        } else {
                            retry_now = true;
                            self.file_auths.retain(|_, id| id != &auth.id);
                            job.parked.push_front(AuthChange {
                                change: SyncChange::Upsert(Box::new((*current).clone())),
                                revision: 0,
                                applied: None,
                                operation: job.operation.clone(),
                            });
                        }
                    }
                }
                Ok(AdoptionResult::Retry) | Err(_) => {
                    if !completion.unsupported_sourceless
                        && self.identity_is_current(&completion.identity)
                    {
                        retry_now = completion.config_generation
                            != self.config_generation.load(Ordering::SeqCst);
                        if !retry_now {
                            self.dirty_reads.insert(
                                completion.key.clone(),
                                DirtyRead {
                                    identity: completion.identity.clone(),
                                    recovery_floor: job.recovery_floor.clone().or_else(|| {
                                        (job.trigger_revision != 0).then(|| RecoveryFloor {
                                            origin: job
                                                .source
                                                .as_ref()
                                                .map(|source| self.change_origin(source))
                                                .unwrap_or_else(|| {
                                                    completion.identity.path.clone()
                                                }),
                                            revision: job.trigger_revision,
                                        })
                                    }),
                                    deadline: tokio::time::Instant::now()
                                        + retry_delay(job.attempt),
                                    attempt: job.attempt.saturating_add(1),
                                    command: job.command.clone(),
                                },
                            );
                        }
                    }
                }
                Ok(AdoptionResult::Unchanged | AdoptionResult::RegistrationChanged) => {}
            }
        }
        // Revision zero is an unversioned recovery, not a superseding event.
        // A positive same-origin removal can cancel it; another origin cannot.
        let origin = job
            .source
            .as_ref()
            .map(|s| self.change_origin(s))
            .unwrap_or_else(|| completion.identity.path.clone());
        let mut floors = BTreeMap::new();
        floors.insert(origin.clone(), job.trigger_revision);
        let mut removals = BTreeMap::new();
        for change in &job.parked {
            let origin = self.change_origin(&change.change);
            let revisions = if matches!(change.change, SyncChange::FileRemoved(_)) {
                &mut removals
            } else {
                &mut floors
            };
            let floor = revisions.entry(origin).or_insert(0);
            *floor = (*floor).max(change.revision);
        }
        let mut pending: Vec<AuthChange> = Vec::new();
        let mut removed = Vec::new();
        for mut change in std::mem::take(&mut job.parked) {
            let change_origin = self.change_origin(&change.change);
            let (_, identity) = self.change_identity(&change.change);
            let watermark = identity
                .id
                .as_ref()
                .and_then(|id| self.revisions.get(id))
                .copied()
                .unwrap_or(0)
                .max(
                    identity
                        .path
                        .as_ref()
                        .and_then(|p| self.revisions.get(p.to_string_lossy().as_ref()))
                        .copied()
                        .unwrap_or(0),
                );
            if change.revision != 0 && change.revision <= watermark {
                if let Some(command) = change.change.take_command() {
                    job.command = CommandSlot::newest(job.command.take(), Some(command));
                }
                Self::acknowledge_change(change);
            } else if matches!(change.change, SyncChange::FileRemoved(_)) {
                let floor = watermark
                    .max(floors.get(&change_origin).copied().unwrap_or(0))
                    .max(if origin.is_none() {
                        job.trigger_revision
                    } else {
                        0
                    });
                if (change.revision > floor || (change.revision == 0 && floor == 0))
                    && removals.get(&change_origin) == Some(&change.revision)
                    && !removed.contains(&change_origin)
                {
                    if !self.queue_auth_change(change) {
                        continue;
                    }
                    removed.push(change_origin.clone());
                    let mut keep = Vec::new();
                    for earlier in pending.drain(..) {
                        if self.change_origin(&earlier.change) == change_origin {
                            Self::acknowledge_change(earlier);
                        } else {
                            keep.push(earlier);
                        }
                    }
                    pending = keep;
                } else {
                    Self::acknowledge_change(change);
                }
            } else if self.dirty_reads.get(&completion.key).is_some_and(|dirty| {
                dirty.recovery_floor.as_ref().is_some_and(|floor| {
                    change_origin == floor.origin
                        && change.revision != 0
                        && change.revision <= floor.revision
                })
            }) {
                // The failed trigger already covers this payload, not its authority.
                if let Some(command) = change.change.take_command() {
                    job.command = CommandSlot::newest(job.command.take(), Some(command));
                }
                job.covered.push(change);
            } else if removed.contains(&change_origin) {
                Self::acknowledge_change(change);
            } else {
                pending.push(change);
            }
        }
        let reset_backoff = !pending.is_empty();
        // An id-only job has no distinct competing origin yet. Its aliased
        // path removal retires that job, including a config-rejected attempt.
        let origin_removed = removed.contains(&origin) || (origin.is_none() && !removed.is_empty());
        if origin_removed {
            job.command = None;
        }
        if let Some(dirty) = self.dirty_reads.get_mut(&completion.key) {
            dirty.command = job.command.clone();
        }
        if retry_now && !origin_removed {
            if let Some(source) = job.source.take() {
                // This source retains provenance, not the authority its read consumed.
                job.command = CommandSlot::newest(job.command.take(), taken);
                pending.insert(
                    0,
                    AuthChange {
                        change: source,
                        revision: job.trigger_revision,
                        applied: job.applied.take(),
                        operation: job.operation.clone(),
                    },
                );
            }
            pending.splice(0..0, std::mem::take(&mut job.covered));
        } else {
            if let Some(applied) = job.applied.take() {
                let _ = applied.send(());
            }
            for change in std::mem::take(&mut job.covered) {
                Self::acknowledge_change(change);
            }
        }
        if !pending.is_empty() {
            // Preserve the first origin, notably an explicit current-origin
            // recovery. Only that origin's highest revision can cover its peers.
            let next_origin = self.change_origin(&pending[0].change);
            // Only the selected origin covers these contexts. Different-origin
            // commands stay parked until their own fresh transaction.
            for change in &mut pending {
                if self.change_origin(&change.change) == next_origin
                    && let Some(command) = change.change.take_command()
                {
                    job.command = CommandSlot::newest(job.command.take(), Some(command));
                }
            }
            // Same-origin replacements are newer: covered arrivals never park.
            if completion.unsupported_sourceless
                && !origin_removed
                && self.identity_is_current(&completion.identity)
                && let Some(floor) = &job.recovery_floor
                && next_origin != floor.origin
            {
                self.claim_recovery_floor(&completion.identity, floor);
            }
            let selected = pending
                .iter()
                .enumerate()
                .filter(|(_, c)| self.change_origin(&c.change) == next_origin)
                .max_by_key(|(_, c)| c.revision)
                .map(|(i, _)| i)
                .unwrap();
            let change = pending.remove(selected);
            let (key, identity) = self.change_identity(&change.change);
            if let Some(active) = self.reads.get_mut(&key) {
                active.pending_command =
                    CommandSlot::newest(active.pending_command.take(), job.command);
                active.parked.push_back(change);
                active.parked.extend(pending);
                return;
            }
            let (covered, parked): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(|c| self.change_origin(&c.change) == next_origin);
            self.dirty_reads.remove(&key);
            self.start_read_with_command(
                key.clone(),
                identity,
                Some(change),
                if retry_now && !reset_backoff {
                    job.attempt
                } else {
                    0
                },
                job.operation,
                job.command,
            );
            let next = self.reads.get_mut(&key).unwrap();
            next.covered = covered;
            next.parked = parked.into();
        } else if (retry_now
            || (CommandSlot::is_live(&job.command)
                && !self.dirty_reads.contains_key(&completion.key)))
            && !origin_removed
            && !self.reads.contains_key(&completion.key)
        {
            self.start_read_with_command(
                completion.key.clone(),
                completion.identity,
                None,
                job.attempt,
                job.operation,
                job.command,
            );
            let next = self.reads.get_mut(&completion.key).unwrap();
            next.applied = job.applied;
            next.recovery_floor = job.recovery_floor;
        } else if completion.unsupported_sourceless
            && !origin_removed
            && self.identity_is_current(&completion.identity)
            && let Some(floor) = job.recovery_floor
        {
            debug_assert!(!self.reads.contains_key(&completion.key));
            self.claim_recovery_floor(&completion.identity, &floor);
        }
    }

    fn claim_recovery_floor(&mut self, identity: &ReadIdentity, floor: &RecoveryFloor) {
        // Settle ordering without publishing the sourceless identity hint.
        if let Some(id) = &identity.id {
            self.claim_revision(id, floor.revision);
        }
        if let Some(path) = &identity.path {
            self.claim_revision(&path.to_string_lossy(), floor.revision);
        }
    }

    fn change_origin(&self, change: &SyncChange) -> Option<PathBuf> {
        match change.payload() {
            SyncChange::FileWritten(file) => Some(self.observation_path(&file.path)),
            SyncChange::FileRemoved(path) => Some(self.observation_path(path)),
            SyncChange::Upsert(auth) => auth
                .attribute("path")
                .map(PathBuf::from)
                .or_else(|| (!auth.file_name.is_empty()).then(|| PathBuf::from(&auth.file_name)))
                .map(|path| self.observation_path(&path)),
            SyncChange::Completed { .. } => unreachable!("payload is unwrapped"),
        }
    }

    fn acknowledge_change(change: AuthChange) {
        if let Some(applied) = change.applied {
            let _ = applied.send(());
        }
    }

    fn retry_dirty_reads(&mut self) {
        let now = tokio::time::Instant::now();
        let due: Vec<_> = self
            .dirty_reads
            .iter()
            .filter(|(_, d)| d.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in due {
            let dirty = self.dirty_reads.remove(&key).unwrap();
            if self.identity_is_current(&dirty.identity) {
                if let Some(job) = self.reads.get_mut(&key) {
                    debug_assert!(false, "dirty retry found an active read");
                    job.pending_command =
                        CommandSlot::newest(job.pending_command.take(), dirty.command);
                    if job.recovery_floor.is_none() {
                        job.recovery_floor = dirty.recovery_floor;
                    }
                    continue;
                }
                self.start_read_with_command(
                    key.clone(),
                    dirty.identity,
                    None,
                    dirty.attempt,
                    None,
                    dirty.command,
                );
                self.reads.get_mut(&key).unwrap().recovery_floor = dirty.recovery_floor;
            }
        }
    }

    fn remove_auth_file(&mut self, path: &Path, revision: u64) {
        self.queue_auth_change(AuthChange {
            change: SyncChange::FileRemoved(path.to_owned()),
            revision,
            applied: None,
            operation: Operation::current(),
        });
    }

    fn load_auth_file(&mut self, file: &AuthFile, revision: u64) {
        self.queue_auth_change(AuthChange {
            change: SyncChange::FileWritten(file.clone()),
            revision,
            applied: None,
            operation: Operation::current(),
        });
    }

    /// Includes worker publications whose completions the loop has not consumed.
    fn file_auth_ids(&self) -> BTreeSet<String> {
        self.file_auths
            .values()
            .cloned()
            .chain(
                self.manager
                    .list()
                    .into_iter()
                    .filter(|auth| auth.attribute("path").is_some())
                    .map(|auth| auth.id.clone()),
            )
            .collect()
    }

    /// Sets the attributes each file credential takes from the config
    /// again, keeping the rest, and updates those that changed (the file
    /// part of upstream's `reloadClients` and `refreshAuthState`, which
    /// rebuild the credentials of each provider whose
    /// `oauth-excluded-models` changed).
    fn apply_config_to_file_auths(&self) {
        let ctx = self.synthesis_context();
        let ids = self.file_auth_ids();
        for id in ids {
            #[cfg(test)]
            if let Some(hook) = &self.file_config_hook {
                hook();
            }
            if let Err(open_ferry_core::manager::EditError::Manager(error)) =
                self.manager.edit_unsaved(&id, |auth| {
                    Ok::<_, std::convert::Infallible>(apply_config_attributes(&ctx, auth))
                })
            {
                tracing::error!("failed to update auth {id}: {error}");
            }
        }
    }

    /// Applies a reloaded config (upstream's `applyConfigRuntime`, with the
    /// watcher's credential diff), loaded from contents with the SHA-256
    /// `sha256`, which the management API saves it over only while the file
    /// holds them.
    async fn apply_config(
        &mut self,
        config: Arc<Config>,
        sha256: Option<String>,
        config_path: &Path,
    ) -> Watching {
        if let Err(error) = authorize_reloaded_config(self.probe.as_ref(), config_path, &config) {
            tracing::error!("failed to reload config: {error}; keeping the current one");
            return Watching::Same;
        }
        let directory_changes = config
            .resolve_auth_dir()
            .map(absolute_dir)
            .is_ok_and(|dir| dir != self.auth_dir);
        let operation = if directory_changes {
            match Operation::current()
                .filter(|op| op.gate().same(&self.gate))
                .map(Ok)
                .unwrap_or_else(|| self.gate.begin_operation())
            {
                Ok(operation) => Some(operation),
                Err(error) => {
                    tracing::warn!(%error, "config auth directory change deferred until reopen");
                    self.deferred_config = Some((config, sha256));
                    return Watching::Same;
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        if directory_changes && let Some((arrived, release)) = &self.directory_apply_hook {
            arrived.notify_one();
            tokio::time::timeout(Duration::from_secs(60), release.notified())
                .await
                .expect("directory apply hook released within 60s");
        }
        in_scope(operation, || {
            self.apply_config_authorized(config, sha256, config_path)
        })
        .await
    }

    fn apply_config_authorized(
        &mut self,
        config: Arc<Config>,
        sha256: Option<String>,
        config_path: &Path,
    ) -> Watching {
        #[cfg(test)]
        {
            self.config_applies += 1;
        }
        self.config_generation.fetch_add(1, Ordering::SeqCst);
        self.deferred_config = None;
        let applied_hash = sha256.clone();
        let previous = std::mem::replace(&mut self.config, config);
        let config = Arc::clone(&self.config);
        self.log_level.set_debug(config.debug);
        if (&previous.host, previous.port, &previous.tls)
            != (&config.host, config.port, &config.tls)
        {
            tracing::warn!("host, port and tls changes take effect after a restart");
        }
        management_listener::warn_on_change(&previous, &config);
        self.manager.set_settings(Settings::from(&*config));
        self.state.set_config(ServerConfig::from(&*config));
        self.management
            .set_loaded_config(Arc::clone(&config), sha256);
        if let Some(updates) = self.management.updates() {
            updates.set_config(&config);
        }
        // Before any credential is registered, so they get its models.
        self.catalogs.update(&config.models);
        if previous.claude_cli != config.claude_cli {
            claude_cli::warn_outdated_guarded(&config.claude_cli, self.claude_guard.clone());
        }
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
            // Claude's switch, native key flags and payload rules follow the
            // whole config, alongside the Codex, Meta and xAI executors.
            self.register_claude_executor();
            self.register_codex_executor();
            self.register_meta_executor();
            self.register_xai_executor();
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
                let authorized = self.gate.write_permit();
                if let Err(error) = &authorized {
                    tracing::warn!(%error, "auth directory not changed");
                } else if let Err(error) = ensure_auth_dir(&auth_dir) {
                    tracing::error!(
                        "failed to create auth directory {}: {error}",
                        auth_dir.display()
                    );
                } else if let Err(error) = self.store.try_set_base_dir(&auth_dir) {
                    // The store keeps its directory, so the credentials and
                    // the watcher stay where they were.
                    tracing::warn!(%error, "auth directory not changed");
                } else {
                    let ids = self.file_auth_ids();
                    self.auth_dir = auth_dir;
                    self.file_auths.clear();
                    for id in ids {
                        self.remove(&id);
                    }
                    // The new watcher reports every file in the new directory.
                    self.applied_reload = None;
                    self.watcher = None;
                    match start_watcher(
                        config_path,
                        &config,
                        Arc::clone(&self.probe),
                        self.store.file_guard(),
                    ) {
                        Ok((watcher, events)) => {
                            if let Some(hash) = &applied_hash {
                                // Seed from the applied snapshot, not a fresh
                                // read which might already contain newer bytes.
                                watcher.record_applied_config_sha256(hash, 0);
                            }
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
            }
            Ok(_) => {}
            Err(error) => tracing::error!("failed to resolve auth directory: {error}"),
        }

        // New providers get executors as their credentials are registered.
        self.sync_config_auths();
        self.prune_compat_executors();
        self.apply_config_to_file_auths();
        let rules = self.rules();
        let catalog = self.catalogs.general();
        for auth in self.manager.list() {
            self.registry.register_auth_with(&auth, &rules, &catalog);
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

/// Has `watcher` reload the config file for reload `ticket`, forced with
/// `force`.
fn ask_reload(watcher: &ConfigWatcher, ticket: u64, force: bool) {
    if force {
        watcher.force_reload_config(ticket);
    } else {
        watcher.reload_config(ticket);
    }
}

/// The config file at `path`, loaded as the watcher loads it after a
/// change: with its `auth-dir` resolved, and the SHA-256 of the contents it
/// loaded from; `None` when the file is empty, which the watcher skips.
fn load_config_with_probe(
    probe: &dyn crate::scope::Probe,
    path: &Path,
) -> Result<Option<(Config, String, Vec<u8>)>, String> {
    let data = crate::settings::read(probe, path)?;
    if data.is_empty() {
        return Ok(None);
    }
    let mut config = Config::load_bytes(&data).map_err(|error| error.to_string())?;
    authorize_config_auth_dir(probe, &config)?;
    match config.resolve_auth_dir() {
        Ok(dir) => {
            config.auth_dir = dir.to_string_lossy().into_owned();
        }
        Err(error) => tracing::error!("failed to resolve auth directory from config: {error}"),
    }
    Ok(Some((
        config,
        open_ferry_core::config::save::sha256_hex(&data),
        data,
    )))
}

/// Checks the auth placement of every reload, including watcher snapshots.
fn authorize_config_auth_dir(
    probe: &dyn crate::scope::Probe,
    config: &Config,
) -> Result<(), String> {
    authorize_resolved_auth_dir(
        probe,
        config.resolve_auth_dir().map_err(|error| error.to_string()),
    )
}

fn authorize_resolved_auth_dir(
    probe: &dyn crate::scope::Probe,
    dir: Result<PathBuf, String>,
) -> Result<(), String> {
    match dir {
        Ok(dir) => crate::scope::authorize_auth_dir(probe, &dir).map_err(|error| error.to_string()),
        Err(reason) if probe.privileged()? => Err(format!(
            "Refusing auth directory: {reason}; set auth-dir to a full path"
        )),
        Err(_) => Ok(()),
    }
}

/// The shared factory for initial, restarted and supervised watchers.
fn start_watcher(
    path: &Path,
    config: &Config,
    probe: crate::scope::RuntimeProbe,
    file_guard: Option<open_ferry_core::auth::file_guard::FileGuard>,
) -> Result<(ConfigWatcher, mpsc::Receiver<WatchEvent>), open_ferry_core::config::WatchError> {
    ConfigWatcher::start_with_guards(path, config, Some(config_read_guard(probe)), file_guard)
}

/// The watcher's check before each read of the config file: the trust check
/// the first load passed, so a privileged process never reads a config file
/// it wouldn't start from.
fn config_read_guard(probe: crate::scope::RuntimeProbe) -> ReadGuard {
    Arc::new(move |path: &Path| authorize_config_read(probe.as_ref(), path))
}

/// [`config_read_guard`]'s check with `probe`.
fn authorize_config_read(probe: &dyn crate::scope::Probe, path: &Path) -> Result<(), String> {
    match crate::scope::authorize_file(probe, path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("config file {} doesn't exist", path.display())),
        Err(error) => Err(error.to_string()),
    }
}

/// Checks a watcher snapshot again before any of its settings are applied.
fn authorize_reloaded_config(
    probe: &dyn crate::scope::Probe,
    path: &Path,
    config: &Config,
) -> Result<(), String> {
    if !crate::scope::authorize_file(probe, path).map_err(|error| error.to_string())? {
        return Err(format!("config file {} doesn't exist", path.display()));
    }
    authorize_config_auth_dir(probe, config)
}

#[cfg(test)]
mod config_trust_tests {
    fn refused_runtime() -> (tempfile::TempDir, PathBuf, Service) {
        use crate::scope::runtime_test_probe::{RuntimeFake, scratch_dir};
        let dir = scratch_dir();
        let path = dir.path().join("config.yaml");
        let old = Arc::new(
            Config::parse(format!(
                "auth-dir: '{}'\napi-keys: [old]\nremote-management:\n  secret-key: test-key\n",
                dir.path().display()
            ))
            .unwrap(),
        );
        std::fs::write(
            &path,
            format!("auth-dir: '{}'\napi-keys: [new]\n", dir.path().display()),
        )
        .unwrap();
        let platform = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        };
        let mut fake = Fake::new(platform, true);
        fake.file(path.to_str().unwrap())
            .dir(dir.path().to_str().unwrap())
            .untrust(path.to_str().unwrap());
        let probe: crate::scope::RuntimeProbe = Arc::new(RuntimeFake::new(fake));
        let service = Service::with_probe(
            old,
            path.clone(),
            dir.path().to_owned(),
            LogLevel::detached(),
            ActiveGate::open(),
            probe,
        );
        (dir, path, service)
    }

    #[test]
    fn an_injected_privileged_probe_refuses_the_startup_load() {
        let (_dir, path, service) = refused_runtime();
        assert!(
            load_config_with_probe(service.probe.as_ref(), &path)
                .unwrap_err()
                .contains("Refusing to load")
        );
        assert!(
            crate::settings::load_with_sha256(service.probe.as_ref(), &path)
                .unwrap_err()
                .contains("Refusing to load")
        );
    }

    #[tokio::test]
    async fn an_injected_privileged_probe_refuses_disk_reloads() {
        let (_dir, path, mut service) = refused_runtime();
        let old = service.config.clone();
        service.reload_from_disk(&path, &mut None).await;
        assert!(Arc::ptr_eq(&old, &service.config));
        assert_eq!(service.config_applies, 0);
    }

    #[tokio::test]
    async fn an_injected_privileged_probe_refuses_snapshot_applies() {
        let (dir, path, mut service) = refused_runtime();
        let old = service.config.clone();
        let incoming = Arc::new(
            Config::parse(format!(
                "auth-dir: '{}'\napi-keys: [new]\n",
                dir.path().display()
            ))
            .unwrap(),
        );
        service.apply_config(incoming, None, &path).await;
        assert!(Arc::ptr_eq(&old, &service.config));
        assert_eq!(service.config_applies, 0);
    }

    #[tokio::test]
    async fn an_injected_privileged_probe_refuses_management_config_reads() {
        use axum::body::{Body, to_bytes};
        use tower_service::Service as _;
        let (_dir, _path, mut service) = refused_runtime();
        service.management = service.management.clone().with_local_password("test-key");
        let mut app = open_ferry_management::api_router(service.management.clone());
        let mut request = axum::http::Request::builder()
            .uri("/v0/management/config.yaml")
            .header("authorization", "Bearer test-key")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let answer = app.call(request).await.unwrap();
        assert_eq!(
            answer.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = to_bytes(answer.into_body(), usize::MAX).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("api-keys"));
    }

    /// A privileged service whose config isn't there yet, in a directory
    /// the probe trusts when `trusted`.
    fn privileged_without_a_config(trusted: bool) -> (tempfile::TempDir, PathBuf, Service) {
        use crate::scope::runtime_test_probe::{RuntimeFake, scratch_dir};
        let dir = scratch_dir();
        // The scratch helper's fallback contains ../..; only the test-generated
        // path is cleaned before the fake privileged policy sees it.
        let scratch_path = absolute_dir(dir.path().join("."));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&scratch_path, std::fs::Permissions::from_mode(0o777))
                .unwrap();
        }
        let path = scratch_path.join("config.yaml");
        let old = Arc::new(
            Config::parse(format!(
                "auth-dir: '{}'\nremote-management:\n  secret-key: test-key\n",
                scratch_path.display()
            ))
            .unwrap(),
        );
        let platform = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        };
        let mut fake = Fake::new(platform, true);
        fake.dir(scratch_path.to_str().unwrap());
        if !trusted {
            fake.untrust(scratch_path.to_str().unwrap());
        }
        let probe: crate::scope::RuntimeProbe = Arc::new(RuntimeFake::new(fake));
        let mut service = Service::with_probe(
            old,
            path.clone(),
            scratch_path,
            LogLevel::detached(),
            ActiveGate::open(),
            probe,
        );
        // Nothing runs the loop, so a write's reload mustn't wait for it.
        service.reload_requests.close();
        (dir, path, service)
    }

    #[tokio::test]
    async fn an_injected_privileged_probe_writes_a_new_config_only_in_a_trusted_directory() {
        use axum::body::{Body, to_bytes};
        use axum::http::StatusCode;
        use tower_service::Service as _;
        for trusted in [true, false] {
            let (_dir, path, mut service) = privileged_without_a_config(trusted);
            service.management = service.management.clone().with_local_password("test-key");
            let mut app = open_ferry_management::api_router(service.management.clone());
            let body = format!(
                "auth-dir: '{}'\napi-keys: [new]\n",
                service.auth_dir.display()
            );
            let mut request = axum::http::Request::builder()
                .method("PUT")
                .uri("/v0/management/config.yaml")
                .header("authorization", "Bearer test-key")
                .body(Body::from(body))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
            ));
            let answer = app.call(request).await.unwrap();
            let status = if trusted {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            let actual = answer.status();
            let body = to_bytes(answer.into_body(), 4096).await.unwrap();
            let response = String::from_utf8_lossy(&body).replace("test-key", "[redacted]");
            assert_eq!(actual, status, "trusted: {trusted}; response: {response}");
            assert_eq!(path.exists(), trusted, "trusted: {trusted}");
        }
    }

    use super::*;
    use crate::os_service::Platform;
    use crate::update::scope_probe::Fake;

    #[cfg(windows)]
    #[tokio::test]
    async fn a_refused_reload_keeps_the_running_config_and_auth_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let old =
            Arc::new(Config::parse(format!("auth-dir: '{}'\n", dir.path().display())).unwrap());
        let mut service = Service::new(
            old.clone(),
            path.clone(),
            dir.path().to_owned(),
            LogLevel::detached(),
        );
        let mut probe = Fake::new(Platform::Windows, true);
        probe
            .file(path.to_str().unwrap())
            .untrust(path.to_str().unwrap());
        std::fs::write(&path, "auth-dir: C:/ProgramData/open-ferry/auth\n").unwrap();
        service
            .reload_from_loaded(&path, &mut None, load_config_with_probe(&probe, &path))
            .await;
        assert!(Arc::ptr_eq(&old, &service.config));
        assert_eq!(service.auth_dir, dir.path());
        assert_eq!(service.config_applies, 0);
        probe.untrusted.clear();
        std::fs::write(&path, "auth-dir: C:/ProgramData/open-ferry/versions\n").unwrap();
        service
            .reload_from_loaded(&path, &mut None, load_config_with_probe(&probe, &path))
            .await;
        assert!(Arc::ptr_eq(&old, &service.config));
        assert_eq!(service.auth_dir, dir.path());
        assert_eq!(service.config_applies, 0);
    }

    #[test]
    fn unresolved_auth_directories_fail_closed_only_under_privilege() {
        let mut probe = Fake::new(Platform::Linux, true);
        let result = authorize_resolved_auth_dir(&probe, Err("HOME is unset".into()));
        assert!(result.unwrap_err().contains("HOME is unset"));
        probe.privileged = Ok(false);
        assert!(authorize_resolved_auth_dir(&probe, Err("HOME is unset".into())).is_ok());
        probe.privileged = Err("token failed".into());
        assert!(authorize_resolved_auth_dir(&probe, Err("HOME is unset".into())).is_err());
    }

    #[tokio::test]
    async fn the_shared_watcher_factory_refuses_config_and_credential_reads() {
        use crate::scope::runtime_test_probe::{RuntimeFake, scratch_dir};
        let dir = scratch_dir();
        let scratch_path = absolute_dir(dir.path().to_path_buf());
        let path = scratch_path.join("config.yaml");
        let credential = scratch_path.join("unsafe.json");
        std::fs::write(&path, "port: 9292\n").unwrap();
        std::fs::write(&credential, br#"{"type":"codex"}"#).unwrap();
        let mut config = Config::default();
        config.auth_dir = scratch_path.to_string_lossy().into_owned();
        let platform = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        };
        let mut fake = Fake::new(platform, true);
        fake.file(path.to_str().unwrap())
            .file(credential.to_str().unwrap())
            .untrust(path.to_str().unwrap())
            .untrust(credential.to_str().unwrap());
        let probe: crate::scope::RuntimeProbe = Arc::new(RuntimeFake::new(fake));
        let (watcher, mut events) = start_watcher(
            &path,
            &config,
            Arc::clone(&probe),
            crate::scope::credential_file_guard(probe),
        )
        .unwrap();
        watcher.force_reload_config(7);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, WatchEvent::Reloaded(7)), "{event:?}");
        // Initial auth events and a forced config snapshot would precede this barrier.
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_privileged_service_loads_only_trusted_credential_leaves() {
        use crate::scope::runtime_test_probe::{RuntimeFake, scratch_dir};
        let dir = scratch_dir();
        let scratch_path = absolute_dir(dir.path().to_path_buf());
        let trusted = scratch_path.join("trusted.json");
        let unsafe_path = scratch_path.join("unsafe.json");
        for path in [&trusted, &unsafe_path] {
            std::fs::write(path, br#"{"type":"codex"}"#).unwrap();
        }
        let platform = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        };
        let mut fake = Fake::new(platform, true);
        fake.file(trusted.to_str().unwrap())
            .file(unsafe_path.to_str().unwrap())
            .untrust(unsafe_path.to_str().unwrap());
        let mut config = Config::default();
        config.auth_dir = scratch_path.to_string_lossy().into_owned();
        let config = Arc::new(config);
        let mut service = Service::with_probe(
            config,
            scratch_path.join("config.yaml"),
            scratch_path.clone(),
            LogLevel::detached(),
            ActiveGate::open(),
            Arc::new(RuntimeFake::new(fake)),
        );
        service.load_file_auths();
        assert_eq!(service.file_auths.len(), 1);
        assert!(service.store.write_file("unsafe.json", b"{}").is_err());
        service.store.write_file("new.json", b"{}").unwrap();
    }

    #[test]
    fn the_watcher_reads_only_a_config_file_the_trust_check_passes() {
        let mut probe = Fake::new(Platform::Linux, true);
        let path = Path::new("/work/config.yaml");
        assert!(
            authorize_config_read(&probe, path)
                .unwrap_err()
                .contains("doesn't exist")
        );
        probe.file("/work/config.yaml");
        assert!(authorize_config_read(&probe, path).is_ok());
        probe.untrust("/work");
        assert!(
            authorize_config_read(&probe, path)
                .unwrap_err()
                .contains("Refusing to load")
        );
        assert!(authorize_config_read(&probe, Path::new("/work/../config.yaml")).is_err());
        probe.privileged = Ok(false);
        assert!(authorize_config_read(&probe, path).is_ok());
    }

    #[test]
    fn an_untrusted_watcher_snapshot_is_refused_before_its_settings_are_applied() {
        let mut probe = Fake::new(Platform::Linux, true);
        let path = Path::new("/work/config.yaml");
        let config = Config::parse("auth-dir: /srv/auth\n").unwrap();
        assert!(authorize_reloaded_config(&probe, path, &config).is_err());
        probe.file("/work/config.yaml").dir("/srv/auth");
        assert!(authorize_reloaded_config(&probe, path, &config).is_ok());
        probe.untrust("/work");
        assert!(
            authorize_reloaded_config(&probe, path, &config)
                .unwrap_err()
                .contains("Refusing to load")
        );
    }

    #[cfg(windows)]
    #[test]
    fn every_reload_checks_the_config_file_again_and_refuses_machine_record_auth_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut probe = Fake::new(Platform::Windows, true);
        let root = "C:/ProgramData/open-ferry";
        probe.dir(root);
        let old = Config::parse("auth-dir: C:/ProgramData/open-ferry/auth\n").unwrap();
        assert!(authorize_config_auth_dir(&probe, &old).is_ok());
        assert!(load_config_with_probe(&probe, &path).is_err());
        probe.file(path.to_str().unwrap());
        std::fs::write(&path, "auth-dir: C:/ProgramData/open-ferry/auth\n").unwrap();
        let loaded = load_config_with_probe(&probe, &path).unwrap().unwrap();
        assert_eq!(
            loaded.1,
            open_ferry_core::config::save::sha256_hex(&loaded.2)
        );
        probe.untrust(path.to_str().unwrap());
        assert!(
            load_config_with_probe(&probe, &path)
                .unwrap_err()
                .contains("Refusing to load")
        );
        probe.untrusted.clear();
        std::fs::write(&path, format!("auth-dir: {root}\n")).unwrap();
        let error = load_config_with_probe(&probe, &path).unwrap_err();
        assert!(error.contains("machine's records"), "{error}");
        // The watcher snapshot follows the same placement rule.
        let incoming = Config::parse(format!("auth-dir: {root}\n")).unwrap();
        assert!(authorize_config_auth_dir(&probe, &incoming).is_err());
        assert_eq!(old.auth_dir, "C:/ProgramData/open-ferry/auth");
        probe.privileged = Ok(false);
        assert!(load_config_with_probe(&probe, &path).is_ok());
    }

    #[test]
    fn watcher_snapshots_cannot_move_auth_files_among_machine_records() {
        for (platform, root) in [
            (Platform::Linux, "/var/lib/open-ferry"),
            (Platform::Windows, "C:/ProgramData/open-ferry"),
        ] {
            // Config resolves paths for this host; the placement helper's
            // string-path tests cover both platforms on every host.
            if Some(platform) != Platform::HOST {
                continue;
            }
            let mut probe = Fake::new(platform, true);
            let config = Config::parse(format!("auth-dir: '{root}'\n")).unwrap();
            assert!(authorize_config_auth_dir(&probe, &config).is_err());
            probe.dir(root);
            let config = Config::parse(format!("auth-dir: '{root}/auth/team'\n")).unwrap();
            assert!(authorize_config_auth_dir(&probe, &config).is_ok());
            probe.privileged = Ok(false);
            let config = Config::parse(format!("auth-dir: '{root}/versions'\n")).unwrap();
            assert!(authorize_config_auth_dir(&probe, &config).is_ok());
        }
    }
}

/// `dir` made absolute against the current directory and cleaned, as the
/// watcher makes it, so that both name an auth file alike; unchanged when
/// that fails, as for an empty path.
fn absolute_dir(dir: PathBuf) -> PathBuf {
    match std::path::absolute(&dir) {
        Ok(absolute) => open_ferry_core::auth::path::clean(&absolute),
        Err(_) => dir,
    }
}

/// Creates the auth directory if needed, readable by its owner only
/// (upstream's `ensureAuthDir`).
pub(crate) fn ensure_auth_dir(dir: &Path) -> io::Result<()> {
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

/// Serves `app` until `stopped` turns true, on the serve loop, which gives
/// the connections graceful shutdown then, as `axum::serve` does. If this
/// is dropped before it ends, the connections are closed.
#[cfg(test)]
async fn serve(
    listener: TcpListener,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    app: axum::Router,
    stopped: watch::Receiver<bool>,
) -> io::Result<()> {
    serve_with(
        serve_loop::ServeControl::new(),
        listener,
        tls_config,
        app,
        stopped,
    )
    .await
}

/// As `serve` (test-only), on `control`, which the run's other listeners
/// share, so one call pauses, releases or stops them all.
async fn serve_with(
    control: serve_loop::ServeControl,
    listener: TcpListener,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    app: axum::Router,
    mut stopped: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut cut_on_drop = serve_loop::CutOnDrop::new(&control);
    let mut serving = std::pin::pin!(serve_loop::run(
        listener.into_std()?,
        tls_config,
        app,
        &control
    ));
    let stop = async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };
    tokio::select! {
        result = &mut serving => {
            cut_on_drop.disarm();
            return result;
        }
        () = stop => control.stop(),
    }
    let result = serving.await;
    cut_on_drop.disarm();
    result
}

/// Turns Nagle's algorithm off on a client's connection, as Go does on
/// every connection upstream's server accepts, so that each part of a
/// streamed answer is sent as soon as it's written. With it on, each part
/// after the first waits for the client to acknowledge the one before,
/// which Linux delays for 40 ms when the client has nothing to send.
pub(crate) fn send_at_once(tcp: &tokio::net::TcpStream) {
    if let Err(error) = tcp.set_nodelay(true) {
        tracing::debug!("couldn't turn Nagle's algorithm off on a connection: {error}");
    }
}

/// Resolves on Ctrl-C, or on SIGTERM on Unix.
pub async fn shutdown_signal() {
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

    #[tokio::test]
    async fn stale_command_absorbs_floorless_dirty_recovery() {
        stale_command_absorbs_dirty(false).await;
    }

    #[tokio::test]
    async fn stale_command_absorbs_floored_dirty_recovery() {
        stale_command_absorbs_dirty(true).await;
    }

    async fn stale_command_absorbs_dirty(floored: bool) {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let first = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        write_command_login(&service, &id, false).await;
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"null").unwrap();
        let revision = next_revision();
        if !floored {
            service.revisions.insert(id.clone(), next_revision());
        }
        deliver_command(&mut service, tagged_change(&path, first), revision).await;
        let key = ReadKey::Id(id.clone());
        assert_eq!(service.dirty_reads[&key].recovery_floor.is_some(), floored);
        let floor = service.dirty_reads[&key].recovery_floor.clone();
        let stale_revision = next_revision();
        service.revisions.insert(id.clone(), next_revision());
        service.dirty_reads.get_mut(&key).unwrap().deadline = tokio::time::Instant::now();
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        std::fs::write(&path, &saved).unwrap();
        let mut held = hold_read(&mut service);
        let (applied, done) = oneshot::channel();
        service.queue_auth_change(AuthChange {
            change: tagged_change(&path, command),
            revision: stale_revision,
            applied: Some(applied),
            operation: None,
        });
        assert_eq!(
            service.reads.len() + service.dirty_reads.len(),
            1,
            "stale command has exactly one runnable job"
        );
        assert_eq!(
            service.reads[&key].attempt, 0,
            "stale command resets recovery backoff"
        );
        assert_eq!(
            service.reads[&key]
                .recovery_floor
                .as_ref()
                .map(|f| f.revision),
            floor.as_ref().map(|f| f.revision),
            "stale command preserves dirty floor"
        );
        held.entered().await;
        service.retry_dirty_reads();
        assert_eq!(
            held.calls.load(Ordering::SeqCst),
            1,
            "old deadline starts no second job"
        );
        if let Some(floor) = floor {
            let covered = service
                .manager
                .capture_token_command(&id, command_partition(false))
                .unwrap();
            let (applied, covered_done) = oneshot::channel();
            service.queue_auth_change(AuthChange {
                change: tagged_change(&path, covered),
                revision: floor.revision,
                applied: Some(applied),
                operation: None,
            });
            tokio::time::timeout(Duration::from_secs(5), covered_done)
                .await
                .expect("folded floor acknowledges covered command within 5s")
                .unwrap();
            assert!(
                service.reads[&key].pending_command.is_some()
                    && service.reads[&key].parked.is_empty(),
                "folded floor covers arrival without another read"
            );
        }
        std::fs::write(&path, saved).unwrap();
        held.release();
        drain_reads(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("folded dirty command acknowledges within 5s")
            .unwrap();
        assert_eq!(
            live_command_token(&service, &id, false),
            "L",
            "folded dirty command adopts once"
        );
        assert!(service.dirty_reads.is_empty() && service.reads.is_empty());
    }

    #[test]
    fn consumed_slot_never_replaces_retained_authority() {
        let (_dir, _path, service, id) = command_fixture();
        let mut retained = CommandSlot::new(
            service
                .manager
                .capture_token_command(&id, command_partition(false))
                .unwrap(),
        );
        retained.arrival = 1;
        let mut consumed = CommandSlot::new(
            service
                .manager
                .capture_token_command(&id, command_partition(false))
                .unwrap(),
        );
        consumed.arrival = 2;
        drop(consumed.authority.lock().unwrap().take());
        consumed.consumed.store(true, Ordering::Release);
        for (current, incoming) in [
            (retained.clone(), consumed.clone()),
            (consumed.clone(), retained.clone()),
        ] {
            let selected = CommandSlot::newest(Some(current), Some(incoming)).unwrap();
            assert!(
                Arc::ptr_eq(&selected.authority, &retained.authority),
                "consumed newest slot preserves retained authority"
            );
        }
        let mut older = consumed.clone();
        older.arrival = 0;
        for (current, incoming) in [(older.clone(), consumed.clone()), (consumed.clone(), older)] {
            let selected = CommandSlot::newest(Some(current), Some(incoming)).unwrap();
            assert_eq!(
                selected.arrival, 2,
                "two consumed slots retain newer provenance"
            );
        }
    }

    #[tokio::test]
    async fn consumed_redelivery_preserves_dirty_command() {
        consumed_redelivery(false).await;
    }

    #[tokio::test]
    async fn concurrently_consumed_covered_arrival_preserves_dirty_command() {
        consumed_redelivery(true).await;
    }

    async fn consumed_redelivery(concurrent: bool) {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let first = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        write_command_login(&service, &id, false).await;
        let change = tagged_change(&path, first);
        deliver_command(&mut service, change.clone(), next_revision()).await;
        mark_command_partition(&service, &id, false, "C").await;
        let second = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        write_command_login(&service, &id, false).await;
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"null").unwrap();
        let covered_revision = next_revision();
        deliver_command(&mut service, tagged_change(&path, second), next_revision()).await;
        let key = ReadKey::Id(id.clone());
        let dirty = &service.dirty_reads[&key];
        let retained = dirty.command.as_ref().unwrap().clone();
        let before = (
            dirty.attempt,
            dirty.deadline,
            dirty.recovery_floor.as_ref().unwrap().revision,
        );
        if concurrent {
            let SyncChange::Completed { command, .. } = &change else {
                unreachable!()
            };
            // Model consumption between taking authority and publishing its bit.
            command.consumed.store(false, Ordering::Release);
            let consumed = command.consumed.clone();
            service.command_arrival_hook =
                Some(Arc::new(move || consumed.store(true, Ordering::Release)));
        }
        let (applied, done) = oneshot::channel();
        service.queue_auth_change(AuthChange {
            change,
            revision: if concurrent {
                covered_revision
            } else {
                next_revision()
            },
            applied: Some(applied),
            operation: None,
        });
        assert!(
            service.dirty_reads.contains_key(&key),
            "consumed redelivery leaves dirty recovery parked"
        );
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("consumed redelivery acknowledges within 5s")
            .unwrap();
        let dirty = &service.dirty_reads[&key];
        assert!(
            Arc::ptr_eq(
                &dirty.command.as_ref().unwrap().authority,
                &retained.authority
            ),
            "consumed covered arrival preserves dirty authority"
        );
        assert_eq!(
            (
                dirty.attempt,
                dirty.deadline,
                dirty.recovery_floor.as_ref().unwrap().revision
            ),
            before,
            "consumed arrival changes no recovery state"
        );
        std::fs::write(&path, saved).unwrap();
        fire_dirty_deadline(&mut service).await;
        assert_eq!(
            live_command_token(&service, &id, false),
            "L",
            "retained command clears marker after consumed redelivery"
        );
    }

    #[tokio::test]
    async fn consumed_clone_falls_back_to_fresh_observation() {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        write_command_login(&service, &id, false).await;
        let change = tagged_change(&path, command);
        deliver_command(&mut service, change.clone(), next_revision()).await;
        mark_command_partition(&service, &id, false, "C").await;
        let SyncChange::Completed { command, .. } = change else {
            unreachable!()
        };
        // A sibling has taken authority but has not yet published the bit.
        command.consumed.store(false, Ordering::Release);
        let mut disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        disk["prefix"] = serde_json::json!("consumed-fresh-prefix");
        std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        let (key, identity) = service.change_identity(&SyncChange::FileWritten(auth_file(&path)));
        service.start_read_with_command(key, identity, None, 0, None, Some(command));
        drain_reads(&mut service).await;
        assert_eq!(
            service.manager.get(&id).unwrap().prefix,
            "consumed-fresh-prefix",
            "consumed clone performs fresh observed adoption"
        );
        assert_eq!(
            live_command_token(&service, &id, false),
            "C",
            "consumed clone observation clears no marker"
        );
    }

    fn consumed_unsupported_fixture() -> (tempfile::TempDir, PathBuf, Service, Auth, CommandSlot) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consumed.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let mut auth = (*service.manager.register_unsaved(auth).unwrap()).clone();
        let command = CommandSlot::new(
            service
                .manager
                .capture_token_command(&auth.id, command_partition(false))
                .unwrap(),
        );
        std::fs::write(&path, br#"{"type":"claude","access_token":"fresh-disk"}"#).unwrap();
        auth.metadata
            .insert("access_token".into(), serde_json::json!("stale-payload"));
        (dir, path, service, auth, command)
    }

    fn consume_slot(command: &CommandSlot) {
        drop(command.authority.lock().unwrap().take());
        command.consumed.store(true, Ordering::Release);
    }

    #[tokio::test]
    async fn consumed_dirty_unsupported_retry_keeps_provenance() {
        let (_dir, path, mut service, auth, command) = consumed_unsupported_fixture();
        let (key, identity) = service.change_identity(&SyncChange::Upsert(Box::new(auth.clone())));
        let floor = next_revision();
        consume_slot(&command);
        service.dirty_reads.insert(
            key.clone(),
            DirtyRead {
                identity,
                recovery_floor: Some(RecoveryFloor {
                    origin: Some(path),
                    revision: floor,
                }),
                deadline: tokio::time::Instant::now(),
                attempt: 1,
                command: Some(command),
            },
        );
        service.retry_dirty_reads();
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("consumed dirty retry completes within 5s")
                .unwrap();
        assert!(
            !completion.unsupported_sourceless,
            "consumed dirty retry uses read_back, never terminal Unsupported"
        );
        service.complete_read(completion);
        assert_eq!(
            service.manager.get(&auth.id).unwrap().metadata["access_token"],
            "fresh-disk",
            "consumed dirty retry adopts fresh read_back record"
        );
        assert_ne!(
            service.revisions.get(&auth.id),
            Some(&floor),
            "consumed dirty retry never claims recovery floor"
        );
        assert!(
            service.reads.is_empty() && service.dirty_reads.is_empty(),
            "consumed dirty retry leaves no command-only work"
        );
    }

    #[tokio::test]
    async fn consumed_arrival_never_publishes_compatibility_payload() {
        let (_dir, _path, mut service, auth, command) = consumed_unsupported_fixture();
        let consumed = command.clone();
        service.command_arrival_hook = Some(Arc::new(move || consume_slot(&consumed)));
        let id = auth.id.clone();
        deliver_command(
            &mut service,
            SyncChange::Completed {
                payload: Box::new(SyncChange::Upsert(Box::new(auth))),
                command,
            },
            next_revision(),
        )
        .await;
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "fresh-disk",
            "consumed arrival never publishes compatibility payload"
        );
    }

    #[tokio::test]
    async fn consumed_dirty_missing_retry_retains_provenance() {
        let (_dir, path, mut service, auth, command) = consumed_unsupported_fixture();
        let (key, identity) = service.change_identity(&SyncChange::Upsert(Box::new(auth.clone())));
        let floor = next_revision();
        consume_slot(&command);
        std::fs::remove_file(&path).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        service.read_hook = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        service.dirty_reads.insert(
            key.clone(),
            DirtyRead {
                identity,
                recovery_floor: Some(RecoveryFloor {
                    origin: Some(path.clone()),
                    revision: floor,
                }),
                deadline: tokio::time::Instant::now(),
                attempt: 1,
                command: Some(command.clone()),
            },
        );
        service.retry_dirty_reads();
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("consumed Missing read completes within 5s")
                .unwrap();
        assert!(
            matches!(completion.result, Ok(AdoptionResult::Retry))
                && !completion.unsupported_sourceless,
            "consumed Missing read retries through read_back"
        );
        service.complete_read(completion);
        assert!(
            service.dirty_reads[&key]
                .command
                .as_ref()
                .is_some_and(|retained| Arc::ptr_eq(&retained.authority, &command.authority)),
            "consumed Missing retry retains provenance in dirty entry"
        );
        assert!(
            service.reads.is_empty(),
            "Missing failure waits for its timer"
        );
        std::fs::write(
            &path,
            br#"{"type":"claude","access_token":"repaired-disk"}"#,
        )
        .unwrap();
        service.dirty_reads.get_mut(&key).unwrap().deadline = tokio::time::Instant::now();
        service.retry_dirty_reads();
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("consumed repaired read completes within 5s")
                .unwrap();
        assert!(
            !completion.unsupported_sourceless,
            "consumed repaired retry uses read_back, never terminal Unsupported"
        );
        service.complete_read(completion);
        assert_eq!(
            service.manager.get(&auth.id).unwrap().metadata["access_token"],
            "repaired-disk",
            "consumed Missing retry adopts repaired disk record"
        );
        assert_ne!(
            service.revisions.get(&auth.id),
            Some(&floor),
            "consumed Missing retry never claims recovery floor"
        );
        assert!(
            service.reads.is_empty() && service.dirty_reads.is_empty(),
            "consumed Missing retry leaves no command-only work"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "consumed Missing failure starts only its required retry"
        );
    }

    #[tokio::test]
    async fn consumed_arrival_config_retry_retains_provenance() {
        let (_dir, _path, mut service, auth, command) = consumed_unsupported_fixture();
        let retained = command.clone();
        let consumed = command.clone();
        service.command_arrival_hook = Some(Arc::new(move || consume_slot(&consumed)));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let generation = service.config_generation.clone();
        service.read_hook = Some(Arc::new(move |_| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                generation.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let id = auth.id.clone();
        let (applied, done) = oneshot::channel();
        service.queue_auth_change(AuthChange {
            change: SyncChange::Completed {
                payload: Box::new(SyncChange::Upsert(Box::new(auth))),
                command,
            },
            revision: next_revision(),
            applied: Some(applied),
            operation: None,
        });
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("consumed config-fenced read completes within 5s")
                .unwrap();
        assert!(
            matches!(completion.result, Ok(AdoptionResult::Retry)),
            "consumed arrival first read fails config fence"
        );
        assert!(
            !completion.command_consumed,
            "config-fenced consumed arrival does not consume the already-empty slot"
        );
        service.complete_read(completion);
        assert!(
            service
                .reads
                .values()
                .next()
                .unwrap()
                .command
                .as_ref()
                .is_some_and(|command| Arc::ptr_eq(&command.authority, &retained.authority)),
            "config-fenced consumed arrival retains provenance on requeued job"
        );
        consume_read(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("consumed config retry acknowledges within 5s")
            .unwrap();
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "fresh-disk",
            "consumed config retry never publishes compatibility payload"
        );
        assert!(
            service.reads.is_empty() && service.dirty_reads.is_empty(),
            "consumed config retry leaves no command-only work"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "consumed config failure starts only its required retry"
        );
    }

    #[tokio::test]
    async fn self_consumed_publish_retry_keeps_provenance() {
        let (_dir, path, mut service, auth, command) = consumed_unsupported_fixture();
        let retained = command.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        service.read_hook = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let id = auth.id.clone();
        let (applied, done) = oneshot::channel();
        service.queue_auth_change(AuthChange {
            change: SyncChange::Completed {
                payload: Box::new(SyncChange::Upsert(Box::new(auth))),
                command,
            },
            revision: next_revision(),
            applied: Some(applied),
            operation: None,
        });
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("self-consumed first read completes within 5s")
                .unwrap();
        let revision = match &completion.result {
            Ok(AdoptionResult::Published { revision, .. }) => *revision,
            _ => panic!("self-consumed first read publishes"),
        };
        assert!(completion.command_consumed, "first read takes its own slot");
        assert!(!CommandSlot::is_live(&Some(retained.clone())));
        std::fs::write(&path, br#"{"type":"claude","access_token":"rotated-disk"}"#).unwrap();
        service.revisions.insert(id.clone(), revision);
        service.complete_read(completion);
        let retains_provenance = service
            .reads
            .values()
            .next()
            .unwrap()
            .command
            .as_ref()
            .is_some_and(|command| Arc::ptr_eq(&command.authority, &retained.authority));
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("self-consumed publish retry completes within 5s")
                .unwrap();
        assert!(
            !completion.unsupported_sourceless,
            "self-consumed publish retry uses read_back"
        );
        service.complete_read(completion);
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "rotated-disk",
            "self-consumed publish retry never publishes stale-payload"
        );
        assert!(
            retains_provenance,
            "self-consumed publish retry retains its consumed slot on requeued job"
        );
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("self-consumed publish retry acknowledges within 5s")
            .unwrap();
        assert!(
            service.reads.is_empty() && service.dirty_reads.is_empty(),
            "self-consumed publish retry leaves no command-only work"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "self-consumed publication starts only its required retry"
        );
    }

    #[tokio::test]
    async fn self_consumed_registration_retry_keeps_provenance() {
        let (_dir, path, mut service, auth, command) = consumed_unsupported_fixture();
        let retained = command.clone();
        let id = auth.id.clone();
        service.manager.remove(&id);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        service.read_hook = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let (applied, done) = oneshot::channel();
        service.queue_auth_change(AuthChange {
            change: SyncChange::Completed {
                payload: Box::new(SyncChange::Upsert(Box::new(auth))),
                command,
            },
            revision: next_revision(),
            applied: Some(applied),
            operation: None,
        });
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("registration first read completes within 5s")
                .unwrap();
        let revision = match &completion.result {
            Ok(AdoptionResult::Published { revision, .. }) => *revision,
            _ => panic!("registration first read publishes"),
        };
        assert!(
            completion.command_consumed,
            "registration read takes its slot"
        );
        assert_eq!(completion.identity.registration, None);
        assert!(!service.identity_is_current(&completion.identity));
        std::fs::write(
            &path,
            br#"{"type":"claude","access_token":"registration-disk"}"#,
        )
        .unwrap();
        service.revisions.insert(id.clone(), revision);
        service.complete_read(completion);
        assert!(
            service
                .reads
                .values()
                .next()
                .unwrap()
                .command
                .as_ref()
                .is_some_and(|slot| slot.consumed.load(Ordering::Acquire)
                    && Arc::ptr_eq(&slot.authority, &retained.authority)),
            "stale registration retry retains taken provenance"
        );
        consume_read(&mut service).await;
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "registration-disk",
            "stale registration retry never publishes stale-payload"
        );
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("registration retry acknowledges within 5s")
            .unwrap();
        assert!(service.reads.is_empty() && service.dirty_reads.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_consumed_publish_retry_keeps_provenance() {
        let (_dir, path, mut service, auth, command) = consumed_unsupported_fixture();
        let retained = command.clone();
        let id = auth.id.clone();
        assert_eq!(auth.attribute("path"), Some(path.to_str().unwrap()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        service.read_hook = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let (applied, done) = oneshot::channel();
        service.queue_auth_change(AuthChange {
            change: SyncChange::Completed {
                payload: Box::new(SyncChange::Upsert(Box::new(auth))),
                command,
            },
            revision: next_revision(),
            applied: Some(applied),
            operation: None,
        });
        let first = tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
            .await
            .expect("two-race first read completes within 5s")
            .unwrap();
        let revision = match &first.result {
            Ok(AdoptionResult::Published { revision, .. }) => *revision,
            _ => panic!("two-race first read publishes"),
        };
        assert!(first.command_consumed);
        service.revisions.insert(id.clone(), revision);
        service.complete_read(first);
        assert!(
            service
                .reads
                .values()
                .next()
                .unwrap()
                .command
                .as_ref()
                .is_some_and(|slot| Arc::ptr_eq(&slot.authority, &retained.authority))
        );
        let second = tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
            .await
            .expect("two-race second read completes within 5s")
            .unwrap();
        assert!(matches!(
            second.result,
            Ok(AdoptionResult::Published { .. })
        ));
        assert!(!second.command_consumed, "second read has provenance only");
        std::fs::write(
            &path,
            br#"{"type":"claude","access_token":"two-race-disk"}"#,
        )
        .unwrap();
        let before = service.manager.get(&id).unwrap();
        service
            .manager
            .edit_unsaved(&id, |live| {
                live.file_name = "renamed-consumed.json".into();
                Ok::<_, ()>(true)
            })
            .unwrap();
        let edited = service.manager.get(&id).unwrap();
        assert_eq!(edited.registration_epoch, before.registration_epoch);
        assert_eq!(edited.attribute("path"), before.attribute("path"));
        assert_eq!(service.store.binding_generation(), second.identity.binding);
        assert!(!service.identity_is_current(&second.identity));
        service.complete_read(second);
        // Leave the file-name edit in place: R3 captured it synchronously.
        consume_read(&mut service).await;
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "two-race-disk",
            "two-race stale retry never publishes stale-payload"
        );
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("two-race retry acknowledges within 5s")
            .unwrap();
        assert!(service.reads.is_empty() && service.dirty_reads.is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two-race chain starts exactly three reads"
        );
    }

    #[tokio::test]
    async fn stale_pending_command_keeps_provenance_without_authority() {
        let (_dir, _path, mut service, auth, command) = consumed_unsupported_fixture();
        let retained = command.clone();
        let id = auth.id.clone();
        let (key, identity) = service.change_identity(&SyncChange::Upsert(Box::new(auth.clone())));
        service.start_read(key.clone(), identity, None, 0, None);
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("forward destination read completes within 5s")
                .unwrap();
        assert!(completion.unsupported_sourceless);
        let (applied, done) = oneshot::channel();
        // The defensive forward parks an unwrapped payload and its separate slot.
        let active = service.reads.get_mut(&key).unwrap();
        active.pending_command = Some(command);
        active.parked.push_back(AuthChange {
            change: SyncChange::Upsert(Box::new(auth)),
            revision: next_revision(),
            applied: Some(applied),
            operation: None,
        });
        service
            .manager
            .edit_unsaved(&id, |live| {
                live.file_name = "renamed-forward.json".into();
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert!(!service.identity_is_current(&completion.identity));
        service.complete_read(completion);
        assert!(
            !retained.consumed.load(Ordering::Acquire),
            "stale forward leaves original consumed flag false"
        );
        consume_read(&mut service).await;
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "fresh-disk",
            "stale forward never publishes stale-payload"
        );
        assert!(
            retained.authority.lock().unwrap().is_some(),
            "stale forward never uses original authority"
        );
        assert!(!retained.consumed.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("stale forward acknowledges within 5s")
            .unwrap();
        assert!(service.reads.is_empty() && service.dirty_reads.is_empty());
    }

    #[test]
    fn command_slot_demotion_preserves_only_provenance() {
        let (_dir, _path, _service, _auth, mut command) = consumed_unsupported_fixture();
        command.arrival = 17;
        let demoted = command.clone().provenance_only();
        assert_eq!(demoted.arrival, command.arrival);
        assert!(demoted.consumed.load(Ordering::Acquire));
        assert!(demoted.authority.lock().unwrap().is_none());
        assert!(!Arc::ptr_eq(&demoted.consumed, &command.consumed));
        assert!(!Arc::ptr_eq(&demoted.authority, &command.authority));
        assert!(!command.consumed.load(Ordering::Acquire));
        assert!(command.authority.lock().unwrap().is_some());
        consume_slot(&command);
        let consumed = command.clone().provenance_only();
        assert_eq!(consumed.arrival, command.arrival);
        assert!(Arc::ptr_eq(&consumed.consumed, &command.consumed));
        assert!(Arc::ptr_eq(&consumed.authority, &command.authority));
    }

    #[tokio::test]
    async fn consumed_only_job_starts_no_followup_or_timer() {
        let (_dir, _path, mut service, auth, command) = consumed_unsupported_fixture();
        consume_slot(&command);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        service.read_hook = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let (key, identity) = service.change_identity(&SyncChange::Upsert(Box::new(auth)));
        service.start_read_with_command(
            key.clone(),
            identity,
            None,
            0,
            None,
            Some(command.clone()),
        );
        // A sibling's consumed pending clone must not schedule another read.
        service.reads.get_mut(&key).unwrap().pending_command = Some(command);
        consume_read(&mut service).await;
        assert!(
            service.reads.is_empty(),
            "consumed-only completion starts no follow-up read"
        );
        assert!(
            service.dirty_reads.is_empty(),
            "consumed-only completion creates no dirty timer"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "consumed-only job starts exactly one read"
        );
    }

    #[tokio::test]
    async fn folded_provider_service_read_keeps_accounting() {
        let (_dir, path, mut service, id) = command_fixture();
        let mut disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        disk["prefix"] = serde_json::json!("folded-service");
        std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        let (key, mut identity) =
            service.change_identity(&SyncChange::FileWritten(auth_file(&path)));
        identity.provider = " CoDeX ".into();
        assert!(
            service.identity_is_current(&identity),
            "folded provider service identity remains current"
        );
        let manager = service.manager.clone();
        service.completion_hook = Some(Arc::new(move |_| {
            manager
                .edit_unsaved(&id, |auth| {
                    auth.provider = " CODEX ".into();
                    Ok::<_, ()>(true)
                })
                .unwrap();
        }));
        service.start_read(key, identity, None, 0, None);
        drain_reads(&mut service).await;
        assert_eq!(
            service.manager.list()[0].prefix,
            "folded-service",
            "folded provider service read publishes fresh disk"
        );
        assert_eq!(
            service.file_auths.get(&path),
            Some(&service.manager.list()[0].id),
            "folded provider publication keeps file accounting"
        );
    }

    fn command_partition(siwc: bool) -> open_ferry_core::auth::CodexCredentialPartition {
        if siwc {
            open_ferry_core::auth::CodexCredentialPartition::Siwc
        } else {
            open_ferry_core::auth::CodexCredentialPartition::Outer
        }
    }

    async fn mark_command_partition(service: &Service, id: &str, siwc: bool, token: &'static str) {
        service
            .manager
            .register_executor(Arc::new(CommandRefresher { siwc, token }));
        tokio::time::timeout(Duration::from_secs(5), service.manager.force_refresh(id))
            .await
            .expect("command fixture rotation finishes within 5s")
            .unwrap();
        assert!(
            !service
                .manager
                .get(id)
                .unwrap()
                .codex_pending_refresh
                .is_empty()
        );
        for runtime_only in [true, false] {
            service
                .manager
                .edit_unsaved(id, |auth| {
                    auth.attributes
                        .insert("runtime_only".into(), runtime_only.to_string());
                    Ok::<_, ()>(true)
                })
                .unwrap();
        }
    }

    fn command_fixture() -> (tempfile::TempDir, PathBuf, Service, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(
            dir.path(),
            "command.json",
            r#", "refresh_token":"A-refresh", "siwc":{"access_token":"S","refresh_token":"S-refresh"}"#,
        );
        let mut service = service(dir.path(), "");
        service.manager = Manager::new(
            Settings::from(&*service.config),
            service.registry.clone(),
            Some(Arc::new(ServiceRefreshStore {
                files: service.store.clone(),
                fail: std::sync::atomic::AtomicBool::new(true),
            })),
        );

        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        (dir, path, service, id)
    }

    fn live_command_token(service: &Service, id: &str, siwc: bool) -> serde_json::Value {
        let auth = service.manager.get(id).unwrap();
        if siwc {
            auth.metadata["siwc"]["access_token"].clone()
        } else {
            auth.metadata["access_token"].clone()
        }
    }

    async fn write_command_login(service: &Service, id: &str, siwc: bool) {
        let mut auth = (*service.manager.get(id).unwrap()).clone();
        if siwc {
            let mut tokens = auth.metadata["siwc"].as_object().unwrap().clone();
            tokens.insert("access_token".into(), serde_json::json!("L"));
            tokens.insert("refresh_token".into(), serde_json::json!("L-refresh"));
            // The bind's own write, under its credential lock, as the CLI and
            // management binds make it.
            let path = service.store.resolve_auth_path(&auth).unwrap();
            let guard = tokio::time::timeout(
                Duration::from_secs(5),
                open_ferry_core::auth::credential_lock::acquire_credential_lock(&path),
            )
            .await
            .expect("direct bind locks within 5s")
            .unwrap();
            service
                .store
                .bind_siwc_locked(&auth, tokens, &guard, &|_, _| Ok(()))
                .unwrap();
        } else {
            auth.metadata["access_token"] = serde_json::json!("L");
            auth.metadata["refresh_token"] = serde_json::json!("L-refresh");
            // This is the explicit login write, not a manager observation/save.
            service
                .store
                .write_file(
                    &auth.file_name,
                    &serde_json::to_vec(&auth.metadata).unwrap(),
                )
                .unwrap();
        }
    }

    fn tagged_change(
        path: &Path,
        command: open_ferry_core::manager::CompletedTokenCommand,
    ) -> SyncChange {
        SyncChange::Completed {
            payload: Box::new(SyncChange::FileWritten(AuthFile {
                path: path.to_owned(),
                data: br#"{"type":"codex","access_token":"stale-payload","refresh_token":"stale-payload-refresh"}"#.to_vec().into(),
            })),
            command: CommandSlot::new(command),
        }
    }

    async fn deliver_command(service: &mut Service, change: SyncChange, revision: u64) {
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change,
            revision,
            applied,
            operation: None,
        });
        drain_reads(service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("tagged completion acknowledges within 5s")
            .unwrap();
    }

    #[tokio::test]
    async fn tagged_unsupported_command_reads_disk_without_retry_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tagged-gemini.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let mut auth = (*service.manager.register_unsaved(auth).unwrap()).clone();
        let command = service
            .manager
            .capture_token_command(&auth.id, command_partition(false))
            .unwrap();
        std::fs::write(&path, br#"{"type":"claude","access_token":"fresh-disk"}"#).unwrap();
        auth.metadata
            .insert("access_token".into(), serde_json::json!("stale-payload"));
        let id = auth.id.clone();
        deliver_command(
            &mut service,
            SyncChange::Completed {
                payload: Box::new(SyncChange::Upsert(Box::new(auth))),
                command: CommandSlot::new(command),
            },
            next_revision(),
        )
        .await;
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "fresh-disk",
            "tagged Unsupported uses fresh disk instead of compatibility payload"
        );
        assert!(
            service.dirty_reads.is_empty() && service.reads.is_empty(),
            "tagged Unsupported settles without a command retry loop"
        );
    }

    #[tokio::test]
    async fn newest_covered_command_beats_older_parked_command() {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        mark_command_partition(&service, &id, true, "S-new").await;
        let outer = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        let siwc = service
            .manager
            .capture_token_command(&id, command_partition(true))
            .unwrap();
        write_command_login(&service, &id, false).await;
        write_command_login(&service, &id, true).await;
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"null").unwrap();
        let covered_revision = next_revision();
        service.load_auth_file(&auth_file(&path), next_revision());
        drain_reads(&mut service).await;
        let mut held = hold_read(&mut service);
        service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
        service.retry_dirty_reads();
        held.entered().await;
        service.queue_auth_change(AuthChange {
            change: tagged_change(&path, outer),
            revision: next_revision(),
            applied: None,
            operation: None,
        });
        service.queue_auth_change(AuthChange {
            change: tagged_change(&path, siwc),
            revision: covered_revision,
            applied: None,
            operation: None,
        });
        std::fs::write(&path, saved).unwrap();
        held.release();
        drain_reads(&mut service).await;
        assert_eq!(
            live_command_token(&service, &id, true),
            "L",
            "newest covered authority wins over older parked command"
        );
        assert_eq!(
            live_command_token(&service, &id, false),
            "B",
            "older parked command cannot replace newer authority"
        );
    }

    #[tokio::test]
    async fn stale_login_command_adopts_fresh_disk_after_watcher() {
        stale_command_after_watcher(false).await;
    }

    #[tokio::test]
    async fn stale_bind_command_adopts_fresh_disk_after_watcher() {
        stale_command_after_watcher(true).await;
    }

    async fn stale_command_after_watcher(siwc: bool) {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, siwc, "B").await;
        let command = service
            .manager
            .capture_token_command(&id, command_partition(siwc))
            .unwrap();
        let revision = next_revision();
        write_command_login(&service, &id, siwc).await;
        service.load_auth_file(&auth_file(&path), next_revision());
        drain_reads(&mut service).await;
        assert_eq!(
            live_command_token(&service, &id, siwc),
            "B",
            "watcher cannot clear command marker"
        );
        deliver_command(&mut service, tagged_change(&path, command), revision).await;
        assert_eq!(
            live_command_token(&service, &id, siwc),
            "L",
            "stale command clears marker through fresh read"
        );
    }

    #[tokio::test]
    async fn delayed_login_command_keeps_newer_discard_marker() {
        let (_dir, path, mut service, id) = command_fixture();
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        let revision = next_revision();
        write_command_login(&service, &id, false).await;
        mark_command_partition(&service, &id, false, "B").await;
        deliver_command(&mut service, tagged_change(&path, command), revision).await;
        assert_eq!(
            live_command_token(&service, &id, false),
            "B",
            "delayed service command keeps newer marker"
        );
    }

    #[tokio::test]
    async fn consumed_service_command_cannot_clear_another_marker() {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        write_command_login(&service, &id, false).await;
        let change = tagged_change(&path, command);
        deliver_command(&mut service, change.clone(), next_revision()).await;
        assert_eq!(live_command_token(&service, &id, false), "L");
        mark_command_partition(&service, &id, false, "C").await;
        deliver_command(&mut service, change, next_revision()).await;
        assert_eq!(
            live_command_token(&service, &id, false),
            "C",
            "consumed service command clears nothing new"
        );
    }

    #[tokio::test]
    async fn dirty_command_survives_to_thirty_second_tail() {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        write_command_login(&service, &id, false).await;
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"null").unwrap();
        deliver_command(&mut service, tagged_change(&path, command), next_revision()).await;
        for _ in 0..4 {
            fire_dirty_deadline(&mut service).await;
        }
        assert!(service.dirty_reads.values().next().unwrap().attempt >= 5);
        std::fs::write(&path, saved).unwrap();
        fire_dirty_deadline(&mut service).await;
        assert_eq!(
            live_command_token(&service, &id, false),
            "L",
            "dirty command tail clears marker without event"
        );
        assert!(service.dirty_reads.is_empty());
    }

    #[tokio::test]
    async fn accepted_removal_drops_dirty_command() {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        std::fs::write(&path, b"null").unwrap();
        deliver_command(&mut service, tagged_change(&path, command), next_revision()).await;
        service.remove_auth_file(&path, next_revision());
        assert!(
            service.dirty_reads.is_empty(),
            "accepted removal drops dirty command context"
        );
        assert!(service.manager.get(&id).is_none());
    }

    #[tokio::test]
    async fn covered_login_during_backoff_keeps_command() {
        covered_command(false).await;
    }

    #[tokio::test]
    async fn covered_login_during_sourceless_retry_keeps_command() {
        covered_command(true).await;
    }

    async fn covered_command(in_flight: bool) {
        let (_dir, path, mut service, id) = command_fixture();
        mark_command_partition(&service, &id, false, "B").await;
        let command = service
            .manager
            .capture_token_command(&id, command_partition(false))
            .unwrap();
        let tagged_revision = next_revision();
        write_command_login(&service, &id, false).await;
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"null").unwrap();
        service.load_auth_file(&auth_file(&path), next_revision());
        drain_reads(&mut service).await;
        let mut held = hold_read(&mut service);
        if in_flight {
            service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
            service.retry_dirty_reads();
            held.entered().await;
        }
        let (applied, mut done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: tagged_change(&path, command),
            revision: tagged_revision,
            applied,
            operation: None,
        });
        assert_eq!(
            done.try_recv(),
            Ok(()),
            "covered command acknowledged exactly once at arrival"
        );
        assert_eq!(
            live_command_token(&service, &id, false),
            "B",
            "covered payload is never published"
        );
        if in_flight {
            held.release();
            drain_reads(&mut service).await;
        } else {
            held.release();
        }
        std::fs::write(&path, saved).unwrap();
        fire_dirty_deadline(&mut service).await;
        assert_eq!(
            live_command_token(&service, &id, false),
            "L",
            "covered command retry clears marker"
        );
        assert!(service.dirty_reads.is_empty());
    }

    #[tokio::test]
    async fn different_origin_claim_rejects_noncurrent_identity() {
        different_origin_claim_guard(false).await;
    }

    #[tokio::test]
    async fn different_origin_claim_rejects_removed_origin() {
        different_origin_claim_guard(true).await;
    }

    async fn different_origin_claim_guard(origin_removed: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claim-a.json");
        let other = dir.path().join("claim-b.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let auth = service.manager.register_unsaved(auth).unwrap();
        let source = SyncChange::Upsert(Box::new((*auth).clone()));
        let (key, mut identity) = service.change_identity(&source);
        if origin_removed {
            // An absent registration lets a revision-zero removal settle the
            // origin without changing the identity/tombstone or claiming a rev.
            service.manager.remove(&auth.id);
            let registration = service.manager.observation_registration(&auth.id);
            identity.registration = registration.0;
            identity.tombstone = registration.1;
        }
        service.read_hook = Some(Arc::new(|_| panic!("guard fixture failed read")));
        service.start_read(
            key,
            identity,
            Some(AuthChange {
                change: source,
                revision: next_revision(),
                applied: None,
                operation: None,
            }),
            0,
            None,
        );
        drain_reads(&mut service).await;
        let mut held = hold_read(&mut service);
        service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
        service.retry_dirty_reads();
        held.entered().await;
        if origin_removed {
            service.remove_auth_file(&path, 0);
        } else {
            service.manager.register_unsaved((*auth).clone()).unwrap();
        }
        let mut replacement = (*auth).clone();
        replacement
            .attributes
            .insert("path".into(), other.to_string_lossy().into_owned());
        service.queue_auth_change(AuthChange {
            change: SyncChange::Upsert(Box::new(replacement)),
            revision: next_revision(),
            applied: None,
            operation: None,
        });
        held.release();
        consume_read(&mut service).await;
        assert!(
            !service
                .revisions
                .contains_key(path.to_string_lossy().as_ref()),
            "different-origin guard prevents recovery floor claim"
        );
        drain_reads(&mut service).await;
    }

    struct ReadHold {
        entered: Arc<tokio::sync::Notify>,
        release: Option<std::sync::mpsc::Sender<()>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ReadHold {
        async fn entered(&self) {
            tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
                .await
                .expect("read enters within 5s");
        }
        fn release(&mut self) {
            self.release.take().unwrap().send(()).unwrap();
        }
    }

    impl Drop for ReadHold {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
        }
    }

    fn hold_read(service: &mut Service) -> ReadHold {
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::channel();
        let released = std::sync::Mutex::new(released);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let arrived = entered.clone();
        let count = calls.clone();
        service.read_hook = Some(Arc::new(move |_| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                arrived.notify_one();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .expect("held read released within 60s");
            }
        }));
        ReadHold {
            entered,
            release: Some(release),
            calls,
        }
    }

    async fn consume_read(service: &mut Service) {
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("read completes within 5s")
                .unwrap();
        service.complete_read(completion);
    }

    // A deterministic deadline hook, exercised through the production select
    // branch rather than a sleep or a watcher event.
    async fn fire_dirty_deadline(service: &mut Service) {
        assert_eq!(
            service.dirty_reads.len(),
            1,
            "dirty entry survives every retry including the 30s tail"
        );
        service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
        service.work(Path::new(""), &mut None).await;
        drain_reads(service).await;
    }

    struct ServiceRefreshStore {
        files: Arc<FileStore>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl AuthStore for ServiceRefreshStore {
        fn list(&self) -> io::Result<Vec<Auth>> {
            self.files.list()
        }
        fn save(&self, auth: &Auth) -> io::Result<String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("injected rotation failure"));
            }
            self.files.save(auth)
        }
        fn delete(&self, id: &str) -> io::Result<()> {
            self.files.delete(id)
        }
        fn read_back(&self, auth: &Auth) -> io::Result<Option<Auth>> {
            self.files.read_back(auth)
        }
        fn read_for_adoption(&self, auth: &Auth) -> io::Result<AdoptionRead> {
            self.files.read_for_adoption(auth)
        }
        fn same_record(&self, a: &Auth, b: &Auth) -> io::Result<bool> {
            self.files.same_record(a, b)
        }
        fn binding_generation(&self) -> u64 {
            self.files.binding_generation()
        }
        fn supports_codex_refresh_patches(&self) -> bool {
            true
        }
    }

    struct NamedRefresher(&'static str);
    impl ProviderExecutor for NamedRefresher {
        fn id(&self) -> &str {
            "codex"
        }
        fn execute(
            &self,
            auth: Arc<Auth>,
            req: Request,
            opts: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Refresher.execute(auth, req, opts)
        }
        fn execute_stream(
            &self,
            auth: Arc<Auth>,
            req: Request,
            opts: Options,
        ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
            Refresher.execute_stream(auth, req, opts)
        }
        fn count_tokens(
            &self,
            auth: Arc<Auth>,
            req: Request,
            opts: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Refresher.count_tokens(auth, req, opts)
        }
        fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
            Box::pin(async move {
                let mut auth = (*auth).clone();
                auth.metadata
                    .insert("access_token".into(), serde_json::json!(self.0));
                auth.metadata.insert(
                    "refresh_token".into(),
                    serde_json::json!(format!("{}-refresh", self.0)),
                );
                Ok(auth)
            })
        }
    }

    struct CommandRefresher {
        siwc: bool,
        token: &'static str,
    }
    impl ProviderExecutor for CommandRefresher {
        fn id(&self) -> &str {
            "codex"
        }
        fn execute(
            &self,
            auth: Arc<Auth>,
            req: Request,
            opts: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Refresher.execute(auth, req, opts)
        }
        fn execute_stream(
            &self,
            auth: Arc<Auth>,
            req: Request,
            opts: Options,
        ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
            Refresher.execute_stream(auth, req, opts)
        }
        fn count_tokens(
            &self,
            auth: Arc<Auth>,
            req: Request,
            opts: Options,
        ) -> BoxFuture<'_, Result<Response, ExecError>> {
            Refresher.count_tokens(auth, req, opts)
        }
        fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
            Box::pin(async move {
                let mut auth = (*auth).clone();
                let metadata = if self.siwc {
                    auth.metadata
                        .get_mut("siwc")
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                } else {
                    &mut auth.metadata
                };
                metadata.insert("access_token".into(), serde_json::json!(self.token));
                metadata.insert(
                    "refresh_token".into(),
                    serde_json::json!(format!("{}-refresh", self.token)),
                );
                Ok(auth)
            })
        }
    }

    fn hold_completion(service: &mut Service) -> ReadHold {
        let hold = hold_read(service);
        service.completion_hook = service.read_hook.take();
        hold
    }

    #[tokio::test]
    async fn older_unsupported_write_during_backoff_preserves_recovery() {
        unsupported_write_during_recovery(false, false).await;
    }

    #[tokio::test]
    async fn older_unsupported_write_during_retry_preserves_recovery() {
        unsupported_write_during_recovery(true, false).await;
    }

    #[tokio::test]
    async fn newer_unsupported_write_during_backoff_replaces_recovery() {
        unsupported_write_during_recovery(false, true).await;
    }

    #[tokio::test]
    async fn newer_unsupported_write_during_retry_replaces_recovery() {
        unsupported_write_during_recovery(true, true).await;
    }

    async fn unsupported_write_during_recovery(in_flight: bool, newer: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late-write.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let auth = service.manager.register_unsaved(auth).unwrap();
        let payload = |value| AuthFile {
            path: path.clone(),
            data: serde_json::to_vec(&serde_json::json!({"type":"claude", "access_token":value}))
                .unwrap()
                .into(),
        };
        let older = next_revision();
        let trigger = next_revision();
        let late = if newer { next_revision() } else { older };
        service.read_hook = Some(Arc::new(|_| panic!("fixture failed trigger")));
        service.load_auth_file(&payload("trigger"), trigger);
        drain_reads(&mut service).await;
        let dirty = service.dirty_reads.values().next().unwrap();
        let deadline = dirty.deadline;
        let attempt = dirty.attempt;
        assert_eq!(attempt, 1);
        let mut held = hold_read(&mut service);
        if in_flight {
            service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
            tokio::time::timeout(
                Duration::from_secs(5),
                service.work(Path::new(""), &mut None),
            )
            .await
            .expect("dirty deadline is serviced within 5s");
            held.entered().await;
        }
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileWritten(payload(if newer { "newer" } else { "older" })),
            revision: late,
            applied,
            operation: None,
        });
        // In the backoff control a bad older event starts a read too. Release
        // that worker and consume it before checking the publication assertion.
        if !in_flight && !service.reads.is_empty() {
            held.entered().await;
        }
        held.release();
        drain_reads(&mut service).await;
        if newer {
            assert_eq!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "newer",
                "newer late payload replaces recovery in either window"
            );
            assert!(service.dirty_reads.is_empty());
        } else {
            assert_ne!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "older",
                "older late payload is never published during recovery"
            );
            if in_flight {
                assert_eq!(held.calls.load(Ordering::SeqCst), 1);
                // The terminal Unsupported retry settles ordering without publication.
                assert!(service.dirty_reads.is_empty());
                assert_eq!(service.revisions.get(&auth.id), Some(&trigger));
                assert_eq!(
                    service.revisions.get(path.to_string_lossy().as_ref()),
                    Some(&trigger)
                );
            } else {
                let dirty = service
                    .dirty_reads
                    .values()
                    .next()
                    .expect("older arrival keeps dirty retry");
                assert_eq!(
                    dirty.deadline, deadline,
                    "covered arrival preserves deadline"
                );
                assert_eq!(dirty.attempt, attempt, "covered arrival preserves backoff");
                assert_eq!(held.calls.load(Ordering::SeqCst), 0);
                tokio::time::timeout(Duration::from_secs(5), fire_dirty_deadline(&mut service))
                    .await
                    .expect("covered recovery finishes within 5s");
                assert!(service.dirty_reads.is_empty());
            }
            assert_eq!(
                service.manager.get(&auth.id).unwrap().metadata,
                auth.metadata
            );
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("late payload acknowledges within 5s"),
            Ok(())
        );
        assert!(service.reads.is_empty());
    }

    #[tokio::test]
    async fn different_origin_terminal_replacement_refuses_older_late_write() {
        different_origin_terminal_replacement(false).await;
    }

    #[tokio::test]
    async fn different_origin_terminal_replacement_accepts_newer_late_write() {
        different_origin_terminal_replacement(true).await;
    }

    async fn different_origin_terminal_replacement(newer: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal-origin-a.json");
        let other = dir.path().join("terminal-origin-b.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let auth = service.manager.register_unsaved(auth).unwrap();
        let payload = |value| AuthFile {
            path: path.clone(),
            data: serde_json::to_vec(&serde_json::json!({"type":"claude", "access_token":value}))
                .unwrap()
                .into(),
        };
        let older = next_revision();
        let trigger = next_revision();
        service.read_hook = Some(Arc::new(|_| panic!("fixture failed trigger")));
        service.load_auth_file(&payload("trigger"), trigger);
        drain_reads(&mut service).await;
        let mut held = hold_read(&mut service);
        service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(5),
            service.work(Path::new(""), &mut None),
        )
        .await
        .expect("different-origin terminal retry starts within 5s");
        held.entered().await;
        // A real Upsert with the same id aliases the active key, but names B.
        let mut replacement = (*auth).clone();
        replacement
            .attributes
            .insert("path".into(), other.to_string_lossy().into_owned());
        let (applied, replaced) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(replacement)),
            revision: next_revision(),
            applied,
            operation: None,
        });
        held.release();
        consume_read(&mut service).await;
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("different-origin replacement completes within 5s")
                .unwrap();
        assert_eq!(completion.identity.path.as_ref(), Some(&other));
        assert!(
            matches!(completion.result, Ok(AdoptionResult::Retry)),
            "different-origin Upsert is refused by the origin check"
        );
        service.complete_read(completion);
        assert!(
            service.dirty_reads.is_empty(),
            "refused different-origin read creates no dirty retry"
        );
        assert!(service.reads.is_empty());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), replaced)
                .await
                .expect("different-origin replacement acknowledges within 5s"),
            Ok(())
        );
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileWritten(payload(if newer { "newer" } else { "older" })),
            revision: if newer { next_revision() } else { older },
            applied,
            operation: None,
        });
        drain_reads(&mut service).await;
        if newer {
            assert_eq!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "newer",
                "newer origin-A payload publishes after different-origin replacement"
            );
        } else {
            assert_ne!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "older",
                "older origin-A payload is never published after different-origin replacement"
            );
            assert_eq!(service.revisions.get(&auth.id), Some(&trigger));
            assert_eq!(
                service.revisions.get(path.to_string_lossy().as_ref()),
                Some(&trigger)
            );
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("origin-A late write acknowledges within 5s"),
            Ok(())
        );
    }

    #[tokio::test]
    async fn terminal_unsupported_recovery_refuses_older_late_write() {
        terminal_unsupported_late_write(false).await;
    }

    #[tokio::test]
    async fn terminal_unsupported_recovery_accepts_newer_late_write() {
        terminal_unsupported_late_write(true).await;
    }

    async fn terminal_unsupported_late_write(newer: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let auth = service.manager.register_unsaved(auth).unwrap();
        let payload = |value| AuthFile {
            path: path.clone(),
            data: serde_json::to_vec(&serde_json::json!({"type":"claude", "access_token":value}))
                .unwrap()
                .into(),
        };
        let older = next_revision();
        let trigger = next_revision();
        service.read_hook = Some(Arc::new(|_| panic!("fixture failed trigger")));
        service.load_auth_file(&payload("trigger"), trigger);
        drain_reads(&mut service).await;
        service.read_hook = None;
        tokio::time::timeout(Duration::from_secs(5), fire_dirty_deadline(&mut service))
            .await
            .expect("terminal Unsupported retry completes within 5s");
        assert!(service.dirty_reads.is_empty());
        assert!(service.reads.is_empty());
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileWritten(payload(if newer { "newer" } else { "older" })),
            revision: if newer { next_revision() } else { older },
            applied,
            operation: None,
        });
        drain_reads(&mut service).await;
        if newer {
            assert_eq!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "newer",
                "newer payload publishes after terminal recovery"
            );
        } else {
            assert_ne!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "older",
                "older late payload is never published after terminal recovery"
            );
            assert_eq!(service.revisions.get(&auth.id), Some(&trigger));
            assert_eq!(
                service.revisions.get(path.to_string_lossy().as_ref()),
                Some(&trigger)
            );
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("post-terminal write acknowledges within 5s"),
            Ok(())
        );
    }

    #[tokio::test]
    async fn noncurrent_terminal_unsupported_recovery_claims_nothing() {
        terminal_unsupported_canceled(false).await;
    }

    #[tokio::test]
    async fn replaced_terminal_unsupported_recovery_claims_nothing() {
        terminal_unsupported_canceled(true).await;
    }

    async fn terminal_unsupported_canceled(replaced: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canceled-terminal.json");
        let mut service = service(dir.path(), "");
        let auth = registered_gemini(&mut service, &path);
        let trigger = next_revision();
        service.read_hook = Some(Arc::new(|_| panic!("fixture failed trigger")));
        service.queue_auth_change(AuthChange {
            change: SyncChange::Upsert(Box::new(auth.clone())),
            revision: trigger,
            applied: None,
            operation: None,
        });
        drain_reads(&mut service).await;
        let before = service.revisions.clone();
        let mut held = hold_read(&mut service);
        service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(5),
            service.work(Path::new(""), &mut None),
        )
        .await
        .expect("canceled terminal retry starts within 5s");
        held.entered().await;
        if replaced {
            service.queue_auth_change(AuthChange {
                change: SyncChange::Upsert(Box::new(auth.clone())),
                revision: next_revision(),
                applied: None,
                operation: None,
            });
        } else {
            service.manager.register_unsaved(auth.clone()).unwrap();
        }
        held.release();
        consume_read(&mut service).await;
        assert_eq!(
            service.revisions, before,
            "noncurrent or replaced terminal recovery claims no watermark"
        );
        if replaced {
            assert_eq!(
                service.reads.len(),
                1,
                "replacement observation remains pending"
            );
        }
        drain_reads(&mut service).await;
    }

    #[tokio::test]
    async fn older_removal_during_backoff_keeps_recovery() {
        removal_during_recovery(false, false, false).await;
    }

    #[tokio::test]
    async fn older_removal_during_retry_keeps_recovery() {
        removal_during_recovery(true, false, false).await;
    }

    #[tokio::test]
    async fn newer_removal_during_backoff_cancels_recovery() {
        removal_during_recovery(false, true, false).await;
    }

    #[tokio::test]
    async fn newer_removal_during_retry_cancels_recovery() {
        removal_during_recovery(true, true, false).await;
    }

    #[tokio::test]
    async fn zero_removal_during_backoff_keeps_existing_rule() {
        removal_during_recovery(false, true, true).await;
    }

    #[tokio::test]
    async fn zero_removal_during_retry_keeps_existing_rule() {
        removal_during_recovery(true, true, true).await;
    }

    async fn removal_during_recovery(in_flight: bool, newer: bool, unversioned: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "pending-removal.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let payload = auth_file(&path);
        let older = next_revision();
        let trigger = next_revision();
        std::fs::write(&path, "null").unwrap();
        service.load_auth_file(&payload, trigger);
        drain_reads(&mut service).await;
        let dirty = service.dirty_reads.values().next().unwrap();
        let deadline = dirty.deadline;
        let attempt = dirty.attempt;
        let mut held = hold_read(&mut service);
        if in_flight {
            service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
            tokio::time::timeout(
                Duration::from_secs(5),
                service.work(Path::new(""), &mut None),
            )
            .await
            .expect("pending-removal retry starts within 5s");
            held.entered().await;
        }
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileRemoved(path.clone()),
            revision: if unversioned {
                0
            } else if newer {
                next_revision()
            } else {
                older
            },
            applied,
            operation: None,
        });
        held.release();
        drain_reads(&mut service).await;
        if newer {
            assert!(
                service.manager.get(&id).is_none(),
                "newer removal unregisters in either recovery window"
            );
            assert!(service.dirty_reads.is_empty());
        } else {
            assert!(
                service.manager.get(&id).is_some(),
                "older removal leaves credential still registered in either recovery window"
            );
            let dirty = service
                .dirty_reads
                .values()
                .next()
                .expect("older removal keeps dirty retry");
            assert_eq!(dirty.recovery_floor.as_ref().unwrap().revision, trigger);
            assert_eq!(
                dirty.attempt,
                attempt + usize::from(in_flight),
                "older removal preserves retry backoff"
            );
            if !in_flight {
                assert_eq!(
                    dirty.deadline, deadline,
                    "older removal preserves dirty deadline"
                );
            }
            assert_eq!(service.file_auths.get(&path), Some(&id));
            std::fs::write(&path, &payload.data).unwrap();
            tokio::time::timeout(Duration::from_secs(5), fire_dirty_deadline(&mut service))
                .await
                .expect("preserved retry observes repaired disk within 5s");
            assert!(service.manager.get(&id).is_some());
            assert!(service.dirty_reads.is_empty());
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("recovery removal acknowledges within 5s"),
            Ok(())
        );
        assert!(service.reads.is_empty());
    }

    #[tokio::test]
    async fn successful_sourceless_retry_claims_only_its_fresh_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "recovery-watermark.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let payload = auth_file(&path);
        std::fs::write(&path, "null").unwrap();
        let trigger = next_revision();
        service.load_auth_file(&payload, trigger);
        drain_reads(&mut service).await;
        assert_eq!(
            service
                .dirty_reads
                .values()
                .next()
                .unwrap()
                .recovery_floor
                .as_ref()
                .unwrap()
                .revision,
            trigger
        );
        std::fs::write(&path, &payload.data).unwrap();
        service.dirty_reads.values_mut().next().unwrap().deadline = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(5),
            service.work(Path::new(""), &mut None),
        )
        .await
        .expect("dirty deadline is serviced within 5s");
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("successful retry completes within 5s")
                .unwrap();
        let fresh = match &completion.result {
            Ok(AdoptionResult::Published { revision, .. }) => *revision,
            _ => panic!("repaired retry publishes a fresh observation"),
        };
        assert!(
            fresh > trigger,
            "successful retry allocates a revision above its trigger"
        );
        service.complete_read(completion);
        assert_eq!(
            service.revisions.get(&id),
            Some(&fresh),
            "retry claims only its fresh revision"
        );
        assert_eq!(
            service.revisions.get(path.to_string_lossy().as_ref()),
            Some(&fresh)
        );
        assert!(
            service.dirty_reads.is_empty(),
            "success ends recovery floor"
        );
        assert!(service.reads.is_empty());
    }

    #[tokio::test]
    async fn noncurrent_read_coalesces_three_parked_changes_without_dirty_floor() {
        let dir = tempfile::tempdir().unwrap();
        let old = codex_file(dir.path(), "coalesce-old.json", "");
        let new = codex_file(dir.path(), "coalesce-new.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let id = service.file_auths[&old].clone();
        let before = service.revisions.clone();
        let payload = auth_file(&old);
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = completed.clone();
        service.completion_hook = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let mut held = hold_read(&mut service);
        service.load_auth_file(&payload, next_revision());
        held.entered().await;
        let mut done = Vec::new();
        for _ in 0..3 {
            let (applied, ack) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::FileWritten(payload.clone()),
                revision: next_revision(),
                applied,
                operation: None,
            });
            done.push(ack);
        }
        service
            .manager
            .edit_unsaved(&id, |auth| {
                auth.attributes
                    .insert("path".into(), new.to_string_lossy().into_owned());
                auth.file_name = "coalesce-new.json".into();
                Ok::<_, ()>(true)
            })
            .unwrap();
        held.release();
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .expect("noncurrent observation completes within 5s")
                .unwrap();
        assert!(matches!(
            completion.result,
            Ok(AdoptionResult::RegistrationChanged)
        ));
        service.complete_read(completion);
        assert!(
            service.dirty_reads.is_empty(),
            "noncurrent completion has no dirty floor"
        );
        assert_eq!(
            service.revisions, before,
            "noncurrent completion advances no watermark"
        );
        for ack in &mut done {
            assert_eq!(ack.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        }
        drain_reads(&mut service).await;
        assert_eq!(
            completed.load(Ordering::SeqCst),
            2,
            "noncurrent parked changes cause exactly one further read"
        );
        for ack in done {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), ack)
                    .await
                    .expect("coalesced noncurrent work acknowledges within 5s"),
                Ok(())
            );
        }
        assert!(service.dirty_reads.is_empty());
        assert_eq!(service.revisions, before);
    }

    #[tokio::test]
    async fn older_parked_unsupported_write_preserves_failed_trigger_recovery() {
        parked_unsupported_write_after_failed_trigger(false).await;
    }

    #[tokio::test]
    async fn newer_parked_unsupported_write_replaces_failed_trigger() {
        parked_unsupported_write_after_failed_trigger(true).await;
    }

    async fn parked_unsupported_write_after_failed_trigger(newer: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed-trigger.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.provider = "claude".into();
        let auth = service.manager.register_unsaved(auth).unwrap();
        let payload = |value| AuthFile {
            path: path.clone(),
            data: serde_json::to_vec(&serde_json::json!({"type":"claude", "access_token":value}))
                .unwrap()
                .into(),
        };
        let mut held = hold_read(&mut service);
        let hook = service.read_hook.take().unwrap();
        let first = std::sync::atomic::AtomicBool::new(true);
        service.read_hook = Some(Arc::new(move |identity| {
            let fail = first.swap(false, Ordering::SeqCst);
            hook(identity);
            if fail {
                panic!("fixture failed trigger");
            }
        }));
        let older = next_revision();
        let trigger = next_revision();
        let parked = if newer { next_revision() } else { older };
        let (applied, mut trigger_done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileWritten(payload("trigger")),
            revision: trigger,
            applied,
            operation: None,
        });
        held.entered().await;
        let (applied, mut parked_done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileWritten(payload(if newer { "newer" } else { "older" })),
            revision: parked,
            applied,
            operation: None,
        });
        assert_eq!(
            trigger_done.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        assert_eq!(
            parked_done.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        held.release();
        consume_read(&mut service).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), trigger_done)
                .await
                .expect("failed trigger acknowledges within 5s"),
            Ok(())
        );
        if newer {
            assert_eq!(
                parked_done.try_recv(),
                Err(oneshot::error::TryRecvError::Empty),
                "newer payload waits for covering read"
            );
            drain_reads(&mut service).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), parked_done)
                    .await
                    .expect("parked payload acknowledges within 5s"),
                Ok(())
            );
            assert_eq!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "newer",
                "newer parked payload supersedes failed trigger"
            );
            assert!(service.dirty_reads.is_empty());
        } else {
            // Drain an incorrectly selected older read too, so the control proves publication, not merely selection.
            drain_reads(&mut service).await;
            assert_ne!(
                service.manager.get(&auth.id).unwrap().metadata["access_token"],
                "older",
                "older parked payload is never published after failed trigger"
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), parked_done)
                    .await
                    .expect("parked payload acknowledges within 5s"),
                Ok(())
            );
            assert_eq!(
                service.dirty_reads.len(),
                1,
                "failed trigger retains dirty recovery"
            );
            assert_eq!(held.calls.load(Ordering::SeqCst), 1);
            fire_dirty_deadline(&mut service).await;
            assert_eq!(
                service.manager.get(&auth.id).unwrap().metadata,
                auth.metadata
            );
        }
    }

    #[tokio::test]
    async fn rejected_removal_keeps_uncovered_id_only_retry() {
        let dir = tempfile::tempdir().unwrap();
        let old = codex_file(dir.path(), "rejected-id-only.json", "");
        let new = codex_file(dir.path(), "current-origin.json", r#", "prefix":"current""#);
        let mut service = service(dir.path(), "");
        let mut held = hold_completion(&mut service);
        let (applied, mut done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(Auth {
                id: "rejected-id-only.json".into(),
                provider: "codex".into(),
                ..Auth::default()
            })),
            revision: next_revision(),
            applied,
            operation: None,
        });
        held.entered().await;
        let id = service.manager.list()[0].id.clone();
        service
            .manager
            .edit_unsaved(&id, |auth| {
                auth.attributes
                    .insert("path".into(), new.to_string_lossy().into_owned());
                auth.file_name = "current-origin.json".into();
                Ok::<_, ()>(true)
            })
            .unwrap();
        let (applied, removed) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileRemoved(old),
            revision: next_revision(),
            applied,
            operation: None,
        });
        service.apply_config_authorized(service.config.clone(), None, Path::new(""));
        held.release();
        consume_read(&mut service).await;
        assert_eq!(
            done.try_recv(),
            Err(oneshot::error::TryRecvError::Empty),
            "rejected removal cannot acknowledge uncovered id-only work"
        );
        assert!(
            !service.reads.is_empty(),
            "rejected removal keeps id-only retry"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), removed)
                .await
                .expect("rejected removal acknowledges within 5s"),
            Ok(())
        );
        drain_reads(&mut service).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("reobserved id-only work acknowledges within 5s"),
            Ok(())
        );
        assert_eq!(service.manager.get(&id).unwrap().prefix, "current");
        assert_eq!(service.file_auths.get(&new), Some(&id));
    }

    #[tokio::test]
    async fn rejected_old_origin_removal_retires_only_stale_mapping() {
        for relative in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let old = dir.path().join("old-origin.json");
            let new = dir.path().join("current-origin.json");
            let mut service = service(dir.path(), "");
            let auth = registered_gemini(&mut service, &old);
            service
                .manager
                .edit_unsaved(&auth.id, |live| {
                    live.attributes
                        .insert("path".into(), new.to_string_lossy().into_owned());
                    live.file_name = "current-origin.json".into();
                    Ok::<_, ()>(true)
                })
                .unwrap();
            service.file_auths.insert(new.clone(), auth.id.clone());
            service.remove_auth_file(
                if relative {
                    Path::new("old-origin.json")
                } else {
                    &old
                },
                next_revision(),
            );
            assert!(
                !service.file_auths.contains_key(&old),
                "rejected removal retires stale old-origin mapping"
            );
            assert_eq!(service.file_auths.get(&new), Some(&auth.id));
            assert_eq!(
                service.manager.get(&auth.id).unwrap().registration_epoch,
                auth.registration_epoch
            );
        }
    }

    #[tokio::test]
    async fn resume_acknowledges_only_after_held_observation_settles() {
        acknowledgement_after_held_observation(false).await;
    }

    #[tokio::test]
    async fn relist_acknowledges_only_after_held_observation_settles() {
        acknowledgement_after_held_observation(true).await;
    }

    async fn acknowledgement_after_held_observation(relist: bool) {
        let dir = tempfile::tempdir().unwrap();
        codex_file(dir.path(), "positive-settlement.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let mut held = hold_read(&mut service);
        let (stop, mut settled, task) = worker::settlement_test_loop(service, relist);
        held.entered().await;
        let early = tokio::time::timeout(Duration::from_millis(50), &mut settled).await;
        assert!(
            early.is_err(),
            "command acknowledgement waits for held observation"
        );
        held.release();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), settled)
                .await
                .expect("settlement acknowledges within 5s"),
            Ok(Ok(()))
        );
        stop.send(tokio::time::Instant::now()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn binding_cleanup_keeps_replacement_registration_and_accounting() {
        for replace in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "cleanup.json", "");
            let mut service = service(dir.path(), "");
            let mut held = hold_completion(&mut service);
            service.load_auth_file(&auth_file(&path), next_revision());
            held.entered().await;
            let published = service.manager.list()[0].clone();
            let id = published.id.clone();
            service.file_auths.insert(path.clone(), id.clone());
            service.store.set_base_dir(other.path());
            let manager = service.manager.clone();
            let id_for_hook = id.clone();
            service.removal_hook = Some(Arc::new(move || {
                if replace {
                    let mut replacement = (*manager.get(&id_for_hook).unwrap()).clone();
                    replacement.prefix = "replacement".into();
                    manager.register_unsaved(replacement).unwrap();
                } else {
                    manager
                        .edit_unsaved(&id_for_hook, |auth| {
                            auth.file_name = "changed-origin.json".into();
                            auth.attributes.remove("path");
                            Ok::<_, ()>(true)
                        })
                        .unwrap();
                }
            }));
            held.release();
            drain_reads(&mut service).await;
            assert!(
                service.manager.get(&id).is_some(),
                "binding cleanup keeps newer registration or origin"
            );
            assert_eq!(
                service.file_auths.get(&path),
                Some(&id),
                "conditional cleanup keeps newer accounting"
            );
            let live = service.manager.get(&id).unwrap();
            if replace {
                assert!(live.registration_epoch > published.registration_epoch);
            } else {
                assert_eq!(live.file_name, "changed-origin.json");
            }
        }
    }

    #[tokio::test]
    async fn parked_revisions_keep_newer_write_and_accept_newest_removal() {
        for newest_removal in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "ordered.json", "");
            let mut service = service(dir.path(), "");
            service.load_file_auths();
            let id = service.manager.list()[0].id.clone();
            let payload = auth_file(&path);
            std::fs::write(&path, "null").unwrap();
            let mut held = hold_read(&mut service);
            service.load_auth_file(&payload, next_revision());
            held.entered().await;
            let older_removal = next_revision();
            let write_revision = next_revision();
            let removal_revision = if newest_removal {
                next_revision()
            } else {
                older_removal
            };
            let (applied, mut written) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::FileWritten(payload),
                revision: write_revision,
                applied,
                operation: None,
            });
            let (applied, removed) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::FileRemoved(path),
                revision: removal_revision,
                applied,
                operation: None,
            });
            held.release();
            consume_read(&mut service).await;
            if newest_removal {
                assert!(
                    service.manager.get(&id).is_none(),
                    "newest parked removal wins"
                );
                assert_eq!(written.try_recv(), Ok(()));
                assert!(service.reads.is_empty());
            } else {
                assert!(
                    service.manager.get(&id).is_some(),
                    "older parked removal cannot discard newer write"
                );
                assert_eq!(
                    written.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty),
                    "newer write waits for covering read"
                );
                drain_reads(&mut service).await;
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), written)
                        .await
                        .expect("written acknowledgement arrives within 5s"),
                    Ok(())
                );
            }
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), removed)
                    .await
                    .expect("removed acknowledgement arrives within 5s"),
                Ok(())
            );
        }
    }

    #[tokio::test]
    async fn revision_zero_recovery_yields_to_removal_in_either_arrival_order() {
        for removal_first in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "zero.json", "");
            let mut service = service(dir.path(), "");
            service.load_file_auths();
            let current = service.manager.list()[0].clone();
            let payload = auth_file(&path);
            std::fs::write(&path, "null").unwrap();
            let mut held = hold_read(&mut service);
            service.load_auth_file(&payload, next_revision());
            held.entered().await;
            let mut changes = vec![
                SyncChange::Upsert(Box::new((*current).clone())),
                SyncChange::FileRemoved(path),
            ];
            if removal_first {
                changes.reverse();
            }
            let mut acknowledgements = Vec::new();
            for change in changes {
                let revision = if matches!(change, SyncChange::FileRemoved(_)) {
                    next_revision()
                } else {
                    0
                };
                let (applied, done) = oneshot::channel();
                service.apply_sync(SyncRequest {
                    change,
                    revision,
                    applied,
                    operation: None,
                });
                acknowledgements.push(done);
            }
            service.apply_config_authorized(service.config.clone(), None, Path::new(""));
            held.release();
            drain_reads(&mut service).await;
            assert!(
                service.manager.get(&current.id).is_none(),
                "positive removal cancels revision-zero recovery in either order"
            );
            for done in acknowledgements {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), done)
                        .await
                        .expect("done acknowledgement arrives within 5s"),
                    Ok(())
                );
            }
            assert!(
                service.dirty_reads.is_empty(),
                "positive removal cancels revision-zero recovery in either order"
            );
            assert_eq!(held.calls.load(Ordering::SeqCst), 1);
        }
    }

    fn registered_gemini(service: &mut Service, path: &Path) -> Auth {
        let auth = Auth {
            id: path.file_name().unwrap().to_string_lossy().into_owned(),
            provider: "gemini".into(),
            file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            attributes: [
                ("path".into(), path.to_string_lossy().into_owned()),
                ("fixture".into(), "keep".into()),
            ]
            .into(),
            metadata: [("access_token".into(), serde_json::json!("keep-token"))]
                .into_iter()
                .collect(),
            ..Auth::default()
        };
        let registered = service.manager.register_unsaved(auth).unwrap();
        service
            .file_auths
            .insert(path.to_owned(), registered.id.clone());
        (*registered).clone()
    }

    #[tokio::test]
    async fn unsupported_coalescing_selects_highest_revision_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        let mut service = service(dir.path(), "");
        let auth = registered_gemini(&mut service, &path);
        let mut held = hold_read(&mut service);
        let (applied, initial) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(auth.clone())),
            revision: next_revision(),
            applied,
            operation: None,
        });
        held.entered().await;
        let low = next_revision();
        let high = next_revision();
        let mut acknowledgements = Vec::new();
        for (revision, value) in [(high, "newer"), (low, "older")] {
            let mut payload = auth.clone();
            payload
                .metadata
                .insert("access_token".into(), serde_json::json!(value));
            let (applied, done) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::Upsert(Box::new(payload)),
                revision,
                applied,
                operation: None,
            });
            acknowledgements.push(done);
        }
        service.apply_config_authorized(service.config.clone(), None, Path::new(""));
        held.release();
        drain_reads(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), initial)
            .await
            .expect("initial acknowledgement arrives within 5s")
            .unwrap();
        for done in acknowledgements {
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("done acknowledgement arrives within 5s")
                .unwrap();
        }
        assert_eq!(
            service.manager.get(&auth.id).unwrap().metadata["access_token"],
            "newer",
            "highest revision Unsupported payload wins"
        );
        assert_eq!(held.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn pathless_dirty_empty_unsupported_uses_event_removal_origin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pathless-empty.json");
        let mut service = service(dir.path(), "");
        let mut auth = registered_gemini(&mut service, &path);
        auth.file_name.clear();
        auth.attributes.remove("path");
        service.manager.register_unsaved(auth.clone()).unwrap();
        service.read_hook = Some(Arc::new(|_| panic!("fixture first read error")));
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(auth.clone())),
            revision: next_revision(),
            applied,
            operation: None,
        });
        drain_reads(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("done acknowledgement arrives within 5s")
            .unwrap();
        assert!(
            service
                .dirty_reads
                .values()
                .next()
                .unwrap()
                .identity
                .path
                .is_none()
        );
        service.read_hook = None;
        service.load_auth_file(
            &AuthFile {
                path,
                data: b"null".to_vec().into(),
            },
            next_revision(),
        );
        let finished = std::panic::AssertUnwindSafe(drain_reads(&mut service));
        // Catch a completion panic as a named regression failure, not a hung task.
        use futures_util::FutureExt;
        assert!(
            finished.catch_unwind().await.is_ok(),
            "empty Unsupported completion never panics on pathless origin"
        );
        assert!(
            service.manager.get(&auth.id).is_none(),
            "pathless Unsupported empty event unregisters"
        );
    }

    #[tokio::test]
    async fn sourceless_unsupported_retry_never_publishes_identity_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sourceless.json");
        let mut service = service(dir.path(), "");
        let auth = registered_gemini(&mut service, &path);
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        service.read_hook = Some(Arc::new(move |_| {
            if first.swap(false, Ordering::SeqCst) {
                panic!("fixture first read error");
            }
        }));
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(auth.clone())),
            revision: next_revision(),
            applied,
            operation: None,
        });
        drain_reads(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("done acknowledgement arrives within 5s")
            .unwrap();
        fire_dirty_deadline(&mut service).await;
        let live = service.manager.get(&auth.id).unwrap();
        assert_eq!(
            live.metadata, auth.metadata,
            "sourceless Unsupported retry keeps metadata"
        );
        assert_eq!(
            live.attributes, auth.attributes,
            "sourceless Unsupported retry keeps attributes"
        );
        assert!(
            service.dirty_reads.is_empty(),
            "Unsupported sourceless observation has no dirty loop"
        );
    }

    #[tokio::test]
    async fn stop_while_resume_observation_is_held() {
        stop_while_observation_is_held(false).await;
    }

    #[tokio::test]
    async fn stop_while_relist_observation_is_held() {
        stop_while_observation_is_held(true).await;
    }

    async fn stop_while_observation_is_held(relist: bool) {
        use open_ferry_management::{CredentialSync, SyncError};
        {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "settlement.json", "");
            let mut service = service(dir.path(), "");
            service.load_file_auths();
            let sync = sync_sender(&mut service);
            let mut held = hold_read(&mut service);
            let (stop, mut settled, task) = worker::settlement_test_loop(service, relist);
            held.entered().await;
            assert_eq!(
                settled.try_recv(),
                Err(oneshot::error::TryRecvError::Empty),
                "resume waits for its observations"
            );
            let payload = auth_file(&path);
            let request = tokio::spawn(async move { sync.file_written(payload).await });
            stop.send(tokio::time::Instant::now()).unwrap();
            let stopped = tokio::time::timeout(Duration::from_secs(5), task).await;
            assert!(
                stopped.is_ok(),
                "stop-aware settlement closes service within 5s"
            );
            stopped.unwrap().unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), request)
                    .await
                    .unwrap()
                    .unwrap(),
                Err(SyncError::Stopped)
            );
            assert!(
                tokio::time::timeout(Duration::from_secs(5), settled)
                    .await
                    .expect("settlement receiver closes within 5s")
                    .is_err(),
                "stopped resume is not acknowledged as settled"
            );
            held.release();
        }
    }

    #[tokio::test]
    async fn coalesced_rekey_parks_without_replacing_active_job() {
        let dir = tempfile::tempdir().unwrap();
        let first = codex_file(dir.path(), "first-job.json", "");
        let second = codex_file(dir.path(), "second-job.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        std::fs::write(&first, "null").unwrap();
        let mut held = hold_read(&mut service);
        service.load_auth_file(&auth_file(&first), next_revision());
        held.entered().await;
        let first_key = service
            .change_identity(&SyncChange::FileWritten(auth_file(&first)))
            .0;
        let (applied, second_done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileWritten(auth_file(&second)),
            revision: next_revision(),
            applied,
            operation: None,
        });
        let second_key = service
            .change_identity(&SyncChange::FileWritten(auth_file(&second)))
            .0;
        let (applied, redirected_done) = oneshot::channel();
        // Exercise the defensive rekey boundary directly: normal queueing would
        // already park this request on the second job.
        service
            .reads
            .get_mut(&first_key)
            .unwrap()
            .parked
            .push_back(AuthChange {
                change: SyncChange::FileWritten(auth_file(&second)),
                revision: next_revision(),
                applied: Some(applied),
                operation: None,
            });
        let original_revision = service.reads[&second_key].trigger_revision;
        held.release();
        let mut deferred = None;
        loop {
            let completion =
                tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if completion.key == first_key {
                service.complete_read(completion);
                break;
            }
            // Keep the second completion unconsumed until rekeying has run.
            deferred = Some(completion);
        }
        if let Some(completion) = deferred {
            service
                .read_sender
                .send(completion)
                .unwrap_or_else(|_| panic!("completion receiver stays open"));
        }
        assert_eq!(
            service.reads[&second_key].trigger_revision, original_revision,
            "coalesced rekey preserves active job"
        );
        drain_reads(&mut service).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), second_done)
                .await
                .expect("second_done acknowledgement arrives within 5s"),
            Ok(())
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), redirected_done)
                .await
                .expect("redirected_done acknowledgement arrives within 5s"),
            Ok(())
        );
    }

    #[tokio::test]
    async fn resume_and_relist_keep_discarded_live_partition() {
        for resume in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(
                dir.path(),
                "resume.json",
                r#", "refresh_token":"A-refresh""#,
            );
            let mut service = service(dir.path(), "");
            let store = Arc::new(ServiceRefreshStore {
                files: service.store.clone(),
                fail: std::sync::atomic::AtomicBool::new(true),
            });
            service.manager = Manager::new(
                Settings::from(&*service.config),
                service.registry.clone(),
                Some(store),
            );
            service.load_file_auths();
            let id = service.manager.list()[0].id.clone();
            service
                .manager
                .register_executor(Arc::new(NamedRefresher("B")));
            service.manager.force_refresh(&id).await.unwrap();
            let mut replacement = (*service.manager.get(&id).unwrap()).clone();
            assert!(!replacement.codex_pending_refresh.is_empty());
            replacement.metadata.clear();
            replacement
                .attributes
                .insert("runtime_only".into(), "true".into());
            service
                .manager
                .run_persistence(move |manager| manager.update(replacement))
                .await
                .unwrap()
                .unwrap();
            service
                .manager
                .edit_unsaved(&id, |auth| {
                    auth.attributes.remove("runtime_only");
                    Ok::<_, ()>(true)
                })
                .unwrap();
            assert!(
                service
                    .manager
                    .get(&id)
                    .unwrap()
                    .codex_pending_refresh
                    .is_empty()
            );
            let disk = std::fs::read(&path).unwrap();
            if resume {
                let config_path = dir.path().join("resume-config.yaml");
                std::fs::write(
                    &config_path,
                    format!(
                        "auth-dir: {}\n",
                        dir.path().to_string_lossy().replace('\\', "/")
                    ),
                )
                .unwrap();
                tokio::time::timeout(
                    Duration::from_secs(5),
                    service.reload_from_disk(&config_path, &mut None),
                )
                .await
                .unwrap();
            } else {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    service.apply_deferred(Path::new(""), &mut None),
                )
                .await
                .unwrap()
                .unwrap();
            }
            drain_reads(&mut service).await;
            assert_eq!(
                service.manager.get(&id).unwrap().metadata["access_token"],
                "B",
                "reload keeps discarded live B"
            );
            service.load_auth_file(&auth_file(&path), next_revision());
            drain_reads(&mut service).await;
            assert_eq!(
                service.manager.get(&id).unwrap().metadata["access_token"],
                "B",
                "reload retains marker for later adoption"
            );
            assert_eq!(std::fs::read(path).unwrap(), disk);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_id_only_read_orders_newer_and_stale_path_removals() {
        for (newer, mismatch) in [(true, false), (false, false), (true, true), (false, true)] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "id-only.json", "");
            let mut service = service(dir.path(), "");
            let stale = next_revision();
            let mut held = hold_read(&mut service);
            let hint = Auth {
                id: "id-only.json".into(),
                provider: "codex".into(),
                ..Auth::default()
            };
            let (applied, done) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::Upsert(Box::new(hint)),
                revision: next_revision(),
                applied,
                operation: None,
            });
            held.entered().await;
            let (applied, removed) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::FileRemoved(path.clone()),
                revision: if newer { next_revision() } else { stale },
                applied,
                operation: None,
            });
            assert_eq!(
                service.reads.values().next().unwrap().parked.len(),
                1,
                "path removal shares initial id-only key"
            );
            if mismatch {
                service.apply_config_authorized(service.config.clone(), None, Path::new(""));
            }
            held.release();
            drain_reads(&mut service).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), done)
                    .await
                    .expect("done acknowledgement arrives within 5s"),
                Ok(())
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), removed)
                    .await
                    .expect("removed acknowledgement arrives within 5s"),
                Ok(())
            );
            assert_eq!(
                service.manager.get("id-only.json").is_none(),
                newer,
                "newer path removal wins; stale removal cancels nothing"
            );
            assert!(service.dirty_reads.is_empty());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "id-only.json", "");
        let mut service = service(dir.path(), "");
        std::fs::write(&path, "null").unwrap();
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(Auth {
                id: "id-only.json".into(),
                provider: "codex".into(),
                ..Auth::default()
            })),
            revision: next_revision(),
            applied,
            operation: None,
        });
        drain_reads(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("done acknowledgement arrives within 5s")
            .unwrap();
        assert_eq!(service.dirty_reads.len(), 1);
        service.remove_auth_file(&path, next_revision());
        assert!(
            service.dirty_reads.is_empty(),
            "path removal cancels id-only dirty timer"
        );
    }

    #[tokio::test]
    async fn config_updates_published_unconsumed_registration() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "published.json", "");
        let mut service = service(dir.path(), "oauth-excluded-models:\n  codex: [old-model]\n");
        let mut held = hold_completion(&mut service);
        service.load_auth_file(&auth_file(&path), next_revision());
        held.entered().await;
        assert!(service.file_auths.is_empty());
        let mut config = (*service.config).clone();
        config
            .oauth_excluded_models
            .insert("codex".into(), vec!["new-model".into()]);
        service.apply_config_authorized(Arc::new(config), None, Path::new(""));
        held.release();
        drain_reads(&mut service).await;
        let live = service.manager.list()[0].clone();
        let mut expected = (*live).clone();
        assert!(
            !apply_config_attributes(&service.synthesis_context(), &mut expected),
            "unconsumed publication receives C1 attributes"
        );
        assert_eq!(service.file_auths.get(&path), Some(&live.id));
    }

    #[tokio::test]
    async fn directory_change_removes_published_unconsumed_registration() {
        let dir = tempfile::tempdir().unwrap();
        let next = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "old-directory.json", "");
        let mut service = service(dir.path(), "");
        let mut held = hold_completion(&mut service);
        service.load_auth_file(&auth_file(&path), next_revision());
        held.entered().await;
        assert!(service.file_auths.is_empty());
        assert_eq!(service.manager.list().len(), 1);
        let mut config = (*service.config).clone();
        config.auth_dir = next.path().to_string_lossy().into_owned();
        service.apply_config_authorized(Arc::new(config), None, Path::new(""));
        assert!(
            service.manager.list().is_empty(),
            "directory switch removes unconsumed publication"
        );
        held.release();
        drain_reads(&mut service).await;
        assert!(service.manager.list().is_empty());
        assert!(service.file_auths.is_empty());
    }

    #[tokio::test]
    async fn changed_published_origin_is_reobserved_before_accounting() {
        for remove_old in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let old = codex_file(dir.path(), "published-old.json", "");
            let new = codex_file(
                dir.path(),
                "published-new.json",
                r#", "prefix":"new-origin""#,
            );
            let mut service = service(dir.path(), "");
            let mut held = hold_completion(&mut service);
            let origins = Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = origins.clone();
            service.read_hook = Some(Arc::new(move |identity| {
                observed.lock().unwrap().push(identity.path.clone());
            }));
            service.load_auth_file(&auth_file(&old), next_revision());
            held.entered().await;
            let published = service.manager.list()[0].clone();
            let id = published.id.clone();
            service
                .manager
                .edit_unsaved(&id, |auth| {
                    auth.attributes
                        .insert("path".into(), new.to_string_lossy().into_owned());
                    auth.file_name = "published-new.json".into();
                    Ok::<_, ()>(true)
                })
                .unwrap();
            if remove_old {
                service.remove_auth_file(&old, next_revision());
            } else {
                service.load_auth_file(&auth_file(&old), next_revision());
            }
            held.release();
            drain_reads(&mut service).await;
            assert_eq!(
                service.file_auths.get(&new),
                Some(&id),
                "changed publication is reobserved and accounted"
            );
            assert_eq!(service.manager.get(&id).unwrap().prefix, "new-origin");
            assert!(
                origins.lock().unwrap().contains(&Some(new.clone())),
                "current-origin recovery runs its own observation"
            );
            assert!(!service.file_auths.contains_key(&old));
            assert_eq!(
                service.manager.get(&id).unwrap().registration_epoch,
                published.registration_epoch,
                "old-origin removal cannot unregister current-origin recovery"
            );
        }
    }

    #[tokio::test]
    async fn parked_removal_cancels_first_config_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "churn.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let mut held = hold_read(&mut service);
        service.load_auth_file(&auth_file(&path), next_revision());
        held.entered().await;
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::FileRemoved(path),
            revision: next_revision(),
            applied,
            operation: None,
        });
        for n in 0..3 {
            let mut config = (*service.config).clone();
            config
                .oauth_excluded_models
                .insert("codex".into(), vec![format!("model-{n}")]);
            service.apply_config_authorized(Arc::new(config), None, Path::new(""));
        }
        held.release();
        consume_read(&mut service).await;
        assert!(
            service.manager.get(&id).is_none(),
            "parked removal lands at first config rejection"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .unwrap(),
            Ok(())
        );
        assert!(service.reads.is_empty(), "parked removal cancels Retry-now");
        assert_eq!(held.calls.load(Ordering::SeqCst), 1);
        assert!(service.dirty_reads.is_empty());
    }

    #[tokio::test]
    async fn failed_read_coalesces_three_parked_changes_and_acknowledgements() {
        for mismatch in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "coalesce.json", "");
            let mut service = service(dir.path(), "");
            service.load_file_auths();
            let payload = auth_file(&path);
            std::fs::write(&path, "null").unwrap();
            if mismatch {
                service.load_auth_file(&payload, next_revision());
                drain_reads(&mut service).await;
                fire_dirty_deadline(&mut service).await;
                fire_dirty_deadline(&mut service).await;
                assert_eq!(service.dirty_reads.values().next().unwrap().attempt, 3);
            }
            let mut held = hold_read(&mut service);
            if mismatch {
                service.dirty_reads.values_mut().next().unwrap().deadline =
                    tokio::time::Instant::now();
                service.work(Path::new(""), &mut None).await;
            } else {
                service.load_auth_file(&payload, next_revision());
            }
            held.entered().await;
            let mut done = Vec::new();
            for _ in 0..3 {
                let (applied, ack) = oneshot::channel();
                service.apply_sync(SyncRequest {
                    change: SyncChange::FileWritten(payload.clone()),
                    revision: next_revision(),
                    applied,
                    operation: None,
                });
                done.push(ack);
            }
            if mismatch {
                let mut config = (*service.config).clone();
                config
                    .oauth_excluded_models
                    .insert("codex".into(), vec!["reset-model".into()]);
                service.apply_config_authorized(Arc::new(config), None, Path::new(""));
            }
            held.release();
            consume_read(&mut service).await;
            assert_eq!(
                service.reads.values().next().unwrap().attempt,
                0,
                "coalesced new event resets config retry backoff"
            );
            for ack in &mut done {
                assert_eq!(
                    ack.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty),
                    "coalesced acknowledgements await covering read"
                );
            }
            drain_reads(&mut service).await;
            assert_eq!(
                held.calls.load(Ordering::SeqCst),
                2,
                "three parked changes cause exactly one further read"
            );
            for ack in done {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), ack)
                        .await
                        .expect("ack acknowledgement arrives within 5s"),
                    Ok(())
                );
            }
            assert_eq!(service.dirty_reads.len(), 1);
            assert_eq!(service.dirty_reads.values().next().unwrap().attempt, 1);
        }
    }

    #[tokio::test]
    async fn unsupported_empty_payload_unregisters_without_blank_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gemini.json");
        std::fs::write(
            &path,
            r#"{"type":"gemini","access_token":"gemini-fixture"}"#,
        )
        .unwrap();
        let mut service = service(dir.path(), "");
        // Legacy Gemini files are not synthesized at startup, but an explicit
        // management Upsert can register this Unsupported requested record.
        let mut auth = Auth {
            id: "gemini.json".into(),
            provider: "gemini".into(),
            file_name: "gemini.json".into(),
            ..Auth::default()
        };
        auth.attributes
            .insert("path".into(), path.to_string_lossy().into_owned());
        auth.metadata
            .insert("access_token".into(), serde_json::json!("gemini-fixture"));
        let (applied, done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(auth)),
            revision: next_revision(),
            applied,
            operation: None,
        });
        drain_reads(&mut service).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("done acknowledgement arrives within 5s")
            .unwrap();
        let id = service.manager.list()[0].id.clone();
        let file = AuthFile {
            path: path.clone(),
            data: b"null".to_vec().into(),
        };
        service.load_auth_file(&file, next_revision());
        drain_reads(&mut service).await;
        assert!(
            service.manager.get(&id).is_none(),
            "unsupported empty payload unregisters instead of publishing a blank hint"
        );
        assert!(service.file_auths.is_empty());
        assert!(service.dirty_reads.is_empty());
    }

    #[tokio::test]
    async fn shutdown_refuses_queued_parked_and_inflight_callers() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "shutdown.json", "");
        let mut service = service(dir.path(), "");
        let sync = sync_sender(&mut service);
        let mut held = hold_read(&mut service);
        let file = auth_file(&path);
        let first_sync = sync.clone();
        let first_file = file.clone();
        let first = tokio::spawn(async move { first_sync.file_written(first_file).await });
        let request = tokio::time::timeout(Duration::from_secs(5), service.sync_requests.recv())
            .await
            .unwrap()
            .unwrap();
        service.apply_sync(request);
        held.entered().await;
        let parked_sync = sync.clone();
        let parked_file = file.clone();
        let parked = tokio::spawn(async move { parked_sync.file_written(parked_file).await });
        let request = tokio::time::timeout(Duration::from_secs(5), service.sync_requests.recv())
            .await
            .unwrap()
            .unwrap();
        service.apply_sync(request);
        let queued_sync = sync.clone();
        let queued = tokio::spawn(async move { queued_sync.file_written(file).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while sync.requests.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service.close_sync();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), parked)
                .await
                .expect("shutdown refuses parked caller within 5s")
                .unwrap(),
            Err(SyncError::Stopped)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), queued)
                .await
                .expect("shutdown refuses queued caller within 5s")
                .unwrap(),
            Err(SyncError::Stopped)
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .expect("shutdown refuses in-flight caller within 5s")
                .unwrap(),
            Err(SyncError::Stopped)
        );
        held.release();
        assert!(service.dirty_reads.is_empty());
    }

    #[tokio::test]
    async fn fresh_service_read_keeps_failed_successor_and_repairs_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "fresh.json", r#","refresh_token":"A-refresh""#);
        let payload = auth_file(&path); // Hold watcher A before P and Q.
        let mut service = service(dir.path(), "");
        let store = Arc::new(ServiceRefreshStore {
            files: service.store.clone(),
            fail: std::sync::atomic::AtomicBool::new(false),
        });
        service.manager = Manager::new(
            Settings::from(&*service.config),
            service.registry.clone(),
            Some(store.clone()),
        );
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        service
            .manager
            .register_executor(Arc::new(NamedRefresher("B")));
        service.manager.force_refresh(&id).await.unwrap();
        store.fail.store(true, Ordering::SeqCst);
        service
            .manager
            .register_executor(Arc::new(NamedRefresher("C")));
        service.manager.force_refresh(&id).await.unwrap();
        let q = service.manager.get(&id).unwrap().codex_pending_refresh[0].clone();
        service
            .handle_and_drain(changed(payload), Path::new(""))
            .await;
        let live = service.manager.get(&id).unwrap();
        assert_eq!(
            live.metadata["access_token"], "C",
            "fresh service read retains C"
        );
        assert!(
            live.codex_pending_refresh.iter().any(|p| p == &q),
            "Q stays pending"
        );
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["access_token"], "B", "disk stays B before retry");
        store.fail.store(false, Ordering::SeqCst);
        service
            .manager
            .run_persistence(move |m| {
                m.edit(&id, |a| {
                    a.metadata.insert("note".into(), serde_json::json!("retry"));
                    Ok::<_, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap();
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["access_token"], "C", "next save repairs disk C");
    }

    #[tokio::test]
    async fn fresh_service_revision_precedes_parked_management_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "revision.json", "");
        let mut service = service(dir.path(), "");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let stale = (*service.manager.get(&id).unwrap()).clone();
        let r1 = next_revision();
        let r2 = next_revision();
        codex_file(
            dir.path(),
            "revision.json",
            r#","access_token":"C","prefix":"fresh""#,
        );
        let mut held = hold_read(&mut service);
        service.load_auth_file(&auth_file(&path), r1);
        held.entered().await;
        let (applied, mut done) = oneshot::channel();
        service.apply_sync(SyncRequest {
            change: SyncChange::Upsert(Box::new(stale)),
            revision: r2,
            applied,
            operation: None,
        });
        assert!(
            done.try_recv().is_err(),
            "same-key acknowledgement is parked"
        );
        held.release();
        let completion =
            tokio::time::timeout(Duration::from_secs(5), service.read_completions.recv())
                .await
                .unwrap()
                .unwrap();
        let r3 = match &completion.result {
            Ok(AdoptionResult::Published { revision, .. }) => *revision,
            _ => panic!("fresh read publishes"),
        };
        service.complete_read(completion);
        assert_eq!(
            service.revisions[&id], r3,
            "watermark is fresh read revision, not trigger"
        );
        assert!(r3 > r2);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("done acknowledgement arrives within 5s"),
            Ok(())
        );
        assert_eq!(service.manager.get(&id).unwrap().prefix, "fresh");
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "C"
        );
        assert!(
            service.reads.is_empty(),
            "stale management result starts no new read"
        );
        // A management hint may identify its record by id only. Existing
        // registrations supply their origin; an initial read supplies its own.
        for registered in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "pathless.json", r#","prefix":"fresh-origin""#);
            let mut service = self::service(dir.path(), "");
            if registered {
                service.load_file_auths();
            }
            let mut hint = Auth {
                id: "pathless.json".into(),
                provider: "codex".into(),
                prefix: "captured".into(),
                ..Auth::default()
            };
            hint.metadata
                .insert("type".into(), serde_json::json!("codex"));
            let (applied, done) = oneshot::channel();
            service.apply_sync(SyncRequest {
                change: SyncChange::Upsert(Box::new(hint)),
                revision: next_revision(),
                applied,
                operation: None,
            });
            drain_reads(&mut service).await;
            let current = service
                .manager
                .get("pathless.json")
                .expect("pathless service observation synthesizes resolved fresh origin");
            assert_eq!(
                current.prefix, "fresh-origin",
                "pathless service observation uses fresh config synthesis"
            );
            assert_eq!(
                service.file_auths.get(&path),
                Some(&current.id),
                "pathless service observation maps resolved origin"
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), done)
                    .await
                    .expect("done acknowledgement arrives within 5s"),
                Ok(())
            );
            assert!(service.dirty_reads.is_empty());
        }
    }

    #[tokio::test]
    async fn service_parks_removal_aliases_until_fresh_completion() {
        for registered in [false, true] {
            for management in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = codex_file(dir.path(), "removal.json", "");
                let mut service = service(dir.path(), "");
                if registered {
                    service.load_file_auths();
                }
                let r1 = next_revision();
                let r2 = next_revision();
                let mut held = hold_read(&mut service);
                service.load_auth_file(&auth_file(&path), r1);
                held.entered().await;
                let (applied, done) = oneshot::channel();
                let alias = PathBuf::from("./removal.json");
                if management {
                    service.apply_sync(SyncRequest {
                        change: SyncChange::FileRemoved(alias),
                        revision: r2,
                        applied,
                        operation: None,
                    });
                } else {
                    service
                        .handle(WatchEvent::AuthRemoved(alias, r2), Path::new(""))
                        .await;
                    drop(applied);
                }
                assert_eq!(
                    service.reads.len(),
                    1,
                    "removal waits behind one logical-key job"
                );
                // Another key can finish while this read is held.
                let other = codex_file(dir.path(), "other.json", "");
                service.load_auth_file(&auth_file(&other), next_revision());
                consume_read(&mut service).await;
                assert!(
                    service.file_auths.contains_key(&other),
                    "other keys continue draining"
                );
                held.release();
                drain_reads(&mut service).await;
                let id = service
                    .file_auths
                    .get(&path)
                    .expect("fresh registration survives parked stale removal");
                assert!(
                    service.manager.get(id).is_some(),
                    "registration survives completion-first removal accounting"
                );
                assert!(service.revisions[id] > r2);
                if management {
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(5), done)
                            .await
                            .expect("done acknowledgement arrives within 5s"),
                        Ok(())
                    );
                }
                assert!(service.dirty_reads.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn service_dirty_failures_preserve_live_pending_and_file_mapping() {
        for failure in [
            "missing",
            "empty",
            "null",
            "invalid",
            "wrong-provider",
            "read-error",
        ] {
            for registered in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let path = codex_file(dir.path(), "dirty.json", r#","refresh_token":"A-refresh""#);
                let payload = auth_file(&path);
                let mut service = service(dir.path(), "");
                if registered {
                    let store = Arc::new(ServiceRefreshStore {
                        files: service.store.clone(),
                        fail: std::sync::atomic::AtomicBool::new(true),
                    });
                    service.manager = Manager::new(
                        Settings::from(&*service.config),
                        service.registry.clone(),
                        Some(store),
                    );
                    service.load_file_auths();
                    let id = service.manager.list()[0].id.clone();
                    service
                        .manager
                        .register_executor(Arc::new(NamedRefresher("pending")));
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        service.manager.force_refresh(&id),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert_eq!(
                        service
                            .manager
                            .get(&id)
                            .unwrap()
                            .codex_pending_refresh
                            .len(),
                        1,
                        "failed-read fixture has unwritten pending work"
                    );
                }
                let before = service.manager.list().first().cloned();
                let mapping = service.file_auths.clone();
                match failure {
                    "missing" => std::fs::remove_file(&path).unwrap(),
                    "empty" => std::fs::write(&path, "").unwrap(),
                    "null" => std::fs::write(&path, "null").unwrap(),
                    "invalid" => std::fs::write(&path, "{broken").unwrap(),
                    "wrong-provider" => std::fs::write(&path, r#"{"type":"gemini"}"#).unwrap(),
                    _ => std::fs::write(
                        &path,
                        vec![
                            b' ';
                            open_ferry_core::auth::file_store::MAX_AUTH_FILE_SIZE as usize + 1
                        ],
                    )
                    .unwrap(),
                }
                service.load_auth_file(&payload, next_revision());
                drain_reads(&mut service).await;
                assert_eq!(
                    service.dirty_reads.len(),
                    1,
                    "every supported failed read is dirty"
                );
                assert_eq!(
                    service.file_auths, mapping,
                    "failed read leaves file mapping untouched"
                );
                if let Some(before) = before {
                    assert!(
                        Arc::ptr_eq(&before, &service.manager.get(&before.id).unwrap()),
                        "failed read publishes nothing and preserves pending"
                    );
                } else {
                    assert!(
                        service.manager.list().is_empty(),
                        "never-registered failed read adopts no payload"
                    );
                }
                assert!(
                    service.revisions.is_empty(),
                    "failed reads claim no revision"
                );
            }
        }
    }

    #[tokio::test]
    async fn service_dirty_tail_adopts_hash_suppressed_repair_without_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "tail.json", r#","prefix":"hashed-A""#);
        let hashed = auth_file(&path);
        let mut service = service(dir.path(), "");
        std::fs::write(&path, "null").unwrap();
        service.load_auth_file(&hashed, next_revision());
        drain_reads(&mut service).await;
        for attempt in 1..=6 {
            assert_eq!(
                service.dirty_reads.len(),
                1,
                "dirty entry survives every retry including the 30s tail"
            );
            let dirty = service.dirty_reads.values().next().unwrap();
            assert_eq!(dirty.attempt, attempt);
            assert_eq!(
                retry_delay(attempt - 1),
                [50, 200, 1000, 5000, 30000, 30000].map(Duration::from_millis)[attempt - 1]
            );
            fire_dirty_deadline(&mut service).await;
        }
        std::fs::write(&path, &hashed.data).unwrap(); // Watcher already hashed A.
        fire_dirty_deadline(&mut service).await;
        assert_eq!(
            service.manager.list()[0].prefix,
            "hashed-A",
            "30s tail adopts repaired hashed bytes without event"
        );
        assert!(
            service.dirty_reads.is_empty(),
            "success leaves no retry timer"
        );
        assert!(service.reads.is_empty());
    }

    #[tokio::test]
    async fn service_dirty_dedup_reset_and_removal_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "reset.json", "");
        let payload = auth_file(&path);
        let mut service = service(dir.path(), "");
        service.handle_and_drain(added(&path), Path::new("")).await;
        let id = service.manager.list()[0].id.clone();
        let stale = service.revisions[&id];
        std::fs::write(&path, "null").unwrap();
        service.load_auth_file(&payload, next_revision());
        drain_reads(&mut service).await;
        fire_dirty_deadline(&mut service).await;
        assert_eq!(service.dirty_reads.values().next().unwrap().attempt, 2);
        service.remove_auth_file(&path, stale);
        assert_eq!(
            service.dirty_reads.len(),
            1,
            "stale removal cancels nothing"
        );
        let mut held = hold_read(&mut service);
        service.load_auth_file(&payload, next_revision());
        assert_eq!(
            service.reads.values().next().unwrap().attempt,
            0,
            "new event resets backoff"
        );
        held.entered().await;
        service.remove_auth_file(&path, stale);
        for _ in 0..4 {
            service.load_auth_file(&payload, next_revision());
        }
        assert_eq!(
            service.reads.len(),
            1,
            "duplicate events have one outstanding read"
        );
        assert!(
            service.dirty_reads.is_empty(),
            "no timer while read is outstanding"
        );
        held.release();
        drain_reads(&mut service).await;
        assert_eq!(
            held.calls.load(Ordering::SeqCst),
            2,
            "parked duplicates coalesce to one further read"
        );
        assert_eq!(
            service.dirty_reads.len(),
            1,
            "duplicate events retain only one dirty timer"
        );
        assert_eq!(
            service.dirty_reads.values().next().unwrap().attempt,
            1,
            "new event resets backoff"
        );
        service.remove_auth_file(&path, next_revision());
        assert!(
            service.dirty_reads.is_empty(),
            "accepted removal cancels retry"
        );
        assert!(service.manager.get(&id).is_none());
        service.retry_dirty_reads();
        assert!(service.reads.is_empty());
    }

    #[tokio::test]
    async fn service_dirty_context_changes_and_shutdown_cancel_retries() {
        for change in [
            "registration",
            "origin",
            "shutdown",
            "new-registration-event",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = codex_file(dir.path(), "context.json", "");
            let payload = auth_file(&path);
            let mut service = service(dir.path(), "");
            service.load_file_auths();
            let id = service.manager.list()[0].id.clone();
            std::fs::write(&path, "null").unwrap();
            service.load_auth_file(&payload, next_revision());
            drain_reads(&mut service).await;
            match change {
                "registration" | "new-registration-event" => {
                    let auth = (*service.manager.get(&id).unwrap()).clone();
                    service.manager.remove(&id);
                    service.manager.register_unsaved(auth).unwrap();
                }
                "origin" => {
                    service
                        .manager
                        .edit_unsaved(&id, |auth| {
                            auth.attributes
                                .insert("path".into(), "changed-origin.json".into());
                            Ok::<_, ()>(true)
                        })
                        .unwrap();
                }
                _ => service.close_sync(),
            }
            if change == "new-registration-event" {
                std::fs::write(&path, &payload.data).unwrap();
                let before = service.manager.get(&id).unwrap();
                service.load_auth_file(&payload, next_revision());
                drain_reads(&mut service).await;
                assert!(
                    !Arc::ptr_eq(&before, &service.manager.get(&id).unwrap()),
                    "new event uses replacement registration instead of stale dirty context"
                );
            } else if !service.dirty_reads.is_empty() {
                service.dirty_reads.values_mut().next().unwrap().deadline =
                    tokio::time::Instant::now();
                service.work(Path::new(""), &mut None).await;
                assert!(
                    service.reads.is_empty(),
                    "context change starts no retry read"
                );
            }
            assert!(
                service.dirty_reads.is_empty(),
                "context change cancels dirty state"
            );
            assert!(
                service.reads.is_empty(),
                "context change starts no retry read"
            );
        }
    }

    #[tokio::test]
    async fn service_config_generation_reruns_held_synthesis_with_current_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "config-generation.json", "");
        let mut service = service(dir.path(), "oauth-excluded-models:\n  codex: [old-model]\n");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        let mut held = hold_read(&mut service);
        service.load_auth_file(&auth_file(&path), next_revision());
        held.entered().await;
        let mut config = (*service.config).clone();
        config
            .oauth_excluded_models
            .insert("codex".into(), vec!["new-model".into()]);
        service.apply_config_authorized(Arc::new(config), None, Path::new(""));
        held.release();
        drain_reads(&mut service).await;
        let live = service.manager.get(&id).unwrap();
        let mut expected = (*live).clone();
        assert!(
            !apply_config_attributes(&service.synthesis_context(), &mut expected),
            "held synthesis publishes C1 attributes, never C0"
        );
        assert_eq!(
            held.calls.load(Ordering::SeqCst),
            2,
            "config mismatch reruns synthesis exactly once"
        );
        assert!(
            service.dirty_reads.is_empty(),
            "config Retry-now pays no dirty penalty"
        );
    }

    #[tokio::test]
    async fn service_observation_rejects_changed_registration_and_origin() {
        for initially_registered in [false, true] {
            for before_read in [false, true] {
                for change_origin in ["registration", "path", "provider", "file-name"] {
                    let dir = tempfile::tempdir().unwrap();
                    let path = codex_file(dir.path(), "validation.json", "");
                    let mut service = service(dir.path(), "");
                    if initially_registered {
                        service.load_file_auths();
                    }
                    let mut held = (!before_read).then(|| hold_read(&mut service));
                    service.load_auth_file(&auth_file(&path), next_revision());
                    if let Some(held) = &mut held {
                        held.entered().await;
                    }
                    let id = open_ferry_core::auth::file_store::id_for(
                        &path,
                        &path.to_string_lossy(),
                        dir.path(),
                    );
                    let mut replacement = identity_hint(
                        &ReadIdentity {
                            id: Some(id.clone()),
                            provider: "codex".into(),
                            path: Some(path.clone()),
                            file_name: "validation.json".into(),
                            registration: None,
                            tombstone: 0,
                            binding: service.store.binding_generation(),
                        },
                        &service.synthesis_context(),
                    );
                    replacement
                        .metadata
                        .insert("access_token".into(), serde_json::json!("replacement"));
                    replacement.prefix = "replacement".into();
                    if initially_registered && change_origin != "registration" {
                        service
                            .manager
                            .edit_unsaved(&id, |auth| {
                                *auth = replacement;
                                match change_origin {
                                    "path" => {
                                        auth.attributes
                                            .insert("path".into(), "changed-origin.json".into());
                                    }
                                    "provider" => auth.provider = "gemini".into(),
                                    "file-name" => auth.file_name = "different.json".into(),
                                    _ => unreachable!(),
                                }
                                Ok::<_, ()>(true)
                            })
                            .unwrap();
                    } else {
                        service.manager.register_unsaved(replacement).unwrap();
                    }
                    let before = service.manager.get(&id).unwrap();
                    if let Some(held) = &mut held {
                        held.release();
                    }
                    drain_reads(&mut service).await;
                    assert!(
                        Arc::ptr_eq(&before, &service.manager.get(&id).unwrap()),
                        "publication rejects replacement registration and origin changes"
                    );
                    assert!(service.file_auths.is_empty() || initially_registered);
                    assert!(
                        service.revisions.is_empty(),
                        "rejected publication claims no revision"
                    );
                    assert!(
                        service.dirty_reads.is_empty(),
                        "changed registration never retries old context"
                    );
                }
            }
        }
    }

    async fn drain_reads(service: &mut Service) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !service.reads.is_empty() {
                let completion = service
                    .read_completions
                    .recv()
                    .await
                    .expect("read completion");
                service.complete_read(completion);
            }
        })
        .await
        .expect("service reads complete within 5s");
    }

    impl Service {
        async fn handle_and_drain(&mut self, event: WatchEvent, config_path: &Path) -> Watching {
            let result = self.handle(event, config_path).await;
            drain_reads(self).await;
            result
        }
    }

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

    /// The watcher's report that the config file changed and loaded as
    /// `config`, from contents these tests don't keep.
    fn config_changed(config: Arc<Config>) -> WatchEvent {
        WatchEvent::ConfigChanged(config, String::new())
    }

    // Ported from sdk/cliproxy/service_codex_executor_binding_test.go (v8.0.27).
    #[tokio::test]
    async fn rebinding_codex_and_xai_keeps_their_shared_session_stores() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(dir.path(), "proxy-url: direct\n");
        let codex = service.manager.executor("codex").unwrap();
        let xai = service.manager.executor("xai").unwrap();
        service.config = Arc::new(Config::parse("proxy-url: none\n").unwrap());
        service.register_codex_executor();
        service.register_xai_executor();
        let next_codex = service.manager.executor("codex").unwrap();
        let next_xai = service.manager.executor("xai").unwrap();
        assert!(!Arc::ptr_eq(&codex, &next_codex));
        assert!(!Arc::ptr_eq(&xai, &next_xai));
        assert!(codex.shares_execution_sessions_with(&*next_codex));
        assert!(xai.shares_execution_sessions_with(&*next_xai));
    }

    // Not upstream's: a client's connection has Nagle's algorithm off, as
    // Go's net package has it on every connection upstream's server accepts.
    #[tokio::test]
    async fn sends_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (tcp, _) = listener.accept().await.unwrap();
        tcp.set_nodelay(false).unwrap();
        send_at_once(&tcp);
        assert!(tcp.nodelay().unwrap());
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

    /// A catalog file in `dir`: the built-in general catalog with the Claude
    /// model `model` added.
    fn catalog_file(dir: &Path, name: &str, model: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, catalog_with(model)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// The built-in general catalog with the Claude model `model` added.
    fn catalog_with(model: &str) -> Vec<u8> {
        let built_in = open_ferry_translate::models::embedded_catalog_json();
        let mut root: serde_json::Value = serde_json::from_str(built_in).unwrap();
        root["claude"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "id": model }));
        serde_json::to_vec(&root).unwrap()
    }

    /// Whether any credential serves `model`.
    fn serves(service: &Service, model: &str) -> bool {
        service
            .manager
            .list()
            .iter()
            .any(|auth| model_ids(service, &auth.id).iter().any(|id| id == model))
    }

    /// Applies the catalog changes the service is told of until a
    /// credential serves `model`.
    async fn await_catalog_model(service: &mut Service, model: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !serves(service, model) {
            let changed = tokio::time::timeout_at(deadline, service.catalog_changes.recv())
                .await
                .unwrap_or_else(|_| panic!("no credential came to serve {model}"))
                .unwrap();
            service.refresh_catalog_models(&changed);
        }
    }

    // Ported from sdk/cliproxy/service_catalogs_test.go
    // (TestServiceCatalogStartupAndConfigReload), with the general catalog
    // in place of Devin's, which isn't ported, and a Claude API key whose
    // models come from it: the file the config names is read at start,
    // before the credentials are registered; a reload naming another file
    // reads that one; a change to the file is read and the credential's
    // models registered again; and after a stop, a new start reads the same
    // source again.
    #[tokio::test]
    async fn catalog_files_are_read_at_start_on_reload_and_when_they_change() {
        let dir = tempfile::tempdir().unwrap();
        let key = "claude-api-key:\n  - api-key: dummy-claude-key\n";
        let models = |path: &str| format!("models:\n  catalog: '{path}'\n{key}");
        let first = catalog_file(dir.path(), "first.json", "claude-from-first-file");
        let mut service = service(dir.path(), &models(&first));
        service.catalogs = CatalogRuntime::new(
            Arc::new(open_ferry_core::registry::CatalogStore::new()),
            Duration::from_millis(10),
        );
        service.start_catalogs();
        service.sync_config_auths();
        assert!(serves(&service, "claude-from-first-file"));

        let second = catalog_file(dir.path(), "second.json", "claude-from-second-file");
        let config = format!("auth-dir: '{}'\n{}", dir.path().display(), models(&second));
        let config = Arc::new(Config::parse(config).unwrap());
        service
            .apply_config(config, None, &dir.path().join("config.yaml"))
            .await;
        assert!(serves(&service, "claude-from-second-file"));
        assert!(!serves(&service, "claude-from-first-file"));

        std::fs::write(&second, catalog_with("claude-from-the-changed-file")).unwrap();
        await_catalog_model(&mut service, "claude-from-the-changed-file").await;
        assert!(!serves(&service, "claude-from-second-file"));

        service.stop_catalogs();
        std::fs::write(&second, catalog_with("claude-after-a-restart")).unwrap();
        service.start_catalogs();
        await_catalog_model(&mut service, "claude-after-a-restart").await;
        service.stop_catalogs();
    }

    // Not upstream's: the update checks follow a reloaded config's
    // `self-update`.
    #[tokio::test]
    async fn a_reload_passes_self_update_to_the_update_checks() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(dir.path(), "");
        let make_fetch = update_checks::http_fetch();
        let updater = Updater {
            base: open_ferry_update::fetch::parse_base_url("http://127.0.0.1:9/").unwrap(),
            keys: open_ferry_update::ReleaseKeys::none(),
            system: Arc::new(open_ferry_update::install::RealSystem { write_probe: false }),
            ..Updater::for_this_binary(
                make_fetch("").unwrap(),
                DataDir::at(dir.path().join("updates")),
            )
            .unwrap()
        };
        let updates = UpdateService::new(updater, make_fetch, &service.config);
        service.management = service.management.clone().with_updates(updates.clone());
        assert_eq!(
            updates.settings().mode,
            open_ferry_update::SelfUpdateMode::Auto
        );

        let config = format!(
            "auth-dir: '{}'
self-update:
  mode: off
",
            dir.path().display()
        );
        service
            .apply_config(
                Arc::new(Config::parse(config).unwrap()),
                None,
                &dir.path().join("config.yaml"),
            )
            .await;

        assert_eq!(
            updates.settings().mode,
            open_ferry_update::SelfUpdateMode::Off
        );
        stop_update_checks(&service.management);
    }

    #[tokio::test]
    async fn bind_checks_the_port_and_brackets_ipv6() {
        let error = bind("127.0.0.1", 70_000).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bind("127.0.0.1", -1).is_err());

        let listener = bind(" 127.0.0.1 ", 0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        // Listening on every interface makes Windows Firewall ask about each
        // test binary built, one per worktree; Linux CI checks it.
        #[cfg(not(windows))]
        {
            let listener = bind("", 0).unwrap();
            assert!(listener.local_addr().unwrap().ip().is_unspecified());
        }
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
        service.handle_and_drain(added(&path), Path::new("")).await;
        codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let rewritten = std::fs::read(&path).unwrap();
        service
            .handle_and_drain(changed(auth_file(&path)), Path::new(""))
            .await;
        assert_eq!(service.manager.list().len(), 1);
        assert_eq!(service.manager.get(&id).unwrap().prefix, "team");
        assert_ne!(written, rewritten);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            rewritten,
            "the file was rewritten"
        );

        service
            .handle_and_drain(removed(&path), Path::new(""))
            .await;
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.file_auths.is_empty());
    }

    // Not upstream's: a resume's `apply_deferred` fails when the auth
    // directory it names, unchanged, can no longer be listed (here replaced
    // by a regular file), and neither forgets the credentials it had nor
    // takes the empty listing for the directory's. No watcher runs here to
    // hold the directory, so it can be replaced on Windows too.
    #[tokio::test]
    async fn apply_deferred_fails_for_an_auth_dir_that_cant_be_listed() {
        let root = tempfile::tempdir().unwrap();
        let auth_dir = root.path().join("auth");
        std::fs::create_dir(&auth_dir).unwrap();
        let path = codex_file(&auth_dir, "codex-a.json", "");
        let mut service = service(&auth_dir, "");
        service.load_file_auths();
        let id = service.manager.list()[0].id.clone();
        assert_eq!(service.file_auths.get(&path), Some(&id));

        std::fs::remove_dir_all(&auth_dir).unwrap();
        std::fs::write(&auth_dir, "a file").unwrap();
        let error = service
            .apply_deferred(Path::new(""), &mut None)
            .await
            .unwrap_err();
        assert!(error.contains("couldn't be read"), "{error}");
        assert!(
            service.manager.get(&id).is_some(),
            "the credential was dropped"
        );
        assert_eq!(service.file_auths.get(&path), Some(&id));
        assert_eq!(service.manager.list().len(), 1);
    }

    #[tokio::test]
    async fn an_auth_file_that_stops_parsing_stays_live_and_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        service.handle_and_drain(added(&path), Path::new("")).await;
        let id = service.manager.list()[0].id.clone();

        let broken = AuthFile {
            path: path.clone(),
            data: Arc::from(&b"{not json"[..]),
        };
        std::fs::write(&path, "{not json").unwrap();
        service
            .handle_and_drain(changed(broken), Path::new(""))
            .await;
        assert!(
            service.manager.get(&id).is_some(),
            "invalid read keeps live registration"
        );
        assert!(!service.registry.models_for_client(&id).is_empty());
        assert_eq!(service.file_auths.get(&path), Some(&id));
        assert_eq!(
            service.dirty_reads.len(),
            1,
            "invalid read retains dirty timer"
        );
    }

    #[tokio::test]
    async fn auth_events_read_current_contents_and_retry_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let checked = auth_file(&path);
        // A write that doesn't parse lands while the event waits.
        std::fs::write(&path, "{not json").unwrap();
        let mut service = service(dir.path(), "");
        service
            .handle_and_drain(
                WatchEvent::AuthAdded(checked, next_revision()),
                Path::new(""),
            )
            .await;
        assert!(
            service.manager.list().is_empty(),
            "captured payload is never adopted after invalid fresh read"
        );
        assert!(service.file_auths.is_empty());
        assert_eq!(
            service.dirty_reads.len(),
            1,
            "never-registered path stays dirty"
        );
    }

    #[tokio::test]
    async fn a_missing_fresh_read_stays_live_until_explicit_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(dir.path(), "codex-a.json", "");
        let mut service = service(dir.path(), "");
        let stale = auth_file(&path);
        service
            .handle_and_drain(
                WatchEvent::AuthAdded(stale.clone(), next_revision()),
                Path::new(""),
            )
            .await;
        let id = service.manager.list()[0].id.clone();

        // The file is deleted before the watcher's report of a change to it
        // is applied.
        std::fs::remove_file(&path).unwrap();
        service
            .handle_and_drain(changed(stale.clone()), Path::new(""))
            .await;
        assert!(
            service.manager.get(&id).is_some(),
            "missing read is not a removal"
        );
        assert_eq!(service.file_auths.get(&path), Some(&id));
        assert_eq!(
            service.dirty_reads.len(),
            1,
            "missing read retains dirty timer"
        );
        service
            .handle_and_drain(WatchEvent::AuthAdded(stale, next_revision()), Path::new(""))
            .await;
        assert!(service.manager.get(&id).is_some());
        service
            .handle_and_drain(removed(&path), Path::new(""))
            .await;
        assert!(service.manager.list().is_empty());
        assert!(service.dirty_reads.is_empty());
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
        let request = tokio::time::timeout(Duration::from_secs(5), service.sync_requests.recv())
            .await
            .expect("request arrival 3080 within 5s")
            .unwrap();
        service.apply_sync(request);
        drain_reads(service).await;
        tokio::time::timeout(Duration::from_secs(5), sent)
            .await
            .expect("sync task acknowledges within 5s (case 1)")
            .unwrap()
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
        codex_file(dir.path(), "codex-a.json", r#","prefix":"other""#);
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
        drop(
            tokio::time::timeout(Duration::from_secs(5), service.sync_requests.recv())
                .await
                .expect("request arrival 3152 within 5s")
                .unwrap(),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), sent)
                .await
                .expect("sync task acknowledges within 5s (case 2)")
                .unwrap(),
            Err(SyncError::Stopped)
        );

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
                operation: None,
            })
            .unwrap_or_else(|_| panic!("the queue is full"));
        service.close_sync();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("management_changes_fail_once_the_loop_stops done acknowledges within 5s")
                .is_err(),
            "shutdown cancels queued observations"
        );
        assert!(service.manager.list().is_empty());
        let error = sync.file_removed(path).await.unwrap_err();
        assert_eq!(error, SyncError::Stopped);
        assert_eq!(error.status().as_u16(), 503);
        assert!(service.manager.list().is_empty());
    }

    #[tokio::test]
    async fn a_directory_apply_stays_admitted_when_quiescing_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let new_auth = dir.path().join("new-auth");
        let mut service = service(dir.path(), "");
        let mut config = Config::clone(&service.config);
        config.auth_dir = new_auth.to_string_lossy().into_owned();
        std::fs::write(&path, format!("auth-dir: {}", new_auth.display())).unwrap();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        service.directory_apply_hook = Some((arrived.clone(), release.clone()));
        let gate = service.gate.clone();
        let changing = tokio::spawn(async move {
            service
                .apply_config(Arc::new(config), Some("new-hash".into()), &path)
                .await;
            service
        });
        tokio::time::timeout(Duration::from_secs(5), arrived.notified())
            .await
            .expect("request arrival 3208 within 5s");
        let closing_gate = gate.clone();
        let closing = tokio::spawn(async move { closing_gate.close(closing_gate.epoch()).await });
        while gate.state() == open_ferry_core::gate::GateState::Open {
            tokio::task::yield_now().await;
        }
        release.notify_one();
        let service = tokio::time::timeout(Duration::from_secs(5), changing)
            .await
            .expect("directory apply task completes within 5s")
            .unwrap();
        assert_eq!(service.config.resolve_auth_dir().unwrap(), new_auth);
        assert_eq!(service.auth_dir, new_auth);
        assert_eq!(service.store.base_dir(), new_auth);
        assert_eq!(service.watcher.as_ref().unwrap().auth_dir(), new_auth);
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("gate close task completes within 5s (case 1)")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_pending_ticket_applies_its_directory_under_its_operation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let new_auth = dir.path().join("new-auth");
        let mut service = service(dir.path(), "");
        std::fs::write(&path, format!("auth-dir: '{}'", dir.path().display())).unwrap();
        let (watcher, receiver) = ConfigWatcher::start(&path, &service.config).unwrap();
        service.watcher = Some(watcher);
        let mut events = Some(receiver);
        let (first, _first_done) = oneshot::channel();
        service.last_reload = 1;
        service.pending_reloads.push_back((
            1,
            ReloadRequest {
                force: false,
                applied: first,
                operation: None,
            },
        ));
        let (applied, done) = oneshot::channel();
        let operation = service.gate.begin_operation().unwrap();
        service
            .request_reload(
                ReloadRequest {
                    force: false,
                    applied,
                    operation: Some(operation),
                },
                &path,
            )
            .await;
        // Consume the initial reload through its marker before rewriting.
        // A fresh watcher reports the old snapshot first; it is not the
        // changed directory this mixed-ticket test must apply.
        let mut saw_old = false;
        loop {
            match next_watched(&mut events).await {
                WatchEvent::ConfigChanged(config, _) => {
                    assert_eq!(config.resolve_auth_dir().unwrap(), dir.path());
                    saw_old = true;
                }
                WatchEvent::Reloaded(2) => break,
                _ => {}
            }
        }
        assert!(saw_old);
        // Hold the new snapshot until after the gate begins closing.
        let gate = service.gate.clone();
        let closing_gate = gate.clone();
        let closing = tokio::spawn(async move { closing_gate.close(closing_gate.epoch()).await });
        while gate.state() == open_ferry_core::gate::GateState::Open {
            tokio::task::yield_now().await;
        }
        std::fs::write(&path, format!("auth-dir: '{}'", new_auth.display())).unwrap();
        service.watcher.as_ref().unwrap().force_reload_config(2);
        // Ignore an earlier unchanged answer; retain the changed snapshot.
        let snapshot = loop {
            let event = next_watched(&mut events).await;
            if matches!(&event, WatchEvent::ConfigChanged(config, _)
                if config.resolve_auth_dir().unwrap() == new_auth)
            {
                break event;
            }
        };
        let (sender, receiver) = mpsc::channel(2);
        sender.send(snapshot).await.unwrap();
        events = Some(receiver);
        service.work(&path, &mut events).await;
        assert_eq!(service.auth_dir, new_auth);
        assert_eq!(service.store.base_dir(), new_auth);
        assert!(service.deferred_config.is_none());
        service.reloaded(2);
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("pending ticket acknowledges within 5s")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("gate close task completes within 5s (case 2)")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn deferred_mixed_reload_tickets_release_the_closing_gate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut service = service(dir.path(), "");
        let (first, mut first_done) = oneshot::channel();
        let (second, mut second_done) = oneshot::channel();
        service.pending_reloads.push_back((
            1,
            ReloadRequest {
                force: false,
                applied: first,
                operation: None,
            },
        ));
        service.pending_reloads.push_back((
            2,
            ReloadRequest {
                force: false,
                applied: second,
                operation: Some(service.gate.begin_operation().unwrap()),
            },
        ));
        let gate = service.gate.clone();
        let closing_gate = gate.clone();
        let closing = tokio::spawn(async move { closing_gate.close(closing_gate.epoch()).await });
        while gate.state() == open_ferry_core::gate::GateState::Open {
            tokio::task::yield_now().await;
        }
        let mut config = Config::clone(&service.config);
        config.auth_dir = dir.path().join("new-auth").to_string_lossy().into_owned();
        // An answer that arrived outside the owned work scope was deferred.
        service
            .handle_and_drain(
                WatchEvent::ConfigChanged(Arc::new(config), "new".into()),
                &path,
            )
            .await;
        assert!(service.deferred_config.is_some());
        service
            .handle_and_drain(WatchEvent::Reloaded(2), &path)
            .await;
        tokio::time::timeout(Duration::from_secs(1), closing)
            .await
            .expect("answered reload tickets kept the gate live")
            .unwrap()
            .unwrap();
        assert!(service.pending_reloads.is_empty());
        assert_eq!(
            first_done.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        assert_eq!(
            second_done.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
    }

    #[tokio::test]
    async fn a_failed_directory_retry_waits_for_the_live_watchers_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let blocked_dir = dir.path().join("a-file");
        std::fs::write(&blocked_dir, "not a directory").unwrap();
        let mut service = service(dir.path(), "");
        std::fs::write(&path, format!("auth-dir: '{}'", dir.path().display())).unwrap();
        let (watcher, _receiver) = ConfigWatcher::start(&path, &service.config).unwrap();
        service.watcher = Some(watcher);
        // Control event delivery to model the live watcher reading a pending
        // reload while its event queue is empty and the gate has reopened.
        let (sender, receiver) = mpsc::channel(1);
        let mut events = Some(receiver);
        let mut config = Config::clone(&service.config);
        config.auth_dir = blocked_dir.to_string_lossy().into_owned();
        service.deferred_config = Some((Arc::new(config), None));
        let (applied, mut done) = oneshot::channel();
        service.pending_reloads.push_back((
            1,
            ReloadRequest {
                force: false,
                applied,
                operation: None,
            },
        ));
        service.work(&path, &mut events).await;
        assert_eq!(service.auth_dir, dir.path());
        assert!(service.deferred_config.is_none(), "retry did not run");
        assert!(service.watcher.is_some());
        assert_eq!(done.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert_eq!(service.pending_reloads.len(), 1);
        sender.send(WatchEvent::Reloaded(1)).await.unwrap();
        service.work(&path, &mut events).await;
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("directory retry acknowledges within 5s")
            .unwrap();
        assert!(service.pending_reloads.is_empty());
    }

    #[tokio::test]
    async fn queued_watcher_events_do_not_starve_sync_or_management_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut service = service(dir.path(), "");
        std::fs::write(
            &path,
            format!("auth-dir: '{}'\nport: 3\n", dir.path().display()),
        )
        .unwrap();
        let sync = sync_sender(&mut service);
        let (sync_applied, mut sync_done) = oneshot::channel();
        sync.requests
            .try_send(SyncRequest {
                change: SyncChange::FileRemoved(dir.path().join("absent.json")),
                revision: next_revision(),
                applied: sync_applied,
                operation: None,
            })
            .unwrap_or_else(|_| panic!("sync queue full"));
        let (reloads, receiver) = mpsc::channel(1);
        service.reload_requests = receiver;
        let (reload_applied, mut reload_done) = oneshot::channel();
        reloads
            .try_send(ReloadRequest {
                force: false,
                applied: reload_applied,
                operation: None,
            })
            .unwrap_or_else(|_| panic!("reload queue full"));
        let (watching, receiver) = mpsc::channel(1);
        let mut events = Some(receiver);
        let mut synced = false;
        let mut reloaded = false;
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..128 {
                // Replenish after each event: the watcher never becomes idle.
                if watching.capacity() == 1 {
                    watching.try_send(WatchEvent::Reloaded(0)).unwrap();
                }
                service.work(&path, &mut events).await;
                synced |= sync_done.try_recv().is_ok();
                reloaded |= reload_done.try_recv().is_ok();
                if synced && reloaded {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(synced, "watcher events starved credential sync");
        assert!(reloaded, "watcher events starved management reload");
        assert_eq!(service.config.port, 3);
    }

    #[tokio::test]
    async fn queued_snapshots_win_over_a_deferred_directory_retry() {
        // Exercise both fair-select starting orders if the retry guard regresses.
        for _ in 0..32 {
            directory_retry_keeps_newer_disk_config(false).await;
        }
    }

    #[tokio::test]
    async fn a_replacement_watcher_reconciles_after_an_older_retry() {
        directory_retry_keeps_newer_disk_config(true).await;
    }

    async fn directory_retry_keeps_newer_disk_config(retry_first: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let auth_a = dir.path().join("a");
        let auth_b = dir.path().join("b");
        let text_a = format!(
            "auth-dir: '{}'
port: 2
",
            auth_a.display()
        );
        let text_b = format!(
            "auth-dir: '{}'
port: 3
",
            auth_b.display()
        );
        let config_a = Arc::new(Config::parse(&text_a).unwrap());
        let config_b = Arc::new(Config::parse(&text_b).unwrap());
        let hash_b = open_ferry_core::config::save::sha256_hex(text_b.as_bytes());
        let mut service = service(dir.path(), "");
        std::fs::write(&path, &text_b).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(WatchEvent::ConfigChanged(config_b, hash_b.clone()))
            .await
            .unwrap();
        let mut events = Some(receiver);
        service.deferred_config = Some((config_a, Some("a".into())));
        if retry_first {
            // Deliberately exercise the old ordering: replacement drops B's
            // receiver, so only the unconditional reconcile can recover it.
            let (config, hash) = service.deferred_config.take().unwrap();
            let watching = service.apply_config(config, hash, &path).await;
            service.follow(watching, &mut events);
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.config.port != 3 {
                service.work(&path, &mut events).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(service.management.config_sha256(), Some(hash_b));
        assert_eq!(service.auth_dir, auth_b);
        assert_eq!(service.store.base_dir(), auth_b);
        assert_eq!(service.watcher.as_ref().unwrap().auth_dir(), auth_b);
        if !retry_first {
            assert!(
                !auth_a.exists(),
                "the older directory was published before B"
            );
        }
    }

    #[tokio::test]
    async fn a_directory_change_is_applied_once_after_watcher_replacement() {
        replacement_watcher_applies_only_new_disk_contents(false).await;
    }

    #[tokio::test]
    async fn a_seeded_replacement_watcher_still_applies_different_disk_contents() {
        replacement_watcher_applies_only_new_disk_contents(true).await;
    }

    async fn replacement_watcher_applies_only_new_disk_contents(different: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let auth = dir.path().join("new-auth");
        let applied_text = format!("auth-dir: '{}'\nport: 2\n", auth.display());
        let disk_text = if different {
            format!("auth-dir: '{}'\nport: 3\n", auth.display())
        } else {
            applied_text.clone()
        };
        std::fs::write(&path, &disk_text).unwrap();
        let mut service = service(dir.path(), "");
        let applied_config = Arc::new(Config::parse(&applied_text).unwrap());
        let hash = open_ferry_core::config::save::sha256_hex(applied_text.as_bytes());
        let watching = service
            .apply_config(applied_config, Some(hash), &path)
            .await;
        let mut events = None;
        service.follow(watching, &mut events);
        // A later marker proves both the seed's recheck and follow's reconcile
        // have run; no sleeps or notify's debounce timing as evidence.
        service.watcher.as_ref().unwrap().reload_config(99);
        loop {
            let event = next_watched(&mut events).await;
            let finished = event == WatchEvent::Reloaded(99);
            let watching = service.handle(event, &path).await;
            service.follow(watching, &mut events);
            if finished {
                break;
            }
        }
        assert_eq!(service.config_applies, if different { 2 } else { 1 });
        assert_eq!(service.config.port, if different { 3 } else { 2 });
        assert_eq!(
            service.management.config_sha256(),
            Some(open_ferry_core::config::save::sha256_hex(
                disk_text.as_bytes()
            ))
        );
    }

    #[tokio::test]
    async fn an_owned_open_reload_uses_the_watcher_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut service = service(dir.path(), "port: 1\n");
        std::fs::write(
            &path,
            format!("auth-dir: '{}'\nport: 2\n", dir.path().display()),
        )
        .unwrap();
        let (watcher, events) = ConfigWatcher::start(&path, &service.config).unwrap();
        service.watcher = Some(watcher);
        let mut events = Some(events);
        let (applied, done) = oneshot::channel();
        let operation = service.gate.begin_operation().unwrap();
        let watching = service
            .request_reload(
                ReloadRequest {
                    force: false,
                    applied,
                    operation: Some(operation),
                },
                &path,
            )
            .await;
        service.follow(watching, &mut events);
        assert_eq!(
            service.config.port, 1,
            "Open reloads must use the watcher ticket path"
        );
        let mut applies = 0;
        loop {
            let event = next_watched(&mut events).await;
            applies += usize::from(matches!(event, WatchEvent::ConfigChanged(..)));
            let finished = event == WatchEvent::Reloaded(1);
            let watching = service.handle_and_drain(event, &path).await;
            service.follow(watching, &mut events);
            if finished {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("an_owned_open_reload_uses_the_watcher_once done acknowledges within 5s")
            .unwrap();
        service.watcher.as_ref().unwrap().reload_config(2);
        loop {
            let event = next_watched(&mut events).await;
            applies += usize::from(matches!(event, WatchEvent::ConfigChanged(..)));
            if event == WatchEvent::Reloaded(2) {
                break;
            }
            let watching = service.handle_and_drain(event, &path).await;
            service.follow(watching, &mut events);
        }
        assert_eq!(applies, 1);
        assert_eq!(service.config.port, 2);
    }

    #[tokio::test]
    async fn a_direct_reload_discards_older_snapshots_and_records_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let text = |port| format!("auth-dir: '{}'\nport: {port}\n", dir.path().display());
        std::fs::write(&path, text(1)).unwrap();
        let mut service = service(dir.path(), "port: 0\n");
        let (watcher, events) = ConfigWatcher::start(&path, &service.config).unwrap();
        watcher.force_reload_config(0);
        service.watcher = Some(watcher);
        let mut events = Some(events);
        let stale = next_watched(&mut events).await;
        assert!(matches!(stale, WatchEvent::ConfigChanged(..)));
        let operation = service.gate.begin_operation().unwrap();
        let gate = service.gate.clone();
        let closing = tokio::spawn(async move { gate.close(gate.epoch()).await });
        tokio::task::yield_now().await;
        assert_eq!(
            service.gate.state(),
            open_ferry_core::gate::GateState::Quiescing
        );
        std::fs::write(&path, text(2)).unwrap();
        let (applied, done) = oneshot::channel();
        let watching = operation
            .clone()
            .scope(async {
                service
                    .request_reload(
                        ReloadRequest {
                            force: false,
                            applied,
                            operation: Some(operation.clone()),
                        },
                        &path,
                    )
                    .await
            })
            .await;
        service.follow(watching, &mut events);
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("direct reload acknowledges within 5s")
            .unwrap();
        assert_eq!(service.config.port, 2);
        let applied_config = Arc::clone(&service.config);
        service.handle_and_drain(stale, &path).await;
        assert!(
            Arc::ptr_eq(&applied_config, &service.config),
            "a stale snapshot reverted the direct apply"
        );
        service.watcher.as_ref().unwrap().reload_config(2);
        loop {
            let event = next_watched(&mut events).await;
            let finished = event == WatchEvent::Reloaded(2);
            let watching = service.handle_and_drain(event, &path).await;
            service.follow(watching, &mut events);
            assert!(
                Arc::ptr_eq(&applied_config, &service.config),
                "the direct config was applied again"
            );
            if finished {
                break;
            }
        }
        drop(operation);
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("gate close task completes within 5s (case 3)")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_direct_reload_fence_applies_a_later_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let text = |port| {
            format!(
                "auth-dir: '{}'
port: {port}
",
                dir.path().display()
            )
        };
        let mut service = service(
            dir.path(),
            "port: 1
",
        );
        std::fs::write(&path, text(1)).unwrap();
        let (watcher, receiver) = ConfigWatcher::start(&path, &service.config).unwrap();
        service.watcher = Some(watcher);
        let mut events = Some(receiver);
        let operation = service.gate.begin_operation().unwrap();
        let gate = service.gate.clone();
        let closing = tokio::spawn(async move { gate.close(gate.epoch()).await });
        while service.gate.state() == open_ferry_core::gate::GateState::Open {
            tokio::task::yield_now().await;
        }
        std::fs::write(&path, text(2)).unwrap();
        let (applied, done) = oneshot::channel();
        let watching = operation
            .clone()
            .scope(service.request_reload(
                ReloadRequest {
                    force: false,
                    applied,
                    operation: Some(operation.clone()),
                },
                &path,
            ))
            .await;
        service.follow(watching, &mut events);
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("a_direct_reload_fence_applies_a_later_valid_file done acknowledges within 5s")
            .unwrap();
        assert_eq!(service.config.port, 2);
        let later = text(3);
        std::fs::write(&path, &later).unwrap();
        service.watcher.as_ref().unwrap().reload_config(2);
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.config.port != 3 {
                service.work(&path, &mut events).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            service.management.config_sha256(),
            Some(open_ferry_core::config::save::sha256_hex(later.as_bytes()))
        );
        drop(operation);
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("gate close task completes within 5s (case 4)")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn an_admitted_reload_applies_under_quiescing_with_a_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let new_auth = dir.path().join("new-auth");
        let mut service = service(dir.path(), "");
        std::fs::write(&path, format!("auth-dir: '{}'\n", dir.path().display())).unwrap();
        let (watcher, _events) = ConfigWatcher::start(&path, &service.config).unwrap();
        service.watcher = Some(watcher);
        let operation = service.gate.begin_operation().unwrap();
        let gate = service.gate.clone();
        let closing = tokio::spawn(async move { gate.close(gate.epoch()).await });
        tokio::task::yield_now().await;
        assert_eq!(
            service.gate.state(),
            open_ferry_core::gate::GateState::Quiescing
        );
        std::fs::write(&path, format!("auth-dir: '{}'\n", new_auth.display())).unwrap();
        let (applied, done) = oneshot::channel();
        let (requests, receiver) = mpsc::channel(1);
        service.reload_requests = receiver;
        requests
            .send(ReloadRequest {
                force: false,
                applied,
                operation: Some(operation),
            })
            .await
            .unwrap();
        // Keep unrelated watcher events out of this interleaving. The
        // request still has a watcher, so enqueue-only would never apply.
        service.work(&path, &mut None).await;
        tokio::time::timeout(Duration::from_secs(1), done)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(service.auth_dir, new_auth);
        assert_eq!(service.store.base_dir(), new_auth);
        assert!(new_auth.is_dir());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), closing)
                .await
                .expect("gate close task completes within 5s (case 5)")
                .unwrap(),
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_quiescing_directory_reload_retries_after_reopen_without_a_disk_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let new_auth = dir.path().join("new-auth");
        let mut service = service(dir.path(), "");
        let original = Arc::clone(&service.config);
        std::fs::write(
            &path,
            format!(
                "auth-dir: '{}'
",
                new_auth.display()
            ),
        )
        .unwrap();
        let (watcher, events) = ConfigWatcher::start(&path, &service.config).unwrap();
        watcher.reload_config(1);
        service.watcher = Some(watcher);
        let mut events = Some(events);
        let snapshot = next_watched(&mut events).await;
        assert!(matches!(snapshot, WatchEvent::ConfigChanged(..)));
        let operation = service.gate.begin_operation().unwrap();
        let gate = service.gate.clone();
        let closing = tokio::spawn(async move { gate.close(gate.epoch()).await });
        tokio::task::yield_now().await;
        assert_eq!(
            service.gate.state(),
            open_ferry_core::gate::GateState::Quiescing
        );
        service.handle_and_drain(snapshot, &path).await;
        assert!(
            Arc::ptr_eq(&original, &service.config),
            "refused config was published"
        );
        assert!(!new_auth.exists());
        let answer = next_watched(&mut events).await;
        assert_eq!(answer, WatchEvent::Reloaded(1));
        service.handle_and_drain(answer, &path).await;
        drop(operation);
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("gate close task completes within 5s (case 6)")
            .unwrap()
            .unwrap();
        service.gate.reopen();
        // No further filesystem event or reload ticket is needed.
        tokio::time::timeout(Duration::from_secs(1), service.work(&path, &mut events))
            .await
            .unwrap();
        assert_eq!(service.config.resolve_auth_dir().unwrap(), new_auth);
        assert_eq!(service.auth_dir, new_auth);
        assert_eq!(service.store.base_dir(), new_auth);
        assert_eq!(service.watcher.as_ref().unwrap().auth_dir(), new_auth);
    }

    #[tokio::test]
    async fn a_stalled_management_shutdown_preserves_the_ledger_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(dir.path(), "");
        let arrived = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        service.shutdown_hooks = Some((arrived.clone(), finished.clone()));
        let (stop, _) = watch::channel(false);
        let control = serve_loop::ServeControl::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let shutdown = shut_down_by(
            &service,
            &stop,
            None,
            &control,
            deadline,
            Duration::from_millis(50),
        );
        let probe = async {
            tokio::time::timeout(Duration::from_secs(5), arrived.notified())
                .await
                .expect("request arrival 3909 within 5s");
        };
        tokio::join!(shutdown, probe);
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "management used up the ledger reserve"
        );
        assert!(tokio::time::Instant::now() < deadline + Duration::from_millis(100));
    }

    #[test]
    fn an_activation_directory_error_keeps_its_io_cause() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, "file").unwrap();
        let (gate, key) = ActiveGate::standby();
        let mut service = Service::with_gate(
            Arc::new(Config::default()),
            dir.path().join("config.yaml"),
            blocked.clone(),
            LogLevel::detached(),
            gate,
        );
        let error = service.first_activation(&key).unwrap_err();
        assert!(error.contains("failed to create auth directory"), "{error}");
        assert!(error.contains("not-a-directory"), "{error}");
    }

    #[tokio::test]
    async fn a_closed_reload_does_not_create_the_new_auth_directory() {
        let dir = tempfile::tempdir().unwrap();
        let new_auth = dir.path().join("must-not-exist");
        let mut service = service(dir.path(), "");
        service.gate.close(service.gate.epoch()).await.unwrap();
        let config = Config::parse(format!("auth-dir: '{}'\n", new_auth.display())).unwrap();
        service
            .apply_config(Arc::new(config), None, &dir.path().join("config.yaml"))
            .await;
        assert!(!new_auth.exists());
        assert_ne!(service.store.base_dir(), new_auth);
    }

    // Not upstream's: a reload the management API asks for is answered once
    // the watcher's reload is applied, those of a watcher that stops are
    // answered, and once the loop stops a reload returns at once.
    #[tokio::test]
    async fn management_reloads_are_answered_once_applied() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let text = |port: u16| format!("auth-dir: '{}'\nport: {port}\n", dir.path().display());
        std::fs::write(&config_path, text(1)).unwrap();
        let mut service = service(dir.path(), "port: 1\n");
        let (watcher, events) = ConfigWatcher::start(&config_path, &service.config).unwrap();
        service.watcher = Some(watcher);
        let mut events = Some(events);
        let (requests, receiver) = mpsc::channel(1);
        service.reload_requests = receiver;
        let reload = Arc::new(ReloadSender { requests });

        std::fs::write(&config_path, text(2)).unwrap();
        let reloaded = tokio::spawn({
            let reload = Arc::clone(&reload);
            async move { reload.reload().await }
        });
        let request = tokio::time::timeout(Duration::from_secs(5), service.reload_requests.recv())
            .await
            .expect("request arrival 3973 within 5s")
            .unwrap();
        assert!(!request.force);
        let watching = service.request_reload(request, &config_path).await;
        service.follow(watching, &mut events);
        let changed = next_watched(&mut events).await;
        assert!(
            matches!(changed, WatchEvent::ConfigChanged(..)),
            "{changed:?}"
        );
        let watching = service.handle_and_drain(changed, &config_path).await;
        service.follow(watching, &mut events);
        assert_eq!(service.config.port, 2);
        assert!(!reloaded.is_finished());
        let answer = next_watched(&mut events).await;
        assert_eq!(answer, WatchEvent::Reloaded(1));
        service.handle_and_drain(answer, &config_path).await;
        tokio::time::timeout(Duration::from_secs(10), reloaded)
            .await
            .unwrap()
            .unwrap();

        // A watcher that stops answers the reloads it was asked for.
        let (applied, done) = oneshot::channel();
        let request = ReloadRequest {
            force: true,
            applied,
            operation: None,
        };
        let watching = service.request_reload(request, &config_path).await;
        service.follow(watching, &mut events);
        service.follow(Watching::Stopped, &mut events);
        assert!(service.watcher.is_none() && events.is_none());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("management_reloads_are_answered_once_applied done acknowledges within 5s"),
            Ok(())
        );

        // Without a watcher the file is loaded here. Not upstream's: an
        // empty file is skipped, as the watcher skips it, forced or not, and
        // management keeps the config the service runs.
        std::fs::write(&config_path, text(3)).unwrap();
        let (applied, done) = oneshot::channel();
        let request = ReloadRequest {
            force: false,
            applied,
            operation: None,
        };
        let watching = service.request_reload(request, &config_path).await;
        service.follow(watching, &mut events);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("management_reloads_are_answered_once_applied done acknowledges within 5s"),
            Ok(())
        );
        assert_eq!(service.config.port, 3);
        let sha256 = service.management.config_sha256();
        std::fs::write(&config_path, "").unwrap();
        for force in [false, true] {
            let (applied, done) = oneshot::channel();
            let watching = service
                .request_reload(
                    ReloadRequest {
                        force,
                        applied,
                        operation: None,
                    },
                    &config_path,
                )
                .await;
            service.follow(watching, &mut events);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), done)
                    .await
                    .expect(
                        "management_reloads_are_answered_once_applied done acknowledges within 5s"
                    ),
                Ok(())
            );
            assert_eq!(service.config.port, 3);
            assert_eq!(service.management.config().port, 3);
            assert_eq!(service.management.config_sha256(), sha256);
        }

        // Once the loop stops, a waiting reload is answered and later ones
        // return at once.
        let (applied, done) = oneshot::channel();
        let request = ReloadRequest {
            force: false,
            applied,
            operation: None,
        };
        reload.requests.try_send(request).unwrap();
        service.close_reloads();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("management_reloads_are_answered_once_applied done acknowledges within 5s"),
            Ok(())
        );
        tokio::time::timeout(Duration::from_secs(10), reload.reload())
            .await
            .unwrap();
    }

    /// The watcher's next event, skipping the auth directory's scan.
    async fn next_watched(events: &mut Option<mpsc::Receiver<WatchEvent>>) -> WatchEvent {
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), next_event(events));
            match next
                .await
                .expect("no watch event")
                .expect("watcher stopped")
            {
                WatchEvent::AuthAdded(..) => {}
                event => return event,
            }
        }
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
        service.handle_and_drain(added(&path), Path::new("")).await;
        let id = service.manager.list()[0].id.clone();
        assert!(!model_ids(&service, &id).is_empty());

        // The watcher reads the enabled file; its report waits.
        let queued = changed(auth_file(&path));

        // The API disables the credential, saving its file, as the status
        // route does.
        let mut auth = Auth::clone(&service.manager.get(&id).unwrap());
        auth.disabled = true;
        auth.status = Status::Disabled;
        let disabled = service
            .manager
            .run_persistence(move |manager| manager.update(auth))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["disabled"], true);
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.upsert(Auth::clone(&disabled)).await
        });
        sent.await.unwrap();
        assert!(model_ids(&service, &id).is_empty());

        service.handle_and_drain(queued, Path::new("")).await;
        assert!(service.manager.get(&id).unwrap().disabled);
        assert!(model_ids(&service, &id).is_empty());

        // A report read after the save is applied.
        service
            .handle_and_drain(changed(auth_file(&path)), Path::new(""))
            .await;
        assert!(service.manager.get(&id).unwrap().disabled);
        assert_eq!(
            service.revisions[&id],
            service.revisions[&path.to_string_lossy().into_owned()],
            "id/path aliases share the watermark"
        );
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
        service.handle_and_drain(added(&path), Path::new("")).await;
        let id = service.manager.list()[0].id.clone();

        let queued = removed(&path);
        codex_file(dir.path(), "codex-a.json", r#","prefix":"team""#);
        let file = auth_file(&path);
        let sent = apply_next(&mut service, &sync, |sync| async move {
            sync.file_written(file).await
        });
        sent.await.unwrap();
        service.handle_and_drain(queued, Path::new("")).await;
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
            operation: None,
        };
        service
            .handle_and_drain(changed(auth_file(&path)), Path::new(""))
            .await;
        service.apply_sync(request);
        drain_reads(&mut service).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect(
                    "stale_removals_and_management_changes_are_skipped done acknowledges within 5s"
                ),
            Ok(())
        );
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

    #[tokio::test]
    async fn watcher_upsert_rebases_unwritten_rotation_from_old_disk() {
        use open_ferry_core::auth::AuthStore;
        struct FailingRefreshStore(Arc<open_ferry_core::auth::FileStore>);
        impl AuthStore for FailingRefreshStore {
            fn list(&self) -> std::io::Result<Vec<Auth>> {
                self.0.list()
            }
            fn save(&self, auth: &Auth) -> std::io::Result<String> {
                if matches!(
                    auth.codex_persist_intent,
                    open_ferry_core::auth::codex_refresh::CodexPersistIntent::Refresh(_)
                ) {
                    return Err(std::io::Error::other("synthetic refresh save failure"));
                }
                self.0.save(auth)
            }
            fn delete(&self, id: &str) -> std::io::Result<()> {
                self.0.delete(id)
            }
            fn read_back(&self, auth: &Auth) -> std::io::Result<Option<Auth>> {
                self.0.read_back(auth)
            }
            fn read_for_adoption(&self, auth: &Auth) -> std::io::Result<AdoptionRead> {
                self.0.read_for_adoption(auth)
            }
            fn binding_generation(&self) -> u64 {
                self.0.binding_generation()
            }

            fn supports_codex_refresh_patches(&self) -> bool {
                true
            }
            fn same_record(&self, a: &Auth, b: &Auth) -> std::io::Result<bool> {
                self.0.same_record(a, b)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = codex_file(
            dir.path(),
            "watcher-race.json",
            r#","refresh_token":"fixture-refresh""#,
        );
        let mut service = service(dir.path(), "");
        service.handle_and_drain(added(&path), Path::new("")).await;
        let old = service.manager.list()[0].as_ref().clone();
        let id = old.id.clone();
        let files = Arc::new(open_ferry_core::auth::FileStore::new(dir.path()));
        service.manager = Manager::new(
            Settings::from(&*service.config),
            service.registry.clone(),
            Some(Arc::new(FailingRefreshStore(files))),
        );
        service.manager.register_unsaved(old.clone()).unwrap();
        service.manager.register_executor(Arc::new(Refresher));
        service.manager.force_refresh(&id).await.unwrap();
        assert_eq!(
            service
                .manager
                .get(&id)
                .unwrap()
                .codex_pending_refresh
                .len(),
            1
        );
        // This file-synthesized, generation-zero event carries the old disk
        // tokens with a settings-only change. It must not act like a login.
        codex_file(
            dir.path(),
            "watcher-race.json",
            r#","refresh_token":"fixture-refresh","prefix":"team""#,
        );
        service
            .handle_and_drain(changed(auth_file(&path)), Path::new(""))
            .await;
        let live = service.manager.get(&id).unwrap();
        assert_eq!(live.metadata["access_token"], "refreshed");
        assert_eq!(live.prefix, "team");
        assert_eq!(live.codex_pending_refresh.len(), 1);
        let saving_id = id.clone();
        service
            .manager
            .run_persistence(move |manager| {
                manager.edit(&saving_id, |auth| {
                    auth.metadata
                        .insert("note".into(), serde_json::json!("retry"));
                    Ok::<_, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "refreshed");
        assert_eq!(saved["prefix"], "team");
        let mut new_login = service.manager.get(&id).unwrap().as_ref().clone();
        new_login.generation = 0;
        new_login.registration_epoch = 0;
        new_login
            .metadata
            .insert("access_token".into(), serde_json::json!("explicit-login"));
        new_login.metadata.insert(
            "refresh_token".into(),
            serde_json::json!("explicit-login-refresh"),
        );
        assert!(service.upsert(new_login, &service.rules()));
        assert_eq!(
            service.manager.get(&id).unwrap().metadata["access_token"],
            "explicit-login"
        );
        assert!(
            service
                .manager
                .get(&id)
                .unwrap()
                .codex_pending_refresh
                .is_empty()
        );
    }

    #[test]
    fn config_apply_keeps_refresh_published_after_its_previous_clone_point() {
        // Fixed code runs the hook before its locked edit; the old get/clone
        // negative control moves it after cloning and demonstrates the race.
        struct FailingStore;
        impl open_ferry_core::auth::AuthStore for FailingStore {
            fn list(&self) -> std::io::Result<Vec<Auth>> {
                Ok(Vec::new())
            }
            fn save(&self, _: &Auth) -> std::io::Result<String> {
                Err(std::io::Error::other("injected save failure"))
            }
            fn delete(&self, _: &str) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(
            dir.path(),
            "oauth-excluded-models:\n  codex: [excluded-model]\n",
        );
        service.manager = Manager::new(
            Settings::from(&*service.config),
            service.registry.clone(),
            Some(Arc::new(FailingStore)),
        );
        service.manager.register_executor(Arc::new(Refresher));
        let auth = Auth {
            id: "config-race.json".into(), provider: "codex".into(),
            metadata: serde_json::json!({"type":"codex", "access_token":"before-config", "refresh_token":"fixture-refresh"}).as_object().unwrap().clone(),
            ..Auth::default()
        };
        service.manager.register_unsaved(auth).unwrap();
        service.file_auths.insert(
            dir.path().join("config-race.json"),
            "config-race.json".into(),
        );
        let manager = service.manager.clone();
        service.file_config_hook = Some(Arc::new(move || {
            let manager = manager.clone();
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(manager.force_refresh("config-race.json"))
                    .unwrap();
            })
            .join()
            .unwrap();
        }));
        service.apply_config_to_file_auths();
        let live = service.manager.get("config-race.json").unwrap();
        assert_eq!(live.metadata["access_token"], "refreshed");
        assert_eq!(live.codex_pending_refresh.len(), 1);
        let mut expected = (*live).clone();
        assert!(
            !apply_config_attributes(&service.synthesis_context(), &mut expected),
            "config attributes were applied to the refreshed live entry"
        );
        assert!(live.attributes.contains_key("excluded_models"));
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
        service.handle_and_drain(added(&path), Path::new("")).await;
        let id = service.manager.list()[0].id.clone();

        // The API changes a field, saving the file; its sync waits.
        let mut auth = Auth::clone(&service.manager.get(&id).unwrap());
        auth.metadata.insert("note".into(), "changed".into());
        let changed = service
            .manager
            .run_persistence(move |manager| manager.update(auth))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let held = tokio::spawn({
            let sync = Arc::clone(&sync);
            async move { sync.upsert(Auth::clone(&changed)).await }
        });
        let request = tokio::time::timeout(Duration::from_secs(5), service.sync_requests.recv())
            .await
            .expect("request arrival 4469 within 5s")
            .unwrap();

        let refreshed = service.manager.force_refresh(&id).await.unwrap();
        assert_eq!(access_token(&service, &id), "refreshed");
        assert!(refreshed.generation > request_generation(&request));

        service.apply_sync(request);
        drain_reads(&mut service).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), held)
                .await
                .expect("held management sync acknowledges within 5s")
                .unwrap(),
            Ok(())
        );
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
    /// (sdk/cliproxy/service_auth.go), which upstream doesn't test.
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

    /// Not upstream's: Go's `filepath.Abs` cleans, but on Unix
    /// `std::path::absolute` alone keeps `..`.
    #[test]
    fn the_auth_dir_is_made_absolute_and_cleaned() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_dir(PathBuf::from("auths/../creds/.")),
            cwd.join("creds")
        );
        assert_eq!(absolute_dir(PathBuf::new()), PathBuf::new());
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
        service
            .handle_and_drain(WatchEvent::AuthAdded(file, revision), &config_path)
            .await;
        assert_eq!(service.manager.list().len(), 1);
        assert!(service.manager.get(&id).is_some());
        assert_eq!(service.file_auths.get(&path), Some(&id));

        service.handle_and_drain(removed(&path), &config_path).await;
        assert!(service.manager.list().is_empty());
        assert!(service.file_auths.is_empty());

        // A reload naming the same directory keeps the watcher.
        let same = Arc::new(Config::parse(std::fs::read_to_string(&config_path).unwrap()).unwrap());
        service.watcher = Some(watcher);
        assert!(matches!(
            service
                .handle_and_drain(config_changed(same), &config_path)
                .await,
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
            service
                .handle_and_drain(config_changed(Arc::new(same)), Path::new(""))
                .await,
            Watching::Same
        ));
        assert!(service.manager.get(&id).is_some());

        // A reload without the key removes it.
        let none = Config::parse(format!("auth-dir: '{}'\n", dir.path().display())).unwrap();
        service
            .handle_and_drain(config_changed(Arc::new(none)), Path::new(""))
            .await;
        assert!(service.manager.get(&id).is_none());
        assert!(service.registry.models_for_client(&id).is_empty());
        assert!(service.config_auths.is_empty());
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "a file was saved"
        );
    }

    // Ported from server_claude_tool_search_test.go and
    // codex_client_models_tool_search_test.go (v8.0.27, MIT).
    #[tokio::test]
    async fn claude_search_reload_changes_the_catalog_and_the_sent_tools() {
        use axum::body::Bytes;
        use open_ferry_core::exec::Format;
        use serde_json::{Value, json};
        use tower_service::Service as _;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, mut seen) = mpsc::unbounded_channel();
        let upstream = axum::Router::new().fallback(move |body: Bytes| {
            let sent = sent.clone();
            async move {
                sent.send(serde_json::from_slice::<Value>(&body).unwrap())
                    .unwrap();
                let events = [
                    json!({"type":"message_start","message":{"id":"msg","type":"message",
                        "model":"claude-sonnet-4-5","role":"assistant","content":[],
                        "usage":{"input_tokens":1,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{
                        "type":"tool_use","id":"search-call","name":"tool_search","input":{}}}),
                    json!({"type":"content_block_delta","index":0,"delta":{
                        "type":"input_json_delta","partial_json":"{\"query\":\"calendar\"}"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
                    json!({"type":"message_stop"}),
                ].map(|event| format!("data: {event}\n\n")).join("");
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], events)
            }
        });
        let serving = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let keys = format!(
            "proxy-url: direct\nclaude-api-key:\n  - api-key: test-search-key\n    base-url: http://{address}\n    native-tool-search: true\n    models:\n      - name: claude-sonnet-4-5\n        alias: search-model\n"
        );
        let mut service = service(dir.path(), &format!("{keys}claude:\n  tool-search: true\n"));
        service.sync_config_auths();
        let auth = service.manager.list().pop().unwrap();
        let mut previous = None;
        for enabled in [true, false, true] {
            if enabled != service.config.claude.tool_search {
                reload(
                    &mut service,
                    dir.path(),
                    &format!("{keys}claude:\n  tool-search: {enabled}\n"),
                )
                .await;
            }
            let mut app = open_ferry_server::router(service.state.clone());
            let answer = app
                .call(
                    axum::http::Request::builder()
                        .uri("/v1/models?client_version=0.153.4")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(answer.status(), axum::http::StatusCode::OK);
            let bytes = axum::body::to_bytes(answer.into_body(), 1 << 20)
                .await
                .unwrap();
            let catalog: Value = serde_json::from_slice(&bytes).unwrap();
            let model = catalog["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["slug"] == "search-model")
                .unwrap();
            assert_eq!(model["supports_search_tool"], enabled);
            let request = Request {
                model: "claude-sonnet-4-5".into(),
                payload: json!({"input":"hello", "tools":[
                    {"type":"tool_search","execution":"client"},
                    {"type":"function","name":"lookup","defer_loading":true,"parameters":{"type":"object"}}
                ]}).to_string().into(),
            };
            let mut options = Options::new(Format::OPENAI_RESPONSE);
            options.original_request = request.payload.clone();
            let current = service.manager.executor("claude").unwrap();
            let mut snapshots = vec![(current.clone(), enabled)];
            if let Some(previous) = previous.replace((current, enabled)) {
                snapshots.push(previous);
            }
            // An already selected executor retains both translations' value
            // after reload; a new manager selection gets the new snapshot.
            for (executor, expected) in snapshots {
                let response = executor
                    .execute(auth.clone(), request.clone(), options.clone())
                    .await
                    .unwrap();
                let response: Value = serde_json::from_slice(&response.payload).unwrap();
                assert_eq!(
                    response["output"][0]["type"],
                    if expected {
                        "tool_search_call"
                    } else {
                        "function_call"
                    }
                );
                let body = tokio::time::timeout(Duration::from_secs(5), seen.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|tool| tool["name"] == "tool_search"),
                    expected
                );
            }
        }
        serving.abort();
    }

    /// Reloads `service` with a config over `dir` holding `extra`.
    async fn reload(service: &mut Service, dir: &Path, extra: &str) -> Watching {
        let config = Config::parse(format!("auth-dir: '{}'\n{extra}", dir.display())).unwrap();
        service
            .handle_and_drain(config_changed(Arc::new(config)), Path::new(""))
            .await
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

        // Runtime state the file doesn't hold. Replace observes disk tokens;
        // install this live-only fixture through a live-locked edit instead.
        let retry = Utc::now() + chrono::Duration::hours(1);
        service
            .manager
            .edit_unsaved(&id, |auth| {
                auth.metadata
                    .insert("access_token".into(), "runtime".into());
                auth.status_message = "cooling".into();
                auth.next_retry_after = Some(retry);
                Ok::<_, ()>(true)
            })
            .unwrap();

        reload(&mut service, dir.path(), "").await;
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(auth.attribute("excluded_models"), Some(own.as_str()));
        let models = model_ids(&service, &id);
        assert_eq!(models.len(), all.len() - 1, "{models:?}");
        assert!(!models.contains(&own), "{models:?}");
        assert_eq!(access_token(&service, &id), "runtime");
        assert_eq!(auth.status_message, "cooling");
        assert_eq!(auth.next_retry_after, Some(retry));
        assert_eq!(service.file_auths.get(&path), Some(&id));

        reload(&mut service, dir.path(), excluded).await;
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
        reload(&mut service, dir.path(), other).await;
        let auth = service.manager.get(&id).unwrap();
        assert_eq!(auth.attribute("excluded_models"), None);
        assert_eq!(auth.attribute("excluded_models_hash"), None);
        assert!(model_ids(&service, &id).contains(&own));

        let another = "oauth-excluded-models:\n  claude:\n    - y\n";
        reload(&mut service, dir.path(), another).await;
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
        service
            .handle_and_drain(config_changed(Arc::new(none)), Path::new(""))
            .await;
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
    /// sdk/cliproxy/service_executor_registration_test.go (v8.0.27, MIT)
    /// for the executors ported: Codex, Meta, Claude, Gemini, Gemini
    /// Interactions, Vertex AI, xAI and the baseline OpenAI-compatible one,
    /// with open-ferry's claude-cli. The plugin executor and the other
    /// providers' aren't ported.
    #[tokio::test]
    async fn registers_the_available_executors() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path(), "");
        for provider in [
            "codex",
            "meta",
            "claude",
            "claude-cli",
            "gemini",
            "gemini-interactions",
            "vertex",
            "xai",
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

    // Not upstream's: C4-2. A refresh publishes while `upsert` is deciding
    // whether its credential is stale. With the check and the replacement
    // under one hold of the manager's lock, the refresh lands after them and
    // keeps its token; a check made before a separate replacement let the
    // older token overwrite it.
    #[tokio::test]
    async fn a_refresh_during_an_upsert_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = service(dir.path(), "");
        let token = |auth: &Auth| auth.metadata["access_token"].as_str().unwrap().to_owned();
        let base = Auth {
            id: "codex-race.json".into(),
            provider: "codex".into(),
            metadata: serde_json::json!({"type": "codex", "access_token": "t0"})
                .as_object()
                .unwrap()
                .clone()
                .into_iter()
                .collect(),
            ..Auth::default()
        };
        let rules = service.rules();
        assert!(service.upsert(base, &rules));
        let live = service.manager.get("codex-race.json").unwrap();

        // What a PATCH queued: the live credential with a note.
        let mut incoming = Auth::clone(&live);
        incoming
            .metadata
            .insert("note".into(), serde_json::json!("patched"));

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = Arc::new(std::sync::Mutex::new(None));
        {
            let manager = service.manager.clone();
            let done = Arc::clone(&done);
            let thread = Arc::clone(&thread);
            let armed = std::sync::atomic::AtomicBool::new(true);
            service.upsert_hook = Some(Arc::new(move || {
                if !armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let manager = manager.clone();
                let finished = Arc::clone(&done);
                let id = live.id.clone();
                let (started_tx, started_rx) = std::sync::mpsc::channel();
                *thread.lock().unwrap() = Some(std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    // Simulate live refresh publication, not Replace, which now
                    // observes disk credentials rather than captured tokens.
                    manager
                        .edit_unsaved(&id, |auth| {
                            auth.metadata
                                .insert("access_token".into(), serde_json::json!("t1"));
                            Ok::<_, ()>(true)
                        })
                        .unwrap();
                    finished.store(true, std::sync::atomic::Ordering::SeqCst);
                }));
                // The thread is certain to start, so wait for it without a
                // limit; then give its publish a bound. With the lock held it
                // can't finish, so the fixed code waits out the bound; code
                // that read before replacing lets it finish at once, so a
                // false pass needs the thread descheduled for all of it.
                started_rx.recv().unwrap();
                for _ in 0..200 {
                    if done.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }));
        }
        assert!(service.upsert(incoming, &rules));
        thread.lock().unwrap().take().unwrap().join().unwrap();

        let now = service.manager.get("codex-race.json").unwrap();
        assert_eq!(token(&now), "t1", "the refresh's token survives");
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
        assert!(matches!(
            reload(&mut service, dir.path(), &first).await,
            Watching::Same
        ));
        assert!(service.manager.get(&alpha_id).is_some());
        let same = service.manager.executor("openai-compatible-alpha").unwrap();
        assert!(Arc::ptr_eq(&same, &alpha_executor));

        // Changed models keep the credential, register the new models, and
        // make the executor again for the new config; a new entry gets a
        // credential and an executor.
        reload(
            &mut service,
            dir.path(),
            &compat(&[alpha("up-2", "a2"), beta.clone()]),
        )
        .await;
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
        reload(
            &mut service,
            dir.path(),
            &compat(std::slice::from_ref(&beta)),
        )
        .await;
        assert!(service.manager.get(&alpha_id).is_none());
        assert!(model_ids(&service, &alpha_id).is_empty());
        assert_eq!(executor_id(&service, "openai-compatible-alpha"), None);
        assert!(service.manager.get(&beta_auth.id).is_some());

        // So does a disabled one; the baseline executor stays.
        let disabled = beta.replace("  - name: beta\n", "  - name: beta\n    disabled: true\n");
        reload(&mut service, dir.path(), &compat(&[disabled])).await;
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
        reload(
            &mut service,
            dir.path(),
            &compat(std::slice::from_ref(&beta)),
        )
        .await;
        assert_eq!(service.config_auths.len(), 1);
        let entries = compat(&[alpha("up-1", "a1"), beta.clone()]);
        let text = format!("auth-dir: '{}'\n{entries}", dir.path().display());
        let mut invalid = Config::parse(text).unwrap();
        invalid.openai_compatibility[1].api_key_entries[0].weight = Some(MAX_WEIGHT + 1);
        service
            .handle_and_drain(config_changed(Arc::new(invalid)), Path::new(""))
            .await;
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
            service
                .handle_and_drain(
                    WatchEvent::ConfigChanged(reloaded, String::new()),
                    Path::new(""),
                )
                .await;
            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("a2")).await;
            assert_eq!((status, body.as_str()), (200, ANSWER));
            let (status, body) = send(addr, "POST", "/v1/chat/completions", &chat("a1")).await;
            assert_ne!(status, 200, "{body}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[1].2, chat("up-2"));
        }
    }

    /// Not upstream's: `xai.inject-x-search` reaches the xAI executor on a
    /// reload, whether it turns on or off, as upstream's every reload makes
    /// the executor again.
    mod xai_requests {
        use std::sync::{Arc, Mutex};

        use axum::body::Bytes;
        use serde_json::Value;
        use tokio::net::TcpListener;
        use tokio::sync::watch;

        use super::super::serve;
        use super::compat_requests::send;
        use super::{reload, service};

        /// The one event of xAI's answer.
        const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"model\":\"grok-4.5\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";

        /// The bodies xAI was sent.
        type Seen = Arc<Mutex<Vec<String>>>;

        /// xAI on a 127.0.0.1 ephemeral port, answering every request
        /// with [`COMPLETED`]; its base URL.
        async fn xai() -> (String, Seen) {
            let seen = Seen::default();
            let record = Arc::clone(&seen);
            let app = axum::Router::new().fallback(move |body: Bytes| {
                let record = Arc::clone(&record);
                async move {
                    let body = String::from_utf8_lossy(&body).into_owned();
                    record.lock().unwrap().push(body);
                    ([("content-type", "text/event-stream")], COMPLETED)
                }
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            (format!("http://{addr}/v1"), seen)
        }

        /// A config with an xAI key for `base_url`, injecting X search if
        /// `inject`.
        fn config(base_url: &str, inject: bool) -> String {
            format!(
                "api-keys: ['client-key']\nxai:\n  inject-x-search: {inject}\nxai-api-key:\n  - api-key: xai-reload-key\n    base-url: {base_url}\n    models:\n      - name: grok-4.5\n        alias: grok-reload\n"
            )
        }

        /// Whether `body` has Grok's X search tool.
        fn has_x_search(body: &str) -> bool {
            let body: Value = serde_json::from_str(body).unwrap();
            body["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "x_search"))
        }

        #[tokio::test]
        async fn inject_x_search_follows_reloads() {
            let dir = tempfile::tempdir().unwrap();
            let (base_url, seen) = xai().await;
            let mut service = service(dir.path(), &config(&base_url, false));
            service.sync_config_auths();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (_stop, stopped) = watch::channel(false);
            tokio::spawn(serve(listener, None, service.app(), stopped));

            let request = r#"{"model":"grok-reload","input":"hello"}"#;
            let (status, body) = send(addr, "POST", "/v1/responses", request).await;
            assert_eq!(status, 200, "{body}");
            reload(&mut service, dir.path(), &config(&base_url, true)).await;
            let (status, body) = send(addr, "POST", "/v1/responses", request).await;
            assert_eq!(status, 200, "{body}");
            reload(&mut service, dir.path(), &config(&base_url, false)).await;
            let (status, body) = send(addr, "POST", "/v1/responses", request).await;
            assert_eq!(status, 200, "{body}");

            let seen = seen.lock().unwrap();
            let injected: Vec<bool> = seen.iter().map(|body| has_x_search(body)).collect();
            assert_eq!(injected, [false, true, false], "{seen:?}");
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
    /// internal/api/server_test.go (v8.0.27, MIT):
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
        use std::process::ExitCode;
        use std::sync::Arc;
        use std::time::Duration;

        use axum::body::Bytes;
        use open_ferry_core::config::{Config, ConfigWatcher, ReadGuard, WatchEvent};
        use open_ferry_core::observe::usage;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::{mpsc, oneshot, watch};

        use open_ferry_core::gate::{ActivationKey, ActiveGate, GateState};

        use super::super::{Options, ReloadRequest, Service, run, serve, shut_down};
        use super::{next_watched, service};
        use crate::keep_alive::{self, KeepAlive};
        use crate::logging::LogLevel;

        const KEYED: &str = "remote-management:\n  secret-key: test-secret\n";
        const LIST: &str = "/v0/management/auth-files";

        /// What a save answers once the service loaded the file it found
        /// changed.
        const CHANGED: &str = r#"{"error":"config_changed","message":"the config file changed on disk since the server loaded it; it has been loaded again, so try again"}"#;

        /// What a save answers when the service couldn't load it.
        const CANT_LOAD: &str = r#"{"error":"config_changed","message":"the config file changed on disk and can't be loaded: fix it, then try again"}"#;

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
            send(addr, method, path, headers, "").await
        }

        /// Sends `method path` with `headers` and `body`, JSON when there
        /// is one, and reads the answer.
        async fn send(
            addr: SocketAddr,
            method: &str,
            path: &str,
            headers: &[(&str, &str)],
            body: &str,
        ) -> Answer {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
            for (name, value) in headers {
                let _ = write!(request, "{name}: {value}\r\n");
            }
            if !body.is_empty() {
                let _ = write!(
                    request,
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                );
            }
            request.push_str("Connection: close\r\n\r\n");
            request.push_str(body);
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
            WatchEvent::ConfigChanged(Arc::new(Config::parse(text).unwrap()), String::new())
        }

        // Not upstream's: the TUI's readiness check passes with the local
        // management password the service was given, and the keep-alive
        // endpoint is served with the other routes' CORS headers.
        #[tokio::test]
        async fn the_tui_signs_in_with_the_local_password() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = service(dir.path(), KEYED);
            service.management = service.management.clone().with_local_password("local-pw");
            let keep_alive = KeepAlive::new("local-pw", keep_alive::TIMEOUT);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (_stop, stopped) = watch::channel(false);
            let app = service.app_with(keep_alive.router());
            tokio::spawn(serve(listener, None, app, stopped));

            assert!(open_ferry_tui::wait_ready(addr.port(), "local-pw").await);
            let password = ("X-Local-Password", "local-pw");
            let answer = fetch(addr, "GET", "/keep-alive", &[password]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert_eq!(answer.body, r#"{"status":"ok"}"#);
            assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
        }

        // Not upstream's: the dashboard's app, its `/management.html` and
        // its API are served beside the management API, with the server's
        // CORS headers and the management key; the ledger is opened in the
        // log directory, and is read after shutdown.
        #[tokio::test]
        async fn the_dashboard_is_served_beside_management() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = service(dir.path(), KEYED);
            let log_dir = service.observability.log_dir.clone().unwrap();
            assert!(log_dir.starts_with(dir.path()), "{}", log_dir.display());
            service.start_ledger();
            let (addr, _stop) = start(&service).await;

            let answer = fetch(addr, "GET", "/dashboard/", &[]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert!(answer.header("content-security-policy").is_some());
            assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
            let path = "/management.html?safe-mode=configure";
            let answer = fetch(addr, "GET", path, &[]).await;
            assert_eq!(answer.status, 302);
            assert_eq!(
                answer.header("location"),
                Some("/dashboard/?safe-mode=configure")
            );

            let ledger = "/open-ferry/api/v1/usage/ledger";
            let answer = fetch(addr, "GET", ledger, &[]).await;
            assert_eq!(answer.status, 401, "{}", answer.body);
            assert!(answer.body.contains("missing_management_key"));
            let key = ("Authorization", "Bearer test-secret");
            let read = || async {
                let answer = fetch(addr, "GET", ledger, &[key]).await;
                assert_eq!(answer.status, 200, "{}", answer.body);
                serde_json::from_str::<serde_json::Value>(&answer.body).unwrap()
            };
            let state = read().await;
            assert_eq!(state["available"], true, "{state}");
            let file = log_dir.join(open_ferry_dashboard::LEDGER_FILE);
            assert_eq!(state["file"], file.display().to_string());
            assert!(file.is_file());

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (stop, stopped) = watch::channel(false);
            let server = tokio::spawn(serve(listener, None, service.app(), stopped));
            let _ = shut_down(&service, &stop, server).await;
            assert_eq!(read().await["available"], true);
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
            let answer = fetch(addr, "DELETE", unported, &[key]).await;
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
        /// a key set. A page for no pending login says so, as a failure.
        #[tokio::test]
        async fn oauth_callback_pages_are_served_beside_the_proxy() {
            let dir = tempfile::tempdir().unwrap();
            let service = service(
                dir.path(),
                "api-keys: ['client-key']
",
            );
            let (addr, _stop) = start(&service).await;
            let title = "<h1>No sign-in is waiting for this page</h1>";

            for path in ["/anthropic/callback", "/codex/callback"] {
                let query = format!("{path}?state=unknown-state&code=c");
                let answer = fetch(addr, "GET", &query, &[]).await;
                assert_eq!(answer.status, 400, "{path}: {}", answer.body);
                assert!(answer.body.contains(title), "{path}: {}", answer.body);
                assert_eq!(
                    answer.header("content-type"),
                    Some("text/html; charset=utf-8")
                );
                assert_eq!(answer.header("cache-control"), Some("no-store"));
                assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
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
            service
                .handle_and_drain(
                    config(dir.path(), &format!("{KEYED}{trusted}")),
                    Path::new(""),
                )
                .await;
            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 403);
            assert_eq!(answer.body, r#"{"error":"remote management disabled"}"#);

            let remote = format!("{KEYED}  allow-remote: true\n{trusted}");
            service
                .handle_and_drain(config(dir.path(), &remote), Path::new(""))
                .await;
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);

            // The trusted proxies are read once, at start, as upstream
            // reads them.
            service
                .handle_and_drain(config(dir.path(), KEYED), Path::new(""))
                .await;
            let answer = fetch(addr, "GET", LIST, &[key, forwarded]).await;
            assert_eq!(answer.status, 403, "{}", answer.body);

            service
                .handle_and_drain(config(dir.path(), ""), Path::new(""))
                .await;
            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!((answer.status, answer.body.as_str()), (404, ""));
        }

        /// Waits until `addr` takes connections.
        async fn wait_listening(addr: SocketAddr) {
            for _ in 0..500 {
                if TcpStream::connect(addr).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("nothing listens on {addr}");
        }

        /// What `/v1/models` answers client key `key`.
        async fn models(addr: SocketAddr, key: &str) -> u16 {
            let header = format!("Bearer {key}");
            fetch(addr, "GET", "/v1/models", &[("Authorization", &header)])
                .await
                .status
        }

        /// The next reload the management API asks the service's loop for.
        async fn next_reload(service: &mut Service) -> ReloadRequest {
            tokio::time::timeout(Duration::from_secs(10), service.reload_requests.recv())
                .await
                .expect("no reload asked for")
                .expect("the reloads closed")
        }

        /// Makes `request` as the service's loop does, applying what the
        /// watcher reports until the reload is answered.
        async fn apply_reload(
            service: &mut Service,
            events: &mut Option<mpsc::Receiver<WatchEvent>>,
            path: &Path,
            request: ReloadRequest,
        ) {
            let watching = service.request_reload(request, path).await;
            service.follow(watching, events);
            let ticket = service.last_reload;
            loop {
                let event = next_watched(events).await;
                let answered = event == WatchEvent::Reloaded(ticket);
                let watching = service.handle_and_drain(event, path).await;
                service.follow(watching, events);
                if answered {
                    return;
                }
            }
        }

        /// Waits until `/v1/models` answers `status` to client key `key`.
        async fn wait_models(addr: SocketAddr, key: &str, status: u16) {
            let header = format!("Bearer {key}");
            for _ in 0..500 {
                let answer = fetch(addr, "GET", "/v1/models", &[("Authorization", &header)]).await;
                if answer.status == status {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("/v1/models never answered {status} to {key}");
        }

        // Not upstream's: the service `run` serves gives the management API
        // the config writer and the reload. A change is saved to the file,
        // keeping its comments, and applied before the answer is sent, so
        // the proxy takes a client key the change added at once; changes
        // made to the file by hand are still followed after it, and a save
        // never writes over one the service hasn't loaded yet.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn management_saves_are_applied_before_the_answer() {
            let dir = tempfile::tempdir().unwrap();
            let auth_dir = dir.path().join("auth");
            std::fs::create_dir(&auth_dir).unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let path = dir.path().join("config.yaml");
            let head = format!(
                "# Kept.\nhost: 127.0.0.1\nport: {port}\nauth-dir: '{}'\n{KEYED}",
                auth_dir.display()
            );
            std::fs::write(&path, format!("{head}api-keys: ['client-key']\n")).unwrap();
            let (config, config_sha256) = Config::load_with_sha256(&path).unwrap();
            let (stop, stopped) = oneshot::channel::<()>();
            let served = tokio::spawn(run(
                config,
                path.clone(),
                auth_dir,
                LogLevel::detached(),
                Options {
                    config_sha256: Some(config_sha256),
                    ..Options::default()
                },
                async move {
                    let _ = stopped.await;
                },
            ));
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            wait_listening(addr).await;
            wait_models(addr, "client-key", 200).await;

            let key = ("Authorization", "Bearer test-secret");
            let keys = r#"["client-key","added-key"]"#;
            let answer = send(addr, "PUT", "/v0/management/api-keys", &[key], keys).await;
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (200, r#"{"status":"ok"}"#)
            );
            let added = ("Authorization", "Bearer added-key");
            let answer = fetch(addr, "GET", "/v1/models", &[added]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            let saved = std::fs::read_to_string(&path).unwrap();
            assert!(saved.starts_with("# Kept.\n"), "{saved}");
            let saved = Config::load(&path).unwrap();
            assert_eq!(saved.api_keys, ["client-key", "added-key"]);

            // A failed save changes nothing, in the file or in the service.
            // The backup the first save left is in the way of the next.
            let backup = dir.path().join("config.yaml.bak");
            assert!(backup.is_file());
            std::fs::remove_file(&backup).unwrap();
            std::fs::create_dir(&backup).unwrap();
            let answer = send(addr, "PUT", "/v0/management/api-keys", &[key], "[]").await;
            assert_eq!(answer.status, 500, "{}", answer.body);
            assert!(
                answer
                    .body
                    .starts_with(r#"{"error":"failed to save config: "#),
                "{}",
                answer.body
            );
            assert_eq!(Config::load(&path).unwrap().api_keys, saved.api_keys);
            let answer = fetch(addr, "GET", "/v1/models", &[added]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            std::fs::remove_dir(&backup).unwrap();

            std::fs::write(&path, format!("{head}api-keys: ['hand-key']\n")).unwrap();
            wait_models(addr, "hand-key", 200).await;
            wait_models(addr, "added-key", 401).await;

            // Made before the service loads a change made in the file, a
            // save answers 409 and writes nothing, and the service loads the
            // file; made again, it keeps the change. The watcher may load
            // the file first, and then the save goes ahead at once.
            let changed = format!("{head}api-keys: ['hand-key', 'new-key']\n");
            open_ferry_core::config::save::write_file(&path, changed.as_bytes()).unwrap();
            let debug = r#"{"value":true}"#;
            let mut runs = 0;
            let answer = loop {
                runs += 1;
                let answer = send(addr, "PUT", "/v0/management/debug", &[key], debug).await;
                if answer.status != 409 || runs == 5 {
                    break answer;
                }
                assert!(
                    answer.body.starts_with(r#"{"error":"config_changed","#),
                    "{}",
                    answer.body
                );
            };
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (200, r#"{"status":"ok"}"#)
            );
            let saved = Config::load(&path).unwrap();
            assert_eq!(saved.api_keys, ["hand-key", "new-key"]);
            assert!(saved.debug);
            wait_models(addr, "new-key", 200).await;

            stop.send(()).unwrap();
            let code = tokio::time::timeout(Duration::from_secs(60), served)
                .await
                .expect("the service didn't stop")
                .unwrap();
            assert_eq!(code, ExitCode::SUCCESS);
        }

        // Not upstream's: when the file was emptied, or doesn't load, the
        // service keeps its config, and so does management: a save answers
        // 409 saying the file needs fixing, every time, and writes nothing,
        // and management still lists the client key the proxy takes. Once
        // the file is fixed the service loads it, and the save goes ahead.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_save_over_a_file_that_cant_load_changes_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let auth_dir = dir.path().join("auth");
            std::fs::create_dir(&auth_dir).unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let path = dir.path().join("config.yaml");
            let head = format!(
                "host: 127.0.0.1\nport: {port}\nauth-dir: '{}'\n{KEYED}",
                auth_dir.display()
            );
            std::fs::write(&path, format!("{head}api-keys: ['client-key']\n")).unwrap();
            let (config, config_sha256) = Config::load_with_sha256(&path).unwrap();
            let (stop, stopped) = oneshot::channel::<()>();
            let served = tokio::spawn(run(
                config,
                path.clone(),
                auth_dir,
                LogLevel::detached(),
                Options {
                    config_sha256: Some(config_sha256),
                    ..Options::default()
                },
                async move {
                    let _ = stopped.await;
                },
            ));
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            wait_listening(addr).await;
            wait_models(addr, "client-key", 200).await;

            let key = ("Authorization", "Bearer test-secret");
            let debug = r#"{"value":true}"#;
            for broken in ["", "port: [\n"] {
                std::fs::write(&path, broken).unwrap();
                for _ in 0..2 {
                    let answer = send(addr, "PUT", "/v0/management/debug", &[key], debug).await;
                    assert_eq!((answer.status, answer.body.as_str()), (409, CANT_LOAD));
                }
                assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
                let answer = fetch(addr, "GET", "/v0/management/api-keys", &[key]).await;
                assert_eq!(
                    (answer.status, answer.body.as_str()),
                    (200, r#"{"api-keys":["client-key"]}"#)
                );
                assert_eq!(models(addr, "client-key").await, 200);
            }

            // Fixed. The watcher may load it before the save does.
            std::fs::write(&path, format!("{head}api-keys: ['fixed-key']\n")).unwrap();
            let mut runs = 0;
            let answer = loop {
                runs += 1;
                let answer = send(addr, "PUT", "/v0/management/debug", &[key], debug).await;
                if answer.status != 409 || runs == 3 {
                    break answer;
                }
                assert_eq!(answer.body, CHANGED);
            };
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (200, r#"{"status":"ok"}"#)
            );
            let saved = Config::load(&path).unwrap();
            assert_eq!(saved.api_keys, ["fixed-key"]);
            assert!(saved.debug);
            let answer = fetch(addr, "GET", "/v0/management/api-keys", &[key]).await;
            assert_eq!(answer.body, r#"{"api-keys":["fixed-key"]}"#);
            assert_eq!(models(addr, "fixed-key").await, 200);
            assert_eq!(models(addr, "client-key").await, 401);

            stop.send(()).unwrap();
            let code = tokio::time::timeout(Duration::from_secs(60), served)
                .await
                .expect("the service didn't stop")
                .unwrap();
            assert_eq!(code, ExitCode::SUCCESS);
        }

        // Not upstream's: when the file holds again what the watcher loaded
        // before the management API's last write (X, then Y saved, then X
        // again before the service reloads), the reload after the write
        // skips it, as upstream's does, and only management has Y. The next
        // save answers 409 once the service has loaded X again, forced, so
        // management and the proxy both run X, and made again it goes ahead.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_conflicting_save_reloads_what_the_watcher_saw_before() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.yaml");
            let x = format!(
                "auth-dir: '{}'\n{KEYED}api-keys: ['x-key']\n",
                dir.path().display()
            );
            std::fs::write(&path, &x).unwrap();
            let (config, sha256) = Config::load_with_sha256(&path).unwrap();
            let mut service = Service::new(
                Arc::new(config),
                path.clone(),
                dir.path().to_owned(),
                LogLevel::detached(),
            );
            service.register_executors();
            service.management = service.management.clone().with_config_sha256(&sha256);
            // The watcher's reads of the file wait while `held` is set, so its
            // debounced reload can't load Y in the moment before the file holds
            // X again, as it could on a busy runner.
            let held = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let guard: ReadGuard = {
                let held = Arc::clone(&held);
                Arc::new(move |_: &Path| -> Result<(), String> {
                    let (on, changed) = &*held;
                    let on = on.lock().unwrap();
                    let _on = changed
                        .wait_timeout_while(on, Duration::from_secs(10), |on| *on)
                        .unwrap();
                    Ok(())
                })
            };
            let hold = |value: bool| {
                let (on, changed) = &*held;
                *on.lock().unwrap() = value;
                changed.notify_all();
            };
            let (watcher, events) =
                ConfigWatcher::start_guarded(&path, &service.config, Some(guard)).unwrap();
            service.watcher = Some(watcher);
            let mut events = Some(events);
            let (addr, _stop) = start(&service).await;
            let key = ("Authorization", "Bearer test-secret");
            let x_sha256 = Some(sha256);

            // The watcher loads X.
            let (applied, done) = oneshot::channel();
            let request = ReloadRequest {
                force: false,
                applied,
                operation: None,
            };
            apply_reload(&mut service, &mut events, &path, request).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), done)
                    .await
                    .expect("openai_compat_providers_follow_reloads done acknowledges within 5s"),
                Ok(())
            );
            assert_eq!(service.management.config_sha256(), x_sha256);

            // The API saves Y, and the file holds X again before the service
            // reloads. The watcher reads nothing until it does.
            hold(true);
            let saving = tokio::spawn(async move {
                let keys = r#"["y-key"]"#;
                send(addr, "PUT", "/v0/management/api-keys", &[key], keys).await
            });
            let request = next_reload(&mut service).await;
            assert!(!request.force);
            open_ferry_core::config::save::write_file(&path, x.as_bytes()).unwrap();
            hold(false);
            apply_reload(&mut service, &mut events, &path, request).await;
            let answer = tokio::time::timeout(Duration::from_secs(5), saving)
                .await
                .expect("management save task answers within 5s (case 1)")
                .unwrap();
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (200, r#"{"status":"ok"}"#)
            );
            assert_ne!(service.management.config_sha256(), x_sha256);
            assert_eq!(service.config.api_keys, ["x-key"]);
            assert_eq!(service.management.config().api_keys, ["y-key"]);

            let debug = r#"{"value":true}"#;
            let saving = tokio::spawn(async move {
                send(addr, "PUT", "/v0/management/debug", &[key], debug).await
            });
            let request = next_reload(&mut service).await;
            assert!(request.force);
            apply_reload(&mut service, &mut events, &path, request).await;
            let answer = tokio::time::timeout(Duration::from_secs(5), saving)
                .await
                .expect("management save task answers within 5s (case 2)")
                .unwrap();
            assert_eq!((answer.status, answer.body.as_str()), (409, CHANGED));
            assert_eq!(service.config.api_keys, ["x-key"]);
            assert_eq!(service.management.config_sha256(), x_sha256);
            let answer = fetch(addr, "GET", "/v0/management/api-keys", &[key]).await;
            assert_eq!(answer.body, r#"{"api-keys":["x-key"]}"#);
            assert_eq!(models(addr, "x-key").await, 200);
            assert_eq!(models(addr, "y-key").await, 401);

            // Made again, the save goes ahead.
            let saving = tokio::spawn(async move {
                send(addr, "PUT", "/v0/management/debug", &[key], debug).await
            });
            let request = next_reload(&mut service).await;
            assert!(!request.force);
            apply_reload(&mut service, &mut events, &path, request).await;
            let answer = tokio::time::timeout(Duration::from_secs(5), saving)
                .await
                .expect("management save task answers within 5s (case 3)")
                .unwrap();
            assert_eq!(
                (answer.status, answer.body.as_str()),
                (200, r#"{"status":"ok"}"#)
            );
            let saved = Config::load(&path).unwrap();
            assert_eq!(saved.api_keys, ["x-key"]);
            assert!(saved.debug && service.config.debug);
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
            service
                .handle_and_drain(config(dir.path(), &real), Path::new(""))
                .await;
            let client = ("Authorization", "Bearer real-key");
            let answer = fetch(addr, "GET", "/v1/models", &[client]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
        }

        /// A service on a closed gate over `dir`, and the key to activate
        /// it, as a supervised run builds it.
        fn standby_service(dir: &Path) -> (Service, ActivationKey) {
            let config = Config::parse(format!("auth-dir: '{}'\n", dir.display())).unwrap();
            let (gate, key) = ActiveGate::standby();
            let mut service = Service::with_gate(
                Arc::new(config),
                dir.join("config.yaml"),
                dir.to_owned(),
                LogLevel::detached(),
                gate,
            );
            service.register_executors();
            (service, key)
        }

        // Not upstream's (review 3 T2, part 1): a service is built closed,
        // and a closed store refuses to rebind the auth directory; the first
        // activation binds it under its `Activation` all the same, and
        // opens the gate.
        #[tokio::test]
        async fn the_first_activation_binds_the_auth_dir_a_closed_store_refuses() {
            let dir = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let (mut service, key) = standby_service(dir.path());
            assert_eq!(service.gate.state(), GateState::Closed);
            assert!(service.store.try_set_base_dir(other.path()).is_err());
            assert_eq!(service.store.base_dir(), service.auth_dir);

            // The directory changed while the service loaded.
            service.auth_dir = other.path().to_owned();
            service.first_activation(&key).unwrap();
            assert_eq!(service.gate.state(), GateState::Closed);
            service.gate.reopen();
            assert_eq!(service.gate.state(), GateState::Open);
            assert_eq!(service.store.base_dir(), service.auth_dir);
            assert_ne!(service.store.base_dir(), dir.path());
        }

        // Not upstream's: the first activation reads the credentials before
        // it opens the gate, a key of another gate starts nothing, and
        // an open gate can't be activated again.
        #[tokio::test]
        async fn the_first_activation_reads_the_credentials_then_opens_once() {
            let dir = tempfile::tempdir().unwrap();
            super::codex_file(dir.path(), "codex.json", "");
            let (mut service, key) = standby_service(dir.path());
            assert!(service.manager.list().is_empty());

            let (_, wrong) = ActiveGate::standby();
            assert!(service.first_activation(&wrong).is_err());
            assert_eq!(service.gate.state(), GateState::Closed);
            assert!(service.manager.list().is_empty());

            service.first_activation(&key).unwrap();
            assert_eq!(service.gate.state(), GateState::Closed);
            service.gate.reopen();
            assert_eq!(service.manager.list().len(), 1);
            assert_eq!(service.gate.state(), GateState::Open);
            assert!(service.first_activation(&key).is_err());
            assert_eq!(service.manager.list().len(), 1);
        }

        // Not upstream's: a run with no supervisor goes through the first
        // activation at start, and serves the credentials of its configured
        // auth directory with the gate open for good.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_unsupervised_run_serves_the_credentials_of_its_auth_dir() {
            let dir = tempfile::tempdir().unwrap();
            let auth_dir = dir.path().join("auth");
            std::fs::create_dir(&auth_dir).unwrap();
            super::codex_file(&auth_dir, "codex.json", "");
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let path = dir.path().join("config.yaml");
            std::fs::write(
                &path,
                format!(
                    "host: 127.0.0.1\nport: {port}\nauth-dir: '{}'\n{KEYED}",
                    auth_dir.display()
                ),
            )
            .unwrap();
            let config = Config::load(&path).unwrap();
            let (stop, stopped) = oneshot::channel::<()>();
            let served = tokio::spawn(run(
                config,
                path,
                auth_dir,
                LogLevel::detached(),
                Options::default(),
                async move {
                    let _ = stopped.await;
                },
            ));
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            wait_listening(addr).await;
            let key = ("Authorization", "Bearer test-secret");
            let answer = fetch(addr, "GET", LIST, &[key]).await;
            assert_eq!(answer.status, 200, "{}", answer.body);
            assert!(answer.body.contains("codex.json"), "{}", answer.body);
            let _ = stop.send(());
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), served)
                    .await
                    .expect("service task exits within 5s")
                    .unwrap(),
                ExitCode::SUCCESS
            );
        }
    }
}
