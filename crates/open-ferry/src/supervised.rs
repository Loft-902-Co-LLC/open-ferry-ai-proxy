// Not upstream's.

//! The blue-green self-update's two entries of the binary.
//!
//! `-supervise` runs the supervisor (see [`open_ferry_supervisor`]): it binds
//! the listening sockets, starts workers that serve them, and switches the
//! workers on an update. [`SupervisedHost`] answers what the supervisor
//! asks of the binary: the listeners (the same hosts, ports and TLS as a
//! plain run's), whether switching is off, and the binary's own version and
//! hash. The supervisor's logs go to standard error only; `main.log` is the
//! workers'.
//!
//! `-system` is the system service's supervisor, running as root or SYSTEM.
//! A hand-run privileged `-supervise` without it is a user supervisor of
//! the root or administrator account, held to the system trust rules.
//!
//! `-worker` runs one worker (see [`worker_entry`]): it takes over the
//! control channel and the sockets its supervisor passed, and serves them.
//!
//! The switch mode is read again from the config file and the environment
//! on every [`Host::effective_mode`], so an edit takes effect without a
//! restart. The topology (hosts, ports and TLS) is read once at start: the
//! sockets are bound once, and a worker whose config says otherwise warns.

use std::ffi::OsStr;
use std::io;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_ferry_core::config::{Config, SelfUpdateMode};
use open_ferry_supervisor::core::{Command, Exit};
use open_ferry_supervisor::driver::ProcessDriver;
use open_ferry_supervisor::endpoint::{Location, ServiceKind};
use open_ferry_supervisor::host::{Host, ModeSnapshot};
use open_ferry_supervisor::protocol::{ListenerTopology, REQUIRED_CAPS, VERSION};
use open_ferry_supervisor::startup::{self, StartupError};
use open_ferry_supervisor::timings::{DETECT_TIMEOUT, Timings};
use open_ferry_supervisor::worker::{AttachError, Hello};
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::receipt::Receipt;
use open_ferry_update::runner::ProcessRunner;
use open_ferry_update::settings::Settings;
use tokio::sync::mpsc;

use crate::flags::Flags;
use crate::{logging, scope, service};

/// A listener the supervisor binds: its name (`main` or `management`), its
/// host as the config writes it (empty for every interface), its port, and
/// whether it serves TLS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Wanted {
    pub name: &'static str,
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// The listeners `config` asks for, `main` first, as `service::run` and
/// its management listener bind them: the proxy's host and port, and
/// `management.separate-address` when it is set. TLS is `tls.enable` for
/// both.
pub(crate) fn wanted(config: &Config) -> Result<Vec<Wanted>, String> {
    let port = u16::try_from(config.port).map_err(|_| format!("invalid port {}", config.port))?;
    let tls = config.tls.enable;
    let mut listeners = vec![Wanted {
        name: "main",
        host: config.host.trim().to_owned(),
        port,
        tls,
    }];
    let separate = config
        .remote_management
        .separate_address()
        .map_err(|error| format!("management.separate-address: {error}"))?;
    if let Some(address) = separate {
        listeners.push(Wanted {
            name: "management",
            host: address.host,
            port: address.port,
            tls,
        });
    }
    Ok(listeners)
}

/// Brackets a bare IPv6 address, as binding a `host:port` text needs.
fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// The address a listener is expected on before it is bound: every
/// interface (IPv6, with IPv4 mapped) for an empty host, else the first
/// address the host resolves to. After binding, the socket's own address is
/// what the topology says.
fn expected_address(wanted: &Wanted) -> SocketAddr {
    let any = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, wanted.port));
    if wanted.host.is_empty() {
        return any;
    }
    format!("{}:{}", bracketed(&wanted.host), wanted.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
        .unwrap_or(any)
}

/// Listens on `host`:`port`, every interface when `host` is empty. On
/// Windows no other socket can bind the port (`SO_EXCLUSIVEADDRUSE`).
#[cfg(windows)]
fn bind_host(host: &str, port: u16) -> io::Result<TcpListener> {
    open_ferry_supervisor::sockets::windows::bind_host(host, port)
}

/// Listens on `host`:`port`, every interface when `host` is empty: IPv6
/// with IPv4 mapped where the system has IPv6, and IPv4 otherwise.
#[cfg(not(windows))]
fn bind_host(host: &str, port: u16) -> io::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};
    let host = host.trim();
    if !host.is_empty() {
        return TcpListener::bind(format!("{}:{port}", bracketed(host)));
    }
    let dual = || -> io::Result<Socket> {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
        socket.set_only_v6(false)?;
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
        Ok(socket)
    };
    let socket = match dual() {
        Ok(socket) => socket,
        Err(_) => {
            let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
            socket.set_reuse_address(true)?;
            socket.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
            socket
        }
    };
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Reads the environment's switch-mode override.
fn environment_mode() -> Option<String> {
    std::env::var(open_ferry_update::MODE_ENV).ok()
}

/// What `effective_mode` last reported and last complained of.
struct ModeState {
    off: bool,
    mode_gen: u64,
    /// The last read failure that was logged, so one that goes on is
    /// logged once.
    complained: Option<String>,
}

/// What the supervisor asks of the binary (see [`Host`]).
pub(crate) struct SupervisedHost {
    config_path: PathBuf,
    wanted: Vec<Wanted>,
    /// The sockets' own addresses, once bound.
    bound: Mutex<Option<Vec<ListenerTopology>>>,
    mode: Mutex<ModeState>,
    env: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// The hash of the image this supervisor was loaded from, taken in
    /// [`SupervisedHost::new`]: `None` when the binary couldn't be read.
    sha256: Option<String>,
    /// Called with how long the update lock has been waited for, for the
    /// Windows service to tell the service manager it is still starting.
    waiting: Option<Box<dyn Fn(Duration) + Send + Sync>>,
}

impl SupervisedHost {
    /// A host for the config at `config_path` (absolute), whose listeners
    /// are `wanted` and whose own binary is `exe`.
    ///
    /// The own hash is taken here, from the file as it is now, not from the
    /// path when it is first asked for: a supervisor waits for the update lock
    /// before anything else, and an install-self or update that replaces the
    /// binary meanwhile must not lend it the new file's hash beside its own
    /// (old) version. It then sees that the installed file isn't its own bytes
    /// and runs degraded until restarted. What remains is the time from the
    /// exec to here, milliseconds. A whole read rather than a kept handle: the
    /// hash is wanted at the first recovery anyway, and no handle is left open
    /// on the running image for the updater to move aside.
    pub(crate) fn new(config_path: PathBuf, exe: PathBuf, wanted: Vec<Wanted>) -> Self {
        let sha256 = std::fs::read(&exe)
            .ok()
            .map(|data| open_ferry_update::release::sha256_hex(&data));
        Self {
            config_path,
            wanted,
            bound: Mutex::new(None),
            // Until a read works, nothing may be switched.
            mode: Mutex::new(ModeState {
                off: true,
                mode_gen: 0,
                complained: None,
            }),
            env: Box::new(environment_mode),
            sha256,
            waiting: None,
        }
    }

    /// Reads the environment's override through `env`, for tests.
    #[cfg(test)]
    pub(crate) fn with_env(
        mut self,
        env: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.env = Box::new(env);
        self
    }

    /// Calls `hook` on each wait for the update lock.
    #[allow(dead_code, reason = "the Windows service sets it")]
    pub(crate) fn with_waiting_hook(
        mut self,
        hook: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        self.waiting = Some(Box::new(hook));
        self
    }
}

impl Host for SupervisedHost {
    fn topology(&self) -> Vec<ListenerTopology> {
        if let Some(bound) = self
            .bound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return bound.clone();
        }
        self.wanted
            .iter()
            .map(|wanted| ListenerTopology {
                name: wanted.name.to_owned(),
                address: expected_address(wanted),
                tls: wanted.tls,
            })
            .collect()
    }

    fn bind(&self) -> io::Result<Vec<(String, TcpListener)>> {
        let mut listeners = Vec::new();
        let mut topology = Vec::new();
        // In the order a plain run binds them; the first failure drops
        // what was bound.
        for wanted in &self.wanted {
            let listener = bind_host(&wanted.host, wanted.port).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to bind the {} listener: {error}", wanted.name),
                )
            })?;
            topology.push(ListenerTopology {
                name: wanted.name.to_owned(),
                address: listener.local_addr()?,
                tls: wanted.tls,
            });
            listeners.push((wanted.name.to_owned(), listener));
        }
        *self.bound.lock().unwrap_or_else(|e| e.into_inner()) = Some(topology);
        Ok(listeners)
    }

    fn effective_mode(&self) -> ModeSnapshot {
        let read = crate::settings::load(&crate::scope::Host, &self.config_path)
            .map(|config| Settings::resolve(&config.self_update, (self.env)().as_deref()).mode)
            .map(|mode| mode == SelfUpdateMode::Off);
        let mut state = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        match read {
            Ok(off) => {
                state.complained = None;
                if off != state.off {
                    state.off = off;
                    state.mode_gen += 1;
                }
            }
            Err(error) => {
                let message = error.to_string();
                if state.complained.as_deref() != Some(message.as_str()) {
                    tracing::warn!(
                        "can't read the self-update mode from the config; keeping the last one ({}): {message}",
                        if state.off { "off" } else { "on" }
                    );
                    state.complained = Some(message);
                }
            }
        }
        ModeSnapshot {
            off: state.off,
            mode_gen: state.mode_gen,
        }
    }

    fn own_version(&self) -> String {
        open_ferry_update::CURRENT_VERSION.to_owned()
    }

    fn own_sha256(&self) -> Option<String> {
        self.sha256.clone()
    }

    fn waiting_for_update_lock(&self, waited: Duration) {
        tracing::warn!(
            "waiting for the update lock for {} seconds; an update or rollback holds it",
            waited.as_secs()
        );
        if let Some(hook) = &self.waiting {
            hook(waited);
        }
    }
}

/// The process's exit status for how the supervisor ended: 0 for a stop,
/// 1 for too many panics or a startup that failed.
pub(crate) fn exit_status(result: &Result<Exit, StartupError>) -> u8 {
    match result {
        Ok(Exit::Stopped) => 0,
        Ok(Exit::TooManyPanics) | Err(_) => 1,
    }
}

/// Sends one `Command::Stop` for the first of `signals` and logs the ones
/// after it, which don't skip the workers' stop. Returns when `signals`
/// closes.
pub(crate) async fn forward_stops(
    signals: mpsc::UnboundedReceiver<&'static str>,
    commands: mpsc::UnboundedSender<Command>,
) {
    forward_stops_logging(signals, commands, log_signal).await;
}

/// The line for a signal: `again` for one after the first.
fn log_signal(name: &'static str, again: bool) {
    if again {
        tracing::warn!("{name} again: already stopping; waiting for the workers to stop");
    } else {
        tracing::info!("{name}: stopping");
    }
}

/// `forward_stops` with the logging as a parameter, so a test can observe
/// when it is called without going through tracing's global state.
async fn forward_stops_logging(
    mut signals: mpsc::UnboundedReceiver<&'static str>,
    commands: mpsc::UnboundedSender<Command>,
    log: impl Fn(&'static str, bool),
) {
    let mut stopping = false;
    while let Some(name) = signals.recv().await {
        if stopping {
            log(name, true);
        } else {
            stopping = true;
            // The stop goes first: logging must not delay it.
            let _ = commands.send(Command::Stop);
            log(name, false);
        }
    }
}

/// Watches for the signals that stop a service and names each one on the
/// returned channel. The handlers are in place when this returns, so a
/// signal after it is never the default's.
#[cfg(unix)]
fn watch_signals() -> io::Result<mpsc::UnboundedReceiver<&'static str>> {
    use tokio::signal::unix::{SignalKind, signal};
    let (tx, rx) = mpsc::unbounded_channel();
    for (kind, name) in [
        (SignalKind::interrupt(), "SIGINT"),
        (SignalKind::terminate(), "SIGTERM"),
    ] {
        let mut stream = signal(kind)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                if tx.send(name).is_err() {
                    break;
                }
            }
        });
    }
    Ok(rx)
}

/// As the Unix one, for the console's control events.
#[cfg(windows)]
fn watch_signals() -> io::Result<mpsc::UnboundedReceiver<&'static str>> {
    use tokio::signal::windows;
    let (tx, rx) = mpsc::unbounded_channel();
    macro_rules! watch {
        ($make:expr, $name:expr) => {{
            let mut stream = $make?;
            let tx = tx.clone();
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    if tx.send($name).is_err() {
                        break;
                    }
                }
            });
        }};
    }
    watch!(windows::ctrl_c(), "Ctrl-C");
    watch!(windows::ctrl_break(), "Ctrl-Break");
    watch!(windows::ctrl_close(), "console close");
    watch!(windows::ctrl_shutdown(), "system shutdown");
    watch!(windows::ctrl_logoff(), "logoff");
    Ok(rx)
}

/// The path with Windows' `\\?\` prefix dropped when the plain path means
/// the same.
fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// `path` made absolute against the working directory.
fn absolute(path: &Path) -> io::Result<PathBuf> {
    std::path::absolute(path)
}

/// Where the supervisor keeps its state, how it is installed and which
/// endpoint it serves.
pub(crate) struct Setup {
    pub config: PathBuf,
    pub kind: ServiceKind,
    pub data: DataDir,
    pub installed: PathBuf,
    #[cfg(feature = "blue-green-test")]
    pub endpoint: Option<Location>,
}

/// The `startup::Config` for `setup`: the update CLI's data directory
/// (`DataDir::for_this_user`, `open-ferry-update`'s `data_dir.rs`), receipt
/// file and versions directory, the binary named as `release::binary_name`
/// names it, and the real timings.
pub(crate) fn startup_config(setup: Setup) -> startup::Config {
    let receipt = match Receipt::load(&setup.data.receipt_file()) {
        Ok(Some(receipt)) => serde_json::to_value(receipt).unwrap_or_default(),
        Ok(None) => serde_json::json!({}),
        Err(error) => {
            tracing::warn!("{error}");
            serde_json::json!({})
        }
    };
    startup::Config {
        data: setup.data,
        installed: setup.installed,
        config: setup.config,
        kind: setup.kind,
        timings: Timings::default(),
        receipt,
        binary_name: open_ferry_update::release::binary_name(open_ferry_update::TARGET).to_owned(),
        runner: Arc::new(ProcessRunner),
        detect_timeout: DETECT_TIMEOUT,
        #[cfg(feature = "blue-green-test")]
        test_endpoint_location: setup.endpoint,
    }
}

/// The endpoint a test names. On Unix `value` is the socket's directory,
/// `<root>/<name>`; on Windows it is the pipe's name.
#[cfg(feature = "blue-green-test")]
fn test_endpoint(value: &str) -> Result<Location, String> {
    #[cfg(unix)]
    {
        let path = Path::new(value);
        let (Some(root), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(format!("-test-endpoint {value:?} isn't <root>/<name>"));
        };
        Location::new(root, name).map_err(|error| format!("-test-endpoint: {error:?}"))
    }
    #[cfg(windows)]
    {
        Location::from_record(value).map_err(|error| format!("-test-endpoint: {error:?}"))
    }
}

/// The data directory `-data-dir` pins, else the user's
/// (`DataDir::for_this_user`).
fn data_dir(flags: &Flags) -> Result<DataDir, String> {
    if flags.data_dir.is_empty() {
        DataDir::for_this_user().map_err(|error| error.to_string())
    } else {
        Ok(DataDir::at(PathBuf::from(&flags.data_dir)))
    }
}

/// Where the data directory and the endpoint are: the real ones, or in a
/// test build the ones the test flags name.
fn locate(flags: &Flags) -> Result<(DataDir, Option<Location>), String> {
    #[cfg(feature = "blue-green-test")]
    {
        let data = if flags.test_data_dir.is_empty() {
            data_dir(flags)?
        } else {
            DataDir::at(absolute(Path::new(&flags.test_data_dir)).map_err(|e| e.to_string())?)
        };
        let endpoint = if flags.test_endpoint.is_empty() {
            None
        } else {
            Some(test_endpoint(&flags.test_endpoint)?)
        };
        Ok((data, endpoint))
    }
    #[cfg(not(feature = "blue-green-test"))]
    {
        Ok((data_dir(flags)?, None))
    }
}

/// `-supervise`: runs the supervisor until it is stopped. The supervisor's
/// own logs go to standard error.
///
/// The data directory is found first, from the environment the supervisor
/// started with. Then the working directory's `.env` is loaded, once, before
/// the logger starts its thread, so the workers, which inherit the
/// supervisor's environment, see its settings (but they never decide where
/// the data directory is: the task's definition pins it with `-data-dir`).
/// A `-worker` loads no `.env`: it has its supervisor's environment.
pub fn supervise(flags: &Flags) -> ExitCode {
    let located = match authorized_location(flags) {
        Ok(located) => located,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let dotenv = std::env::current_dir()
        .ok()
        .map(|dir| crate::load_dotenv(&dir.join(".env")));
    if let Some(Err(error)) = &dotenv
        && error.is_refused()
    {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    let log = logging::init_stderr();
    if let Some(Err(error)) = dotenv
        && !error.is_not_found()
    {
        tracing::warn!("failed to load .env file: {error}");
    }
    // Every exit path, a panic's unwind included, waits at most a second for
    // the queued lines.
    log.flush_after(|| run_supervisor(flags, &log, located, None))
}

/// Refuses a system supervisor outside the system service's account.
fn system_account(
    system: bool,
    runs_as_system: Result<bool, String>,
    platform: crate::os_service::Platform,
) -> Result<(), String> {
    if !system {
        return Ok(());
    }
    if runs_as_system? {
        return Ok(());
    }
    let start = match platform {
        crate::os_service::Platform::Windows => "sc start open-ferry",
        crate::os_service::Platform::Linux => "systemctl start open-ferry",
        crate::os_service::Platform::MacOs => "use an unprivileged user supervisor on macOS",
    };
    Err(format!(
        "a system supervisor runs only as the system service; {start}"
    ))
}

pub(crate) fn require_system_account() -> Result<(), String> {
    system_account(
        true,
        scope::runs_as_system(),
        crate::os_service::Platform::HOST.unwrap_or(crate::os_service::Platform::MacOs),
    )
}

/// The privileged gate, before a receipt is read or startup writes anything.
fn authorize_data(probe: &dyn scope::Probe, data: &DataDir) -> Result<(), scope::Refusal> {
    let root = data.root().to_str().ok_or_else(|| scope::Refusal {
        reason: format!("{} isn't valid Unicode", data.root().display()),
        remedy: None,
    })?;
    scope::authorize(probe, root)
}

fn authorized_location(flags: &Flags) -> Result<(DataDir, Option<Location>), String> {
    if flags.system {
        require_system_account()?;
    }
    let located = locate(flags)?;
    authorize_data(&scope::Host, &located.0).map_err(|refusal| refusal.to_string())?;
    Ok(located)
}

/// The SCM entry: `.env` was loaded before the dispatcher started a thread.
/// Its Stop and Shutdown controls replace console signal watchers.
#[cfg(windows)]
pub(crate) fn supervise_service(
    flags: &Flags,
    dotenv: Option<Result<(), crate::dotenv::Error>>,
    stops: mpsc::UnboundedReceiver<&'static str>,
) -> ExitCode {
    let located = match authorized_location(flags) {
        Ok(located) => located,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let log = logging::init_stderr();
    if let Some(Err(error)) = dotenv
        && !error.is_not_found()
    {
        tracing::warn!("failed to load .env file: {error}");
    }
    log.flush_after(|| run_supervisor(flags, &log, located, Some(stops)))
}

fn run_supervisor(
    flags: &Flags,
    log: &logging::StderrLevel,
    located: (DataDir, Option<Location>),
    stops: Option<mpsc::UnboundedReceiver<&'static str>>,
) -> ExitCode {
    let setup = || -> Result<(SupervisedHost, Setup), String> {
        let config_path = if flags.config.is_empty() {
            std::env::current_dir()
                .map_err(|error| format!("failed to get the working directory: {error}"))?
                .join("config.yaml")
        } else {
            absolute(Path::new(&flags.config))
                .map_err(|error| format!("failed to find the config: {error}"))?
        };
        let config = crate::settings::load(&crate::scope::Host, &config_path)
            .map_err(|error| format!("failed to load config: {error}"))?;
        log.set_debug(config.debug);
        let wanted = wanted(&config)?;
        let exe = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map(plain)
            .map_err(|error| format!("failed to find the open-ferry binary: {error}"))?;
        let (data, endpoint) = located;
        #[cfg(not(feature = "blue-green-test"))]
        let _ = endpoint;
        let kind = if flags.system {
            ServiceKind::System
        } else {
            ServiceKind::User
        };
        let host = SupervisedHost::new(config_path.clone(), exe.clone(), wanted);
        Ok((
            host,
            Setup {
                config: config_path,
                kind,
                data,
                installed: exe,
                #[cfg(feature = "blue-green-test")]
                endpoint,
            },
        ))
    };
    let (host, setup) = match setup() {
        Ok(ready) => ready,
        Err(message) => {
            tracing::error!("{message}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("open-ferry Version: {}", env!("CARGO_PKG_VERSION"));
    tracing::info!("supervising {}", setup.config.display());
    let kind = setup.kind;
    let cfg = startup_config(setup);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let signals = match stops {
            Some(stops) => stops,
            None => match watch_signals() {
                Ok(signals) => signals,
                Err(error) => {
                    tracing::error!("failed to watch for stop signals: {error}");
                    return None;
                }
            },
        };
        let (command_tx, commands) = mpsc::unbounded_channel();
        // A stop sent before the core runs waits in the channel.
        tokio::spawn(forward_stops(signals, command_tx.clone()));
        let driver = ProcessDriver::new(kind, cfg.timings.exit_poll);
        Some(
            open_ferry_supervisor::supervise::supervise(
                &cfg,
                &host,
                &driver,
                &open_ferry_supervisor::clock::SystemClock,
                command_tx,
                commands,
                open_ferry_supervisor::supervise::default_limit(),
            )
            .await,
        )
    });
    let Some(result) = result else {
        return ExitCode::FAILURE;
    };
    match &result {
        Ok(Exit::Stopped) => tracing::info!("the supervisor stopped"),
        Ok(Exit::TooManyPanics) => tracing::error!("the supervisor exits: too many panics"),
        Err(error) => tracing::error!("the supervisor failed: {error}"),
    }
    ExitCode::from(exit_status(&result))
}

/// `-worker`: takes over the control channel and sockets `control` names,
/// and serves them as a supervised worker. It never returns on success:
/// the worker ends the process itself. A failure to take over or attach is
/// one line on standard error and a non-zero exit.
pub fn worker_entry(flags: &Flags) -> ExitCode {
    let fail = |message: String| {
        eprintln!("{message}");
        ExitCode::FAILURE
    };
    // Before the runtime or any thread: no thread can see the default.
    #[cfg(unix)]
    leave_terminal_stops_to_the_supervisor();
    // Single-threaded: on Unix this removes `LISTEN_FDS` from the
    // environment, and it must be before the runtime and the logger.
    let pending = match open_ferry_supervisor::worker::take(OsStr::new(&flags.worker_control)) {
        Ok(pending) => pending,
        Err(error) => {
            return fail(format!(
                "worker: can't take over the control channel: {error}"
            ));
        }
    };
    let config_path = if flags.config.is_empty() {
        return fail("worker: -config is needed".to_owned());
    } else {
        match absolute(Path::new(&flags.config)) {
            Ok(path) => path,
            Err(error) => return fail(format!("worker: can't find the config: {error}")),
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return fail(format!(
                "worker: failed to start the async runtime: {error}"
            ));
        }
    };
    let log_level = logging::init_supervised();
    let hello = Hello {
        max_version: VERSION,
        min_supervisor: VERSION,
        version: open_ferry_update::CURRENT_VERSION.to_owned(),
        caps: REQUIRED_CAPS.iter().map(|cap| (*cap).to_owned()).collect(),
    };
    // `service::worker::run` never returns, and the runtime is not dropped
    // here: it ends with the process.
    runtime.block_on(async move {
        let attached = match pending.attach(hello).await {
            Ok(attached) => attached,
            Err(error) => return fail(attach_failure(&error)),
        };
        let options = service::Options {
            local_password: String::new(),
            keep_alive: false,
            announce: false,
            // The worker reads the config and its hash itself.
            config_sha256: None,
            self_update: true,
        };
        service::worker::run(
            config_path,
            log_level,
            options,
            attached.channel,
            service::worker::Welcome::from_attached(attached.welcome),
            attached.listeners,
        )
        .await
    })
}

/// A worker's SIGINT and SIGTERM do nothing, so its stop stays under the
/// supervisor's control channel.
///
/// The supervisor spawns workers in its own process group, so a terminal's
/// Ctrl-C (SIGINT to the foreground group) or a service manager's SIGTERM to
/// the whole cgroup reaches them as well, and would kill them at once,
/// skipping the graceful stop. A no-op *handler* is installed, not `SIG_IGN`:
/// exec resets a caught signal to its default but keeps an ignored one, so
/// processes the worker starts keep the default disposition.
///
/// Windows needs nothing: workers are spawned with `CREATE_NO_WINDOW`, so
/// they share no console, and so no console event, with the supervisor.
#[cfg(unix)]
fn leave_terminal_stops_to_the_supervisor() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler is async-signal-safe (it does nothing), and
        // `action` is fully initialised before the call.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = do_nothing as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(unix)]
extern "C" fn do_nothing(_signal: libc::c_int) {}

/// The line for a failed attach.
fn attach_failure(error: &AttachError) -> String {
    format!("worker: can't attach to the supervisor: {error:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct GateProbe {
        platform: crate::os_service::Platform,
        privileged: Result<bool, String>,
        trusted: bool,
        checked: std::cell::Cell<usize>,
    }

    impl scope::Probe for GateProbe {
        fn platform(&self) -> crate::os_service::Platform {
            self.platform
        }
        fn privileged(&self) -> Result<bool, String> {
            self.privileged.clone()
        }
        fn account_dir(&self) -> Result<String, String> {
            panic!("must not locate account records")
        }
        fn program_data(&self) -> Result<String, String> {
            Ok("C:/ProgramData".into())
        }
        fn state(&self, _: &str) -> scope::State {
            self.checked.set(self.checked.get() + 1);
            scope::State::Directory
        }
        fn entries(&self, _: &str, _: usize) -> io::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn trusted_dir(&self, _: &str) -> Result<(), String> {
            if self.trusted {
                Ok(())
            } else {
                Err("data is untrusted".into())
            }
        }
        fn trusted_file(&self, _: &str) -> Result<(), String> {
            panic!("no files")
        }
        fn readable_file(&self, _path: &str) -> io::Result<()> {
            Ok(())
        }

        fn receipt(
            &self,
            _: &str,
        ) -> Result<Option<Receipt>, open_ferry_update::receipt::ReceiptError> {
            panic!("must not read a receipt")
        }
        fn canonical(&self, _: &str) -> io::Result<String> {
            panic!("must not resolve paths")
        }
        fn create_machine_dir(&mut self, _: &str) -> io::Result<()> {
            panic!("must not provision")
        }
    }

    #[test]
    fn the_privileged_gate_passes_only_trusted_records_and_prints_the_remedy() {
        use crate::os_service::Platform;
        for (platform, path, remedy) in [
            (Platform::Linux, "/root/data", "without sudo"),
            (
                Platform::Windows,
                "C:/Users/admin/data",
                "not opened as administrator",
            ),
        ] {
            let data = DataDir::at(path);
            let mut probe = GateProbe {
                platform,
                privileged: Ok(false),
                trusted: false,
                checked: 0.into(),
            };
            assert_eq!(authorize_data(&probe, &data), Ok(()));
            assert_eq!(probe.checked.get(), 0, "unprivileged paths are untouched");
            probe.privileged = Ok(true);
            let refusal = authorize_data(&probe, &data).unwrap_err();
            assert_eq!(refusal.reason, "data is untrusted");
            assert!(refusal.to_string().starts_with("data is untrusted. "));
            assert!(refusal.to_string().contains(remedy), "{refusal}");
            probe.trusted = true;
            assert_eq!(authorize_data(&probe, &data), Ok(()));
            probe.privileged = Err("token failed".into());
            assert!(
                authorize_data(&probe, &data)
                    .unwrap_err()
                    .to_string()
                    .contains("token failed")
            );
        }
        let probe = GateProbe {
            platform: Platform::MacOs,
            privileged: Ok(true),
            trusted: true,
            checked: 0.into(),
        };
        assert!(authorize_data(&probe, &DataDir::at("/root/data")).is_err());
        assert_eq!(probe.checked.get(), 0);
    }

    #[test]
    fn a_system_supervisor_needs_root_or_system_even_for_an_administrator() {
        use crate::os_service::Platform;
        for (platform, start) in [
            (Platform::Windows, "sc start open-ferry"),
            (Platform::Linux, "systemctl start open-ferry"),
        ] {
            assert_eq!(
                system_account(false, Err("not queried".into()), platform),
                Ok(())
            );
            assert_eq!(system_account(true, Ok(true), platform), Ok(()));
            let message = system_account(true, Ok(false), platform).unwrap_err();
            assert_eq!(
                message,
                format!("a system supervisor runs only as the system service; {start}")
            );
            assert_eq!(
                system_account(true, Err("account failed".into()), platform),
                Err("account failed".into())
            );
        }
        assert!(
            system_account(true, Ok(false), Platform::MacOs)
                .unwrap_err()
                .contains("unprivileged user supervisor")
        );
    }

    #[cfg(feature = "blue-green-test")]
    #[test]
    fn the_test_data_directory_is_the_one_the_gate_will_check() {
        let dir = tempfile::tempdir().unwrap();
        let flags = Flags {
            data_dir: dir.path().join("ordinary").to_string_lossy().into_owned(),
            test_data_dir: dir.path().join("test").to_string_lossy().into_owned(),
            ..Flags::default()
        };
        assert_eq!(locate(&flags).unwrap().0.root(), dir.path().join("test"));
    }

    #[cfg(unix)]
    fn current(signal: libc::c_int) -> libc::sigaction {
        // SAFETY: a null new action only reads the current one.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut old), 0);
            old
        }
    }

    /// SIGINT and SIGTERM get a handler that does nothing: neither the default
    /// (which kills the worker) nor `SIG_IGN` (which processes it starts would
    /// inherit). The test restores what it found.
    #[cfg(unix)]
    #[test]
    fn a_worker_catches_sigint_and_sigterm_with_a_handler_and_does_not_ignore_them() {
        let signals = [libc::SIGINT, libc::SIGTERM];
        let before = signals.map(current);
        leave_terminal_stops_to_the_supervisor();
        for signal in signals {
            let now = current(signal);
            assert_ne!(now.sa_sigaction, libc::SIG_DFL, "signal {signal}");
            assert_ne!(now.sa_sigaction, libc::SIG_IGN, "signal {signal}");
            assert_eq!(
                now.sa_sigaction,
                do_nothing as extern "C" fn(libc::c_int) as libc::sighandler_t
            );
        }
        for (signal, old) in signals.into_iter().zip(before) {
            // SAFETY: puts back an action read from the same signal.
            unsafe {
                libc::sigaction(signal, &old, std::ptr::null_mut());
            }
        }
    }

    fn config(extra: &str) -> Config {
        Config::parse(format!("{extra}\n")).unwrap()
    }

    fn write(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("config.yaml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn local(name: &'static str, tls: bool) -> Wanted {
        Wanted {
            name,
            host: "127.0.0.1".to_owned(),
            port: 0,
            tls,
        }
    }

    #[test]
    fn the_topology_is_the_main_listener_alone_without_a_management_port() {
        let config = config("host: 127.0.0.1\nport: 8317");
        let listeners = wanted(&config).unwrap();
        assert_eq!(
            listeners,
            [Wanted {
                name: "main",
                host: "127.0.0.1".into(),
                port: 8317,
                tls: false
            }]
        );
        let host = SupervisedHost::new("c.yaml".into(), "x".into(), listeners);
        assert_eq!(
            host.topology(),
            [ListenerTopology {
                name: "main".into(),
                address: "127.0.0.1:8317".parse().unwrap(),
                tls: false
            }]
        );
    }

    #[test]
    fn a_separate_management_address_is_a_second_listener_and_tls_covers_both() {
        let config = config(
            "host: 127.0.0.1\nport: 8317\ntls:\n  enable: true\nremote-management:\n  separate-address: 127.0.0.1:8320",
        );
        let listeners = wanted(&config).unwrap();
        assert_eq!(listeners.len(), 2);
        assert_eq!((listeners[0].name, listeners[0].tls), ("main", true));
        assert_eq!(
            (listeners[1].name, listeners[1].port, listeners[1].tls),
            ("management", 8320, true)
        );
        let host = SupervisedHost::new("c.yaml".into(), "x".into(), listeners);
        let topology = host.topology();
        assert_eq!(topology[1].address, "127.0.0.1:8320".parse().unwrap());
    }

    #[test]
    fn an_empty_host_is_every_interface() {
        let config = config("host: \"\"\nport: 8317");
        let listeners = wanted(&config).unwrap();
        assert_eq!(listeners[0].host, "");
        let host = SupervisedHost::new("c.yaml".into(), "x".into(), listeners);
        assert_eq!(host.topology()[0].address, "[::]:8317".parse().unwrap());
    }

    #[test]
    fn a_port_out_of_range_is_refused() {
        let mut config = config("host: 127.0.0.1");
        config.port = 70000;
        assert!(wanted(&config).is_err());
    }

    #[test]
    fn bind_listens_on_loopback_and_the_topology_then_names_the_sockets() {
        let host = SupervisedHost::new(
            "c.yaml".into(),
            "x".into(),
            vec![local("main", false), local("management", true)],
        );
        let listeners = host.bind().unwrap();
        assert_eq!(
            listeners
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["main", "management"]
        );
        let topology = host.topology();
        for ((_, listener), shown) in listeners.iter().zip(&topology) {
            assert_eq!(listener.local_addr().unwrap(), shown.address);
            assert!(shown.address.ip().is_loopback());
            assert_ne!(shown.address.port(), 0);
        }
        assert!(!topology[0].tls && topology[1].tls);
    }

    #[test]
    fn a_taken_port_fails_the_bind_and_keeps_none() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let host = SupervisedHost::new(
            "c.yaml".into(),
            "x".into(),
            vec![
                local("main", false),
                Wanted {
                    name: "management",
                    host: "127.0.0.1".into(),
                    port,
                    tls: false,
                },
            ],
        );
        // On Unix the second bind of a bound port fails; on Windows too,
        // exclusively.
        let error = host.bind().unwrap_err();
        assert!(error.to_string().contains("management"), "{error}");
        // Not bound: the topology is still the expected one.
        assert_eq!(host.topology()[0].address.port(), 0);
    }

    fn mode_host(dir: &Path, env: Arc<Mutex<Option<String>>>) -> SupervisedHost {
        SupervisedHost::new(
            dir.join("config.yaml"),
            "x".into(),
            vec![local("main", false)],
        )
        .with_env(move || env.lock().unwrap().clone())
    }

    #[test]
    fn the_mode_is_read_again_and_its_generation_moves_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let env = Arc::new(Mutex::new(None));
        let host = mode_host(dir.path(), env.clone());
        // No file yet: switching stays off, and the generation holds.
        assert_eq!(
            host.effective_mode(),
            ModeSnapshot {
                off: true,
                mode_gen: 0
            }
        );
        write(dir.path(), "self-update:\n  mode: auto\n");
        let first = host.effective_mode();
        assert!(!first.off);
        assert_eq!(host.effective_mode(), first, "no change, no new generation");
        write(dir.path(), "self-update:\n  mode: \"off\"\n");
        let second = host.effective_mode();
        assert!(second.off);
        assert!(second.mode_gen > first.mode_gen);
        write(dir.path(), "self-update:\n  mode: notify\n");
        let third = host.effective_mode();
        assert!(!third.off);
        assert!(third.mode_gen > second.mode_gen);
    }

    #[test]
    fn the_environment_can_turn_switching_off_and_back() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "self-update:\n  mode: auto\n");
        let env = Arc::new(Mutex::new(None));
        let host = mode_host(dir.path(), env.clone());
        let on = host.effective_mode();
        assert!(!on.off);
        *env.lock().unwrap() = Some("off".to_owned());
        let off = host.effective_mode();
        assert!(off.off && off.mode_gen > on.mode_gen);
        *env.lock().unwrap() = None;
        let back = host.effective_mode();
        assert!(!back.off && back.mode_gen > off.mode_gen);
        // The environment only lowers the mode: a config that is off stays off.
        write(dir.path(), "self-update:\n  mode: \"off\"\n");
        *env.lock().unwrap() = Some("auto".to_owned());
        assert!(host.effective_mode().off);
    }

    #[test]
    fn a_config_that_can_no_longer_be_read_keeps_the_last_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "self-update:\n  mode: auto\n");
        let host = mode_host(dir.path(), Arc::new(Mutex::new(None)));
        let before = host.effective_mode();
        assert!(!before.off);
        std::fs::write(&path, "port: [not, a, port\n").unwrap();
        assert_eq!(host.effective_mode(), before);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(host.effective_mode(), before);
    }

    #[test]
    fn own_sha256_is_the_hash_of_the_binary_file_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("open-ferry");
        std::fs::write(&exe, b"abc").unwrap();
        let host = SupervisedHost::new("c.yaml".into(), exe, Vec::new());
        assert_eq!(
            host.own_sha256().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let missing = SupervisedHost::new("c.yaml".into(), dir.path().join("none"), Vec::new());
        assert_eq!(missing.own_sha256(), None);
    }

    #[test]
    fn own_sha256_stays_the_hash_of_the_image_when_the_binary_is_replaced_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("open-ferry");
        std::fs::write(&exe, b"abc").unwrap();
        let host = SupervisedHost::new("c.yaml".into(), exe.clone(), Vec::new());
        // As install-self does: the old file aside, a different one in its place.
        let new = dir.path().join("open-ferry.new");
        std::fs::write(&new, b"a different binary").unwrap();
        let aside = dir.path().join("open-ferry.old");
        std::fs::rename(&exe, &aside).unwrap();
        std::fs::rename(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"a different binary");
        assert_eq!(
            host.own_sha256().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn waiting_for_the_update_lock_calls_the_hook_with_the_wait() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let host = SupervisedHost::new("c.yaml".into(), "x".into(), Vec::new())
            .with_waiting_hook(move |waited| record.lock().unwrap().push(waited));
        host.waiting_for_update_lock(Duration::from_secs(30));
        assert_eq!(*seen.lock().unwrap(), [Duration::from_secs(30)]);
        // Without a hook it only logs.
        SupervisedHost::new("c.yaml".into(), "x".into(), Vec::new())
            .waiting_for_update_lock(Duration::from_secs(1));
    }

    #[test]
    fn the_version_is_the_binarys() {
        let host = SupervisedHost::new("c.yaml".into(), "x".into(), Vec::new());
        assert_eq!(host.own_version(), open_ferry_update::CURRENT_VERSION);
    }

    #[test]
    fn exit_status_is_zero_only_for_a_stop() {
        assert_eq!(exit_status(&Ok(Exit::Stopped)), 0);
        assert_eq!(exit_status(&Ok(Exit::TooManyPanics)), 1);
        assert_eq!(exit_status(&Err(StartupError::AlreadyRunning)), 1);
        assert_eq!(exit_status(&Err(StartupError::Stopped)), 1);
        assert_eq!(
            exit_status(&Err(StartupError::Io(io::Error::other("x")))),
            1
        );
    }

    #[tokio::test]
    async fn the_first_signal_sends_one_stop_and_later_ones_send_none() {
        let (signals, source) = mpsc::unbounded_channel();
        let (tx, mut commands) = mpsc::unbounded_channel();
        // Sent before anything listens: the channel keeps them.
        signals.send("SIGTERM").unwrap();
        signals.send("SIGINT").unwrap();
        signals.send("SIGINT").unwrap();
        drop(signals);
        forward_stops(source, tx).await;
        assert!(matches!(commands.recv().await, Some(Command::Stop)));
        assert!(commands.recv().await.is_none(), "a second stop was sent");
    }

    /// The stop is on the command channel before the first signal is logged,
    /// so a log call that is slow cannot delay it. The logging is a
    /// parameter, so the test sees the channel at the moment of each log call
    /// and depends on no tracing state shared with other tests.
    #[test]
    fn the_stop_is_sent_before_the_signal_is_logged() {
        let (tx, commands) = mpsc::unbounded_channel();
        let commands = std::cell::RefCell::new(commands);
        let (signals, source) = mpsc::unbounded_channel();
        signals.send("SIGTERM").unwrap();
        signals.send("SIGINT").unwrap();
        drop(signals);
        let seen = std::cell::RefCell::new(Vec::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(forward_stops_logging(source, tx, |name, again| {
            let stop = matches!(commands.borrow_mut().try_recv(), Ok(Command::Stop));
            seen.borrow_mut().push((name, again, stop));
        }));
        // The first signal was logged with the Stop already sent; the second
        // was logged as a repeat, with no second Stop behind it.
        assert_eq!(
            *seen.borrow(),
            [("SIGTERM", false, true), ("SIGINT", true, false)],
            "the line was logged first"
        );
    }

    #[test]
    fn the_startup_config_uses_the_update_clis_layout() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path().join("data"));
        let cfg = startup_config(Setup {
            config: dir.path().join("config.yaml"),
            kind: ServiceKind::System,
            data: data.clone(),
            installed: dir.path().join("open-ferry"),
            #[cfg(feature = "blue-green-test")]
            endpoint: None,
        });
        assert_eq!(cfg.data, data);
        assert_eq!(cfg.kind, ServiceKind::System);
        assert_eq!(cfg.timings, Timings::default());
        assert_eq!(cfg.detect_timeout, DETECT_TIMEOUT);
        assert_eq!(
            cfg.binary_name,
            open_ferry_update::release::binary_name(open_ferry_update::TARGET)
        );
        #[cfg(feature = "blue-green-test")]
        assert!(cfg.test_endpoint_location.is_none());
        assert_eq!(cfg.receipt, serde_json::json!({}));
        std::fs::create_dir_all(data.root()).unwrap();
        std::fs::write(
            data.receipt_file(),
            r#"{"format":1,"installer":"install.sh","version":"0.1.0","binary":"/b/open-ferry","target":"t","installed_at":"2026-10-08T12:00:00Z"}"#,
        )
        .unwrap();
        let cfg = startup_config(Setup {
            config: dir.path().join("config.yaml"),
            kind: ServiceKind::User,
            data,
            installed: dir.path().join("open-ferry"),
            #[cfg(feature = "blue-green-test")]
            endpoint: None,
        });
        assert_eq!(cfg.receipt["binary"], "/b/open-ferry");
    }

    #[cfg(feature = "blue-green-test")]
    #[test]
    fn the_test_flags_name_the_data_directory_and_the_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let flags = Flags {
            test_data_dir: dir.path().join("data").to_string_lossy().into_owned(),
            ..Flags::default()
        };
        let (data, endpoint) = locate(&flags).unwrap();
        assert_eq!(data.root(), dir.path().join("data"));
        assert!(endpoint.is_none());
        assert!(test_endpoint("not-absolute-and-no-parent").is_err());
    }
}
