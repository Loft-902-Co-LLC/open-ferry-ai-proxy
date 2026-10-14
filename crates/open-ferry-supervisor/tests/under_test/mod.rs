//! Not upstream's: the process tests' worker-under-test layer (L9.14).
//!
//! Everything a process test knows about the worker it runs lives here: the
//! binary (`worker_binary`), how a version is staged (`Fx::stage`: the binary
//! plus its `test-marker` file), where its fault and barrier files go (`Fx::fault`,
//! `Fx::hold`, ...), how its event log is read (`Fx::events`), and how its
//! version is told apart (`Ev::marker`). Phase 2c points the same tests at the
//! real worker by changing `worker_binary`, `Fx::stage` and the readers.
//!
//! The supervisor under test is the real `supervise()` with the real
//! `ProcessDriver`, run in a child process of the test binary (this module's
//! `supervisor_child`, re-run with `--exact under_test::supervisor_child`).
//! The child wraps the driver to log what the supervisor does to each worker
//! (`driver.log`); a test reads that log, the workers' event log, the journal
//! and the state file, and asserts the four columns of design 13 item 8 with
//! `Fx::assert_columns`.
//!
//! No test sleeps to prove anything: it waits, with a bound, for an event, a
//! barrier file, a journal state or a process exit. Every child is ended by its
//! handle when its guard drops, also when the test panics.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use open_ferry_supervisor::clock::SystemClock;
use open_ferry_supervisor::core::Command;
use open_ferry_supervisor::driver::{
    Cmd, Driver, EventSender, ProcessDriver, SpawnError, SpawnRequest, WorkerPort,
};
use open_ferry_supervisor::endpoint::{Identity, Location, ServiceKind, connect};
use open_ferry_supervisor::host::{Host, ModeSnapshot};
use open_ferry_supervisor::protocol::{
    Admission, Channel, EndpointReply, EndpointRequest, Frame, How, ListenerTopology, VERSION,
};
use open_ferry_supervisor::startup::Config;
use open_ferry_supervisor::supervise::{default_limit, supervise};
use open_ferry_supervisor::timings::Timings;
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::runner::Runner;
use open_ferry_update::staging::hash_file;
use serde_json::{Value, json};

pub const NAME: &str = if cfg!(windows) {
    "open-ferry.exe"
} else {
    "open-ferry"
};
pub const A: &str = "1.0.0";
pub const B: &str = "2.0.0";
pub const C: &str = "3.0.0";

/// How long any one wait lasts. A wrong outcome ends far sooner; this is only
/// the bound that keeps a failure from hanging.
pub const LONG: Duration = Duration::from_secs(90);
pub const POLL: Duration = Duration::from_millis(20);

/// Writes a worker binary the supervisor can execute: on Unix a file made by
/// `fs::write` has no execute bit, and the worker would not start.
pub fn write_executable(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Which worker the tests run against (L9.14). The binary is not named here:
/// it is whichever of the two the including test crate was built with. The
/// supervisor crate's tests have `fake_worker` (and `worker_probe`); the
/// `open-ferry` crate's have the real `open-ferry` and neither of the others.
/// Cargo sets `CARGO_BIN_EXE_<name>` for a test crate only for its own
/// package's binaries, so the choice needs no flag that could be set wrongly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fake,
    Real,
}

const FAKE_WORKER: Option<&str> = option_env!("CARGO_BIN_EXE_fake_worker");
const REAL_WORKER: Option<&str> = option_env!("CARGO_BIN_EXE_open-ferry");
const WORKER_PROBE: Option<&str> = option_env!("CARGO_BIN_EXE_worker_probe");

pub fn kind() -> Kind {
    match (FAKE_WORKER, REAL_WORKER) {
        (Some(_), _) => Kind::Fake,
        (None, Some(_)) => Kind::Real,
        (None, None) => panic!("neither fake_worker nor open-ferry is a binary of this crate"),
    }
}

/// What the worker under test can do. A test needing one the worker lacks
/// returns at its start with `lacks`: it is skipped by capability, never by
/// the worker's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// The fake-only `send-fail` fault (a send to the supervisor fails).
    SendFail,
    /// The fake-only `signins-pending` fault (a status reports pending sign-ins).
    SigninsPending,
    /// The fake-only `spawn-grandchild` fault.
    SpawnGrandchild,
    /// `worker_probe`, the stand-in with modes (`Fx::stage_probe`).
    Probe,
}

pub fn has(cap: Cap) -> bool {
    match cap {
        Cap::SendFail | Cap::SigninsPending | Cap::SpawnGrandchild => kind() == Kind::Fake,
        Cap::Probe => WORKER_PROBE.is_some(),
    }
}

/// True (after saying so) when the worker under test lacks `cap`: the caller
/// returns, and the test is skipped.
pub fn lacks(cap: Cap) -> bool {
    let lacking = !has(cap);
    if lacking {
        eprintln!("skipped: the {:?} worker has no {cap:?}", kind());
    }
    lacking
}

/// The worker under test (L9.14).
pub fn worker_binary() -> &'static Path {
    Path::new(FAKE_WORKER.or(REAL_WORKER).expect("a worker binary"))
}

/// The bytes the artifact of `version` has: the worker binary and a trailer
/// that makes every version hash differently.
pub fn artifact_bytes(version: &str) -> Vec<u8> {
    static BASE: OnceLock<Vec<u8>> = OnceLock::new();
    let base = BASE.get_or_init(|| lean(worker_binary()));
    with_trailer(base, version)
}

/// `version`'s bytes with the last one flipped: what a test that damages a
/// file on purpose (`flip_last_byte`, `Fx::corrupt`) leaves, named so that
/// `Columns::installed_bytes` can expect it.
#[allow(dead_code)]
pub fn damaged_artifact(version: &str) -> Vec<u8> {
    let mut bytes = artifact_bytes(version);
    *bytes.last_mut().unwrap() ^= 1;
    bytes
}

/// The bytes of a worker binary. A debug build on Linux carries about 170 MB
/// of DWARF, and the supervisor hashes the artifact (unoptimised sha2 does a
/// few MB a second), which made every start take half a minute. The debug
/// sections are stripped when `strip` is there; the program is unchanged.
fn lean(path: &Path) -> Vec<u8> {
    #[cfg(target_os = "linux")]
    {
        let out = std::env::temp_dir().join(format!(
            "ofp-pt-lean-{}-{}",
            std::process::id(),
            path.file_name().unwrap().to_string_lossy()
        ));
        // `strip` gets 60 s. On expiry it is killed and the original bytes
        // are used: slower to start, but the same program.
        let stripped = std::process::Command::new("strip")
            .arg("--strip-debug")
            .arg("-o")
            .arg(&out)
            .arg(path)
            .spawn()
            .ok()
            .is_some_and(|mut child| {
                let until = Instant::now() + Duration::from_secs(60);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break status.success(),
                        Ok(None) if Instant::now() < until => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break false;
                        }
                    }
                }
            });
        let bytes = stripped.then(|| std::fs::read(&out).ok()).flatten();
        let _ = std::fs::remove_file(&out);
        if let Some(bytes) = bytes {
            return bytes;
        }
    }
    std::fs::read(path).expect("read the worker binary")
}

fn with_trailer(base: &[u8], version: &str) -> Vec<u8> {
    let mut bytes = base.to_vec();
    bytes.extend_from_slice(
        format!(
            "
--artifact {version}--"
        )
        .as_bytes(),
    );
    bytes
}

/// A stand-in worker with modes (`worker_probe`), for the branches the fake
/// cannot reach (a worker that never greets, one that needs a newer supervisor).
/// Its mode is read from `<config>/mode`. 2c has no use for it.
fn probe_bytes(version: &str) -> Vec<u8> {
    static BASE: OnceLock<Vec<u8>> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        lean(Path::new(WORKER_PROBE.expect(
            "this crate has no worker_probe (check `has(Cap::Probe)`)",
        )))
    });
    with_trailer(base, version)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Whether process `pid` still runs (a zombie, not yet reaped, does not).
pub fn alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.trim_start().chars().next())
                    != Some('Z')
            }
            Err(_) => false,
        }
    }
    #[cfg(windows)]
    {
        use open_ferry_supervisor::process::windows::ProcessHandle;
        match ProcessHandle::open_pid(pid) {
            Ok(Some(handle)) => matches!(handle.try_wait(), Ok(None)),
            Ok(None) => false,
            Err(_) => true,
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // SAFETY: signal 0 only checks the id.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

// ---- the child: the real supervisor ---------------------------------------

/// A runner that knows the artifacts by hash: `open-ferry <version>`.
struct MapRunner(HashMap<String, String>);

impl Runner for MapRunner {
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let sha = hash_file(binary).map_err(|error| error.to_string())?;
            match self.0.get(&sha) {
                Some(version) => Ok(format!("open-ferry {version}")),
                None => Err(format!("{} is not a known artifact", binary.display())),
            }
        })
    }
}

/// The host: a main listener and a separate management listener, the mode
/// from the file `host-mode` in the fixture (`off <gen>` or `on <gen>`), and
/// this binary's hash.
struct ChildHost {
    main: TcpListener,
    management: TcpListener,
    fixture: PathBuf,
    sha: String,
}

impl Host for ChildHost {
    fn topology(&self) -> Vec<ListenerTopology> {
        ["main", "management"]
            .into_iter()
            .zip([&self.main, &self.management])
            .map(|(name, listener)| ListenerTopology {
                name: name.to_owned(),
                address: listener.local_addr().expect("address"),
                tls: false,
            })
            .collect()
    }
    fn bind(&self) -> io::Result<Vec<(String, TcpListener)>> {
        Ok(vec![
            ("main".to_owned(), self.main.try_clone()?),
            ("management".to_owned(), self.management.try_clone()?),
        ])
    }
    fn effective_mode(&self) -> ModeSnapshot {
        let text = std::fs::read_to_string(self.fixture.join("host-mode")).unwrap_or_default();
        let mut words = text.split_whitespace();
        let off = words.next() == Some("off");
        let mode_gen = words.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        // The reading is logged so a test can wait until the supervisor has
        // seen a mode, rather than sleep and hope it has.
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.fixture.join("mode.reads"))
        {
            use std::io::Write;
            let _ = writeln!(log, "{} {mode_gen}", if off { "off" } else { "on" });
        }
        ModeSnapshot { off, mode_gen }
    }
    fn own_version(&self) -> String {
        A.to_owned()
    }
    fn own_sha256(&self) -> Option<String> {
        Some(self.sha.clone())
    }
    fn waiting_for_update_lock(&self, _waited: Duration) {}
}

/// Appends one JSON line per fact to `driver.log`.
struct DriverLog {
    file: Mutex<std::fs::File>,
    pids: Mutex<Vec<u32>>,
    spawned: Mutex<usize>,
}

impl DriverLog {
    fn line(&self, mut value: Value) {
        use std::io::Write;
        value["at"] = json!(now_ms());
        let mut text = value.to_string();
        text.push('\n');
        let _ = self.file.lock().unwrap().write_all(text.as_bytes());
    }
    fn alive_now(&self) -> Vec<u32> {
        self.pids
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|pid| alive(*pid))
            .collect()
    }
}

/// The real driver, logging every spawn and every command sent to a worker,
/// each with the workers still running at that moment.
struct Logged {
    inner: ProcessDriver,
    log: Arc<DriverLog>,
}

impl Driver for Logged {
    fn spawn(
        &self,
        request: SpawnRequest,
        events: EventSender,
    ) -> impl std::future::Future<Output = Result<WorkerPort, SpawnError>> + Send {
        let inner = self.inner.clone();
        let log = self.log.clone();
        async move {
            let role = format!("{:?}", request.role).to_lowercase();
            let version = request.version.clone();
            let binary = request.binary.display().to_string();
            let sha = request.sha256.clone();
            let before = log.alive_now();
            match inner.spawn(request, events).await {
                Ok(mut port) => {
                    let pid = port.identity.pid;
                    let number = {
                        let mut count = log.spawned.lock().unwrap();
                        *count += 1;
                        *count
                    };
                    log.pids.lock().unwrap().push(pid);
                    log.line(json!({
                        "k": "spawn", "n": number, "pid": pid, "role": role,
                        "version": version, "binary": binary, "sha": sha, "alive": before,
                    }));
                    let real = port.cmd.clone();
                    let (cmd, mut commands) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
                    port.cmd = cmd;
                    tokio::spawn(async move {
                        while let Some(command) = commands.recv().await {
                            let name = match &command {
                                Cmd::Send(message) => format!(
                                    "send:{}",
                                    serde_json::to_value(message)
                                        .ok()
                                        .and_then(|v| v["type"].as_str().map(str::to_owned))
                                        .unwrap_or_default()
                                ),
                                Cmd::Close => "close".to_owned(),
                                Cmd::Kill => "kill".to_owned(),
                            };
                            log.line(json!({
                                "k": "cmd", "pid": pid, "cmd": name, "alive": log.alive_now(),
                            }));
                            let _ = real.send(command);
                        }
                    });
                    Ok(port)
                }
                Err(error) => {
                    log.line(json!({
                        "k": "spawn_failed", "version": version, "binary": binary,
                        "error": error.to_string(), "alive": before,
                    }));
                    Err(error)
                }
            }
        }
    }
}

fn ms(spec: &Value, key: &str, default: u64) -> Duration {
    Duration::from_millis(spec["timings"][key].as_u64().unwrap_or(default))
}

async fn child_main(fixture: PathBuf) -> String {
    let spec: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("child.json")).expect("child.json"))
            .expect("child.json parses");
    let versions: HashMap<String, String> = spec["versions"]
        .as_object()
        .expect("versions")
        .iter()
        .map(|(sha, version)| (sha.clone(), version.as_str().unwrap().to_owned()))
        .collect();
    let defaults = Timings::default();
    let timings = Timings {
        load: ms(&spec, "load", defaults.load.as_millis() as u64),
        active_load: ms(
            &spec,
            "active_load",
            defaults.active_load.as_millis() as u64,
        ),
        handover: ms(&spec, "handover", defaults.handover.as_millis() as u64),
        ready: ms(&spec, "ready", defaults.ready.as_millis() as u64),
        resume: ms(&spec, "resume", defaults.resume.as_millis() as u64),
        watch: ms(&spec, "watch", defaults.watch.as_millis() as u64),
        drain: ms(&spec, "drain", defaults.drain.as_millis() as u64),
        signin_wait: ms(
            &spec,
            "signin_wait",
            defaults.signin_wait.as_millis() as u64,
        ),
        stop: ms(&spec, "stop", defaults.stop.as_millis() as u64),
        pin_ttl: ms(&spec, "pin_ttl", defaults.pin_ttl.as_millis() as u64),
        status_every: ms(
            &spec,
            "status_every",
            defaults.status_every.as_millis() as u64,
        ),
        install_retry: ms(&spec, "install_retry", 500),
        install_retry_max: ms(&spec, "install_retry", 500),
        restart_first: ms(&spec, "restart_first", 50),
        restart_max: ms(&spec, "restart_max", 200),
        exit_poll: ms(&spec, "exit_poll", 20),
        ..defaults
    };
    let installed = fixture.join("bin").join(NAME);
    let cfg = Config {
        data: DataDir::at(data_in(&fixture)),
        installed: installed.clone(),
        config: fixture.join(spec["config"].as_str().unwrap_or("config.yaml")),
        kind: ServiceKind::User,
        timings,
        receipt: json!({}),
        binary_name: NAME.to_owned(),
        runner: Arc::new(MapRunner(versions)),
        detect_timeout: Duration::from_secs(5),
        #[cfg(feature = "blue-green-test")]
        test_endpoint_location: own_endpoint(&fixture),
    };
    #[cfg(windows)]
    let bind = || {
        open_ferry_supervisor::sockets::windows::bind(
            "127.0.0.1:0".parse().expect("address"),
            open_ferry_supervisor::sockets::windows::BindOptions::named_host(),
        )
        .expect("bind")
    };
    #[cfg(unix)]
    let bind = || TcpListener::bind("127.0.0.1:0").expect("bind");
    let (main, management) = (bind(), bind());
    std::fs::write(
        fixture.join("ports.json"),
        json!({
            "main": main.local_addr().unwrap().port(),
            "management": management.local_addr().unwrap().port(),
        })
        .to_string(),
    )
    .unwrap();
    let host = ChildHost {
        main,
        management,
        fixture: fixture.clone(),
        // The installed file's hash at the start, unless the test fixed the
        // supervisor's own hash: damaging the installed file later must not make
        // the damaged bytes the supervisor's own.
        sha: match spec["own_sha256"].as_str() {
            Some(sha) => sha.to_owned(),
            None => hash_file(&installed).expect("hash the installed file"),
        },
    };
    let driver = Logged {
        inner: ProcessDriver::new(ServiceKind::User, timings.exit_poll),
        log: Arc::new(DriverLog {
            file: Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(fixture.join("driver.log"))
                    .unwrap(),
            ),
            pids: Mutex::new(Vec::new()),
            // Numbering goes on across supervisors that share the data directory.
            spawned: Mutex::new(
                std::fs::read_to_string(fixture.join("driver.log"))
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| line.contains("\"k\":\"spawn\""))
                    .count(),
            ),
        }),
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // A test ends the start-up or the run the way a service stop does, by
    // creating `stop-early` in the fixture (`Fx::request_stop`).
    let stopper = tx.clone();
    let stop_file = fixture.join("stop-early");
    tokio::spawn(async move {
        while !stop_file.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = stopper.send(Command::Stop);
    });
    let exit = supervise(
        &cfg,
        &host,
        &driver,
        &SystemClock,
        tx.clone(),
        rx,
        default_limit(),
    )
    .await;
    format!("{exit:?}")
}

/// Writes the supervisor's warnings and errors to stderr, which is
/// `child.log`: without a subscriber the `tracing` macros go nowhere, and a
/// test waits on these lines as barriers (a lock wait's warning).
struct WarningsToStderr;

impl tracing::Subscriber for WarningsToStderr {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        eprintln!("{} {}", event.metadata().level(), message.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The entry point of the supervisor child. Only meaningful with
/// `OFP_PT_FIXTURE` set, which `Fx::start` does.
#[test]
#[ignore = "subprocess entry point, driven only with a temporary fixture"]
fn supervisor_child() {
    let Some(fixture) = std::env::var_os("OFP_PT_FIXTURE") else {
        return;
    };
    #[cfg(unix)]
    watch_test_parent();
    let _ = tracing::subscriber::set_global_default(WarningsToStderr);
    let fixture = PathBuf::from(fixture);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let outcome = runtime.block_on(child_main(fixture.clone()));
    std::fs::write(fixture.join("child.exit"), outcome).unwrap();
}

/// A nextest timeout kills the test without running Sup::drop. This helper
/// leads its own group, outside nextest's Unix group, so watch the original
/// parent and end our entire group when reparented. Workers never setsid.
#[cfg(unix)]
fn watch_test_parent() {
    let parent: libc::pid_t = std::env::var("OFP_PT_PARENT")
        .expect("the spawning test's pid")
        .parse()
        .expect("a pid");
    std::thread::spawn(move || {
        loop {
            // SAFETY: getppid only queries this process's parent.
            if unsafe { libc::getppid() } != parent {
                // SAFETY: Fx::spawn made us the group leader; this group contains
                // only our helper and workers, and our live pid reserves its id.
                unsafe {
                    if libc::getpgrp() == libc::getpid() {
                        libc::killpg(libc::getpgrp(), libc::SIGKILL);
                    }
                }
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}

/// The name the test harness lists `supervisor_child` under, which the parent
/// passes with `--exact`. This module is the crate root's `under_test` when a
/// test file declares it, and a deeper module when another crate includes the
/// test file with `#[path]`, so the name is the module's own path without the
/// crate's name.
fn child_test_name() -> String {
    let path = module_path!();
    let inside = path.split_once("::").map_or(path, |(_, rest)| rest);
    format!("{inside}::supervisor_child")
}

// ---- the parent -----------------------------------------------------------

/// Plain cargo test shares descriptors: on Unix an unrelated fork can inherit
/// an executable write descriptor (ETXTBSY) or keep a lock alive. Serialize
/// these tests there; Windows permits four. Nextest gives each test its own
/// process, with concurrency bounded by the configured process test group.
struct Gate {
    taken: Mutex<usize>,
    free: Condvar,
}

fn gate() -> &'static Gate {
    static GATE: OnceLock<Gate> = OnceLock::new();
    GATE.get_or_init(|| Gate {
        taken: Mutex::new(0),
        free: Condvar::new(),
    })
}

pub struct Permit;

impl Permit {
    pub fn take() -> Permit {
        let limit = if cfg!(windows) { 4 } else { 1 };
        let mut taken = gate().taken.lock().unwrap();
        while *taken >= limit {
            taken = gate().free.wait(taken).unwrap();
        }
        *taken += 1;
        Permit
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        *gate().taken.lock().unwrap() -= 1;
        gate().free.notify_one();
    }
}

/// A fresh directory under a path the data directory's trust check accepts
/// (the target directory on Windows, or `OFP_TEST_SCRATCH` when set: CI's
/// runner, whose drive root is owned by NETWORK SERVICE; `$HOME` elsewhere).
pub fn scratch() -> tempfile::TempDir {
    if cfg!(windows) {
        match std::env::var_os("OFP_TEST_SCRATCH") {
            Some(root) => tempfile::tempdir_in(root).unwrap(),
            None => tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
        }
    } else {
        tempfile::tempdir_in(std::env::var_os("HOME").expect("HOME is set")).unwrap()
    }
}

// ---- isolation ------------------------------------------------------------

/// A loopback TCP port that is bound but not listening, held for the life of
/// the test process: a connect to it is refused at once and reaches nothing.
/// Update checks are pointed here (`isolate`).
pub fn sink_port() -> u16 {
    static SINK: OnceLock<(socket2::Socket, u16)> = OnceLock::new();
    SINK.get_or_init(|| {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        socket.bind(&loopback.into()).expect("bind the sink");
        let port = socket
            .local_addr()
            .unwrap()
            .as_socket()
            .expect("an inet address")
            .port();
        (socket, port)
    })
    .1
}

/// Whether the variable `name` may not reach a child: one that identifies a
/// client or an account (ANTHROPIC*, CLAUDE*, OPENAI*, CODEX*), a proxy, or
/// one that overrides open-ferry's update or management settings.
fn withheld(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["ANTHROPIC", "CLAUDE", "OPENAI", "CODEX", "OPEN_FERRY_"]
        .iter()
        .any(|prefix| upper.starts_with(prefix))
        || upper.ends_with("_PROXY")
        || upper == "PROXY"
        || upper == "MANAGEMENT_PASSWORD"
        || upper == "WRITABLE_PATH"
}

/// Makes `command` run isolated: none of the variables `withheld` names
/// reaches it, and its update checks and installers go to the sink. A test
/// that needs one of them sets it after this, on purpose.
pub fn isolate(command: &mut std::process::Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(withheld) {
            command.env_remove(&name);
        }
    }
    let sink = format!("http://127.0.0.1:{}/releases", sink_port());
    command
        .env("OPEN_FERRY_UPDATE_BASE_URL", &sink)
        .env("OPEN_FERRY_INSTALL_BASE_URL", &sink);
}

/// A minimal valid open-ferry config for the real worker: no providers, the
/// auth directory under `dir`, `127.0.0.1` only, on `port` (0 for none to
/// name), with the self-update `mode` and the checks a month apart.
pub fn config_text(dir: &Path, port: u16, mode: &str) -> String {
    let auth = dir.join("auth");
    std::fs::create_dir_all(&auth).unwrap();
    format!(
        "host: 127.0.0.1\nport: {port}\nauth-dir: {}\nself-update:\n  mode: {mode}\n  check-every: 720h\n",
        yaml_single_quoted(&auth.display().to_string())
    )
}

/// `text` as a single-quoted YAML scalar: backslashes stay as they are and
/// each `'` is doubled.
pub fn yaml_single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Where the fixture's supervisor binds its endpoint, and where the fixture connects.
///
/// On Unix `None` would be the per-user socket, which a real supervisor of this user may
/// hold; a fixture that reached it would be answered by the wrong supervisor. So the
/// fixture has its own, `<fixture>/e/supervisor.sock`, and `open` never resolves the
/// default. A socket path is limited to 104 bytes on macOS and 108 on Linux; a longer one
/// fails the set-up (no fallback).
/// On Windows the pipe is derived from the data directory
/// (`Location::resolve`, src/endpoint/windows.rs:103), so each fixture has its own and
/// nothing needs injecting.
#[cfg(unix)]
fn own_endpoint(fixture: &Path) -> Option<Location> {
    let root = std::fs::canonicalize(fixture).expect("canonical fixture root");
    let location = Location::new(&root, "e").expect("the fixture's endpoint location");
    let socket = location.socket_path();
    assert!(
        socket.as_os_str().len() <= 103,
        "SET-UP: the fixture's socket path is {} bytes, over the product limit of 103: {}",
        socket.as_os_str().len(),
        socket.display()
    );
    Some(location)
}

#[cfg(windows)]
fn own_endpoint(_fixture: &Path) -> Option<Location> {
    None
}

/// The location a client of the fixture connects to: the fixture's own on Unix, the
/// data directory's pipe on Windows.
pub fn client_location(fixture: &Path) -> io::Result<Location> {
    if let Some(own) = own_endpoint(fixture) {
        return Ok(own);
    }
    let identity = Identity::current(ServiceKind::User)?;
    Location::resolve(&identity, &data_in(fixture)).map_err(io::Error::other)
}

/// The fixture's data directory: `open-ferry` in the fixture when the fixture
/// was made by `Fx::new_for_this_user_layout` (so a child with `LOCALAPPDATA`
/// or `XDG_DATA_HOME` at the fixture finds it as the user's data directory),
/// otherwise `data`.
pub fn data_in(fixture: &Path) -> PathBuf {
    let own = fixture.join("open-ferry");
    if own.is_dir() {
        own
    } else {
        fixture.join("data")
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

/// The timings a supervisor under test runs with, in milliseconds. Defaults
/// are generous (nothing here is a ceiling a loaded machine can break); a test
/// that needs a limit to expire shortens that one.
#[derive(Clone, Debug)]
pub struct Tm {
    pub load: u64,
    /// The wait for an active worker's `loaded` after its handshake.
    pub active_load: u64,
    pub handover: u64,
    pub ready: u64,
    pub resume: u64,
    pub watch: u64,
    pub drain: u64,
    pub signin_wait: u64,
    pub stop: u64,
    pub status_every: u64,
    pub install_retry: u64,
    /// The wait after the first failed start of the active worker, and the
    /// most it grows to (it doubles each time).
    pub restart_first: u64,
    pub restart_max: u64,
}

impl Default for Tm {
    fn default() -> Self {
        Tm {
            load: 30_000,
            active_load: 180_000,
            handover: 30_000,
            ready: 30_000,
            resume: 15_000,
            watch: 1_500,
            drain: 10_000,
            signin_wait: 2_000,
            stop: 10_000,
            status_every: 200,
            install_retry: 500,
            restart_first: 50,
            restart_max: 200,
        }
    }
}

impl Tm {
    fn json(&self) -> Value {
        json!({
            "load": self.load, "active_load": self.active_load, "handover": self.handover, "ready": self.ready,
            "resume": self.resume, "watch": self.watch, "drain": self.drain,
            "signin_wait": self.signin_wait, "stop": self.stop,
            "status_every": self.status_every, "install_retry": self.install_retry,
            "restart_first": self.restart_first, "restart_max": self.restart_max,
        })
    }
}

/// One worker event line.
#[derive(Clone, Debug, PartialEq)]
pub struct Ev {
    pub pid: u32,
    pub marker: String,
    pub event: String,
    pub lease: bool,
}

/// Why the fixture does not trust its endpoint (`Fx::identity`).
#[derive(Debug)]
pub enum Refusal {
    /// No record of the child's own yet: poll again, trust nothing.
    Wait(String),
    /// Another supervisor may be answering.
    Foreign(String),
}

impl Refusal {
    pub fn why(&self) -> &str {
        match self {
            Refusal::Wait(why) | Refusal::Foreign(why) => why,
        }
    }
}

/// The supervisor child, ended by its handle when this drops.
pub struct Sup {
    child: Option<std::process::Child>,
    /// The exit status seen without reaping (Unix), so a later look agrees.
    #[cfg(unix)]
    seen: Option<std::process::ExitStatus>,
    #[cfg_attr(not(unix), allow(dead_code))]
    root: PathBuf,
}

/// Looks at a child's exit without reaping it: `waitid` with `WNOWAIT` leaves
/// the zombie in place, so its pid, and the process group id it leads, stay
/// reserved. `std`'s `try_wait` reaps, which is why it is not used on Unix.
#[cfg(unix)]
fn peek_exit(pid: u32) -> Option<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid with WNOWAIT only reads the state of our own child.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // SAFETY: reading the fields waitid just filled in.
    let (child, code, status) = unsafe { (info.si_pid(), info.si_code, info.si_status()) };
    if rc != 0 || child == 0 {
        return None;
    }
    // Back to the raw wait status that `ExitStatus` is made of.
    let raw = if code == libc::CLD_EXITED {
        (status & 0xff) << 8
    } else if code == libc::CLD_DUMPED {
        (status & 0x7f) | 0x80
    } else {
        status & 0x7f
    };
    Some(std::process::ExitStatus::from_raw(raw))
}

impl Sup {
    /// Takes a supervisor the test started itself (a real `open-ferry -supervise`)
    /// under the guard that ends it by its handle. On Unix the child must lead
    /// its own process group (`process_group(0)`), as `Fx::spawn`'s does.
    pub fn adopt(child: std::process::Child, root: &Path) -> Sup {
        Sup {
            child: Some(child),
            #[cfg(unix)]
            seen: None,
            root: root.to_owned(),
        }
    }
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    /// The exit status once it has exited. On Unix this never reaps: the
    /// zombie keeps the pid, and so the group id, until the guard drops.
    pub fn try_exit(&mut self) -> Option<std::process::ExitStatus> {
        #[cfg(unix)]
        {
            if self.seen.is_none() {
                self.seen = peek_exit(self.child.as_ref().unwrap().id());
            }
            self.seen
        }
        #[cfg(not(unix))]
        {
            self.child.as_mut().unwrap().try_wait().unwrap()
        }
    }
    /// Ends it by its handle. Nothing runs inside it afterwards: no stop, no
    /// cleanup of its own. On Unix it is not reaped here (see `try_exit`).
    pub fn kill(&mut self) {
        let _ = self.child.as_mut().unwrap().kill();
        let until = Instant::now() + Duration::from_secs(10);
        while self.try_exit().is_none() && Instant::now() < until {
            std::thread::sleep(POLL);
        }
        #[cfg(not(unix))]
        let _ = self.child.as_mut().unwrap().wait();
    }
}

impl Drop for Sup {
    /// Ends the supervisor and, on Unix, its whole process group. The child
    /// leads its own group (see `Fx::spawn`) and the supervisor starts its
    /// workers without `setsid`, so they stay in it. A `drop-channel` fake never
    /// sees EOF and would otherwise sleep for 600 s holding its lease. The wait
    /// for the workers to end is bounded. A worker in another group is not
    /// reached; on Windows the supervisor's job object ends the workers when it
    /// dies.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                // INVARIANT: this child has not been reaped. `kill`, `try_exit`
                // and `start` only peek (`peek_exit`), so the leader is at
                // worst a zombie, and a zombie keeps its pid, which is the
                // group id. Nothing can have taken that id, so this signals
                // our own group and no other. Reap only after the signal.
                let group = child.id() as libc::pid_t;
                // SAFETY: killpg only signals; see the invariant above.
                unsafe { libc::killpg(group, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                let until = Instant::now() + Duration::from_secs(10);
                let pids = events_pids(&self.root);
                while pids.iter().any(|&pid| alive(pid)) && Instant::now() < until {
                    std::thread::sleep(POLL);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// The pids of every worker that logged an event in the fixture at `root`.
#[cfg(unix)]
fn events_pids(root: &Path) -> Vec<u32> {
    let text = std::fs::read_to_string(data_in(root).join(".blue-green-test").join("events.log"))
        .unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
        .collect()
}

/// What a switch request answered, as it came.
pub struct Sw {
    rx: Receiver<EndpointReply>,
    pub seen: Vec<EndpointReply>,
}

impl Sw {
    fn pump(&mut self, until: Instant) -> Option<EndpointReply> {
        let wait = until.saturating_duration_since(Instant::now());
        let reply = self.rx.recv_timeout(wait).ok()?;
        self.seen.push(reply.clone());
        Some(reply)
    }

    /// The operation id once `accepted` arrives.
    pub fn accepted(&mut self) -> String {
        let until = Instant::now() + LONG;
        loop {
            if let Some(EndpointReply::Accepted { id, .. }) = self
                .seen
                .iter()
                .find(|reply| matches!(reply, EndpointReply::Accepted { .. }))
            {
                return id.clone();
            }
            match self.pump(until) {
                Some(EndpointReply::Error { reason, .. }) => panic!("the switch failed: {reason}"),
                Some(_) => {}
                None => panic!("no accepted reply: {:?}", self.seen),
            }
        }
    }

    /// The `done` result, or the reason of an `error` reply.
    pub fn done(&mut self) -> Result<Value, String> {
        let until = Instant::now() + LONG;
        loop {
            for reply in &self.seen {
                match reply {
                    EndpointReply::Done { result, .. } => return Ok(result.clone()),
                    EndpointReply::Error { reason, .. } => return Err(reason.clone()),
                    _ => {}
                }
            }
            if self.pump(until).is_none() {
                panic!("no done reply: {:?}", self.seen);
            }
        }
    }

    /// Waits for a `progress` reply of `phase`.
    pub fn progress(&mut self, phase: &str) {
        let until = Instant::now() + LONG;
        loop {
            if self.seen.iter().any(
                |reply| matches!(reply, EndpointReply::Progress { phase: p, .. } if p == phase),
            ) {
                return;
            }
            if self.pump(until).is_none() {
                panic!("no progress {phase}: {:?}", self.seen);
            }
        }
    }
}

/// One test's directory: the data directory, the installed file, and the
/// workers' versions, with the helpers to drive and read them.
pub struct Fx {
    pub dir: tempfile::TempDir,
    /// sha256 to version, for the runner.
    shas: Mutex<HashMap<String, String>>,
    /// The config the workers are given, relative to the fixture.
    config: Mutex<String>,
    /// The hash the supervisor child takes as its own, when the test fixes it
    /// (`keep_own_hash`); otherwise the child hashes the installed file at its start.
    own_sha: Mutex<Option<String>>,
    /// The pid of the supervisor child spawned last.
    child: Mutex<Option<u32>>,
    _permit: Permit,
}

/// A staged version.
#[derive(Clone, Debug)]
pub struct Staged {
    pub version: String,
    pub sha: String,
}

impl Fx {
    /// A fixture with `1.0.0` (A) installed and kept, marker `A`.
    pub fn new() -> Fx {
        Fx::build(false)
    }

    /// As `new`, but the data directory is `<root>/open-ferry`, which is where
    /// a child with `LOCALAPPDATA` and `XDG_DATA_HOME` set to `root()` looks
    /// for the user's data directory.
    pub fn new_for_this_user_layout() -> Fx {
        Fx::build(true)
    }

    fn build(user_layout: bool) -> Fx {
        let permit = Permit::take();
        let dir = scratch();
        let fx = Fx {
            dir,
            shas: Mutex::new(HashMap::new()),
            config: Mutex::new("config.yaml".to_owned()),
            own_sha: Mutex::new(None),
            child: Mutex::new(None),
            _permit: permit,
        };
        std::fs::create_dir_all(fx.root().join("bin")).unwrap();
        if user_layout {
            // Before any `data()`: the directory's existence names it.
            std::fs::create_dir_all(fx.root().join("open-ferry")).unwrap();
        }
        std::fs::create_dir_all(fx.data().join(".blue-green-test")).unwrap();
        // The fake ignores its config; the real worker reads one.
        let config = match kind() {
            Kind::Fake => String::new(),
            Kind::Real => config_text(fx.root(), 0, "auto"),
        };
        std::fs::write(fx.root().join("config.yaml"), config).unwrap();
        write_executable(&fx.installed(), &artifact_bytes(A));
        fx.stage(A, "A");
        fx
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }
    pub fn data(&self) -> PathBuf {
        data_in(self.root())
    }
    pub fn installed(&self) -> PathBuf {
        self.root().join("bin").join(NAME)
    }
    /// Where `version` is kept (`versions/<version>/`): the location a
    /// kept-copy rung runs from.
    pub fn kept(&self, version: &str) -> PathBuf {
        self.data_dir().binary(version, NAME)
    }
    pub fn seams(&self) -> PathBuf {
        self.data().join(".blue-green-test")
    }
    fn data_dir(&self) -> DataDir {
        DataDir::at(self.data())
    }

    /// Keeps `version` under `versions/`: the worker binary with the version's
    /// trailer, and the `test-marker` file beside it. Call before `start`:
    /// nothing is written while a supervisor forks.
    pub fn stage(&self, version: &str, marker: &str) -> Staged {
        let binary = self.data_dir().binary(version, NAME);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        write_executable(&binary, &artifact_bytes(version));
        std::fs::write(binary.parent().unwrap().join("test-marker"), marker).unwrap();
        let sha = hash_file(&binary).unwrap();
        self.shas
            .lock()
            .unwrap()
            .insert(sha.clone(), version.to_owned());
        Staged {
            version: version.to_owned(),
            sha,
        }
    }

    /// Keeps `version` as a `worker_probe` in `mode` (see its doc comment):
    /// the workers' config becomes the directory it reads the mode from.
    pub fn stage_probe(&self, version: &str, mode: &str) -> Staged {
        let binary = self.data_dir().binary(version, NAME);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        write_executable(&binary, &probe_bytes(version));
        std::fs::create_dir_all(self.root().join("cfg")).unwrap();
        std::fs::write(self.root().join("cfg").join("mode"), mode).unwrap();
        *self.config.lock().unwrap() = "cfg".to_owned();
        let sha = hash_file(&binary).unwrap();
        self.shas
            .lock()
            .unwrap()
            .insert(sha.clone(), version.to_owned());
        Staged {
            version: version.to_owned(),
            sha,
        }
    }

    /// Changes one byte of the kept artifact of `version`, so its hash no
    /// longer matches what the journal holds.
    pub fn corrupt(&self, version: &str) {
        let binary = self.data_dir().binary(version, NAME);
        let mut bytes = std::fs::read(&binary).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&binary, bytes).unwrap();
    }

    /// The same for the installed file: its bytes no longer match any version.
    /// Call when no worker runs from it.
    pub fn corrupt_installed(&self) {
        let binary = self.installed();
        let mut bytes = std::fs::read(&binary).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&binary, bytes).unwrap();
    }

    /// Fixes the hash every supervisor child started from now on takes as its
    /// own to the installed file's hash now. Call while the installed file is whole.
    pub fn keep_own_hash(&self) {
        *self.own_sha.lock().unwrap() = Some(hash_file(&self.installed()).unwrap());
    }

    pub fn sha(&self, version: &str) -> String {
        hash_file(&self.data_dir().binary(version, NAME)).unwrap()
    }

    /// A pin for `version` as the CLI makes it before a switch.
    pub fn pin(&self, version: &str) -> String {
        open_ferry_update::pins::PinStore::new(&self.data_dir().versions())
            .create(version, &self.sha(version), chrono::Utc::now())
            .expect("a pin")
            .id
    }

    // ---- faults and barriers ----------------------------------------------

    fn seam_file(&self, name: &str, ext: &str) -> PathBuf {
        self.seams()
            .join(format!("{}{ext}", name.replace(':', "_")))
    }

    /// Sets a worker fault: `fault("fail-load", "@B")`, `fault("exit-after", "go@B")`.
    pub fn fault(&self, kind: &str, value: &str) {
        std::fs::write(self.seams().join(format!("fault.{kind}")), value).unwrap();
    }
    pub fn clear_fault(&self, kind: &str) {
        let _ = std::fs::remove_file(self.seams().join(format!("fault.{kind}")));
    }
    /// Makes a seam fail, such as `journal.fail:J3` or `state.fail:S1`.
    pub fn fail(&self, name: &str) {
        std::fs::write(self.seam_file(name, ".fail"), b"").unwrap();
    }
    pub fn unfail(&self, name: &str) {
        let _ = std::fs::remove_file(self.seam_file(name, ".fail"));
    }
    /// How many times a failing seam fired.
    pub fn failed(&self, name: &str) -> u64 {
        std::fs::metadata(self.seam_file(name, ".failed")).map_or(0, |m| m.len())
    }
    /// Makes a barrier block when it is reached.
    pub fn hold(&self, name: &str) {
        std::fs::write(self.seam_file(name, ".hold"), b"").unwrap();
    }
    pub fn arrived(&self, name: &str) -> bool {
        self.seam_file(name, ".arrived").exists()
    }
    /// Waits for a barrier to be reached.
    pub fn wait_arrived(&self, name: &str) {
        self.wait_for(&format!("barrier {name} reached"), || {
            self.arrived(name).then_some(())
        });
    }
    pub fn release(&self, name: &str) {
        std::fs::write(self.seam_file(name, ".release"), b"").unwrap();
    }

    /// Arms a barrier that has been released before, so its next arrival holds
    /// again (a hold is taken once: the release file stays).
    pub fn rehold(&self, name: &str) {
        for ext in [".arrived", ".release"] {
            let _ = std::fs::remove_file(self.seam_file(name, ext));
        }
        self.hold(name);
    }

    /// Takes the workers' lease (`active-worker.lock`) as a second process
    /// would; the lease is free again when the returned file drops. Panics when
    /// a worker holds it.
    pub fn hold_lease(&self) -> std::fs::File {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.data().join("active-worker.lock"))
            .unwrap();
        file.try_lock().expect("the lease is free");
        file
    }

    /// The commands the supervisor sent to worker `n`, in order.
    pub fn sent_to(&self, n: usize) -> Vec<String> {
        let Some(pid) = self.pid_of(n) else {
            return Vec::new();
        };
        self.driver_log()
            .iter()
            .filter(|v| v["k"] == "cmd" && v["pid"] == pid)
            .map(|v| v["cmd"].as_str().unwrap().to_owned())
            .collect()
    }

    // ---- the supervisor ---------------------------------------------------

    /// Writes the host's mode file.
    pub fn mode(&self, off: bool, mode_gen: u64) {
        let path = self.root().join("host-mode");
        let temporary = self.root().join("host-mode.tmp");
        std::fs::write(
            &temporary,
            format!("{} {mode_gen}", if off { "off" } else { "on" }),
        )
        .unwrap();
        std::fs::rename(temporary, path).unwrap();
    }

    /// Waits until the supervisor has read the mode `off`/`on` with `mode_gen`
    /// from the host file (a reading it acted on, or will, before any later).
    pub fn wait_mode_read(&self, off: bool, mode_gen: u64) {
        let want = format!("{} {mode_gen}", if off { "off" } else { "on" });
        self.wait_for(&format!("the supervisor to read mode {want}"), || {
            std::fs::read_to_string(self.root().join("mode.reads"))
                .ok()?
                .lines()
                .any(|line| line == want)
                .then_some(())
        });
    }

    fn write_child(&self, timings: &Tm) {
        let versions: HashMap<String, String> = self.shas.lock().unwrap().clone();
        std::fs::write(
            self.root().join("child.json"),
            json!({
                "timings": timings.json(), "versions": versions,
                "config": *self.config.lock().unwrap(),
                "own_sha256": *self.own_sha.lock().unwrap(),
            })
            .to_string(),
        )
        .unwrap();
    }

    /// Starts a supervisor on this data directory without waiting for it.
    pub fn spawn(&self, timings: &Tm) -> Sup {
        self.write_child(timings);
        let output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("child.log"))
            .unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &child_test_name(), "--ignored", "--nocapture"])
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        // The fixture is its working directory, and the workers inherit it:
        // the logs directory is picked as the first writable `logs` in the
        // working directory, so from the package root a stray
        // `crates/open-ferry/logs` would take the logs and the usage ledger.
        command.current_dir(self.root());
        // The workers inherit what it has.
        isolate(&mut command);
        command.env("OFP_PT_FIXTURE", self.root());
        #[cfg(unix)]
        command.env("OFP_PT_PARENT", std::process::id().to_string());
        // Its own process group, so that `Sup`'s drop can end the workers too.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().expect("start the supervisor child");
        *self.child.lock().unwrap() = Some(child.id());
        Sup {
            child: Some(child),
            #[cfg(unix)]
            seen: None,
            root: self.root().to_owned(),
        }
    }

    /// Starts a supervisor and waits until its active worker runs.
    pub fn start(&self, timings: &Tm) -> Sup {
        let mut sup = self.spawn(timings);
        self.wait_for("the supervisor's active worker", || {
            if let Some(status) = sup.try_exit() {
                panic!("the supervisor exited early ({status})\n{}", self.dump());
            }
            let status = self.status()?;
            (status["state"] == "idle"
                && status["workers"]
                    .as_array()
                    .is_some_and(|w| w.iter().any(|w| w["role"] == "active")))
            .then_some(())
        });
        sup
    }

    /// The fixture's own `supervisor.json` must name the location the client uses and the
    /// child the fixture started, or whatever answers is not the fixture's supervisor.
    /// Only a record that does is a verdict of `Ok`. A record that is absent, unreadable,
    /// malformed, or names a process that is not alive and not the child (the one a
    /// restart has not yet replaced) is `Wait`: the caller may poll again, but must not
    /// trust any answer. A live process that is not the child, or another endpoint, is
    /// `Foreign`: another supervisor may be answering.
    pub fn identity(&self) -> Result<(), Refusal> {
        let wait = |why: String| Err(Refusal::Wait(why));
        let Some(child) = *self.child.lock().unwrap() else {
            return wait("no supervisor has been started".to_owned());
        };
        let path = self.data().join("supervisor.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => return wait(format!("{} is not readable: {error}", path.display())),
        };
        let Ok(record) = serde_json::from_str::<Value>(&text) else {
            return wait(format!("{} is not JSON: {text:?}", path.display()));
        };
        let want = client_location(self.root())
            .expect("the client's location")
            .to_record();
        let Some(endpoint) = record["endpoint"].as_str() else {
            return wait(format!("{} names no endpoint: {text:?}", path.display()));
        };
        if endpoint != want {
            return Err(Refusal::Foreign(format!(
                "SET-UP: supervisor.json names the endpoint {endpoint:?}, not the one the fixture connects to ({want:?}); another supervisor may be answering"
            )));
        }
        // Compared as read: a pid that does not fit a `u32` is no process's and
        // truncating it could make it the child's. A pid that is missing or not a
        // number is refused, not waited for.
        let Some(named) = record["pid"].as_u64() else {
            return Err(Refusal::Foreign(format!(
                "SET-UP: supervisor.json names no usable pid: {text:?}"
            )));
        };
        if named == u64::from(child) {
            return Ok(());
        }
        let Ok(pid) = u32::try_from(named) else {
            return Err(Refusal::Foreign(format!(
                "SET-UP: supervisor.json names pid {named}, which is out of range, not the child ({child})"
            )));
        };
        if alive(pid) {
            return Err(Refusal::Foreign(format!(
                "SET-UP: supervisor.json names pid {pid}, a live process, not the child the fixture started ({child}); another supervisor may be answering"
            )));
        }
        wait(format!(
            "supervisor.json names pid {pid}, not the child ({child}): a record left by a supervisor that has gone"
        ))
    }

    /// `identity`, as an assertion: anything but the child's own record panics.
    #[track_caller]
    pub fn check_the_supervisor_is_ours(&self) {
        if let Err(refusal) = self.identity() {
            panic!("{}", refusal.why());
        }
    }

    /// Waits, bounded, for the child's own record; a foreign one panics at once.
    fn wait_ours(&self) {
        self.wait_for(
            "supervisor.json to name the child and the client's endpoint",
            || match self.identity() {
                Ok(()) => Some(()),
                Err(Refusal::Wait(_)) => None,
                Err(foreign) => panic!("{}", foreign.why()),
            },
        );
    }

    /// Tells the supervisor child to stop, as a service stop does: it ends
    /// its start-up wait, or its run (`stop-early`, read by `child_main`).
    pub fn request_stop(&self) {
        std::fs::write(self.root().join("stop-early"), "").unwrap();
    }

    /// What the supervisor child's run ended with, once it has (`child.exit`).
    pub fn child_exit(&self) -> Option<String> {
        std::fs::read_to_string(self.root().join("child.exit")).ok()
    }

    /// The supervisor child's stderr so far: its warnings and errors.
    pub fn child_log(&self) -> String {
        std::fs::read_to_string(self.root().join("child.log")).unwrap_or_default()
    }

    /// Waits for the child supervisor to exit by itself.
    pub fn wait_exit(&self, sup: &mut Sup) -> std::process::ExitStatus {
        self.wait_for("the supervisor to exit", || sup.try_exit())
    }

    // ---- the endpoint -----------------------------------------------------

    async fn open(
        root: &Path,
    ) -> io::Result<Channel<impl tokio::io::AsyncRead, impl tokio::io::AsyncWrite>> {
        let identity = Identity::current(ServiceKind::User)?;
        let location = client_location(root)?;
        let stream = connect(&location, &identity)
            .await
            .map_err(io::Error::other)?;
        let (reader, writer) = tokio::io::split(stream);
        Ok(Channel::new(reader, writer))
    }

    fn ask(&self, request: EndpointRequest) -> Option<EndpointReply> {
        match self.identity() {
            Ok(()) => {}
            // Not the child's record (yet): nothing is asked, so nothing is trusted.
            Err(Refusal::Wait(_)) => return None,
            Err(foreign) => panic!("{}", foreign.why()),
        }
        endpoint_ask(self.root(), request)
    }

    /// The port of the main listener the child bound.
    fn main_port(&self) -> Option<u16> {
        self.port("main")
    }

    /// The port of the listener `name` ("main" or "management") the child bound.
    fn port(&self, name: &str) -> Option<u16> {
        let ports = std::fs::read_to_string(self.root().join("ports.json")).ok()?;
        Some(serde_json::from_str::<Value>(&ports).ok()?[name].as_u64()? as u16)
    }

    /// Opens a connection to the main listener and sends a request without its
    /// end. The fake worker that accepted it keeps it open until it is drained
    /// or stopped, which holds a drain open (L9.19). Dropping it closes it.
    pub fn hold_partial(&self) -> std::net::TcpStream {
        use std::io::Write;
        let port = self.main_port().expect("the main listener's port");
        let mut stream = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(5),
        )
        .expect("connect to the main listener");
        stream
            .write_all(
                b"GET / HTTP/1.1
Host: test
",
            )
            .expect("send a partial request");
        stream
    }

    /// Waits until the active worker counts at least `n` live connections.
    pub fn wait_live(&self, n: u64) {
        self.wait_for(
            &format!("{n} live connections on the active worker"),
            || {
                let status = self.status()?;
                status["workers"]
                    .as_array()?
                    .iter()
                    .any(|w| w["role"] == "active" && w["live"].as_u64().unwrap_or(0) >= n)
                    .then_some(())
            },
        );
    }

    /// One plain HTTP request to the main listener the child bound: the status
    /// code and the marker header of whichever worker answered. `None` when
    /// nothing answers within a few seconds.
    pub fn get(&self, path: &str) -> Option<(u16, Option<String>)> {
        self.get_on("main", path)
    }

    /// The same on the separate management listener. The fake worker answers a
    /// request there with 503 while it is `handing-over`, and with its own
    /// marker header and another status otherwise (L9.11: a real worker may
    /// answer 401 or 404 there, so callers must not insist on 200).
    pub fn get_management(&self, path: &str) -> Option<(u16, Option<String>)> {
        self.get_on("management", path)
    }

    fn get_on(&self, listener: &str, path: &str) -> Option<(u16, Option<String>)> {
        http_get(self.port(listener)?, path)
    }

    /// The status snapshot, `None` when the endpoint cannot be reached.
    pub fn status(&self) -> Option<Value> {
        match self.ask(EndpointRequest::Status { v: VERSION })? {
            EndpointReply::Status { snapshot, .. } => Some(snapshot),
            other => panic!("not a status: {other:?}"),
        }
    }

    pub fn cancel(&self) -> EndpointReply {
        self.ask(EndpointRequest::Cancel { v: VERSION })
            .expect("a cancel reply")
    }

    pub fn stop(&self) -> EndpointReply {
        self.ask(EndpointRequest::Stop { v: VERSION })
            .expect("a stop reply")
    }

    /// Asks for a switch to `to`, with its own pin and hash.
    pub fn switch(&self, to: &str, how: How, auto: bool, admitted: Admission) -> Sw {
        let pin = self.pin(to);
        self.switch_with(to, &self.sha(to), &pin, how, auto, admitted)
    }

    /// A manual update, admitted while the mode is on.
    pub fn update(&self, to: &str) -> Sw {
        self.switch(
            to,
            How::Update,
            false,
            Admission {
                while_off: false,
                mode_gen: 0,
            },
        )
    }

    pub fn rollback(&self, to: &str) -> Sw {
        self.switch(
            to,
            How::Rollback,
            false,
            Admission {
                while_off: false,
                mode_gen: 0,
            },
        )
    }

    pub fn switch_with(
        &self,
        to: &str,
        sha: &str,
        pin: &str,
        how: How,
        auto: bool,
        admitted: Admission,
    ) -> Sw {
        let request = EndpointRequest::Switch {
            v: VERSION,
            to: to.to_owned(),
            sha256: sha.to_owned(),
            pin: pin.to_owned(),
            how,
            auto,
            admitted,
        };
        let (tx, rx) = channel();
        let root = self.root().to_owned();
        self.wait_ours();
        runtime().spawn(async move {
            let mut channel = match Fx::open(&root).await {
                Ok(channel) => channel,
                Err(error) => {
                    let _ = tx.send(EndpointReply::Error {
                        v: VERSION,
                        reason: format!("connect: {error}"),
                    });
                    return;
                }
            };
            if channel.send(&request).await.is_err() {
                return;
            }
            while let Ok(Some(frame)) = channel.receive::<EndpointReply>().await {
                if tx.send(frame.message).is_err() {
                    return;
                }
            }
        });
        Sw {
            rx,
            seen: Vec::new(),
        }
    }

    // ---- reading what happened --------------------------------------------

    /// The workers' event log.
    pub fn events(&self) -> Vec<Ev> {
        let text = std::fs::read_to_string(self.seams().join("events.log")).unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|v| Ev {
                pid: v["pid"].as_u64().unwrap_or(0) as u32,
                marker: v["marker"].as_str().unwrap_or("").to_owned(),
                event: v["event"].as_str().unwrap_or("").to_owned(),
                lease: v["lease"].as_bool().unwrap_or(false),
            })
            .collect()
    }

    /// The pids of the grandchildren the workers started (the
    /// `spawn-grandchild` fault), in the order they were logged.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn grandchildren(&self) -> Vec<u32> {
        let text = std::fs::read_to_string(self.seams().join("events.log")).unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| v["event"] == "grandchild")
            .filter_map(|v| v["child_pid"].as_u64())
            .map(|pid| pid as u32)
            .collect()
    }

    /// Whether the worker of `marker` logged `event`.
    pub fn logged(&self, marker: &str, event: &str) -> bool {
        self.events()
            .iter()
            .any(|e| e.marker == marker && e.event == event)
    }

    pub fn count(&self, marker: &str, event: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e.marker == marker && e.event == event)
            .count()
    }

    /// The driver's log: one JSON value per line, in order.
    pub fn driver_log(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.root().join("driver.log")).unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The pid of the `n`th worker the supervisor started (1 is A).
    pub fn pid_of(&self, n: usize) -> Option<u32> {
        self.driver_log()
            .iter()
            .find(|v| v["k"] == "spawn" && v["n"] == n)
            .and_then(|v| v["pid"].as_u64())
            .map(|p| p as u32)
    }

    /// The journal file as JSON, `None` when there is none.
    pub fn journal(&self) -> Option<Value> {
        let bytes = std::fs::read(self.data().join("switch-journal.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn state(&self) -> open_ferry_update::state::State {
        open_ferry_update::state::State::load(&self.data_dir().state_file())
    }

    /// Puts an older switch's record in the state's `last_switch` and
    /// `history`, before any supervisor runs, so that a test can show the
    /// others are kept.
    pub fn seed_history(&self) -> open_ferry_update::state::SwitchRecord {
        let old = open_ferry_update::state::SwitchRecord {
            from: "0.9.0".to_owned(),
            to: A.to_owned(),
            at: "2026-01-01T00:00:00Z".to_owned(),
            how: "update".to_owned(),
            id: "old-operation".to_owned(),
            result: Some("switched".to_owned()),
            blue_green: true,
            confirmed: true,
            ..Default::default()
        };
        let path = self.data_dir().state_file();
        let mut state = open_ferry_update::state::State::load(&path);
        state.last_switch = Some(old.clone());
        state.history = vec![old.clone()];
        state.save(&path).expect("seed the state");
        old
    }

    /// Exactly one history entry has the operation's `id` (none when `id` is
    /// `None`), and the seeded entry is there once, unchanged: nothing was
    /// replayed twice and nothing was lost.
    #[track_caller]
    pub fn assert_history(&self, id: Option<&str>, old: &open_ferry_update::state::SwitchRecord) {
        let state = self.state();
        let ids: Vec<&str> = state.history.iter().map(|r| r.id.as_str()).collect();
        let report = |why: String| -> ! {
            panic!(
                "{why}: history ids {ids:?}
{}",
                self.dump()
            )
        };
        let count = |wanted: &str| ids.iter().filter(|i| **i == wanted).count();
        if count(&old.id) != 1 || !state.history.iter().any(|r| r == old) {
            report("the seeded entry is not there once and unchanged".to_owned());
        }
        match id {
            Some(id) if count(id) != 1 || ids.len() != 2 => {
                report(format!("not exactly one entry for {id} beside the old one"))
            }
            None if ids.len() != 1 => report("a history entry was added".to_owned()),
            _ => {}
        }
    }

    /// Everything that may explain a failure.
    pub fn dump(&self) -> String {
        let tail = |name: &str| {
            let text = std::fs::read_to_string(self.root().join(name)).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(60)..].join("\n")
        };
        format!(
            "--- child.log\n{}\n--- driver.log\n{}\n--- events\n{:?}\n--- journal\n{}\n--- state\n{:?}\n",
            tail("child.log"),
            tail("driver.log"),
            self.events(),
            self.journal().map_or("none".to_owned(), |j| j.to_string()),
            self.state().last_switch,
        )
    }

    /// Polls `f` every few milliseconds until it gives `Some`, bounded by
    /// `LONG`; a timeout panics with the logs.
    pub fn wait_for<T>(&self, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + LONG;
        loop {
            if let Some(found) = f() {
                return found;
            }
            if Instant::now() >= until {
                panic!("timed out waiting for {what}\n{}", self.dump());
            }
            std::thread::sleep(POLL);
        }
    }

    /// Waits until the journal holds no open switch and no outcome the state
    /// has not taken, and the state file has the record of `id`.
    pub fn wait_settled(&self) {
        self.wait_for("the journal to settle", || {
            let journal = self.journal()?;
            (journal["switch"].is_null() && journal["outcomes"].as_array()?.is_empty())
                .then_some(())
        });
    }

    /// Waits until the newest worker the supervisor started has reported
    /// `loaded` (its own event, by pid) and the journal's ladder count is
    /// back at 0, or there is no ladder record. `wait_settled` doesn't wait
    /// for this: a supervisor stopped or snapshotted before it leaves the
    /// try in flight counted against its rung.
    pub fn wait_loaded(&self) {
        self.wait_for(
            "the newest worker to load and its try to be uncounted",
            || {
                let pid = self.driver_log().iter().rev().find(|v| v["k"] == "spawn")?["pid"]
                    .as_u64()? as u32;
                if !self
                    .events()
                    .iter()
                    .any(|e| e.pid == pid && (e.event == "loaded" || e.event == "active"))
                {
                    return None;
                }
                let journal = self.journal()?;
                (journal["ladder"]
                    .get("attempts")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    == 0)
                    .then_some(())
            },
        );
    }

    /// Waits for a journal condition.
    pub fn wait_journal(&self, what: &str, mut f: impl FnMut(&Value) -> bool) {
        self.wait_for(what, || f(&self.journal()?).then_some(()));
    }

    /// Waits until worker `n` (1 is A) is no longer running.
    pub fn wait_dead(&self, n: usize) {
        let pid = self.wait_for(&format!("worker {n} to be started"), || self.pid_of(n));
        self.wait_for(&format!("worker {n} to be gone"), || {
            (!alive(pid)).then_some(())
        });
    }

    /// The four columns of design 13 item 8.
    #[track_caller]
    pub fn assert_columns(&self, columns: Columns<'_>) {
        let log = self.driver_log();
        let spawns: Vec<&Value> = log.iter().filter(|v| v["k"] == "spawn").collect();
        let report = |why: String| -> ! { panic!("{why}\n{}", self.dump()) };
        if let Some(count) = columns.spawns
            && spawns.len() != count
        {
            report(format!(
                "{} workers were started, not {count}",
                spawns.len()
            ));
        }
        if let Some(count) = columns.spawn_failed {
            let failed = log.iter().filter(|v| v["k"] == "spawn_failed").count();
            if failed != count {
                report(format!("{failed} spawns failed, not {count}"));
            }
        }
        if columns.probe_gone {
            // A worker that failed its handshake has no spawn record; the
            // probe wrote its pid beside its config.
            let text = std::fs::read_to_string(self.root().join("cfg").join("pid"))
                .unwrap_or_else(|_| report("the probe never started".to_owned()));
            if alive(text.trim().parse().unwrap()) {
                report("the probe still runs".to_owned());
            }
        }
        let pid = |n: usize| -> u32 {
            spawns
                .iter()
                .find(|v| v["n"] == n)
                .unwrap_or_else(|| report(format!("worker {n} was never started")))["pid"]
                .as_u64()
                .unwrap() as u32
        };
        // Column 1: what was killed and waited for.
        for &n in columns.killed {
            let target = pid(n);
            let at = log
                .iter()
                .position(|v| v["k"] == "cmd" && v["pid"] == target && v["cmd"] == "kill")
                .unwrap_or_else(|| report(format!("worker {n} was not terminated by handle")));
            if alive(target) {
                report(format!("worker {n} still runs"));
            }
            // What the supervisor does next to another worker (a spawn or a
            // command) happens only after the killed one is gone.
            for later in log[at + 1..].iter().filter(|v| v["pid"] != target) {
                let list = later["alive"].as_array();
                if list.is_some_and(|l| l.iter().any(|p| p == target)) {
                    report(format!(
                        "worker {n} was still running when the supervisor went on: {later}"
                    ));
                }
            }
        }
        for &n in columns.gone {
            if alive(pid(n)) {
                report(format!("worker {n} still runs"));
            }
        }
        for &n in columns.running {
            if !alive(pid(n)) {
                report(format!("worker {n} is gone"));
            }
        }
        // Column 2: what resumed or restarted.
        for &n in columns.resumed {
            let target = pid(n);
            if !log
                .iter()
                .any(|v| v["k"] == "cmd" && v["pid"] == target && v["cmd"] == "send:resume")
            {
                report(format!("worker {n} was not sent resume"));
            }
            // The worker's own word for it: it answered `resume` and logged it.
            // The fake sends `Resumed` before it appends the event, so the
            // event may trail the command: wait for it, bounded.
            let until = Instant::now() + Duration::from_secs(5);
            while !self
                .events()
                .iter()
                .any(|e| e.pid == target && e.event == "resumed")
            {
                if Instant::now() >= until {
                    report(format!("worker {n} never logged `resumed`"));
                }
                std::thread::sleep(POLL);
            }
        }
        for &n in columns.not_resumed {
            let target = pid(n);
            if log
                .iter()
                .any(|v| v["k"] == "cmd" && v["pid"] == target && v["cmd"] == "send:resume")
            {
                report(format!("worker {n} was sent resume"));
            }
        }
        // "Never knew": the worker was told nothing (no hand over, no stop, no
        // close or kill) and still serves: `GET /` answers with its marker on
        // the main listener, and the separate management listener answers with
        // the worker's marker and not the 503 a handing-over worker gives
        // (L9.11: any other status will do; a real worker says 401 or 404).
        for &n in columns.not_handed_over {
            let target = pid(n);
            for cmd in ["send:hand_over", "send:stop", "close", "kill"] {
                if log
                    .iter()
                    .any(|v| v["k"] == "cmd" && v["pid"] == target && v["cmd"] == cmd)
                {
                    report(format!("worker {n} was sent {cmd}"));
                }
            }
            let version = pid_version(&spawns, n);
            let marker = match version.as_str() {
                A => "A",
                B => "B",
                C => "C",
                other => report(format!("worker {n} has the unknown version {other}")),
            };
            match self.get("/") {
                Some((200, Some(got))) if got == marker => {}
                other => report(format!("worker {n} does not serve GET /: {other:?}")),
            }
            match self.get_management("/v0/management/config") {
                Some((status, Some(got))) if got == marker && status != 503 => {}
                other => report(format!(
                    "worker {n}'s management listener does not answer with its marker and a status other than 503: {other:?}"
                )),
            }
        }
        // Column 3: the installed file's bytes.
        // Required: a test that leaves it out fails, so none can skip it. A
        // test that damaged the file on purpose names the bytes it expects
        // (`installed_bytes`) and still gets every other column.
        let held = std::fs::read(self.installed()).unwrap();
        match columns.installed_bytes {
            Some(expected) => {
                if held != expected {
                    report("the installed file does not hold the bytes expected".to_owned());
                }
            }
            None => {
                if columns.installed.is_empty() {
                    report("the installed column is required: name the version".to_owned());
                }
                if held != artifact_bytes(columns.installed) {
                    report(format!(
                        "the installed file does not hold {}",
                        columns.installed
                    ));
                }
            }
        }
        // Column 4: the state record and the journal.
        let state = self.state();
        let failed = match columns.state {
            None => report("the state column is required: set `state` to a `StateCol`".to_owned()),
            Some(StateCol::Unchecked(reason)) => {
                if reason.is_empty() {
                    report("StateCol::Unchecked needs a reason".to_owned());
                }
                None
            }
            Some(StateCol::Record(record, failed)) => {
                let Some(last) = state.last_switch.clone() else {
                    report("the state has no switch record".to_owned())
                };
                let got = (
                    last.result.clone().unwrap_or_default(),
                    last.phase.clone().unwrap_or_default(),
                    last.blame.clone().unwrap_or_default(),
                    last.returned_to.clone().unwrap_or_default(),
                    last.confirmed,
                );
                let want = (
                    record.result.to_owned(),
                    record.phase.to_owned(),
                    record.blame.to_owned(),
                    record.returned_to.to_owned(),
                    record.confirmed,
                );
                if got != want {
                    report(format!(
                        "the state record is (result, phase, blame, returned_to, confirmed) {got:?}, not {want:?}"
                    ));
                }
                Some(failed)
            }
            Some(StateCol::NoRecord(failed)) => {
                if state.last_switch.is_some() {
                    report(format!(
                        "the state has a switch record: {:?}",
                        state.last_switch
                    ));
                }
                Some(failed)
            }
            Some(StateCol::Older(older, failed)) => {
                if state.last_switch.as_ref() != Some(older) {
                    report(format!(
                        "the last switch record is not the older one: {:?}",
                        state.last_switch
                    ));
                }
                Some(failed)
            }
        };
        if let Some(failed) = failed
            && state.failed.iter().map(String::as_str).collect::<Vec<_>>() != failed
        {
            report(format!(
                "state.failed is {:?}, not {failed:?}",
                state.failed
            ));
        }
        match columns.journal {
            JournalState::Closed => {
                let journal = self
                    .journal()
                    .unwrap_or_else(|| report("there is no journal".to_owned()));
                if !journal["switch"].is_null() {
                    report(format!(
                        "the journal still has a switch: {}",
                        journal["switch"]
                    ));
                }
                if !journal["outcomes"].as_array().unwrap().is_empty() {
                    report(format!(
                        "the journal still has outcomes: {}",
                        journal["outcomes"]
                    ));
                }
            }
            JournalState::Open => {
                let journal = self
                    .journal()
                    .unwrap_or_else(|| report("there is no journal".to_owned()));
                if journal["switch"].is_null() {
                    report("the journal has no open switch, and recovery needs it".to_owned());
                }
            }
            JournalState::Unchecked(_reason) => {}
        }
    }

    /// Settled recovery: stops `second`, starts a third supervisor on the same
    /// data directory and shows it changes nothing. It runs `runs`, the state
    /// is equal and the journal is equal but for `seq` (its write count) and
    /// `workers` (the running processes), which differ between supervisors by
    /// nature, and the history still has the operation `id` once beside `old`.
    #[track_caller]
    pub fn assert_third_start_changes_nothing(
        &self,
        mut second: Sup,
        runs: &str,
        id: Option<&str>,
        old: &open_ferry_update::state::SwitchRecord,
    ) {
        let unsequenced = |mut journal: Option<Value>| {
            if let Some(object) = journal.as_mut().and_then(|j| j.as_object_mut()) {
                object.remove("seq");
                object.remove("workers");
            }
            journal
        };
        self.wait_loaded();
        let (state, journal) = (self.state(), unsequenced(self.journal()));
        self.stop();
        self.wait_exit(&mut second);
        let _third = self.start(&Tm::default());
        self.wait_settled();
        self.wait_loaded();
        let last = self
            .driver_log()
            .iter()
            .rev()
            .find(|v| v["k"] == "spawn")
            .cloned()
            .expect("a worker");
        assert_eq!(last["sha"], self.sha(runs), "the third runs {runs}");
        assert_eq!(
            self.state(),
            state,
            "the third supervisor changed the state"
        );
        assert_eq!(
            unsequenced(self.journal()),
            journal,
            "the third supervisor changed the journal"
        );
        self.assert_history(id, old);
    }
}

/// One plain HTTP request to `127.0.0.1:port`: the status code and the marker
/// header (`x-open-ferry-test-marker`) of whichever worker answered. `None`
/// when nothing answers within a few seconds, or the answer is not HTTP.
pub fn http_get(port: u16, path: &str) -> Option<(u16, Option<String>)> {
    use std::io::{Read, Write};
    let wait = Duration::from_secs(5);
    let mut stream = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        wait,
    )
    .ok()?;
    stream.set_read_timeout(Some(wait)).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    let _ = stream.read_to_string(&mut text);
    let status = text.lines().next()?.split(' ').nth(1)?.parse().ok()?;
    let marker = text.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .starts_with("x-open-ferry-test-marker:")
            .then(|| line.split_once(':').map(|(_, v)| v.trim().to_owned()))
            .flatten()
    });
    Some((status, marker))
}

/// One request to the endpoint of the supervisor of the fixture at `root`,
/// and its first reply; `None` when nothing answers within 20 seconds. It
/// trusts nothing: a caller that must know whose supervisor answers checks
/// `Fx::identity` first (`Fx::ask` does).
pub fn endpoint_ask(root: &Path, request: EndpointRequest) -> Option<EndpointReply> {
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut channel = Fx::open(root).await.ok()?;
            channel.send(&request).await.ok()?;
            let frame: Frame<EndpointReply> = channel.receive().await.ok()??;
            Some(frame.message)
        })
        .await
        .ok()
        .flatten()
    })
}

/// The text of `-test-endpoint` that puts a real `open-ferry -supervise` on the
/// endpoint `client_location(root)` connects to. On Windows the pipe is derived
/// from the data directory, so there is nothing to name: `None`.
pub fn endpoint_flag(root: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        own_endpoint(root).map(|_| root.join("e").display().to_string())
    }
    #[cfg(windows)]
    {
        let _ = root;
        None
    }
}

/// The version of the `n`th spawn in the driver log.
fn pid_version(spawns: &[&Value], n: usize) -> String {
    spawns
        .iter()
        .find(|v| v["n"] == n)
        .and_then(|v| v["version"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The state record a branch leaves, by its fields.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rec<'a> {
    pub result: &'a str,
    pub phase: &'a str,
    pub blame: &'a str,
    pub returned_to: &'a str,
    pub confirmed: bool,
}

/// What `assert_columns` checks. Workers are named by the order the supervisor
/// started them: 1 is A, and so on.
#[derive(Default)]
pub struct Columns<'a> {
    /// How many workers were started in all.
    pub spawns: Option<usize>,
    /// How many spawns failed their handshake (no pid was ever returned).
    pub spawn_failed: Option<usize>,
    /// The `worker_probe` staged by `stage_probe` is gone.
    pub probe_gone: bool,
    /// Terminated by their handle, and gone before the supervisor went on.
    pub killed: &'a [usize],
    /// Gone, however they went.
    pub gone: &'a [usize],
    /// Still running.
    pub running: &'a [usize],
    /// Sent `resume`.
    pub resumed: &'a [usize],
    pub not_resumed: &'a [usize],
    /// Never sent a hand over, a stop, a close or a kill, and still serving.
    pub not_handed_over: &'a [usize],
    /// The version whose bytes the installed file holds. Required (empty
    /// fails): design 13 item 8 names it as a column of every process test.
    pub installed: &'a str,
    /// For a test that damaged the installed file on purpose: the bytes it
    /// must hold, instead of `installed`'s. Every other column still applies.
    pub installed_bytes: Option<&'a [u8]>,
    /// The state column: exactly one selector, so no test can name two or
    /// leave it out (unset fails). Each one that looks at the state carries the
    /// exact `failed` list, so that is never left unchecked by omission.
    pub state: Option<StateCol<'a>>,
    /// What the journal must hold. The default is closed.
    pub journal: JournalState,
}

/// The state column of `Columns`: one selector.
#[derive(Clone, Copy, Debug)]
pub enum StateCol<'a> {
    /// The last switch record is this one, and `failed` is exactly the list.
    Record(Rec<'a>, &'a [&'a str]),
    /// The state has no switch record at all, and `failed` is exactly the list.
    NoRecord(&'a [&'a str]),
    /// The last record is exactly this older operation's (from
    /// `seed_history`), the switch under test having left none of its own, and
    /// `failed` is exactly the list.
    Older(&'a open_ferry_update::state::SwitchRecord, &'a [&'a str]),
    /// Neither the record nor `failed` is looked at, for the reason given,
    /// which names the ignored test that holds the design's expectation.
    Unchecked(&'static str),
}

/// What `assert_columns` requires of the journal.
#[derive(Clone, Copy, Debug, Default)]
pub enum JournalState {
    /// No open switch and no outcome the state has not taken.
    #[default]
    Closed,
    /// A switch is open: recovery must keep it for the next start.
    Open,
    /// Not looked at. The reason is stated where it is used.
    #[allow(dead_code)]
    Unchecked(&'static str),
}
