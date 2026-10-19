//! Not upstream's: the supervisor's whole life over a temporary data
//! directory, with a fake host, runner and driver: a clean stop, and the
//! panic re-entry (lead answer 1) driven by the `supervisor-panic` fault.

use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_ferry_supervisor::clock::SystemClock;
use open_ferry_supervisor::core::{Command, Exit};
use open_ferry_supervisor::driver::{
    Cmd, Driver, Event, EventSender, ExitInfo, SpawnError, SpawnRequest, WorkerPort,
};
use open_ferry_supervisor::endpoint::{Location, ServiceKind};
use open_ferry_supervisor::host::{Host, ModeSnapshot};
use open_ferry_supervisor::process::ProcessIdentity;
use open_ferry_supervisor::protocol::ListenerTopology;
use open_ferry_supervisor::startup::Config;
use open_ferry_supervisor::supervise::{default_limit, supervise};
use open_ferry_supervisor::timings::Timings;
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::runner::Runner;
use open_ferry_update::staging::hash_file;

const NAME: &str = if cfg!(windows) {
    "open-ferry.exe"
} else {
    "open-ferry"
};

struct FakeHost {
    version: &'static str,
    sha: Option<String>,
    /// A main listener, for a real worker: on Unix the process driver hands
    /// one to the child and refuses to start it without. The fake workers
    /// take none.
    main: Option<TcpListener>,
}

impl Host for FakeHost {
    fn topology(&self) -> Vec<ListenerTopology> {
        self.main
            .iter()
            .map(|listener| ListenerTopology {
                name: "main".to_owned(),
                address: listener.local_addr().expect("address"),
                tls: false,
            })
            .collect()
    }
    fn bind(&self) -> io::Result<Vec<(String, TcpListener)>> {
        self.main
            .iter()
            .map(|listener| Ok(("main".to_owned(), listener.try_clone()?)))
            .collect()
    }
    fn effective_mode(&self) -> ModeSnapshot {
        ModeSnapshot {
            off: false,
            auto: true,
            mode_gen: 0,
        }
    }
    fn own_version(&self) -> String {
        self.version.to_owned()
    }
    fn own_sha256(&self) -> Option<String> {
        self.sha.clone()
    }
    fn waiting_for_update_lock(&self, _waited: Duration) {}
}

struct FakeRunner;

impl Runner for FakeRunner {
    fn version<'a>(
        &'a self,
        _binary: &'a Path,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Ok("open-ferry 1.0.0".to_owned()) })
    }
}

/// Workers that exit when they are stopped, closed or killed.
#[derive(Default)]
struct FakeDriver {
    spawned: Arc<AtomicUsize>,
    stopped: Arc<Mutex<Vec<usize>>>,
    /// While set, the second spawn does not return (the start-over after a
    /// panic waits here, before the new core exists).
    hold_second: Arc<std::sync::atomic::AtomicBool>,
}

impl Driver for FakeDriver {
    fn spawn(
        &self,
        request: SpawnRequest,
        events: EventSender,
    ) -> impl std::future::Future<Output = Result<WorkerPort, SpawnError>> + Send {
        let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let number = self.spawned.fetch_add(1, Ordering::SeqCst) + 1;
        let id = request.id;
        let stopped = self.stopped.clone();
        let hold = (number == 2).then(|| self.hold_second.clone());
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                let leave = match command {
                    Cmd::Send(message) => {
                        matches!(
                            message,
                            open_ferry_supervisor::protocol::Message::Stop { .. }
                        )
                    }
                    Cmd::Close | Cmd::Kill => true,
                };
                if leave {
                    break;
                }
            }
            stopped.lock().unwrap().push(number);
            let _ = events.send((
                id,
                Event::Exited(ExitInfo {
                    code: Some(0),
                    success: true,
                    text: "stopped".to_owned(),
                }),
            ));
        });
        let port = WorkerPort {
            id,
            identity: ProcessIdentity {
                pid: 4242,
                started: 7,
            },
            reported_version: request.version.clone(),
            protocol: 1,
            caps: Vec::new(),
            cmd,
        };
        async move {
            if let Some(hold) = hold {
                while hold.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            Ok(port)
        }
    }
}

/// Plain cargo test shares a descriptor table: an unrelated fork can inherit
/// an executable write descriptor (ETXTBSY) or a lock descriptor. Keep writes
/// and forks from different tests apart. Nextest isolates each test process.
static ENDPOINT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Waits for this test's turn. Each test has its own runtime thread, so the
/// short blocking sleeps hold up no other test.
fn take_turn() -> tokio::sync::MutexGuard<'static, ()> {
    loop {
        if let Ok(turn) = ENDPOINT.try_lock() {
            return turn;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

struct Rig {
    _turn: Option<tokio::sync::MutexGuard<'static, ()>>,
    _dir: tempfile::TempDir,
    _sockets: Option<tempfile::TempDir>,
    cfg: Config,
    host: FakeHost,
}

/// A fresh directory for one test. The data directory's trust check looks at
/// every ancestor, so the directory must not sit under the source tree (owned
/// by the host user when a container runs as root) nor under `/tmp` (anyone can
/// write to it). Off Windows it is under the user's home directory; Windows'
/// temp directory does not pass the check either, so there it stays under the
/// target directory the user owns, or under `OFP_TEST_SCRATCH` when set (CI's
/// runner, whose drive root is owned by NETWORK SERVICE).
fn scratch() -> tempfile::TempDir {
    if cfg!(windows) {
        match std::env::var_os("OFP_TEST_SCRATCH") {
            Some(root) => tempfile::tempdir_in(root).unwrap(),
            None => tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
        }
    } else {
        let home = std::env::var_os("HOME").expect("HOME is set");
        tempfile::tempdir_in(home).unwrap()
    }
}

/// The endpoint this test's supervisor binds, owned by the test. On Unix the
/// default is the user's own per-UID socket, which a real supervisor may hold,
/// and which a client of ours would then reach. The socket directory is short
/// (a Unix socket path is limited to 104 or 108 bytes) and under the home
/// directory: the path policy refuses a chain through `/tmp`, which anyone can
/// write to.
/// Windows derives the pipe from the data directory already.
#[cfg(windows)]
fn own_endpoint() -> (Option<Location>, Option<tempfile::TempDir>) {
    (None, None)
}

#[cfg(unix)]
fn own_endpoint() -> (Option<Location>, Option<tempfile::TempDir>) {
    let home = std::env::var_os("HOME").expect("HOME is set");
    let dir = tempfile::Builder::new()
        .prefix("o-")
        .tempdir_in(home)
        .unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let location = Location::new(&root, "e").unwrap();
    assert!(
        location.socket_path().as_os_str().len() <= 103,
        "SET-UP: canonical socket path exceeds 103 bytes: {}",
        location.socket_path().display()
    );
    (Some(location), Some(dir))
}

fn rig() -> Rig {
    let turn = take_turn();
    let mut rig = rig_under_gate();
    rig._turn = Some(turn);
    rig
}

/// Caller holds the one outer gate when constructing multiple fixtures.
fn rig_under_gate() -> Rig {
    let dir = scratch();
    let root = dir.path().join("data");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let installed: PathBuf = bin.join(NAME);
    std::fs::write(&installed, b"binary one").unwrap();
    let sha = hash_file(&installed).unwrap();
    let timings = Timings {
        stop: Duration::from_millis(500),
        ..Timings::default()
    };
    let (test_endpoint_location, sockets) = own_endpoint();
    let cfg = Config {
        data: DataDir::at(root),
        installed,
        config: dir.path().join("config.yaml"),
        kind: ServiceKind::User,
        timings,
        receipt: serde_json::json!({}),
        binary_name: NAME.to_owned(),
        runner: Arc::new(FakeRunner),
        detect_timeout: Duration::from_secs(5),
        #[cfg(feature = "blue-green-test")]
        test_endpoint_location,
    };
    Rig {
        _turn: None,
        _dir: dir,
        _sockets: sockets,
        cfg,
        host: FakeHost {
            version: "1.0.0",
            sha: Some(sha),
            main: None,
        },
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..600 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Asks the running supervisor's endpoint for its status snapshot.
async fn status(own: Option<Location>, root: &Path) -> serde_json::Value {
    use open_ferry_supervisor::endpoint::{Identity, connect};
    use open_ferry_supervisor::protocol::{
        Channel, EndpointReply, EndpointRequest, Frame, VERSION,
    };
    let identity = Identity::current(ServiceKind::User).unwrap();
    let location = match own {
        Some(location) => location,
        None => Location::resolve(&identity, root).unwrap(),
    };
    let stream = connect(&location, &identity).await.expect("connect");
    let (reader, writer) = tokio::io::split(stream);
    let mut channel = Channel::new(reader, writer);
    channel
        .send(&EndpointRequest::Status { v: VERSION })
        .await
        .unwrap();
    let frame: Frame<EndpointReply> = channel.receive().await.unwrap().expect("a reply");
    match frame.message {
        EndpointReply::Status { snapshot, .. } => snapshot,
        other => panic!("not a status: {other:?}"),
    }
}

/// Runs a supervisor over the rig's data dir until its first worker is up,
/// asks it for its status, and stops it. Returns the status.
async fn status_of_a_start(rig: &Rig) -> serde_json::Value {
    let driver = FakeDriver::default();
    let root = rig.cfg.data.root().to_path_buf();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let observer = async {
        until("the worker", || driver.spawned.load(Ordering::SeqCst) == 1).await;
        let shown = status(rig.cfg.test_endpoint_location.clone(), &root).await;
        tx.send(Command::Stop).unwrap();
        shown
    };
    let (exit, shown) = tokio::join!(
        supervise(
            &rig.cfg,
            &rig.host,
            &driver,
            &SystemClock,
            tx.clone(),
            rx,
            default_limit()
        ),
        observer
    );
    assert_eq!(exit.unwrap(), Exit::Stopped);
    shown
}

/// A state file whose last outcome needed a restart, written by a supervisor
/// of `by`.
fn needing_a_restart(rig: &Rig, by: &str) {
    use open_ferry_update::journal::Outcome;
    use open_ferry_update::state::SwitchRecord;
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let outcome = Outcome {
        record: SwitchRecord {
            id: "needs".into(),
            from: "1.0.0".into(),
            to: "2.0.0".into(),
            restart_needed: true,
            restart_needed_by: Some(by.to_owned()),
            ..Default::default()
        },
        mark_failed: None,
    };
    open_ferry_supervisor::statefile::apply_locked_by(&rig.cfg.data, "S2", &outcome, by).unwrap();
}

fn flag_in_the_file(rig: &Rig) -> bool {
    open_ferry_update::state::State::load_strict(&rig.cfg.data.state_file())
        .unwrap()
        .needs_restart
}

/// A stop while startup waits for the update lock: `supervise` returns
/// `Stopped` with no worker spawned and the lock free afterwards.
#[tokio::test]
async fn a_stop_while_startup_waits_for_the_update_lock_stops_with_no_worker() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let held = rig.cfg.data.try_lock().unwrap().expect("lock");
    let driver = FakeDriver::default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sender = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send(Command::Stop).unwrap();
    };
    let (exit, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            supervise(
                &rig.cfg,
                &rig.host,
                &driver,
                &SystemClock,
                tx.clone(),
                rx,
                default_limit()
            ),
            sender
        )
    })
    .await
    .expect("the stop was not seen during the wait");
    assert_eq!(exit.unwrap(), Exit::Stopped);
    assert_eq!(driver.spawned.load(Ordering::SeqCst), 0);
    drop(held);
    assert!(
        rig.cfg.data.try_lock().unwrap().is_some(),
        "the update lock is not free"
    );
}

/// A stop after the lock was taken is the core's: the worker that was started
/// is stopped through the normal path.
#[tokio::test]
async fn a_stop_after_the_update_lock_is_taken_reaches_the_core_and_stops_the_worker() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let held = rig.cfg.data.try_lock().unwrap().expect("lock");
    let driver = FakeDriver::default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let script = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(held);
        until("the worker", || driver.spawned.load(Ordering::SeqCst) == 1).await;
        tx.send(Command::Stop).unwrap();
    };
    let (exit, ()) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            supervise(
                &rig.cfg,
                &rig.host,
                &driver,
                &SystemClock,
                tx.clone(),
                rx,
                default_limit()
            ),
            script
        )
    })
    .await
    .expect("finishes");
    assert_eq!(exit.unwrap(), Exit::Stopped);
    assert_eq!(driver.spawned.load(Ordering::SeqCst), 1);
    assert_eq!(*driver.stopped.lock().unwrap(), vec![1], "stop_all ran");
}

#[tokio::test]
async fn a_restart_at_the_same_version_keeps_needs_restart_and_shows_it() {
    let rig = rig();
    needing_a_restart(&rig, "1.0.0");
    let shown = status_of_a_start(&rig).await;
    assert_eq!(shown["needs_restart"], true, "{shown}");
    assert!(flag_in_the_file(&rig));
}

#[tokio::test]
async fn a_restart_at_another_version_clears_needs_restart_in_the_file_and_the_status() {
    let rig = rig();
    needing_a_restart(&rig, "0.9.0");
    let shown = status_of_a_start(&rig).await;
    assert_eq!(shown["needs_restart"], false, "{shown}");
    assert!(!flag_in_the_file(&rig), "cleared in the file");
}

/// Item 5, after a replay: the outcome that needed the restart is in the
/// journal, not yet in the state file. Start-up replays it, and the status the
/// running supervisor gives says what the file says.
#[tokio::test]
async fn a_replayed_outcome_that_needs_a_restart_is_shown_as_the_file_has_it() {
    use open_ferry_update::journal::{Artifact, Installed, Journal, JournalStore, Outcome};
    use open_ferry_update::state::SwitchRecord;
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.version_dir("1.0.0")).unwrap();
    std::fs::copy(&rig.cfg.installed, rig.cfg.data.binary("1.0.0", NAME)).unwrap();
    let sha = rig.host.sha.clone().unwrap();
    let mut journal = Journal::first_start(
        Artifact {
            version: "1.0.0".into(),
            sha256: sha.clone(),
            size: 10,
        },
        Some(Installed {
            version: "1.0.0".into(),
            sha256: sha,
        }),
    );
    journal.push_outcome(Outcome {
        record: SwitchRecord {
            id: "replayed".into(),
            from: "0.9.0".into(),
            to: "1.0.0".into(),
            at: chrono::Utc::now().to_rfc3339(),
            confirmed: true,
            result: Some("switched".into()),
            restart_needed: true,
            restart_needed_by: Some("1.0.0".into()),
            ..Default::default()
        },
        mark_failed: None,
    });
    JournalStore::new(rig.cfg.data.root())
        .write(&mut journal, "test")
        .unwrap();
    assert!(!rig.cfg.data.state_file().exists(), "nothing replayed yet");
    let shown = status_of_a_start(&rig).await;
    assert!(flag_in_the_file(&rig), "the replay wrote the flag");
    assert_eq!(
        shown["needs_restart"],
        flag_in_the_file(&rig),
        "the status shows what the file has: {shown}"
    );
}

#[tokio::test]
async fn a_stop_ends_the_supervisor_and_stops_its_worker() {
    let rig = rig();
    let driver = FakeDriver::default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stopper = async {
        until("the worker", || driver.spawned.load(Ordering::SeqCst) == 1).await;
        tx.send(Command::Stop).unwrap();
    };
    let (exit, ()) = tokio::join!(
        supervise(
            &rig.cfg,
            &rig.host,
            &driver,
            &SystemClock,
            tx.clone(),
            rx,
            default_limit()
        ),
        stopper
    );
    assert_eq!(exit.unwrap(), Exit::Stopped);
    assert_eq!(*driver.stopped.lock().unwrap(), [1]);
}

#[cfg(feature = "blue-green-test")]
mod panics {
    use super::*;
    use open_ferry_supervisor::reentry::ReentryLimit;

    fn arm(root: &Path) {
        let dir = root.join(".blue-green-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fault.supervisor-panic.fail"), b"").unwrap();
    }

    fn armed(root: &Path) -> bool {
        root.join(".blue-green-test")
            .join("fault.supervisor-panic.fail")
            .exists()
    }

    #[tokio::test]
    async fn a_panic_stops_the_workers_and_the_supervisor_starts_over() {
        let rig = rig();
        let driver = FakeDriver::default();
        let root = rig.cfg.data.root().to_path_buf();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stopper = async {
            until("the first worker", || {
                driver.spawned.load(Ordering::SeqCst) == 1
            })
            .await;
            arm(&root);
            until("the second worker", || {
                driver.spawned.load(Ordering::SeqCst) == 2
            })
            .await;
            tx.send(Command::Stop).unwrap();
        };
        let (exit, ()) = tokio::join!(
            supervise(
                &rig.cfg,
                &rig.host,
                &driver,
                &SystemClock,
                tx.clone(),
                rx,
                default_limit()
            ),
            stopper
        );
        assert_eq!(exit.unwrap(), Exit::Stopped);
        // The first worker was stopped for the re-entry, the second for the stop.
        assert_eq!(*driver.stopped.lock().unwrap(), [1, 2]);
        assert!(!armed(&root), "the fault fires once");
    }

    /// A real core panic, then the start-over: while the new core is not up
    /// yet (its worker is still being started), status shows the restarting
    /// snapshot, not the dead core's last picture.
    #[tokio::test]
    async fn status_between_a_panic_and_the_new_core_shows_the_restarting_snapshot() {
        let rig = rig();
        let driver = FakeDriver::default();
        let root = rig.cfg.data.root().to_path_buf();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        driver.hold_second.store(true, Ordering::SeqCst);
        let observer = async {
            until("the first worker", || {
                driver.spawned.load(Ordering::SeqCst) == 1
            })
            .await;
            arm(&root);
            // The start-over spawns the second worker, which is held.
            until("the second spawn", || {
                driver.spawned.load(Ordering::SeqCst) == 2
            })
            .await;
            let shown = status(rig.cfg.test_endpoint_location.clone(), &root).await;
            driver.hold_second.store(false, Ordering::SeqCst);
            tx.send(Command::Stop).unwrap();
            shown
        };
        let (exit, shown) = tokio::join!(
            supervise(
                &rig.cfg,
                &rig.host,
                &driver,
                &SystemClock,
                tx.clone(),
                rx,
                default_limit()
            ),
            observer
        );
        assert_eq!(exit.unwrap(), Exit::Stopped);
        assert_eq!(shown["state"], "degraded", "{shown}");
        assert!(
            shown["degraded"]
                .as_str()
                .is_some_and(|why| why.contains("starting over")),
            "{shown}"
        );
        assert!(shown["switch"].is_null(), "{shown}");
    }

    #[tokio::test]
    async fn too_many_panics_in_the_window_end_the_supervisor() {
        let rig = rig();
        let driver = FakeDriver::default();
        let root = rig.cfg.data.root().to_path_buf();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // One re-entry is allowed: the second panic is the last.
        let limit = ReentryLimit::new(1, Duration::from_secs(600));
        let rearm = async {
            until("the first worker", || {
                driver.spawned.load(Ordering::SeqCst) == 1
            })
            .await;
            arm(&root);
            until("the second worker", || {
                driver.spawned.load(Ordering::SeqCst) == 2
            })
            .await;
            arm(&root);
        };
        let (exit, ()) = tokio::join!(
            supervise(
                &rig.cfg,
                &rig.host,
                &driver,
                &SystemClock,
                tx.clone(),
                rx,
                limit
            ),
            rearm
        );
        assert_eq!(exit.unwrap(), Exit::TooManyPanics);
        assert_eq!(driver.spawned.load(Ordering::SeqCst), 2);
        assert_eq!(*driver.stopped.lock().unwrap(), [1, 2]);
    }
}

/// Item 2 with a real child: B is `worker_probe`, started by the real driver,
/// and its termination is held by the blue-green gate, so the survivor of a
/// cancelled, timed out or failed handshake outlives its kill. The switch must
/// stay open, and answer nothing, until the child's exit is observed.
#[cfg(feature = "blue-green-test")]
mod real_survivor {
    use super::*;
    use open_ferry_supervisor::core::SwitchRequest;
    use open_ferry_supervisor::driver::ProcessDriver;
    use open_ferry_supervisor::protocol::{Admission, EndpointReply};
    use open_ferry_update::journal::How;
    use std::sync::atomic::AtomicBool;

    /// Version 1.0.0 is the fake; 2.0.0 is the real probe.
    struct Mixed {
        fake: FakeDriver,
        real: ProcessDriver,
        /// The real driver has returned its error for the probe's spawn.
        real_failed: AtomicBool,
    }

    impl Driver for Mixed {
        async fn spawn(
            &self,
            request: SpawnRequest,
            events: EventSender,
        ) -> Result<WorkerPort, SpawnError> {
            if request.version == "2.0.0" {
                let spawned = self.real.spawn(request, events).await;
                if spawned.is_err() {
                    self.real_failed.store(true, Ordering::SeqCst);
                }
                spawned
            } else {
                self.fake.spawn(request, events).await
            }
        }
    }

    enum How2 {
        /// The Stop arrives while B's handshake is held.
        Stop,
        /// The handshake outlives the load timeout (5 s here: long enough for a slow host to hash and start the probe).
        Timeout,
        /// The probe demands a newer supervisor protocol.
        Incompatible,
    }

    async fn run(how: How2) {
        let mut rig = rig();
        let root = rig._dir.path().to_path_buf();
        let config = root.join("probe-config");
        std::fs::create_dir_all(&config).unwrap();
        let mode = match how {
            How2::Incompatible => "min2-live",
            _ => "stall",
        };
        std::fs::write(config.join("mode"), mode).unwrap();
        rig.cfg.config = config.clone();
        // Unix: the real child is handed a main listener (see `FakeHost`).
        #[cfg(unix)]
        {
            rig.host.main = Some(TcpListener::bind("127.0.0.1:0").unwrap());
        }
        if matches!(how, How2::Timeout) {
            rig.cfg.timings.load = Duration::from_secs(5);
        }
        let data = rig.cfg.data.root().to_path_buf();
        let driver = Mixed {
            fake: FakeDriver::default(),
            real: ProcessDriver::new(ServiceKind::User, Duration::from_millis(20)),
            real_failed: AtomicBool::new(false),
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (replies_tx, mut replies) = tokio::sync::mpsc::unbounded_channel();
        let released = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        let script = async {
            until("A", || driver.fake.spawned.load(Ordering::SeqCst) == 1).await;
            // B's kept binary is the probe itself, with a live pin for it.
            let probe = Path::new(env!("CARGO_BIN_EXE_worker_probe"));
            let sha = hash_file(probe).unwrap();
            let kept = rig.cfg.data.binary("2.0.0", NAME);
            std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
            std::fs::copy(probe, &kept).unwrap();
            let pin = open_ferry_update::pins::PinStore::new(&rig.cfg.data.versions())
                .create("2.0.0", &sha, chrono::Utc::now())
                .unwrap();
            let gate_dir = data.join(".blue-green-test");
            std::fs::create_dir_all(&gate_dir).unwrap();
            let gate = gate_dir.join("termination-timeout.hold");
            std::fs::write(&gate, "yes").unwrap();
            let request = || SwitchRequest {
                to: "2.0.0".to_owned(),
                sha256: sha.clone(),
                pin: pin.id.clone(),
                how: How::Update,
                auto: false,
                admitted: Admission {
                    while_off: false,
                    mode_gen: 0,
                },
            };
            let sent = std::time::Instant::now();
            tx.send(Command::Switch {
                request: request(),
                replies: replies_tx.clone(),
            })
            .unwrap();
            if matches!(how, How2::Incompatible | How2::Stop) {
                // Incompatible: the probe answers at once. Stop: it stalls.
                until("the probe", || config.join("pid").exists()).await;
            }
            if matches!(how, How2::Stop) {
                tx.send(Command::Stop).unwrap();
            }
            // The cleanup is held at the gate: the child is alive.
            until("the held termination", || {
                gate_dir.join("termination-timeout.arrived").exists()
            })
            .await;
            let child: u32 = std::fs::read_to_string(config.join("pid"))
                .expect("the child existed before the cleanup")
                .trim()
                .parse()
                .unwrap();
            if matches!(how, How2::Timeout) {
                assert!(
                    sent.elapsed() >= Duration::from_secs(5),
                    "the handshake timeout fired early"
                );
            }
            if matches!(how, How2::Incompatible) {
                // Inspect only once the driver has returned its error.
                until("the driver's error", || {
                    driver.real_failed.load(Ordering::SeqCst)
                })
                .await;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert!(
                !finished.load(Ordering::SeqCst),
                "the supervisor ended while the child lived"
            );
            // No terminal reply of any kind yet, `Error` included.
            let early: Vec<EndpointReply> =
                std::iter::from_fn(|| replies.try_recv().ok()).collect();
            assert!(
                !early.iter().any(|reply| matches!(
                    reply,
                    EndpointReply::Done { .. } | EndpointReply::Error { .. }
                )),
                "the switch answered while the child lived: {early:?}"
            );
            // J1 is on disk: the switch is still open.
            let journal = open_ferry_update::journal::JournalStore::new(&data)
                .load()
                .unwrap();
            assert!(
                matches!(journal, open_ferry_update::journal::JournalLoad::Loaded(ref j) if j.switch.is_some()),
                "J1 was cleared while the child lived"
            );
            // The status shows the switch and the child, not as active or standby.
            let shown = super::status(rig.cfg.test_endpoint_location.clone(), &data).await;
            assert!(!shown["switch"].is_null(), "{shown}");
            let survivor = shown["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["pid"] == child)
                .unwrap_or_else(|| panic!("the child is not in the status: {shown}"));
            assert_eq!(survivor["role"], "survivor", "{shown}");
            // Nothing else starts: a second switch is refused as busy.
            let (second_tx, mut second) = tokio::sync::mpsc::unbounded_channel();
            tx.send(Command::Switch {
                request: request(),
                replies: second_tx,
            })
            .unwrap();
            let refused = tokio::time::timeout(Duration::from_secs(10), second.recv())
                .await
                .expect("a reply to the second switch");
            assert!(
                matches!(&refused, Some(EndpointReply::Error { reason, .. }) if reason.contains("busy")),
                "{refused:?}"
            );
            released.store(true, Ordering::SeqCst);
            std::fs::remove_file(gate).unwrap();
            // The switch ends once the child is gone.
            let terminal = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let reply @ (EndpointReply::Done { .. } | EndpointReply::Error { .. }) =
                        replies.recv().await.expect("the switch's answer")
                    {
                        break reply;
                    }
                }
            })
            .await
            .expect("the answer after the child's exit");
            match (&how, &terminal) {
                (How2::Stop, EndpointReply::Error { reason, .. }) => {
                    assert!(reason.contains("stopping"), "{reason}");
                }
                (How2::Timeout, EndpointReply::Done { result, .. }) => {
                    assert_eq!(result["result"], "failed-to-load", "{result}");
                    assert_eq!(result["blame"], "new-version", "{result}");
                    assert!(
                        result["why"].as_str().unwrap().contains("load timeout"),
                        "{result}"
                    );
                }
                (How2::Incompatible, EndpointReply::Done { result, .. }) => {
                    assert_eq!(result["result"], "failed-to-load", "{result}");
                    assert_eq!(result["blame"], "environment", "{result}");
                }
                (_, other) => panic!("the wrong answer: {other:?}"),
            }
            if !matches!(how, How2::Stop) {
                tx.send(Command::Stop).unwrap();
            }
        };
        let (exit, ()) = tokio::join!(
            async {
                let exit = Box::pin(supervise(
                    &rig.cfg,
                    &rig.host,
                    &driver,
                    &SystemClock,
                    tx.clone(),
                    rx,
                    default_limit(),
                ))
                .await;
                assert!(
                    released.load(Ordering::SeqCst),
                    "the supervisor ended before the child's exit was possible"
                );
                finished.store(true, Ordering::SeqCst);
                exit
            },
            script
        );
        assert_eq!(exit.unwrap(), Exit::Stopped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_during_a_real_handshake_waits_for_the_childs_exit() {
        Box::pin(run(How2::Stop)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timed_out_real_handshake_waits_for_the_childs_exit() {
        Box::pin(run(How2::Timeout)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_real_handshake_waits_for_the_childs_exit() {
        Box::pin(run(How2::Incompatible)).await;
    }
}

/// Not upstream's: separate roots coexist even in the same runtime/process.
#[tokio::test]
async fn two_supervisors_bind_independently_and_stopping_one_leaves_the_other_answering() {
    let _turn = take_turn();
    let a = rig_under_gate();
    let b = rig_under_gate();
    let da = FakeDriver::default();
    let db = FakeDriver::default();
    let (ta, ra) = tokio::sync::mpsc::unbounded_channel();
    let (tb, rb) = tokio::sync::mpsc::unbounded_channel();
    let observer = async {
        until("both workers", || {
            da.spawned.load(Ordering::SeqCst) == 1 && db.spawned.load(Ordering::SeqCst) == 1
        })
        .await;
        let sa = status(a.cfg.test_endpoint_location.clone(), a.cfg.data.root()).await;
        let sb = status(b.cfg.test_endpoint_location.clone(), b.cfg.data.root()).await;
        assert_eq!(sa["state"], "idle");
        assert_eq!(sb["state"], "idle");
        ta.send(Command::Stop).unwrap();
    };
    let first = async {
        let (exit, ()) = tokio::join!(
            supervise(
                &a.cfg,
                &a.host,
                &da,
                &SystemClock,
                ta.clone(),
                ra,
                default_limit()
            ),
            observer
        );
        assert_eq!(exit.unwrap(), Exit::Stopped);
        let still = status(b.cfg.test_endpoint_location.clone(), b.cfg.data.root()).await;
        assert_eq!(still["state"], "idle");
        tb.send(Command::Stop).unwrap();
    };
    let ((), exit) = tokio::join!(
        first,
        supervise(
            &b.cfg,
            &b.host,
            &db,
            &SystemClock,
            tb.clone(),
            rb,
            default_limit()
        )
    );
    assert_eq!(exit.unwrap(), Exit::Stopped);
}
