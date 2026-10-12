//! Not upstream's: supervisor startup over temporary data directories, with a
//! fake host, runner and driver (v3 10.4, T6, 3.3, M9).

use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use open_ferry_supervisor::clock::SystemClock;
use open_ferry_supervisor::core::Command;
use open_ferry_supervisor::driver::{
    Driver, EventSender, Registry, SpawnError, SpawnRequest, WorkerPort,
};
use open_ferry_supervisor::endpoint::{Location, ServiceKind};
use open_ferry_supervisor::host::{Host, ModeSnapshot};
use open_ferry_supervisor::process::ProcessIdentity;
use open_ferry_supervisor::protocol::ListenerTopology;
use open_ferry_supervisor::startup::{
    Config, EarlyCommands, StartupError, reenter, start, start_watching,
};
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
    sha: Option<String>,
    waits: AtomicUsize,
    /// Runs once, when the sockets are bound: after recovery, before the
    /// start-up try is counted.
    on_bind: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Host for FakeHost {
    fn topology(&self) -> Vec<ListenerTopology> {
        Vec::new()
    }
    fn bind(&self) -> io::Result<Vec<(String, TcpListener)>> {
        if let Some(hook) = self.on_bind.lock().unwrap().take() {
            hook();
        }
        Ok(Vec::new())
    }
    fn effective_mode(&self) -> ModeSnapshot {
        ModeSnapshot {
            off: false,
            mode_gen: 0,
        }
    }
    fn own_version(&self) -> String {
        "1.0.0".to_owned()
    }
    fn own_sha256(&self) -> Option<String> {
        self.sha.clone()
    }
    fn waiting_for_update_lock(&self, _waited: Duration) {
        self.waits.fetch_add(1, Ordering::SeqCst);
    }
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

#[derive(Default)]
struct FakeDriver {
    requests: Mutex<Vec<SpawnRequest>>,
    locked_data: Option<PathBuf>,
}

impl Driver for FakeDriver {
    fn spawn(
        &self,
        request: SpawnRequest,
        _events: EventSender,
    ) -> impl std::future::Future<Output = Result<WorkerPort, SpawnError>> + Send {
        if let Some(root) = &self.locked_data {
            assert!(
                DataDir::at(root).try_lock().unwrap().is_none(),
                "spawn without the update lock"
            );
        }
        let (cmd, _rx) = tokio::sync::mpsc::unbounded_channel();
        let port = WorkerPort {
            id: request.id,
            identity: ProcessIdentity {
                pid: 4242,
                started: 7,
            },
            reported_version: request.version.clone(),
            protocol: 1,
            caps: Vec::new(),
            cmd,
        };
        self.requests.lock().unwrap().push(request);
        async move { Ok(port) }
    }
}

/// On Unix the endpoint is one per user and its namespace lock is held for as
/// long as a supervisor lives, so tests that start one take turns.
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
    _turn: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    _sockets: Option<tempfile::TempDir>,
    cfg: Config,
    host: FakeHost,
    installed: PathBuf,
}

/// A fresh directory for one test. The data directory's trust check looks at
/// every ancestor, so the directory must not sit under the source tree (owned
/// by the host user when a container runs as root) nor under `/tmp` (anyone can
/// write to it). Off Windows it is under the user's home directory; Windows'
/// temp directory does not pass the check either, so there it stays under the
/// target directory the user owns, or under `OFP_TEST_SCRATCH` when set (CI's
/// runner, whose drive root is owned by NETWORK SERVICE).
fn scratch() -> tempfile::TempDir {
    if let Some(parent) = std::env::var_os("OFP_T5_CHILD_SCRATCH") {
        return tempfile::tempdir_in(parent).unwrap();
    }
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
    let location = Location::new(dir.path(), "e").unwrap();
    (Some(location), Some(dir))
}

fn rig() -> Rig {
    let turn = take_turn();
    let dir = scratch();
    let root = dir.path().join("data");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let installed = bin.join(NAME);
    std::fs::write(&installed, b"binary one").unwrap();
    let sha = hash_file(&installed).unwrap();
    let timings = Timings {
        lock_warn_every: Duration::from_millis(60),
        lock_poll: Duration::from_millis(10),
        ..Timings::default()
    };
    let (endpoint_location, sockets) = own_endpoint();
    let cfg = Config {
        data: DataDir::at(root),
        installed: installed.clone(),
        config: dir.path().join("config.yaml"),
        kind: ServiceKind::User,
        timings,
        receipt: serde_json::json!({}),
        binary_name: NAME.to_owned(),
        runner: Arc::new(FakeRunner),
        detect_timeout: Duration::from_secs(5),
        endpoint_location,
    };
    Rig {
        _turn: turn,
        _dir: dir,
        _sockets: sockets,
        cfg,
        host: FakeHost {
            sha: Some(sha),
            waits: AtomicUsize::new(0),
            on_bind: Mutex::new(None),
        },
        installed,
    }
}

/// `Config::endpoint_location` is where the endpoint is bound and what
/// `supervisor.json` records: an injected location is used as given, and the
/// default (per-user on Unix) is not resolved, bound or recorded.
#[cfg(unix)]
#[tokio::test]
async fn an_injected_endpoint_location_is_bound_and_recorded_and_not_the_default() {
    use open_ferry_supervisor::endpoint::{Identity, connect};
    let rig = rig();
    let own = rig.cfg.endpoint_location.clone().expect("an own location");
    let identity = Identity::current(ServiceKind::User).unwrap();
    let default = Location::resolve(&identity, rig.cfg.data.root()).unwrap();
    assert_ne!(own.to_record(), default.to_record());
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("start");
    assert!(
        started.endpoint.is_some(),
        "{:?}",
        started.switching_disabled
    );
    let record = std::fs::read_to_string(rig.cfg.data.root().join("supervisor.json")).unwrap();
    assert!(record.contains(&own.to_record()), "{record}");
    assert!(!record.contains(&default.to_record()), "{record}");
    // The injected socket is the one a client reaches.
    assert!(connect(&own, &identity).await.is_ok());
    assert!(std::path::Path::new(&own.to_record()).exists());
}

#[tokio::test]
async fn a_first_start_keeps_a_copy_writes_the_journal_and_runs_from_it() {
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("start");
    assert_eq!(started.switching_disabled, None);
    let run = started.run.clone().expect("a run");
    assert_eq!(run.artifact.version, "1.0.0");
    assert_eq!(run.binary, rig.cfg.data.binary("1.0.0", NAME));
    let on_disk = open_ferry_update::journal::JournalStore::new(rig.cfg.data.root())
        .load()
        .unwrap();
    let open_ferry_update::journal::JournalLoad::Loaded(on_disk) = on_disk else {
        panic!("journal not written")
    };
    assert_eq!(on_disk.workers.len(), 1, "spawn was not journalled");
    assert!(rig.cfg.data.root().join("supervisor.json").exists());
    assert!(started.endpoint.is_some());
    let requests = driver.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].binary, run.binary);
    assert_eq!(started.worker, Some(requests[0].id));
    let journal = started.journal.expect("journal");
    assert_eq!(journal.workers.len(), 1);
}

#[tokio::test]
async fn launch_replaces_stale_standby_and_draining_worker_records() {
    use open_ferry_update::journal::{JournalStore, WorkerRole};
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let mut journal = first.journal.clone().unwrap();
    let mut stale = journal.workers[0].clone();
    stale.pid = 9991;
    stale.role = WorkerRole::Standby;
    journal.workers.push(stale.clone());
    stale.pid = 9992;
    stale.role = WorkerRole::Draining;
    journal.workers.push(stale);
    JournalStore::new(rig.cfg.data.root())
        .write(&mut journal, "workers")
        .unwrap();
    drop(first);
    let mut registry = Registry::new();
    let second = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let workers = second.journal.unwrap().workers;
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].role, WorkerRole::Active);
    assert_eq!(workers[0].pid, 4242);
}

#[tokio::test]
async fn a_newer_journal_runs_the_installed_file_and_changes_nothing() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let journal = rig
        .cfg
        .data
        .root()
        .join(open_ferry_update::journal::JOURNAL_FILE);
    let text = br#"{"format": 9999}"#;
    std::fs::write(&journal, text).unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("start");
    assert!(started.switching_disabled.is_some());
    assert!(started.endpoint.is_some());
    assert!(rig.cfg.data.root().join("supervisor.json").exists());
    assert!(started.journal.is_none());
    assert_eq!(std::fs::read(&journal).unwrap(), text);
    assert_eq!(started.run.expect("run").binary, rig.installed);
    assert_eq!(driver.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_installed_file_that_is_not_ours_with_no_journal_runs_nothing() {
    let mut rig = rig();
    rig.host.sha = Some("0".repeat(64));
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let journal = rig
        .cfg
        .data
        .root()
        .join(open_ferry_update::journal::JOURNAL_FILE);
    std::fs::write(&journal, br#"{"format": 9999}"#).unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("start");
    assert!(started.run.is_none());
    assert!(started.degraded.is_some());
    assert!(driver.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_second_supervisor_is_refused() {
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("start");
    let mut other = Registry::new();
    let second = start(&rig.cfg, &rig.host, &driver, &mut other, &SystemClock).await;
    assert!(matches!(second, Err(StartupError::AlreadyRunning)));
    drop(first);
}

/// T6: with the update lock held elsewhere, startup waits without limit,
/// warns, calls the host, and does nothing at all until it is released.
#[tokio::test]
async fn startup_waits_for_the_update_lock_and_does_nothing_before_it() {
    let rig = Arc::new(rig());
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let held = rig.cfg.data.try_lock().unwrap().expect("lock");
    let driver = Arc::new(FakeDriver::default());
    let task = {
        let (rig, driver) = (rig.clone(), driver.clone());
        tokio::spawn(async move {
            let mut registry = Registry::new();
            start(&rig.cfg, &rig.host, &*driver, &mut registry, &SystemClock)
                .await
                .map(|started| started.run.is_some())
        })
    };
    // Longer than several warning intervals (a real 40 s wait is the same
    // path; only the interval is short).
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.host.waits.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("three lock warnings");
    assert!(!task.is_finished(), "startup gave up waiting for the lock");
    assert!(
        rig.host.waits.load(Ordering::SeqCst) >= 3,
        "the host was not told"
    );
    assert!(!rig.cfg.data.root().join("supervisor.json").exists());
    assert!(
        !rig.cfg
            .data
            .root()
            .join(open_ferry_update::journal::JOURNAL_FILE)
            .exists()
    );
    assert!(!rig.cfg.data.versions().exists());
    assert!(driver.requests.lock().unwrap().is_empty());
    drop(held);
    let ran = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("finishes once the lock is free")
        .unwrap()
        .expect("start");
    assert!(ran);
    assert_eq!(driver.requests.lock().unwrap().len(), 1);
}

fn switch_to(to: &str) -> Command {
    use open_ferry_supervisor::core::SwitchRequest;
    use open_ferry_supervisor::protocol::Admission;
    use open_ferry_update::journal::How;
    let (replies, _) = tokio::sync::mpsc::unbounded_channel();
    Command::Switch {
        request: SwitchRequest {
            to: to.to_owned(),
            sha256: String::new(),
            pin: String::new(),
            how: How::Update,
            auto: false,
            admitted: Admission {
                while_off: false,
                mode_gen: 0,
            },
        },
        replies,
    }
}

/// A stop that arrives while startup waits for the update lock ends the start
/// with nothing spawned and no lock held, and the commands before it are kept,
/// in order, for the core.
#[tokio::test]
async fn a_stop_during_the_update_lock_wait_ends_the_start_with_nothing_started() {
    let rig = Arc::new(rig());
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let held = rig.cfg.data.try_lock().unwrap().expect("lock");
    let driver = Arc::new(FakeDriver::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = {
        let (rig, driver) = (rig.clone(), driver.clone());
        tokio::spawn(async move {
            let mut registry = Registry::new();
            let mut early = EarlyCommands::new(&mut rx);
            let result = start_watching(
                &rig.cfg,
                &rig.host,
                &*driver,
                &mut registry,
                &SystemClock,
                Some(&mut early),
            )
            .await;
            (result.map(|_| ()), early.into_kept().len())
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.host.waits.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the wait began");
    tx.send(switch_to("a")).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!task.is_finished(), "a command that is not a stop ended it");
    tx.send(Command::Stop).unwrap();
    let (result, kept) = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("a stop ends the wait")
        .unwrap();
    assert!(matches!(result, Err(StartupError::Stopped)), "{result:?}");
    assert_eq!(kept, 1, "the command before the stop is kept");
    assert!(driver.requests.lock().unwrap().is_empty());
    assert!(!rig.cfg.data.root().join("supervisor.json").exists());
    // The update lock is the one held here, and the supervisor lock is free.
    assert!(rig.cfg.data.try_lock().unwrap().is_none());
    drop(held);
    assert!(rig.cfg.data.try_lock().unwrap().is_some(), "lock leaked");
    assert!(
        open_ferry_supervisor::process::SupervisorLock::try_acquire(rig.cfg.data.root()).is_ok(),
        "the supervisor lock was kept"
    );
}

/// The commands that are not a stop come back in the order they were sent,
/// those taken during the wait first, then those still queued.
#[tokio::test]
async fn commands_during_the_update_lock_wait_are_kept_in_order() {
    let rig = Arc::new(rig());
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let held = rig.cfg.data.try_lock().unwrap().expect("lock");
    let driver = Arc::new(FakeDriver::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = {
        let (rig, driver) = (rig.clone(), driver.clone());
        tokio::spawn(async move {
            let mut registry = Registry::new();
            let mut early = EarlyCommands::new(&mut rx);
            let started = start_watching(
                &rig.cfg,
                &rig.host,
                &*driver,
                &mut registry,
                &SystemClock,
                Some(&mut early),
            )
            .await
            .expect("start");
            drop(started);
            early
                .into_kept()
                .into_iter()
                .map(|command| match command {
                    Command::Switch { request, .. } => request.to,
                    Command::Stop => "stop".to_owned(),
                })
                .collect::<Vec<_>>()
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.host.waits.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the wait began");
    // Both are queued before the lock is let go, so startup, which takes
    // what is queued when it holds the lock, has them by the time it commits:
    // no wait for their consumption is needed.
    tx.send(switch_to("a")).unwrap();
    tx.send(switch_to("b")).unwrap();
    drop(held);
    let kept = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("finishes once the lock is free")
        .unwrap();
    assert_eq!(kept, ["a", "b"]);
    assert_eq!(driver.requests.lock().unwrap().len(), 1);
}

/// A stop that is queued when the lock is free is not lost to a lock that has
/// just become free: startup takes what is queued before it commits, and ends
/// with nothing spawned or written.
#[tokio::test]
async fn a_stop_queued_when_the_update_lock_is_free_ends_the_start() {
    let rig = rig();
    let driver = FakeDriver::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(switch_to("a")).unwrap();
    tx.send(Command::Stop).unwrap();
    let mut registry = Registry::new();
    let mut early = EarlyCommands::new(&mut rx);
    let result = start_watching(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut registry,
        &SystemClock,
        Some(&mut early),
    )
    .await;
    assert!(
        matches!(result, Err(StartupError::Stopped)),
        "{:?}",
        result.map(|_| ())
    );
    assert_eq!(early.into_kept().len(), 1, "the command before the stop");
    assert!(driver.requests.lock().unwrap().is_empty());
    assert!(!rig.cfg.data.root().join("supervisor.json").exists());
    assert!(
        rig.cfg.data.try_lock().unwrap().is_some(),
        "the update lock was kept"
    );
    assert!(
        open_ferry_supervisor::process::SupervisorLock::try_acquire(rig.cfg.data.root()).is_ok(),
        "the supervisor lock was kept"
    );
}

/// The fallback after a start-up count that could not be written waits for the
/// update lock too, with no worker yet: a stop there ends the start with
/// nothing spawned and the locks let go.
#[tokio::test]
async fn a_stop_during_the_fallback_lock_wait_ends_the_start() {
    let rig = Arc::new(rig());
    let driver = Arc::new(FakeDriver::default());
    let mut registry = Registry::new();
    // A first start writes the journal.
    drop(
        start(&rig.cfg, &rig.host, &*driver, &mut registry, &SystemClock)
            .await
            .expect("start"),
    );
    assert_eq!(driver.requests.lock().unwrap().len(), 1);
    // Startup's own launch binds the sockets after recovery and before the
    // start-up try is counted. There the journal becomes unwritable, so the
    // count fails and the launch falls back to the installed file, and the
    // update lock is held by someone else.
    let held: Arc<Mutex<Option<Box<dyn std::any::Any + Send>>>> = Arc::new(Mutex::new(None));
    let root = rig.cfg.data.root().to_path_buf();
    *rig.host.on_bind.lock().unwrap() = Some(Box::new({
        let held = held.clone();
        move || {
            std::fs::write(
                root.join(open_ferry_update::journal::JOURNAL_FILE),
                b"not a journal",
            )
            .unwrap();
            let lock = DataDir::at(&root).try_lock().unwrap().expect("lock");
            *held.lock().unwrap() = Some(Box::new(lock));
        }
    }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = {
        let (rig, driver) = (rig.clone(), driver.clone());
        tokio::spawn(async move {
            let mut registry = Registry::new();
            let mut early = EarlyCommands::new(&mut rx);
            start_watching(
                &rig.cfg,
                &rig.host,
                &*driver,
                &mut registry,
                &SystemClock,
                Some(&mut early),
            )
            .await
            .map(|_| ())
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.host.waits.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the fallback waits for the lock");
    tx.send(Command::Stop).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("a stop ends the fallback's wait")
        .unwrap();
    assert!(matches!(result, Err(StartupError::Stopped)), "{result:?}");
    assert_eq!(driver.requests.lock().unwrap().len(), 1, "nothing spawned");
    held.lock().unwrap().take();
    assert!(rig.cfg.data.try_lock().unwrap().is_some(), "lock leaked");
}

/// A stop that is queued before startup begins also ends it when the update
/// lock cannot be opened at all and startup would otherwise run on without
/// it.
#[tokio::test]
async fn a_queued_stop_ends_the_start_when_the_update_lock_cannot_be_opened() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    make_untrusted(rig.cfg.data.root());
    let target = rig._dir.path().join("must-not-be-created");
    let lock = rig.cfg.data.root().join("update.lock");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &lock).unwrap();
    #[cfg(windows)]
    {
        let result = std::process::Command::new(std::env::var_os("COMSPEC").unwrap())
            .args(["/c", "mklink", "/J"])
            .arg(&lock)
            .arg(&target)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }
    let driver = FakeDriver::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(Command::Stop).unwrap();
    let mut registry = Registry::new();
    let mut early = EarlyCommands::new(&mut rx);
    let result = start_watching(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut registry,
        &SystemClock,
        Some(&mut early),
    )
    .await;
    assert!(
        matches!(result, Err(StartupError::Stopped)),
        "{:?}",
        result.map(|_| ())
    );
    assert!(!target.exists());
    assert!(driver.requests.lock().unwrap().is_empty(), "a worker ran");
    assert!(!rig.cfg.data.root().join("supervisor.json").exists());
}

/// 3.3: a restart finds the installed file gone and puts the confirmed
/// version's kept copy back, then runs from `versions/`.
#[tokio::test]
async fn a_restart_restores_a_missing_installed_file_from_the_kept_copy() {
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("first");
    drop(first);
    std::fs::remove_file(&rig.installed).unwrap();
    let mut registry = Registry::new();
    let second = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .expect("second");
    assert!(
        rig.installed.exists(),
        "the installed file was not restored"
    );
    assert_eq!(
        hash_file(&rig.installed).unwrap(),
        second.run.expect("run").artifact.sha256
    );
}

fn make_untrusted(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    #[cfg(windows)]
    {
        let result = std::process::Command::new("icacls")
            .arg(path)
            .args(["/grant", "*S-1-1-0:(W)"])
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn disabled_startup_never_reads_a_fifo_in_versions() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let rig = rig();
    let version = rig.cfg.data.version_dir("1.0.0");
    std::fs::create_dir_all(&version).unwrap();
    let fifo = rig.cfg.data.binary("1.0.0", NAME);
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and belongs to this fixture.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    make_untrusted(rig.cfg.data.root());
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock),
    )
    .await;
    // Release the writer even on failure, so a reverted blocking hash can finish.
    drop(held);
    let started = result
        .expect("disabled startup must not read versions")
        .unwrap();
    assert!(started.switching_disabled.is_some());
    assert_eq!(started.run.unwrap().binary, rig.installed);
}

#[tokio::test]
async fn an_untrusted_selected_version_runs_only_the_installed_file() {
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    drop(first);
    let store = open_ferry_update::journal::JournalStore::new(rig.cfg.data.root());
    let before = std::fs::read(store.path()).unwrap();
    make_untrusted(&rig.cfg.data.version_dir("1.0.0"));
    let mut registry = Registry::new();
    let second = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(second.switching_disabled.is_some());
    assert!(second.journal.is_none());
    assert_eq!(second.run.unwrap().binary, rig.installed);
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert_eq!(
        driver.requests.lock().unwrap().last().unwrap().binary,
        rig.installed
    );
}

fn recovery_listing(root: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, dir: &Path, paths: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap();
            if ["supervisor.lock", "update.lock", "supervisor.json"]
                .iter()
                .any(|name| relative == Path::new(name))
            {
                continue;
            }
            paths.push(relative.to_path_buf());
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, paths);
            }
        }
    }
    let mut paths = Vec::new();
    visit(root, root, &mut paths);
    paths.sort();
    paths
}

#[tokio::test]
async fn an_untrusted_journal_runs_only_our_installed_file_without_recovery_writes() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    let journal = rig
        .cfg
        .data
        .root()
        .join(open_ferry_update::journal::JOURNAL_FILE);
    let original = b"unreadable journal: must not quarantine or stage";
    std::fs::write(&journal, original).unwrap();
    make_untrusted(&journal);
    // Runtime discovery and lifetime locks are expected; every recovery path
    // beneath the root is otherwise unchanged, including unexpected new paths.
    let before = recovery_listing(rig.cfg.data.root());
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(
        started
            .switching_disabled
            .as_ref()
            .unwrap()
            .contains("not trusted")
    );
    assert!(started.journal.is_none());
    assert_eq!(started.run.unwrap().binary, rig.installed);
    assert_eq!(std::fs::read(&journal).unwrap(), original);
    assert_eq!(recovery_listing(rig.cfg.data.root()), before);
    assert!(!rig.cfg.data.versions().exists());
    assert!(!rig.cfg.data.root().join("update-state.json").exists());
    assert_eq!(std::fs::read(&rig.installed).unwrap(), b"binary one");
}

#[tokio::test]
async fn reentry_rechecks_directories_and_never_stages_into_an_untrusted_root() {
    let mut rig = rig();
    rig.cfg.timings.stop = Duration::ZERO;
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let mut started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let worker = started.worker.unwrap();
    registry
        .events()
        .send((
            worker,
            open_ferry_supervisor::driver::Event::Exited(open_ferry_supervisor::driver::ExitInfo {
                code: Some(0),
                success: true,
                text: "exit".into(),
            }),
        ))
        .unwrap();
    std::fs::remove_file(
        rig.cfg
            .data
            .root()
            .join(open_ferry_update::journal::JOURNAL_FILE),
    )
    .unwrap();
    make_untrusted(rig.cfg.data.root());
    reenter(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut registry,
        &SystemClock,
        &mut started,
    )
    .await
    .unwrap();
    assert!(started.switching_disabled.is_some());
    assert!(started.journal.is_none());
    assert_eq!(started.run.unwrap().binary, rig.installed);
    assert!(
        !rig.cfg
            .data
            .root()
            .join(open_ferry_update::journal::JOURNAL_FILE)
            .exists()
    );
}

#[tokio::test]
async fn reentry_waits_for_a_kill_survivors_exit_before_recovery_or_launch() {
    use open_ferry_supervisor::driver::{Cmd, Event, ExitInfo};
    let mut rig = rig();
    rig.cfg.timings.stop = Duration::ZERO;
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let mut started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let id = started.worker.unwrap();
    let mut port = registry.port(id).unwrap().clone();
    let (cmd, mut commands) = tokio::sync::mpsc::unbounded_channel();
    port.cmd = cmd;
    registry.add(port);
    let events = registry.events();
    let journal = rig
        .cfg
        .data
        .root()
        .join(open_ferry_update::journal::JOURNAL_FILE);
    let before = std::fs::read(&journal).unwrap();
    let reentry = reenter(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut registry,
        &SystemClock,
        &mut started,
    );
    tokio::pin!(reentry);
    tokio::select! {
        result = &mut reentry => panic!("reentry ended before kill: {result:?}"),
        () = async { while commands.recv().await != Some(Cmd::Kill) {} } => {}
    }
    // The kill did not end the fake worker. Drive reentry beyond its kill wait.
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut reentry)
            .await
            .is_err()
    );
    assert_eq!(driver.requests.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    events
        .send((
            id,
            Event::Exited(ExitInfo {
                code: Some(0),
                success: true,
                text: "exit".into(),
            }),
        ))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), reentry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(driver.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_untrusted_directory_still_holds_both_startup_locks() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    make_untrusted(rig.cfg.data.root());
    let update = rig.cfg.data.try_lock().unwrap().unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let future = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("started without the update lock: {}", result.is_ok()),
        () = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while rig.host.waits.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await; }
            }).await.unwrap();
        } => {}
    }
    assert!(
        matches!(open_ferry_supervisor::process::SupervisorLock::try_acquire(rig.cfg.data.root()), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
    );
    assert!(driver.requests.lock().unwrap().is_empty());
    assert!(!rig.cfg.data.root().join("supervisor.json").exists());
    drop(update);
    let started = future.await.unwrap();
    assert!(started.lock.is_some());
    assert!(started.switching_disabled.is_some());
    assert_eq!(started.run.unwrap().binary, rig.installed);
}

#[tokio::test]
async fn an_untrusted_installed_directory_disables_switching_before_staging() {
    let rig = rig();
    make_untrusted(rig.installed.parent().unwrap());
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(started.switching_disabled.is_some());
    assert!(started.journal.is_none());
    assert_eq!(started.run.unwrap().binary, rig.installed);
    assert!(!rig.cfg.data.versions().exists());
}

#[tokio::test]
async fn a_failed_startup_install_records_failure_and_uses_the_configured_first_retry() {
    use open_ferry_update::journal::{Artifact, InstallPending, Installed, Journal, JournalStore};
    let mut rig = rig();
    rig.cfg.timings.install_retry = Duration::from_secs(10);
    std::fs::create_dir_all(rig.cfg.data.version_dir("2.0.0")).unwrap();
    let binary = rig.cfg.data.binary("2.0.0", NAME);
    std::fs::write(&binary, b"binary two").unwrap();
    let target = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&binary).unwrap(),
        size: 10,
    };
    let mut journal = Journal::first_start(
        target.clone(),
        Some(Installed {
            version: "1.0.0".into(),
            sha256: rig.host.sha.clone().unwrap(),
        }),
    );
    journal.install_pending = Some(InstallPending {
        target,
        source: binary.to_string_lossy().into_owned(),
        aside: String::new(),
        attempts: 0,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: Some("install-test".into()),
    });
    JournalStore::new(rig.cfg.data.root())
        .write(&mut journal, "test")
        .unwrap();
    let mut state = open_ferry_update::state::State::default();
    state.upsert_record(open_ferry_update::state::SwitchRecord {
        id: "install-test".into(),
        from: "1.0.0".into(),
        to: "2.0.0".into(),
        at: chrono::Utc::now().to_rfc3339(),
        confirmed: true,
        result: Some("switched".into()),
        installed_binary: Some("pending".into()),
        ..Default::default()
    });
    state.save(&rig.cfg.data.state_file()).unwrap();
    // Occupy every unique copy name, so preparation genuinely fails on both OSes.
    for n in 0..100 {
        let suffix = if n == 0 {
            format!(".new.{}", std::process::id())
        } else {
            format!(".new.{}.{n}", std::process::id())
        };
        let mut name = rig.installed.as_os_str().to_owned();
        name.push(suffix);
        std::fs::create_dir(PathBuf::from(name)).unwrap();
    }
    let now = chrono::Utc::now();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let pending = started.journal.unwrap().install_pending.unwrap();
    assert_eq!(pending.attempts, 1);
    let wait = chrono::DateTime::parse_from_rfc3339(pending.next_try.as_ref().unwrap())
        .unwrap()
        .signed_duration_since(now);
    assert!(
        (9..=12).contains(&wait.num_seconds()),
        "wrong retry: {wait}"
    );
    let state = open_ferry_update::state::State::load(&rig.cfg.data.state_file());
    let record = state
        .history
        .iter()
        .find(|record| record.id == "install-test")
        .unwrap();
    assert!(
        record
            .installed_binary
            .as_ref()
            .unwrap()
            .starts_with("failed: ")
    );
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_install_failure_delivers_the_full_outcome_after_the_first_replay_failed() {
    use open_ferry_update::journal::{
        Artifact, InstallPending, Installed, Journal, JournalStore, Outcome,
    };
    use open_ferry_update::state::{State, SwitchRecord};
    let rig = Arc::new(rig());
    std::fs::create_dir_all(rig.cfg.data.version_dir("2.0.0")).unwrap();
    let binary = rig.cfg.data.binary("2.0.0", NAME);
    std::fs::write(&binary, b"binary two").unwrap();
    let target = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&binary).unwrap(),
        size: 10,
    };
    let mut journal = Journal::first_start(
        target.clone(),
        Some(Installed {
            version: "1.0.0".into(),
            sha256: rig.host.sha.clone().unwrap(),
        }),
    );
    journal.install_pending = Some(InstallPending {
        target,
        source: binary.to_string_lossy().into_owned(),
        aside: String::new(),
        attempts: 0,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: Some("replay-install".into()),
    });
    journal.push_outcome(Outcome {
        record: SwitchRecord {
            id: "replay-install".into(),
            from: "1.0.0".into(),
            to: "2.0.0".into(),
            at: chrono::Utc::now().to_rfc3339(),
            confirmed: true,
            result: Some("switched".into()),
            installed_binary: Some("pending".into()),
            ..Default::default()
        },
        mark_failed: None,
    });
    JournalStore::new(rig.cfg.data.root())
        .write(&mut journal, "test")
        .unwrap();
    std::fs::create_dir(rig.cfg.data.state_file()).unwrap();
    for n in 0..100 {
        let suffix = if n == 0 {
            format!(".new.{}", std::process::id())
        } else {
            format!(".new.{}.{n}", std::process::id())
        };
        let mut name = rig.installed.as_os_str().to_owned();
        name.push(suffix);
        std::fs::create_dir(PathBuf::from(name)).unwrap();
    }
    let faults = rig.cfg.data.root().join(".blue-green-test");
    std::fs::create_dir(&faults).unwrap();
    std::fs::write(faults.join("startup.before_install.hold"), "yes").unwrap();
    let task = {
        let rig = rig.clone();
        tokio::spawn(async move {
            start(
                &rig.cfg,
                &rig.host,
                &FakeDriver::default(),
                &mut Registry::new(),
                &SystemClock,
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !faults.join("startup.before_install.arrived").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_dir(rig.cfg.data.state_file()).unwrap();
    std::fs::write(faults.join("startup.before_install.release"), "yes").unwrap();
    let first = task.await.unwrap().unwrap();
    let record = State::load(&rig.cfg.data.state_file())
        .history
        .into_iter()
        .find(|record| record.id == "replay-install")
        .expect("the full outcome reached state");
    assert_eq!(record.from, "1.0.0");
    assert_eq!(record.to, "2.0.0");
    assert!(record.confirmed);
    assert_eq!(record.result.as_deref(), Some("switched"));
    assert!(record.installed_binary.unwrap().starts_with("failed: "));
    drop(first);
    let second = start(
        &rig.cfg,
        &rig.host,
        &FakeDriver::default(),
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    let state = State::load(&rig.cfg.data.state_file());
    assert_eq!(
        state
            .history
            .iter()
            .filter(|record| record.id == "replay-install")
            .count(),
        1
    );
    assert!(
        state
            .history
            .iter()
            .any(|record| record.id == "replay-install"
                && record.confirmed
                && record.to == "2.0.0")
    );
    assert!(second.journal.unwrap().outcomes.is_empty());
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_journals_the_prepared_aside_and_cancellation_keeps_the_install_lock() {
    use open_ferry_update::journal::{
        Artifact, InstallPending, Installed, Journal, JournalLoad, JournalStore,
    };
    let rig = Arc::new(rig());
    std::fs::create_dir_all(rig.cfg.data.version_dir("2.0.0")).unwrap();
    let binary = rig.cfg.data.binary("2.0.0", NAME);
    std::fs::write(&binary, b"binary two").unwrap();
    let target = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&binary).unwrap(),
        size: 10,
    };
    let preferred = rig.installed.with_extension("conflicting-aside");
    std::fs::write(&preferred, b"not the installed binary").unwrap();
    let mut journal = Journal::first_start(
        target.clone(),
        Some(Installed {
            version: "1.0.0".into(),
            sha256: rig.host.sha.clone().unwrap(),
        }),
    );
    journal.install_pending = Some(InstallPending {
        target,
        source: binary.to_string_lossy().into_owned(),
        aside: preferred.to_string_lossy().into_owned(),
        attempts: 0,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: None,
    });
    let store = JournalStore::new(rig.cfg.data.root());
    store.write(&mut journal, "test").unwrap();
    let faults = rig.cfg.data.root().join(".blue-green-test");
    std::fs::create_dir(&faults).unwrap();
    std::fs::write(faults.join("journal.after_J-install.hold"), "yes").unwrap();
    let task = {
        let rig = rig.clone();
        tokio::spawn(async move {
            start(
                &rig.cfg,
                &rig.host,
                &FakeDriver::default(),
                &mut Registry::new(),
                &SystemClock,
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !faults.join("journal.after_J-install.arrived").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let JournalLoad::Loaded(on_disk) = store.load().unwrap() else {
        panic!("no prepared journal")
    };
    let actual = on_disk.install_pending.unwrap().aside;
    assert_ne!(actual, preferred.to_string_lossy());
    if cfg!(windows) {
        let mut expected = rig.installed.as_os_str().to_owned();
        expected.push(".old");
        assert_eq!(Path::new(&actual), Path::new(&expected));
        assert!(
            !Path::new(&actual).exists(),
            "the first rename ran before journalling"
        );
    } else {
        assert!(actual.is_empty(), "Unix needs no aside");
    }
    assert_eq!(std::fs::read(&rig.installed).unwrap(), b"binary one");
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert!(
        rig.cfg.data.try_lock().unwrap().is_none(),
        "cancelling recovery released the install lock"
    );
    std::fs::write(faults.join("journal.after_J-install.release"), "yes").unwrap();
    wait_for_free_update_lock(&rig.cfg.data).await;
    assert_eq!(std::fs::read(&rig.installed).unwrap(), b"binary two");
}

#[tokio::test]
async fn an_untrusted_root_never_creates_a_linked_update_lock_target_and_still_runs() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    make_untrusted(rig.cfg.data.root());
    let target = rig._dir.path().join("must-not-be-created");
    let lock = rig.cfg.data.root().join("update.lock");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &lock).unwrap();
    #[cfg(windows)]
    {
        let result = std::process::Command::new(std::env::var_os("COMSPEC").unwrap())
            .args(["/c", "mklink", "/J"])
            .arg(&lock)
            .arg(&target)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }
    let driver = FakeDriver::default();
    let started = start(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    assert!(!target.exists());
    assert!(started.switching_disabled.is_some());
    assert_eq!(started.run.unwrap().binary, rig.installed);
    assert!(
        started
            .notes
            .iter()
            .any(|note| note.contains("without the update lock"))
    );
    assert_eq!(driver.requests.lock().unwrap().len(), 1);
}

#[cfg(feature = "blue-green-test")]
async fn wait_for_free_update_lock(data: &DataDir) {
    // A concurrently forked child can retain the lock fd until exec, even with CLOEXEC.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(lock) = data.try_lock().unwrap() {
                drop(lock);
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the update lock becomes free after the fork-to-exec window");
}

#[tokio::test]
async fn disabled_start_and_reentry_hold_the_update_lock_through_spawn() {
    let rig = rig();
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    make_untrusted(rig.cfg.data.root());
    let driver = FakeDriver {
        locked_data: Some(rig.cfg.data.root().to_owned()),
        ..FakeDriver::default()
    };
    let mut registry = Registry::new();
    let mut started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(
        rig.cfg.data.try_lock().unwrap().is_some(),
        "startup leaked its update lock"
    );
    // This fake driver's old port owns no process. A fresh registry exercises
    // recovery without inventing an exit for a real child.
    let mut registry = Registry::new();
    reenter(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut registry,
        &SystemClock,
        &mut started,
    )
    .await
    .unwrap();
    assert_eq!(driver.requests.lock().unwrap().len(), 2);
    assert!(
        rig.cfg.data.try_lock().unwrap().is_some(),
        "reentry leaked its update lock"
    );
}

async fn refuses_untrusted_install_source(pending: bool) {
    use open_ferry_update::journal::{Artifact, InstallPending, Installed, Journal, JournalStore};
    let rig = rig();
    let data = &rig.cfg.data;
    std::fs::create_dir_all(data.version_dir("1.0.0")).unwrap();
    std::fs::create_dir_all(data.version_dir("2.0.0")).unwrap();
    std::fs::copy(&rig.installed, data.binary("1.0.0", NAME)).unwrap();
    std::fs::write(data.binary("2.0.0", NAME), b"binary two").unwrap();
    let old = Artifact {
        version: "1.0.0".to_owned(),
        sha256: rig.host.sha.clone().unwrap(),
        size: 10,
    };
    let confirmed = Artifact {
        version: "2.0.0".to_owned(),
        sha256: hash_file(&data.binary("2.0.0", NAME)).unwrap(),
        size: 10,
    };
    let mut journal = Journal::first_start(confirmed.clone(), Some(Installed::from(&old)));
    if pending {
        journal.install_pending = Some(InstallPending {
            target: confirmed,
            source: data.binary("2.0.0", NAME).to_string_lossy().into_owned(),
            started: chrono::Utc::now().to_rfc3339(),
            attempts: 0,
            aside: String::new(),
            next_try: None,
            record_id: None,
        });
    }
    let store = JournalStore::new(data.root());
    store.write(&mut journal, "fixture").unwrap();
    let before = std::fs::read(store.path()).unwrap();
    let version = if pending { "2.0.0" } else { "1.0.0" };
    make_untrusted(&data.version_dir(version));
    std::fs::remove_file(&rig.installed).unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(!rig.installed.exists(), "untrusted source was installed");
    assert!(
        started
            .switching_disabled
            .as_ref()
            .unwrap()
            .contains(version)
    );
    assert!(started.notes.iter().any(|note| note.contains("not safe")));
    if pending {
        assert!(started.journal.is_none());
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    } else {
        let selected = data.binary("2.0.0", NAME);
        assert_eq!(started.run.as_ref().unwrap().binary, selected);
        assert!(
            started.worker.is_some(),
            "trusted unaffected worker must start"
        );
        assert_eq!(driver.requests.lock().unwrap()[0].binary, selected);
        let next = started.journal.as_ref().unwrap();
        assert_eq!(next.active.version, "2.0.0");
        assert_eq!(next.confirmed.version, "2.0.0");
        assert_eq!(next.installed, journal.installed);
        assert_eq!(next.install_pending, journal.install_pending);
    }
}

#[tokio::test]
async fn an_untrusted_pending_place_installs_nothing() {
    refuses_untrusted_install_source(true).await;
}

#[tokio::test]
async fn an_untrusted_recorded_restore_installs_nothing() {
    refuses_untrusted_install_source(false).await;
}

#[tokio::test]
async fn a_refused_restore_does_not_revive_a_superseded_pending_install() {
    use open_ferry_update::journal::{
        Admitted, Artifact, Blame, How, InstallPending, Installed, Journal, JournalLoad,
        JournalStore, Phase, Return, Switch,
    };
    let rig = rig();
    let data = &rig.cfg.data;
    let mut artifacts = Vec::new();
    for (version, bytes) in [
        ("1.0.0", b"binary one"),
        ("2.0.0", b"binary two"),
        ("3.0.0", b"binary tri"),
    ] {
        std::fs::create_dir_all(data.version_dir(version)).unwrap();
        let binary = data.binary(version, NAME);
        std::fs::write(&binary, bytes).unwrap();
        artifacts.push(Artifact {
            version: version.into(),
            sha256: hash_file(&binary).unwrap(),
            size: bytes.len() as u64,
        });
    }
    let [a, b, c]: [Artifact; 3] = artifacts.try_into().unwrap();
    let mut journal = Journal::first_start(b.clone(), Some(Installed::from(&a)));
    journal.install_pending = Some(InstallPending {
        target: b.clone(),
        source: data.binary(&b.version, NAME).to_string_lossy().into_owned(),
        aside: "saved-aside".into(),
        attempts: 1,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: Some("install-b".into()),
    });
    journal.switch = Some(Switch {
        id: "return-c".into(),
        how: How::Rollback,
        from: b,
        to: c.clone(),
        phase: Phase::TakingOver,
        started: chrono::Utc::now().to_rfc3339(),
        watch_until: None,
        admitted: Admitted::default(),
        return_: Some(Return {
            target: c.clone(),
            blame: Blame::Environment,
            why: "decided return".into(),
            decided: chrono::Utc::now().to_rfc3339(),
            installed: false,
        }),
    });
    let store = JournalStore::new(data.root());
    store.write(&mut journal, "fixture").unwrap();
    make_untrusted(&data.version_dir(&a.version));
    std::fs::remove_file(&rig.installed).unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert_eq!(started.run.as_ref().unwrap().artifact, c);
    assert!(started.worker.is_some());
    assert!(
        started
            .switching_disabled
            .as_deref()
            .is_some_and(|why| why.contains("1.0.0"))
    );
    assert!(!rig.installed.exists());
    let next = started.journal.as_ref().unwrap();
    assert!(next.install_pending.is_none(), "superseded B was revived");
    let JournalLoad::Loaded(on_disk) = store.load().unwrap() else {
        panic!("no journal")
    };
    assert_eq!(&on_disk, next);
    assert!(next.outcomes.iter().any(|outcome| {
        outcome.record.id == "install-b"
            && outcome.record.installed_binary.as_deref() == Some("superseded")
    }));
}

/// Spawns succeed except for the versions it is told to refuse.
#[derive(Default)]
struct RefusingDriver {
    refuse: Mutex<Vec<String>>,
    spawned: Mutex<Vec<String>>,
}

impl RefusingDriver {
    fn refusing(versions: &[&str]) -> Self {
        Self {
            refuse: Mutex::new(versions.iter().map(|v| (*v).to_owned()).collect()),
            spawned: Mutex::new(Vec::new()),
        }
    }
}

impl Driver for RefusingDriver {
    fn spawn(
        &self,
        request: SpawnRequest,
        _events: EventSender,
    ) -> impl std::future::Future<Output = Result<WorkerPort, SpawnError>> + Send {
        let refused = {
            let refuse = self.refuse.lock().unwrap();
            refuse.iter().any(|v| v == "*" || *v == request.version)
        };
        let (cmd, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = if refused {
            Err(SpawnError::Start(io::Error::other("refused")))
        } else {
            self.spawned.lock().unwrap().push(request.version.clone());
            Ok(WorkerPort {
                id: request.id,
                identity: ProcessIdentity {
                    pid: 4242,
                    started: 7,
                },
                reported_version: request.version.clone(),
                protocol: 1,
                caps: Vec::new(),
                cmd,
            })
        };
        async move { result }
    }
}

/// A journal that left a decided return open: 2.0.0 was confirmed, the switch
/// to 3.0.0 failed, and the return to 2.0.0 had been journalled. 1.0.0 is
/// `previous` and the installed file. Returns the three artifacts.
fn open_return(
    rig: &Rig,
) -> (
    open_ferry_update::journal::Artifact,
    open_ferry_update::journal::Artifact,
    open_ferry_update::journal::Artifact,
) {
    use open_ferry_update::journal::{
        Admitted, Artifact, Blame, How, Installed, Journal, JournalStore, Phase, Return, Switch,
    };
    let data = &rig.cfg.data;
    let mut artifacts = Vec::new();
    for (version, bytes) in [
        ("1.0.0", &b"binary one"[..]),
        ("2.0.0", &b"binary two"[..]),
        ("3.0.0", &b"binary tri"[..]),
    ] {
        std::fs::create_dir_all(data.version_dir(version)).unwrap();
        let binary = data.binary(version, NAME);
        std::fs::write(&binary, bytes).unwrap();
        artifacts.push(Artifact {
            version: version.into(),
            sha256: hash_file(&binary).unwrap(),
            size: bytes.len() as u64,
        });
    }
    let [a, b, c]: [Artifact; 3] = artifacts.try_into().unwrap();
    let mut journal = Journal::first_start(b.clone(), Some(Installed::from(&a)));
    journal.previous = Some(a.clone());
    journal.switch = Some(Switch {
        id: "return-c".into(),
        how: How::Update,
        from: b.clone(),
        to: c.clone(),
        phase: Phase::TakingOver,
        started: chrono::Utc::now().to_rfc3339(),
        watch_until: None,
        admitted: Admitted::default(),
        return_: Some(Return {
            target: b.clone(),
            blame: Blame::Environment,
            why: "decided return".into(),
            decided: chrono::Utc::now().to_rfc3339(),
            installed: false,
        }),
    });
    JournalStore::new(data.root())
        .write(&mut journal, "fixture")
        .unwrap();
    (a, b, c)
}

fn forward_records(rig: &Rig) -> Vec<open_ferry_update::state::SwitchRecord> {
    open_ferry_update::state::State::load(&rig.cfg.data.state_file())
        .history
        .into_iter()
        .filter(|record| record.id == "return-c")
        .collect()
}

#[tokio::test]
async fn a_recovered_return_that_never_starts_a_worker_does_not_claim_one() {
    let rig = rig();
    let (_a, b, _c) = open_return(&rig);
    let driver = RefusingDriver::refusing(&["*"]);
    for round in 0..3 {
        let mut registry = Registry::new();
        let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
            .await
            .unwrap();
        assert!(started.worker.is_none(), "round {round}");
        assert!(started.degraded.is_some(), "round {round}");
        let journal = started.journal.as_ref().unwrap();
        let kept = journal
            .switch
            .as_ref()
            .and_then(|switch| switch.return_.as_ref())
            .expect("the return stays in the journal until a worker of it starts");
        assert_eq!(kept.target, b);
        let records = forward_records(&rig);
        assert_eq!(records.len(), 1, "round {round}: {records:?}");
        assert!(records[0].returned_to.is_none(), "{:?}", records[0]);
        assert!(records[0].result.is_some());
        drop(started);
    }
}

/// Round 5, item 3 (a): a crash after J-R1 and before the record, and no
/// artifact verifies. The forward switch still has one record (the stored
/// blame, `mark_failed` for it, no `returned_to`), the switch stays in the
/// journal, and a third start still has one entry.
#[tokio::test]
async fn a_recovered_return_that_nothing_can_carry_is_recorded_once() {
    use open_ferry_update::journal::{Blame, JournalLoad, JournalStore};
    let rig = rig();
    let (a, b, c) = open_return(&rig);
    for artifact in [&a, &b, &c] {
        std::fs::write(rig.cfg.data.binary(&artifact.version, NAME), b"garbage").unwrap();
    }
    // The installed file is no known hash either.
    std::fs::write(&rig.cfg.installed, b"a stranger").unwrap();
    let store = JournalStore::new(rig.cfg.data.root());
    let JournalLoad::Loaded(mut journal) = store.load().unwrap() else {
        panic!("no journal")
    };
    journal
        .switch
        .as_mut()
        .unwrap()
        .return_
        .as_mut()
        .unwrap()
        .blame = Blame::NewVersion;
    store.write(&mut journal, "fixture").unwrap();
    let driver = RefusingDriver::default();
    for round in 0..3 {
        let mut registry = Registry::new();
        let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
            .await
            .unwrap();
        assert!(started.worker.is_none(), "round {round}");
        assert!(started.degraded.is_some(), "round {round}");
        let journal = started.journal.as_ref().unwrap();
        assert!(
            journal
                .switch
                .as_ref()
                .is_some_and(|switch| switch.return_.is_some()),
            "round {round}: the switch is kept"
        );
        let records = forward_records(&rig);
        assert_eq!(records.len(), 1, "round {round}: {records:?}");
        assert!(records[0].returned_to.is_none(), "{:?}", records[0]);
        assert_eq!(records[0].blame.as_deref(), Some("new-version"));
        let state = open_ferry_update::state::State::load(&rig.cfg.data.state_file());
        assert_eq!(
            state.failed,
            std::slice::from_ref(&c.version),
            "round {round}"
        );
        drop(started);
    }
}

/// Item 3 (a), the journal half: with the state file unwritable the outcome
/// stays in the journal (the arm for nothing-can-run applies the plan's
/// journal write), and the next start gives it to the state, once.
#[tokio::test]
async fn an_unstarted_return_outcome_waits_in_the_journal_for_the_state() {
    let rig = rig();
    let (a, b, c) = open_return(&rig);
    for artifact in [&a, &b, &c] {
        std::fs::write(rig.cfg.data.binary(&artifact.version, NAME), b"garbage").unwrap();
    }
    std::fs::write(&rig.cfg.installed, b"a stranger").unwrap();
    let driver = RefusingDriver::default();
    std::fs::create_dir(rig.cfg.data.state_file()).unwrap();
    let first = start(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    assert!(first.worker.is_none());
    let journal = first.journal.as_ref().unwrap();
    assert_eq!(journal.outcomes.len(), 1, "{:?}", journal.outcomes);
    assert!(journal.outcomes[0].record.returned_to.is_none());
    assert!(first.state_pending);
    drop(first);
    std::fs::remove_dir(rig.cfg.data.state_file()).unwrap();
    let second = start(
        &rig.cfg,
        &rig.host,
        &driver,
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    assert!(second.journal.as_ref().unwrap().outcomes.is_empty());
    let records = forward_records(&rig);
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].returned_to.is_none());
}

/// Round 5, item 4: the return target fails verification and `previous` is
/// gone, so recovery runs the installed file (I). If I's spawn fails, nothing
/// is confirmed that never ran: `confirmed` is unchanged and the return is
/// kept. A start where I does spawn runs J-R2: `confirmed` is I, one entry.
#[tokio::test]
async fn the_installed_rebuild_confirms_nothing_until_its_worker_starts() {
    let rig = rig();
    let (a, b, _c) = open_return(&rig);
    std::fs::write(rig.cfg.data.binary(&b.version, NAME), b"garbage").unwrap();
    std::fs::remove_dir_all(rig.cfg.data.version_dir(&a.version)).unwrap();
    let failing = RefusingDriver::refusing(&["*"]);
    let first = start(
        &rig.cfg,
        &rig.host,
        &failing,
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    assert!(first.worker.is_none());
    let journal = first.journal.as_ref().unwrap();
    assert_eq!(journal.confirmed, b, "nothing was confirmed that never ran");
    assert!(
        journal
            .switch
            .as_ref()
            .is_some_and(|switch| switch.return_.is_some()),
        "the return is kept"
    );
    drop(first);
    let working = RefusingDriver::default();
    let second = start(
        &rig.cfg,
        &rig.host,
        &working,
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    assert!(second.worker.is_some());
    let journal = second.journal.as_ref().unwrap();
    assert!(journal.switch.is_none(), "J-R2 cleared the switch");
    assert_eq!(journal.confirmed.sha256, a.sha256, "confirmed is I");
    let records = forward_records(&rig);
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].returned_to.is_some());
}

#[tokio::test]
async fn a_recovered_return_that_starts_on_a_later_rung_confirms_it_once() {
    use open_ferry_update::journal::{JournalLoad, JournalStore};
    let rig = rig();
    let (a, b, _c) = open_return(&rig);
    // The journalled target's copy fails its hash, so recovery falls back to
    // `previous` and journals that as the return's target.
    std::fs::write(rig.cfg.data.binary(&b.version, NAME), b"binary xxx").unwrap();
    let driver = RefusingDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(started.worker.is_some());
    assert_eq!(started.run.as_ref().unwrap().artifact, a);
    assert_eq!(
        *driver.spawned.lock().unwrap(),
        std::slice::from_ref(&a.version)
    );
    let journal = started.journal.as_ref().unwrap();
    assert!(journal.switch.is_none(), "J-R2 cleared the switch");
    assert_eq!(journal.confirmed, a);
    assert!(journal.outcomes.is_empty(), "{:?}", journal.outcomes);
    let records = forward_records(&rig);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].returned_to.as_deref(), Some("1.0.0"));
    let JournalLoad::Loaded(on_disk) = JournalStore::new(rig.cfg.data.root()).load().unwrap()
    else {
        panic!("no journal")
    };
    assert_eq!(&on_disk, journal);
}

#[tokio::test]
async fn a_refused_restore_keeps_degraded_restart_inputs() {
    use open_ferry_update::journal::{Artifact, Installed, Journal, JournalStore};
    let rig = rig();
    let data = &rig.cfg.data;
    std::fs::create_dir_all(data.version_dir("1.0.0")).unwrap();
    std::fs::copy(&rig.installed, data.binary("1.0.0", NAME)).unwrap();
    let a = Artifact {
        version: "1.0.0".into(),
        sha256: rig.host.sha.clone().unwrap(),
        size: 10,
    };
    let missing = Artifact {
        version: "2.0.0".into(),
        sha256: "missing".into(),
        size: 10,
    };
    let mut journal = Journal::first_start(missing, Some(Installed::from(&a)));
    JournalStore::new(data.root())
        .write(&mut journal, "fixture")
        .unwrap();
    make_untrusted(&data.version_dir(&a.version));
    std::fs::remove_file(&rig.installed).unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(started.degraded.is_some(), "degraded restart was lost");
    let disabled = started.switching_disabled.as_ref().unwrap();
    assert!(disabled.contains("1.0.0"));
    assert!(started.notes.iter().any(|note| note == disabled));
    assert!(started.run.is_none());
    assert!(started.worker.is_none());
    let now = tokio::time::Instant::now();
    let runtime = open_ferry_supervisor::core::Runtime::from_started(
        &started,
        &registry,
        &rig.cfg,
        &SystemClock,
        now,
    );
    assert!(
        runtime.restart.is_some(),
        "disabled degraded runtime must restart"
    );
    assert_eq!(
        runtime.restart.as_ref().unwrap().due,
        now + rig.cfg.timings.restart_first
    );
    assert_eq!(runtime.disabled, started.switching_disabled);
    assert_eq!(runtime.degraded, started.degraded);
    assert!(driver.requests.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn an_install_copy_clears_foreign_write_permissions() {
    use open_ferry_supervisor::install::{InstallResult, place};
    use open_ferry_update::journal::Artifact;
    use std::os::unix::fs::PermissionsExt;
    let rig = rig();
    let source = rig.cfg.data.binary("1.0.0", NAME);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::copy(&rig.installed, &source).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o777)).unwrap();
    let target = Artifact {
        version: "1.0.0".to_owned(),
        sha256: rig.host.sha.clone().unwrap(),
        size: 10,
    };
    for missing in [false, true] {
        if missing {
            std::fs::remove_file(&rig.installed).unwrap();
        }
        assert_eq!(
            place(&rig.cfg.paths(), &target, None, "old", None),
            InstallResult::Replaced
        );
        assert_eq!(
            std::fs::metadata(&rig.installed)
                .unwrap()
                .permissions()
                .mode()
                & 0o022,
            0
        );
    }
}

#[tokio::test]
async fn an_untrusted_version_is_unverified_before_hashing() {
    let rig = rig();
    let version = rig.cfg.data.version_dir("1.0.0");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::copy(&rig.installed, rig.cfg.data.binary("1.0.0", NAME)).unwrap();
    make_untrusted(&version);
    // A newer journal must not even enumerate these candidates.
    let journal = rig
        .cfg
        .data
        .root()
        .join(open_ferry_update::journal::JOURNAL_FILE);
    std::fs::write(&journal, br#"{"format":9999}"#).unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(started.switching_disabled.is_some());
    assert!(!started.notes.iter().any(|note| note.contains("unverified")));
    assert_eq!(started.run.unwrap().binary, rig.installed);
    drop(started.endpoint);
    drop(started.lock);
    std::fs::remove_file(&journal).unwrap();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(
        started
            .notes
            .iter()
            .any(|note| note.contains("unverified") && note.contains("1.0.0"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_version_fifo_is_never_hashed_on_the_trusted_root_path() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    drop(first);
    let binary = rig.cfg.data.binary("1.0.0", NAME);
    std::fs::remove_file(&binary).unwrap();
    let name = std::ffi::CString::new(binary.as_os_str().as_bytes()).unwrap();
    // SAFETY: this NUL-terminated path belongs to the fixture.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&binary)
        .unwrap();
    let mut registry = Registry::new();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock),
    )
    .await;
    drop(held); // Let a reverted blocking hash finish even when the test fails.
    let started = result.expect("FIFO must not be hashed").unwrap();
    assert!(
        started
            .notes
            .iter()
            .any(|note| note.contains("not a regular file"))
    );
    assert!(started.switching_disabled.is_some());
    assert_eq!(started.run.unwrap().binary, rig.installed);
}

#[cfg(unix)]
#[tokio::test]
async fn a_version_symlink_is_unverified_without_hashing_its_target() {
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    drop(first);
    let binary = rig.cfg.data.binary("1.0.0", NAME);
    std::fs::remove_file(&binary).unwrap();
    std::os::unix::fs::symlink(&rig.installed, &binary).unwrap();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(
        started
            .notes
            .iter()
            .any(|note| note.contains("unverified") && note.contains("not a regular file"))
    );
    assert!(started.switching_disabled.is_some());
    assert_eq!(started.run.unwrap().binary, rig.installed);
}

#[tokio::test]
async fn first_start_refuses_an_untrusted_reused_version_before_journaling() {
    let rig = rig();
    let binary = rig.cfg.data.binary("1.0.0", NAME);
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::copy(&rig.installed, &binary).unwrap();
    make_untrusted(binary.parent().unwrap());
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(started.switching_disabled.is_some());
    assert!(started.journal.is_none());
    assert!(!started.store.path().exists());
    assert_eq!(started.run.unwrap().binary, rig.installed);
    assert_eq!(driver.requests.lock().unwrap()[0].binary, rig.installed);
}

#[tokio::test]
async fn adoption_refuses_an_untrusted_reused_version_before_journaling() {
    use open_ferry_update::journal::{Artifact, Installed, Journal, JournalStore};
    let rig = rig();
    for version in ["1.0.0", "2.0.0"] {
        std::fs::create_dir_all(rig.cfg.data.version_dir(version)).unwrap();
    }
    std::fs::copy(&rig.installed, rig.cfg.data.binary("1.0.0", NAME)).unwrap();
    let previous = rig.cfg.data.binary("2.0.0", NAME);
    std::fs::write(&previous, b"previous binary").unwrap();
    let artifact = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&previous).unwrap(),
        size: 15,
    };
    let mut journal = Journal::first_start(artifact.clone(), Some(Installed::from(&artifact)));
    let store = JournalStore::new(rig.cfg.data.root());
    store.write(&mut journal, "fixture").unwrap();
    let before = std::fs::read(store.path()).unwrap();
    make_untrusted(&rig.cfg.data.version_dir("1.0.0"));
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert!(started.switching_disabled.is_some());
    assert!(started.journal.is_none());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert_eq!(started.run.unwrap().binary, rig.installed);
    assert_eq!(driver.requests.lock().unwrap()[0].binary, rig.installed);
}

#[tokio::test]
async fn verified_installed_reuse_of_an_untrusted_version_runs_the_installed_file() {
    struct ChangedDuringStage {
        binary: PathBuf,
        called: AtomicUsize,
    }
    impl Runner for ChangedDuringStage {
        fn version<'a>(
            &'a self,
            staged: &'a Path,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.called.fetch_add(1, Ordering::SeqCst);
                // Facts saw no kept binary: recovery must select VerifiedInstalled.
                // Introduce the reusable but untrusted destination during staging,
                // after facts/preflight and before the kept-run publication.
                std::fs::create_dir_all(self.binary.parent().unwrap()).unwrap();
                std::fs::copy(staged, &self.binary).unwrap();
                make_untrusted(self.binary.parent().unwrap());
                Ok("open-ferry 1.0.0".into())
            })
        }
    }
    let mut rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    drop(first);
    let before = std::fs::read(
        rig.cfg
            .data
            .root()
            .join(open_ferry_update::journal::JOURNAL_FILE),
    )
    .unwrap();
    let binary = rig.cfg.data.binary("1.0.0", NAME);
    std::fs::remove_file(&binary).unwrap();
    let runner = Arc::new(ChangedDuringStage {
        binary,
        called: AtomicUsize::new(0),
    });
    rig.cfg.runner = runner.clone();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert_eq!(
        runner.called.load(Ordering::SeqCst),
        1,
        "VerifiedInstalled staging did not run"
    );
    assert!(started.switching_disabled.is_some());
    assert!(started.journal.is_none());
    assert_eq!(std::fs::read(started.store.path()).unwrap(), before);
    assert_eq!(started.run.unwrap().binary, rig.installed);
}

#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn a_restore_after_an_aside_crash_does_not_journal_a_new_aside() {
    use open_ferry_update::journal::{Artifact, InstallPending, JournalStore};
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let mut journal = first.journal.clone().unwrap();
    drop(first);
    journal.previous = Some(journal.confirmed.clone());
    let target = Artifact {
        version: "2.0.0".into(),
        sha256: "0".repeat(64),
        size: 10,
    };
    journal.active = target.clone();
    journal.confirmed = target.clone();
    let aside = rig.installed.with_extension("crashed-aside");
    std::fs::rename(&rig.installed, &aside).unwrap();
    journal.install_pending = Some(InstallPending {
        target,
        source: rig
            .cfg
            .data
            .binary("2.0.0", NAME)
            .to_string_lossy()
            .into_owned(),
        aside: aside.to_string_lossy().into_owned(),
        attempts: 0,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: None,
    });
    let store = JournalStore::new(rig.cfg.data.root());
    store.write(&mut journal, "fixture").unwrap();
    let seams = rig.cfg.data.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("journal.fail_J-install.fail"), b"").unwrap();
    let mut registry = Registry::new();
    let restarted = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&rig.installed).unwrap(), b"binary one");
    assert_eq!(std::fs::read(&aside).unwrap(), b"binary one");
    assert_eq!(restarted.run.unwrap().artifact.version, "1.0.0");
    assert!(restarted.journal.unwrap().install_pending.is_none());
}

#[derive(Clone, Default)]
struct StartupLog(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for StartupLog {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct MessageText(String);
        impl tracing::field::Visit for MessageText {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut text = MessageText(String::new());
        event.record(&mut text);
        self.0.lock().unwrap().push(text.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn an_orphaned_partial_outcome_is_noted_once_at_startup_and_retained() {
    // The warning is process-global; other startup fixtures may also create
    // orphans. Exercise repeated recovery in a fresh process, not after them.
    if std::env::var_os("OFP_T4_ORPHAN_CHILD").is_none() {
        let _turn = take_turn();
        let owned = scratch();
        let marker = owned.path().join("completed");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "an_orphaned_partial_outcome_is_noted_once_at_startup_and_retained",
                "--nocapture",
            ])
            .env("OFP_T4_ORPHAN_CHILD", "1")
            .env("OFP_T5_CHILD_MARKER", &marker)
            .env("OFP_T5_CHILD_SCRATCH", owned.path())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "orphan recovery subprocess: {status}");
                assert!(marker.is_file(), "orphan recovery child did not run");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("orphan recovery subprocess exceeded its watchdog");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    use open_ferry_update::journal::{JournalStore, Outcome};
    let rig = rig();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let first = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let mut journal = first.journal.clone().unwrap();
    drop(first);
    let outcome = Outcome {
        record: open_ferry_update::state::SwitchRecord {
            id: "orphan-install".into(),
            installed_binary: Some("replaced".into()),
            ..Default::default()
        },
        mark_failed: None,
    };
    let mut second = outcome.clone();
    second.record.id = "another-orphan-install".into();
    journal.push_outcome(outcome.clone());
    journal.push_outcome(second.clone());
    let store = JournalStore::new(rig.cfg.data.root());
    store.write(&mut journal, "fixture").unwrap();
    let mut registry = Registry::new();
    use tracing::instrument::WithSubscriber;
    let log = StartupLog::default();
    let mut started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .with_subscriber(log.clone())
        .await
        .unwrap();
    assert_eq!(
        started
            .notes
            .iter()
            .filter(|note| note.contains("orphaned partial outcome orphan-install"))
            .count(),
        1
    );
    assert!(started.state_pending);
    assert_eq!(
        started.journal.as_ref().unwrap().outcomes,
        vec![outcome.clone(), second.clone()]
    );
    for _ in 0..3 {
        assert!(
            open_ferry_supervisor::statefile::apply_locked(&rig.cfg.data, "replay", &outcome)
                .is_err()
        );
    }
    assert_eq!(
        started
            .notes
            .iter()
            .filter(|note| note.contains("orphaned partial outcome"))
            .count(),
        2
    );
    for _ in 0..3 {
        let mut fresh_registry = Registry::new();
        reenter(
            &rig.cfg,
            &rig.host,
            &driver,
            &mut fresh_registry,
            &SystemClock,
            &mut started,
        )
        .with_subscriber(log.clone())
        .await
        .unwrap();
        assert!(started.state_pending);
        assert_eq!(
            started.journal.as_ref().unwrap().outcomes,
            vec![outcome.clone(), second.clone()]
        );
    }
    assert_eq!(
        started
            .notes
            .iter()
            .filter(|note| note.contains("orphaned partial outcome"))
            .count(),
        2
    );
    assert!(!rig.cfg.data.state_file().exists());
    assert_eq!(
        log.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("orphaned partial outcome orphan-install"))
            .count(),
        1
    );
    assert_eq!(
        log.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("orphaned partial outcome another-orphan-install"))
            .count(),
        1
    );
    std::fs::write(
        std::env::var_os("OFP_T5_CHILD_MARKER").unwrap(),
        b"completed",
    )
    .unwrap();
}

#[cfg(all(windows, feature = "blue-green-test"))]
#[tokio::test]
#[ignore = "subprocess entry point, driven only with a temporary fixture"]
async fn startup_crash_child() {
    let root = PathBuf::from(std::env::var_os("OFP_STARTUP_FIXTURE").unwrap());
    let data = DataDir::at(root.join("data"));
    let host = FakeHost {
        sha: Some(hash_file(&data.binary("1.0.0", NAME)).unwrap()),
        waits: AtomicUsize::new(0),
        on_bind: Mutex::new(None),
    };
    let cfg = Config {
        data,
        installed: root.join("bin").join(NAME),
        config: root.join("config.yaml"),
        kind: ServiceKind::User,
        timings: Timings::default(),
        receipt: serde_json::json!({}),
        binary_name: NAME.into(),
        runner: Arc::new(FakeRunner),
        detect_timeout: Duration::from_secs(5),
        endpoint_location: None,
    };
    let started = start(
        &cfg,
        &host,
        &FakeDriver::default(),
        &mut Registry::new(),
        &SystemClock,
    )
    .await
    .unwrap();
    assert!(
        started.switching_disabled.is_none(),
        "{:?}",
        started.switching_disabled
    );
}

#[cfg(all(windows, feature = "blue-green-test"))]
fn start_fixture_child(root: &Path) -> std::process::Child {
    let output = std::fs::File::create(root.join("startup-child.log")).unwrap();
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "startup_crash_child", "--ignored", "--nocapture"])
        .env("OFP_STARTUP_FIXTURE", root)
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap()
}

#[cfg(all(windows, feature = "blue-green-test"))]
async fn child_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if result.is_err() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    result.expect("startup child finishes")
}

#[cfg(all(windows, feature = "blue-green-test"))]
async fn windows_rename_startup_fixture(crash: bool) {
    use open_ferry_update::journal::{
        Artifact, InstallPending, Installed, Journal, JournalLoad, JournalStore,
    };
    let rig = rig();
    for version in ["1.0.0", "2.0.0"] {
        std::fs::create_dir_all(rig.cfg.data.version_dir(version)).unwrap();
    }
    std::fs::copy(&rig.installed, rig.cfg.data.binary("1.0.0", NAME)).unwrap();
    let binary = rig.cfg.data.binary("2.0.0", NAME);
    std::fs::write(&binary, b"binary two").unwrap();
    let target = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&binary).unwrap(),
        size: 10,
    };
    let preferred = rig.installed.with_extension("conflicting-aside");
    std::fs::write(&preferred, b"not our aside").unwrap();
    let mut journal = Journal::first_start(
        target.clone(),
        Some(Installed {
            version: "1.0.0".into(),
            sha256: rig.host.sha.clone().unwrap(),
        }),
    );
    journal.install_pending = Some(InstallPending {
        target,
        source: binary.to_string_lossy().into_owned(),
        aside: preferred.to_string_lossy().into_owned(),
        attempts: 0,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: None,
    });
    let store = JournalStore::new(rig.cfg.data.root());
    store.write(&mut journal, "fixture").unwrap();
    let seams = rig.cfg.data.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    let marker = seams.join(if crash {
        "install.between_renames.hold"
    } else {
        "install.between_renames.fail"
    });
    std::fs::write(&marker, b"").unwrap();
    let mut child = start_fixture_child(rig._dir.path());
    if crash {
        let arrived = tokio::time::timeout(Duration::from_secs(20), async {
            while !seams.join("install.between_renames.arrived").exists() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "startup child exited before the rename barrier"
                );
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Kill only the process handle created by this test, including on timeout.
        child.kill().unwrap();
        child.wait().unwrap();
        arrived.expect("between-renames barrier reached");
        assert!(!rig.installed.exists(), "first rename did not run");
    } else {
        assert!(
            child_exit(&mut child).await.success(),
            "{}",
            std::fs::read_to_string(rig._dir.path().join("startup-child.log")).unwrap()
        );
        assert_eq!(
            std::fs::read(&rig.installed).unwrap(),
            b"binary one",
            "failed second rename did not put the old file back"
        );
    }
    let JournalLoad::Loaded(on_disk) = store.load().unwrap() else {
        panic!("no journal")
    };
    let pending = on_disk.install_pending.unwrap();
    let mut expected = rig.installed.as_os_str().to_owned();
    expected.push(".old");
    let actual = Path::new(&pending.aside);
    assert_eq!(
        actual,
        Path::new(&expected),
        "journal did not name the real aside"
    );
    if crash {
        assert_eq!(std::fs::read(actual).unwrap(), b"binary one");
    } else {
        assert!(!actual.exists());
        assert_eq!(pending.attempts, 1);
    }
    std::fs::remove_file(marker).unwrap();
    let mut restarted = start_fixture_child(rig._dir.path());
    assert!(
        child_exit(&mut restarted).await.success(),
        "{}",
        std::fs::read_to_string(rig._dir.path().join("startup-child.log")).unwrap()
    );
    assert_eq!(std::fs::read(&rig.installed).unwrap(), b"binary two");
    let JournalLoad::Loaded(completed) = store.load().unwrap() else {
        panic!("no recovery journal")
    };
    assert!(completed.install_pending.is_none());
    assert_eq!(completed.installed.unwrap().version, "2.0.0");
    assert_eq!(std::fs::read(&preferred).unwrap(), b"not our aside");
}

#[cfg(all(windows, feature = "blue-green-test"))]
#[tokio::test]
async fn startup_recovers_a_crash_between_the_windows_renames() {
    windows_rename_startup_fixture(true).await;
}

#[cfg(all(windows, feature = "blue-green-test"))]
#[tokio::test]
async fn startup_puts_back_the_old_file_when_the_second_windows_rename_fails() {
    windows_rename_startup_fixture(false).await;
}

#[cfg(unix)]
fn t4_fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the NUL-terminated path is owned by this fixture.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "subprocess entry point with an isolated umask and FIFO watchdog"]
async fn staging_trust_child() {
    use open_ferry_supervisor::endpoint::Identity;
    use open_ferry_supervisor::trust::{TrustRules, check_file};
    use open_ferry_update::journal::{Artifact, Installed, Journal, JournalStore};
    use std::os::unix::fs::PermissionsExt;
    struct WatchingRunner {
        paths: Mutex<Vec<PathBuf>>,
        introduce_fifo: Option<PathBuf>,
    }
    impl Runner for WatchingRunner {
        fn version<'a>(
            &'a self,
            path: &'a Path,
            _: Duration,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                let rules =
                    TrustRules::for_identity(&Identity::current(ServiceKind::User).unwrap());
                check_file(path, &rules).unwrap();
                for entry in [
                    path.to_path_buf(),
                    path.parent().unwrap().to_path_buf(),
                    path.parent().unwrap().parent().unwrap().to_path_buf(),
                ] {
                    assert_eq!(
                        std::fs::metadata(entry).unwrap().permissions().mode() & 0o022,
                        0
                    );
                }
                self.paths.lock().unwrap().push(path.to_owned());
                if let Some(binary) = &self.introduce_fifo {
                    t4_fifo(binary);
                }
                Ok("open-ferry 1.0.0".into())
            })
        }
    }
    let case = std::env::var("OFP_T4_CASE").unwrap();
    let mut rig = rig();
    let own = rig.cfg.data.binary("1.0.0", NAME);
    std::fs::create_dir_all(rig.cfg.data.root()).unwrap();
    if case != "umask" {
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        if case != "verified" {
            t4_fifo(&own);
        }
        if case == "adoption" || case == "verified" {
            let artifact = if case == "verified" {
                Artifact {
                    version: "1.0.0".into(),
                    sha256: rig.host.sha.clone().unwrap(),
                    size: 10,
                }
            } else {
                let other = rig.cfg.data.binary("2.0.0", NAME);
                std::fs::create_dir_all(other.parent().unwrap()).unwrap();
                std::fs::write(&other, b"binary two").unwrap();
                Artifact {
                    version: "2.0.0".into(),
                    sha256: hash_file(&other).unwrap(),
                    size: 10,
                }
            };
            let mut journal =
                Journal::first_start(artifact.clone(), Some(Installed::from(&artifact)));
            JournalStore::new(rig.cfg.data.root())
                .write(&mut journal, "fixture")
                .unwrap();
        }
    }
    let runner = Arc::new(WatchingRunner {
        paths: Mutex::new(Vec::new()),
        introduce_fifo: (case == "verified").then_some(own),
    });
    rig.cfg.runner = runner.clone();
    if case == "umask" {
        // SAFETY: this ignored entry point runs only in its own subprocess.
        unsafe {
            libc::umask(0);
        }
        std::fs::set_permissions(&rig.installed, std::fs::Permissions::from_mode(0o6777)).unwrap();
    }
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    assert_eq!(
        runner.paths.lock().unwrap().len(),
        1,
        "staging path was not probed"
    );
    if case == "umask" {
        assert!(started.switching_disabled.is_none(), "{:?}", started.notes);
        let binary = &started.run.as_ref().unwrap().binary;
        assert_eq!(
            std::fs::metadata(binary).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    } else {
        assert!(started.switching_disabled.is_some());
        assert!(
            started
                .notes
                .iter()
                .any(|note| note.contains("not a regular file")),
            "{:?}",
            started.notes
        );
        assert_eq!(started.run.unwrap().binary, rig.installed);
    }
    std::fs::write(
        std::env::var_os("OFP_T5_CHILD_MARKER").unwrap(),
        b"completed",
    )
    .unwrap();
}

#[cfg(unix)]
fn staging_watchdog(case: &str) {
    // Keep the parent suite off the per-user endpoint while its child uses it.
    let _turn = take_turn();
    let owned = scratch();
    let marker = owned.path().join("completed");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "staging_trust_child", "--ignored", "--nocapture"])
        .env("OFP_T4_CASE", case)
        .env("OFP_T5_CHILD_MARKER", &marker)
        .env("OFP_T5_CHILD_SCRATCH", owned.path())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "staging {case}: {status}");
            assert!(marker.is_file(), "staging {case} child did not run");
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("staging {case} hung while opening a refused FIFO");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn staging_fifo_first_start_is_bounded() {
    staging_watchdog("first");
}
#[cfg(unix)]
#[test]
fn staging_fifo_adoption_is_bounded() {
    staging_watchdog("adoption");
}
#[cfg(unix)]
#[test]
fn staging_fifo_verified_installed_is_bounded() {
    staging_watchdog("verified");
}
#[cfg(unix)]
#[test]
fn staging_modes_with_umask_zero_are_safe_at_the_probe() {
    staging_watchdog("umask");
}

#[cfg(feature = "blue-green-test")]
async fn recovered_install_sync_failure(superseded: bool, awaiting_replay: bool) {
    use open_ferry_update::journal::{
        Admitted, Artifact, Blame, How, InstallPending, Installed, Journal, JournalStore, Phase,
        Return, Switch,
    };
    let rig = rig();
    let data = &rig.cfg.data;
    for version in ["1.0.0", "2.0.0"] {
        std::fs::create_dir_all(data.version_dir(version)).unwrap();
    }
    std::fs::copy(&rig.installed, data.binary("1.0.0", NAME)).unwrap();
    std::fs::write(data.binary("2.0.0", NAME), b"binary two").unwrap();
    let a = Artifact {
        version: "1.0.0".into(),
        sha256: rig.host.sha.clone().unwrap(),
        size: 10,
    };
    let b = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&data.binary("2.0.0", NAME)).unwrap(),
        size: 10,
    };
    let mut journal = Journal::first_start(b.clone(), Some(Installed::from(&a)));
    journal.previous = Some(a.clone());
    journal.install_pending = Some(InstallPending {
        target: b.clone(),
        source: data.binary("2.0.0", NAME).to_string_lossy().into_owned(),
        aside: "saved-aside".into(),
        attempts: 1,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: Some("install-b".into()),
    });
    if superseded {
        std::fs::remove_file(data.binary("2.0.0", NAME)).unwrap();
    } else {
        std::fs::copy(data.binary("2.0.0", NAME), &rig.installed).unwrap();
        journal.switch = Some(Switch {
            id: "return".into(),
            how: How::Rollback,
            from: b.clone(),
            to: a.clone(),
            phase: Phase::TakingOver,
            started: chrono::Utc::now().to_rfc3339(),
            watch_until: None,
            admitted: Admitted::default(),
            return_: Some(Return {
                target: b.clone(),
                blame: Blame::Environment,
                why: "return test".into(),
                decided: chrono::Utc::now().to_rfc3339(),
                installed: false,
            }),
        });
    }
    let original = open_ferry_update::state::SwitchRecord {
        id: "install-b".into(),
        from: a.version.clone(),
        to: b.version.clone(),
        at: chrono::Utc::now().to_rfc3339(),
        confirmed: true,
        result: Some("switched".into()),
        installed_binary: Some("pending".into()),
        ..Default::default()
    };
    if awaiting_replay {
        journal.push_outcome(open_ferry_update::journal::Outcome {
            record: original.clone(),
            mark_failed: None,
        });
    } else {
        let mut state = open_ferry_update::state::State::default();
        state.upsert_record(original.clone());
        state.save(&data.state_file()).unwrap();
    }
    let pending = journal.install_pending.clone();
    let store = JournalStore::new(data.root());
    store.write(&mut journal, "fixture").unwrap();
    let seams = data.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("startup.recovered_install_sync.fail"), b"").unwrap();
    let driver = FakeDriver::default();
    let mut registry = Registry::new();
    let started = start(&rig.cfg, &rig.host, &driver, &mut registry, &SystemClock)
        .await
        .unwrap();
    let next = started.journal.as_ref().unwrap();
    assert!(next.switch.is_none(), "the resolved return was revived");
    assert_eq!(next.active, next.confirmed);
    if superseded {
        assert!(
            next.install_pending.is_none(),
            "superseded pending install revived"
        );
        assert_eq!(next.confirmed, a);
        assert!(
            !started
                .notes
                .iter()
                .any(|note| note.contains("retaining the pending install"))
        );
    } else {
        assert_eq!(next.install_pending, pending);
        assert_eq!(next.installed, journal.installed);
        assert_eq!(next.confirmed, b);
        assert!(
            started
                .notes
                .iter()
                .any(|note| note.contains("retaining the pending install"))
        );
        let state = open_ferry_update::state::State::load(&data.state_file());
        assert!(
            state
                .history
                .iter()
                .chain(state.last_switch.iter())
                .any(|record| record.id == "return")
        );
        let records: Vec<_> = state
            .history
            .iter()
            .chain(state.last_switch.iter())
            .filter(|record| record.id == "install-b")
            .collect();
        assert_eq!(
            records,
            vec![&original],
            "install report changed before sync"
        );
        assert!(
            !next
                .outcomes
                .iter()
                .any(|outcome| outcome.record.id == "install-b"),
            "the deferred report should be delivered as pending, never as replaced"
        );
    }
    let open_ferry_update::journal::JournalLoad::Loaded(on_disk) = store.load().unwrap() else {
        panic!("no journal")
    };
    assert_eq!(&on_disk, next);
    assert_eq!(started.run.as_ref().unwrap().artifact, next.active);
}

#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn recovery_sync_failure_keeps_the_resolved_return_and_pending_install() {
    recovered_install_sync_failure(false, false).await;
}
#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn recovery_sync_failure_does_not_revive_a_superseded_install() {
    recovered_install_sync_failure(true, false).await;
}

#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn recovery_sync_failure_defers_a_full_install_outcome_awaiting_replay() {
    recovered_install_sync_failure(false, true).await;
}

#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn recovery_reloads_the_install_tasks_published_journal_after_join_error() {
    use open_ferry_update::journal::{
        Artifact, InstallPending, Installed, Journal, JournalLoad, JournalStore,
    };
    let rig = rig();
    let data = &rig.cfg.data;
    std::fs::create_dir_all(data.version_dir("2.0.0")).unwrap();
    std::fs::write(data.binary("2.0.0", NAME), b"binary two").unwrap();
    let target = Artifact {
        version: "2.0.0".into(),
        sha256: hash_file(&data.binary("2.0.0", NAME)).unwrap(),
        size: 10,
    };
    let mut journal = Journal::first_start(
        target.clone(),
        Some(Installed {
            version: "1.0.0".into(),
            sha256: rig.host.sha.clone().unwrap(),
        }),
    );
    journal.install_pending = Some(InstallPending {
        target,
        source: data.binary("2.0.0", NAME).to_string_lossy().into_owned(),
        aside: "before-preparation".into(),
        attempts: 0,
        next_try: None,
        started: chrono::Utc::now().to_rfc3339(),
        record_id: None,
    });
    std::fs::remove_file(&rig.installed).unwrap();
    let store = JournalStore::new(data.root());
    store.write(&mut journal, "fixture").unwrap();
    let seq = journal.seq;
    let seams = data.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("startup.after_prepared_journal.fail"), b"").unwrap();
    let mut registry = Registry::new();
    let started = start(
        &rig.cfg,
        &rig.host,
        &FakeDriver::default(),
        &mut registry,
        &SystemClock,
    )
    .await
    .unwrap();
    let recovered = started.journal.as_ref().unwrap();
    assert_eq!(
        recovered.seq,
        seq + 4,
        "J-install, J6, J-L (the start-up try's count), workers all advance the sequence"
    );
    let pending = recovered.install_pending.as_ref().unwrap();
    assert!(
        pending.aside.is_empty(),
        "preparation cleared the aside for missing-file placement"
    );
    assert_eq!(
        pending.attempts, 1,
        "the task must actually fail with a join error"
    );
    assert!(!rig.installed.exists());
    let JournalLoad::Loaded(on_disk) = store.load().unwrap() else {
        panic!("no journal")
    };
    assert_eq!(&on_disk, recovered);
}
