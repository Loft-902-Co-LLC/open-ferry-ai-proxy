//! Not upstream's: the real driver starts `worker_probe` and the registry runs
//! it, on both OSes. Real processes, real control channels, 127.0.0.1 only.

use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use open_ferry_supervisor::driver::{
    Driver, Event, ProcessDriver, Registry, SpawnError, SpawnRequest, WorkerId,
};
use open_ferry_supervisor::endpoint::ServiceKind;
use open_ferry_supervisor::protocol::{Installation, ListenerTopology, Message, Role};
use open_ferry_supervisor::timings::Timings;

const LONG: Duration = Duration::from_secs(30);

fn probe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_worker_probe"))
}

fn listeners() -> Arc<Vec<(String, TcpListener)>> {
    #[cfg(windows)]
    let make = || {
        open_ferry_supervisor::sockets::windows::bind(
            "127.0.0.1:0".parse().expect("address"),
            open_ferry_supervisor::sockets::windows::BindOptions::named_host(),
        )
        .expect("bind")
    };
    #[cfg(unix)]
    let make = || TcpListener::bind("127.0.0.1:0").expect("bind");
    Arc::new(vec![("main".to_owned(), make())])
}

struct Rig {
    dir: tempfile::TempDir,
    registry: Registry,
    driver: ProcessDriver,
    listeners: Arc<Vec<(String, TcpListener)>>,
}

impl Rig {
    fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("mode"), mode).expect("mode");
        Self {
            dir,
            registry: Registry::new(),
            driver: ProcessDriver::new(ServiceKind::User, Duration::from_millis(20)),
            listeners: listeners(),
        }
    }

    fn request(&mut self, sha256: &str, handshake: Duration) -> SpawnRequest {
        let address = self.listeners[0].1.local_addr().expect("address");
        SpawnRequest {
            id: self.registry.next_id(),
            version: "probe-1".into(),
            sha256: sha256.to_owned(),
            binary: probe().to_owned(),
            role: Role::Active,
            switch_id: None,
            config: self.dir.path().to_owned(),
            installation: Installation {
                installed: probe().to_owned(),
                data_dir: self.dir.path().to_owned(),
                receipt: serde_json::Value::Null,
            },
            topology: vec![ListenerTopology {
                name: "main".into(),
                address,
                tls: false,
            }],
            listeners: self.listeners.clone(),
            budgets: Timings::default().budgets(),
            handshake_timeout: handshake,
            log: Some(self.dir.path().join("worker.log")),
        }
    }

    async fn start(&mut self) -> Result<WorkerId, SpawnError> {
        let request = self.request(&probe_hash(), LONG);
        let id = request.id;
        let port = self.driver.spawn(request, self.registry.events()).await?;
        self.registry.add(port);
        Ok(id)
    }

    /// Waits for the probe's `loaded`, so a later close is not mistaken for a
    /// failed write on its side.
    async fn loaded(&mut self, id: WorkerId) {
        let (from, event) = tokio::time::timeout(LONG, self.registry.next_event())
            .await
            .expect("an event");
        assert_eq!(
            (from, event),
            (id, Event::Message(Message::Loaded { ms: 1 }))
        );
    }

    /// Events until `id` exits, in order.
    async fn until_exit(&mut self, id: WorkerId) -> Vec<Event> {
        let mut seen = Vec::new();
        tokio::time::timeout(LONG, async {
            loop {
                let (from, event) = self.registry.next_event().await;
                if from != id {
                    continue;
                }
                let done = matches!(event, Event::Exited(_));
                seen.push(event);
                if done {
                    break;
                }
            }
        })
        .await
        .expect("the worker exits");
        seen
    }
}

fn probe_hash() -> String {
    open_ferry_update::staging::hash_file(probe()).expect("hash")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_started_worker_reports_loaded_and_a_stop_ends_it_cleanly() {
    let mut rig = Rig::new("");
    let id = rig.start().await.expect("starts");
    let port = rig.registry.port(id).expect("port");
    assert_eq!(port.reported_version, "probe-1");
    assert_eq!(port.protocol, 1);
    let (from, first) = tokio::time::timeout(LONG, rig.registry.next_event())
        .await
        .expect("an event");
    assert_eq!(
        (from, first),
        (id, Event::Message(Message::Loaded { ms: 1 }))
    );
    assert!(rig.registry.send(id, Message::Stop { budget_ms: 1000 }));
    let events = rig.until_exit(id).await;
    match events.last() {
        Some(Event::Exited(exit)) => assert!(exit.success, "{exit:?}"),
        other => panic!("not an exit: {other:?}"),
    }
    assert!(
        rig.registry.port(id).is_none(),
        "an exited worker is removed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_channel_stops_a_worker_and_the_exit_is_the_last_event() {
    let mut rig = Rig::new("");
    let id = rig.start().await.expect("starts");
    rig.loaded(id).await;
    assert!(rig.registry.close(id));
    let events = rig.until_exit(id).await;
    assert!(matches!(events.last(), Some(Event::Exited(exit)) if exit.success));
    // Nothing follows the exit.
    assert!(
        events[..events.len() - 1]
            .iter()
            .all(|event| !matches!(event, Event::Exited(_)))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_ends_a_worker_that_ignores_stop_and_eof() {
    let mut rig = Rig::new("ignore");
    let id = rig.start().await.expect("starts");
    rig.loaded(id).await;
    assert!(rig.registry.send(id, Message::Stop { budget_ms: 10 }));
    wait_for_file(&rig.dir.path().join("ignored-stop")).await;
    assert!(rig.registry.close(id));
    wait_for_file(&rig.dir.path().join("ignored-eof")).await;
    assert!(rig.registry.port(id).is_some(), "it ignored both");
    assert!(rig.registry.kill(id));
    let events = rig.until_exit(id).await;
    assert!(matches!(events.last(), Some(Event::Exited(exit)) if !exit.success));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_all_kills_only_the_workers_that_do_not_stop() {
    let mut rig = Rig::new("");
    let polite = rig.start().await.expect("starts");
    std::fs::write(rig.dir.path().join("mode"), "ignore").expect("mode");
    let stubborn = rig.start().await.expect("starts");
    let killed = rig
        .registry
        .stop_all(Duration::from_secs(7), Duration::from_secs(10))
        .await;
    // The stubborn worker is killed after the budget; the polite one is not.
    assert_eq!(killed, vec![stubborn]);
    assert!(rig.registry.running().is_empty());
    assert_ne!(polite, stubborn);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_binary_that_does_not_hash_as_expected_is_not_started() {
    let mut rig = Rig::new("");
    let request = rig.request(&"0".repeat(64), LONG);
    match rig.driver.spawn(request, rig.registry.events()).await {
        Err(SpawnError::HashMismatch { found, .. }) => assert_eq!(found, probe_hash()),
        other => panic!("not refused: {other:?}"),
    }
    assert!(!rig.dir.path().join("out").exists(), "the probe never ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_binary_is_reported_as_missing() {
    let mut rig = Rig::new("");
    let mut request = rig.request(&probe_hash(), LONG);
    request.binary = rig.dir.path().join("no-such-binary");
    assert!(matches!(
        rig.driver.spawn(request, rig.registry.events()).await,
        Err(SpawnError::Missing)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_that_never_greets_times_out_and_leaves_no_process() {
    let mut rig = Rig::new("stall");
    let request = rig.request(&probe_hash(), Duration::from_millis(1500));
    let started = std::time::Instant::now();
    assert!(matches!(
        rig.driver.spawn(request, rig.registry.events()).await,
        Err(SpawnError::Timeout)
    ));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_probe_gone(rig.dir.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_that_needs_a_newer_supervisor_is_refused_and_ended() {
    let mut rig = Rig::new("min2-live");
    let request = rig.request(&probe_hash(), LONG);
    match rig.driver.spawn(request, rig.registry.events()).await {
        Err(SpawnError::Incompatible { needs_supervisor }) => assert_eq!(needs_supervisor, 2),
        other => panic!("not refused: {other:?}"),
    }
    assert_probe_gone(rig.dir.path()).await;
}

#[cfg(feature = "blue-green-test")]
struct NoRecovery;

#[cfg(feature = "blue-green-test")]
impl open_ferry_supervisor::host::Host for NoRecovery {
    fn topology(&self) -> Vec<ListenerTopology> {
        panic!("recovery went ahead")
    }
    fn bind(&self) -> std::io::Result<Vec<(String, TcpListener)>> {
        panic!("recovery went ahead")
    }
    fn effective_mode(&self) -> open_ferry_supervisor::host::ModeSnapshot {
        panic!("recovery went ahead")
    }
    fn own_version(&self) -> String {
        panic!("recovery went ahead")
    }
    fn own_sha256(&self) -> Option<String> {
        panic!("recovery went ahead")
    }
    fn waiting_for_update_lock(&self, _waited: Duration) {
        panic!("recovery went ahead")
    }
}

#[cfg(feature = "blue-green-test")]
impl open_ferry_update::runner::Runner for NoRecovery {
    fn version<'a>(
        &'a self,
        _binary: &'a Path,
        _timeout: Duration,
    ) -> open_ferry_update::fetch::BoxFuture<'a, Result<String, String>> {
        panic!("recovery went ahead")
    }
}

#[cfg(feature = "blue-green-test")]
async fn check_handshake_survivor(cancel: bool) {
    let mut rig = Rig::new(if cancel { "stall" } else { "min2-live" });
    let faults = rig.dir.path().join(".blue-green-test");
    std::fs::create_dir(&faults).unwrap();
    let gate = faults.join("termination-timeout.hold");
    std::fs::write(&gate, "yes").unwrap();
    let request = rig.request(&probe_hash(), LONG);
    let id = request.id;
    let driver = rig.driver.clone();
    let events = rig.registry.events();
    let spawning = tokio::spawn(async move { driver.spawn(request, events).await });
    wait_for_file(&rig.dir.path().join("pid")).await;
    if cancel {
        spawning.abort();
        assert!(spawning.await.unwrap_err().is_cancelled());
    } else {
        assert!(matches!(
            spawning.await.unwrap(),
            Err(SpawnError::Incompatible { .. })
        ));
    }
    wait_for_file(&faults.join("termination-timeout.arrived")).await;
    assert_eq!(rig.registry.running(), vec![id]);
    let request = rig.request(&probe_hash(), LONG);
    assert!(matches!(
        rig.driver.spawn(request, rig.registry.events()).await,
        Err(SpawnError::SurvivorPending)
    ));
    {
        let waiting = rig.registry.wait_until_empty();
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err()
        );
    }
    assert_eq!(
        rig.registry.running(),
        vec![id],
        "a wait deadline was mistaken for an exit"
    );
    let cfg = open_ferry_supervisor::startup::Config {
        data: open_ferry_update::data_dir::DataDir::at(rig.dir.path().join("recovery")),
        installed: probe().to_owned(),
        config: rig.dir.path().to_owned(),
        kind: ServiceKind::User,
        timings: Timings::default(),
        receipt: serde_json::Value::Null,
        binary_name: "worker_probe".to_owned(),
        runner: Arc::new(NoRecovery),
        detect_timeout: LONG,
        endpoint_location: None,
    };
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            open_ferry_supervisor::startup::start(
                &cfg,
                &NoRecovery,
                &rig.driver,
                &mut rig.registry,
                &open_ferry_supervisor::clock::SystemClock,
            )
        )
        .await
        .is_err(),
        "startup recovery bypassed a handshake survivor"
    );
    std::fs::remove_file(gate).unwrap();
    assert!(rig.registry.kill(id));
    tokio::time::timeout(LONG, rig.registry.wait_until_empty())
        .await
        .unwrap();
    assert!(rig.registry.running().is_empty());
    assert_probe_gone(rig.dir.path()).await;
    std::fs::write(rig.dir.path().join("mode"), "").unwrap();
    let next = rig.start().await.unwrap();
    rig.registry.kill(next);
    rig.until_exit(next).await;
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_handshake_survivor_blocks_recovery_and_the_next_spawn_until_exit() {
    check_handshake_survivor(false).await;
}

#[cfg(feature = "blue-green-test")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_handshake_survivor_blocks_recovery_and_the_next_spawn_until_exit() {
    check_handshake_survivor(true).await;
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(LONG, async {
        while !path.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("probe event");
}

async fn assert_probe_gone(dir: &Path) {
    let pid: u32 = std::fs::read_to_string(dir.join("pid"))
        .expect("probe ran")
        .parse()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            #[cfg(unix)]
            let gone = open_ferry_supervisor::process::unix::start_time(pid)
                .unwrap()
                .is_none();
            #[cfg(windows)]
            let gone = open_ferry_supervisor::process::windows::ProcessHandle::open_pid(pid)
                .unwrap()
                .is_none_or(|handle| handle.try_wait().unwrap().is_some());
            if gone {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed spawn left its process alive");
}

#[tokio::test]
async fn a_slow_reader_cannot_delay_kill_behind_queued_writes() {
    let mut rig = Rig::new("no-read");
    let id = rig.start().await.unwrap();
    rig.loaded(id).await;
    for _ in 0..100 {
        rig.registry.send(
            id,
            Message::Unsupported {
                message_type: "x".repeat(60_000),
            },
        );
    }
    rig.registry.kill(id);
    tokio::time::timeout(Duration::from_secs(3), rig.until_exit(id))
        .await
        .expect("kill blocked behind send");
    assert_probe_gone(rig.dir.path()).await;
}

#[cfg(windows)]
#[tokio::test]
async fn a_death_before_connect_is_observed_without_waiting_for_load_timeout() {
    let mut rig = Rig::new("early-exit");
    let request = rig.request(&probe_hash(), LONG);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        rig.driver.spawn(request, rig.registry.events()),
    )
    .await
    .expect("early exit was not watched");
    assert!(
        matches!(result, Err(SpawnError::Handshake(ref reason)) if reason.contains("exited before connecting"))
    );
    assert_probe_gone(rig.dir.path()).await;
}

/// The messages of the events a test's thread logs, each with the monotonic
/// instant the subscriber saw it.
#[cfg(all(windows, feature = "blue-green-test"))]
#[derive(Clone, Default)]
struct Logged(std::sync::Arc<std::sync::Mutex<Vec<(std::time::Instant, String)>>>);

#[cfg(all(windows, feature = "blue-green-test"))]
impl tracing::Subscriber for Logged {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
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
        self.0
            .lock()
            .unwrap()
            .push((std::time::Instant::now(), message.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[cfg(all(windows, feature = "blue-green-test"))]
#[tokio::test]
async fn connect_and_hello_share_one_load_deadline() {
    let mut rig = Rig::new("delayed-stall");
    // The probe connects 5 s after it starts and then never greets. One shared
    // budget of 8 s ends this 8 s after the spawn, so about 3 s after the
    // supervisor accepted the connection; a hello with a budget of its own
    // would end it 8 s after that. Measured from the driver's own
    // `driver.accepted` seam, which a hold and an already present release turn
    // into a file (so neither start-up, the hash nor a late write of the
    // probe's marker counts). Both instants are on the test process's
    // monotonic clock: a thread polls for that file every 25 ms and notes
    // `Instant::now()` when it first sees it, which can only be later than the
    // accept, by at most one interval, so the elapsed time is read short by at
    // most 25 ms. The ceiling is 5.5 s: 2.5 s above the correct time (3 s) and,
    // with the poll's error included, more than 2.4 s below a fresh-budget
    // hello (8 s, read as at least 7.975 s). A slow start-up only moves the
    // correct time earlier; it takes more than 3 s of it to miss the connect.
    let request = rig.request(&probe_hash(), Duration::from_secs(8));
    let seams = rig.dir.path().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("driver.accepted.hold"), []).unwrap();
    std::fs::write(seams.join("driver.accepted.release"), []).unwrap();
    let logged = Logged::default();
    let arrived = seams.join("driver.accepted.arrived");
    let spawned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let spawned = spawned.clone();
        std::thread::spawn(move || {
            loop {
                if arrived.exists() {
                    return Some(std::time::Instant::now());
                }
                if spawned.load(std::sync::atomic::Ordering::SeqCst) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };
    let result = {
        let _guard = tracing::subscriber::set_default(logged.clone());
        rig.driver.spawn(request, rig.registry.events()).await
    };
    let ended_at = std::time::Instant::now();
    spawned.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        matches!(result, Err(SpawnError::Timeout)),
        "{:?}",
        result.err()
    );
    // The test is about hello's budget. The probe must have connected before
    // the deadline (its marker), and the supervisor must have taken the
    // connection and then timed out in the hello phase: the marker alone says
    // only that the probe's open succeeded, so a late accept (the connect phase
    // timing out instead) is a set-up that was not met, not a pass.
    assert!(
        rig.dir.path().join("connected").is_file(),
        "set-up not met: the probe did not connect before the load deadline"
    );
    let logged = logged.0.lock().unwrap().clone();
    assert!(
        logged
            .iter()
            .any(|(_, line)| line.contains("connected but did not greet")),
        "set-up not met: the timeout did not come from the hello phase: {logged:?}"
    );
    // The driver's seam wrote the file when it took the connection: the
    // moment the connect phase was over, on the server's side.
    let accepted = watcher
        .join()
        .unwrap()
        .expect("set-up not met: the supervisor never accepted the connection");
    assert!(
        accepted < ended_at,
        "set-up not met: the accept was first seen only after the spawn ended"
    );
    let elapsed = ended_at.duration_since(accepted);
    assert!(
        elapsed < Duration::from_millis(5500),
        "hello got a separate budget: {elapsed:?} after the accept"
    );
    assert_probe_gone(rig.dir.path()).await;
}

/// On Windows, under `OFP_TEST_SCRATCH` when set (CI's runner, whose drive
/// root is owned by NETWORK SERVICE), else the target directory.
fn trusted_scratch() -> tempfile::TempDir {
    if cfg!(windows) {
        match std::env::var_os("OFP_TEST_SCRATCH") {
            Some(root) => tempfile::tempdir_in(root).unwrap(),
            None => tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
        }
    } else {
        tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap()
    }
}

fn foreign_writable(path: &Path) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trust_refusal_has_its_own_error_and_starts_no_worker() {
    let mut rig = Rig::new("");
    let dir = trusted_scratch();
    let binary = dir.path().join(probe().file_name().unwrap());
    std::fs::copy(probe(), &binary).unwrap();
    foreign_writable(&binary);
    let mut request = rig.request(&probe_hash(), LONG);
    request.binary = binary;
    let result = rig.driver.spawn(request, rig.registry.events()).await;
    assert!(
        matches!(result, Err(SpawnError::Untrusted(ref reason)) if reason.contains("write")),
        "{result:?}"
    );
    assert!(rig.registry.running().is_empty());
    assert!(!rig.dir.path().join("out").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_survivor_refusal_has_its_own_error() {
    let mut rig = Rig::new("");
    let request = rig.request(&probe_hash(), LONG);
    let id = request.id;
    // Returned, but not yet added: the real child is still tracked as a survivor.
    let port = rig
        .driver
        .spawn(request, rig.registry.events())
        .await
        .unwrap();
    let request = rig.request(&probe_hash(), LONG);
    assert!(matches!(
        rig.driver.spawn(request, rig.registry.events()).await,
        Err(SpawnError::SurvivorPending)
    ));
    rig.registry.add(port);
    rig.registry.kill(id);
    rig.until_exit(id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_writable_version_directory_is_refused_by_the_real_driver() {
    let mut rig = Rig::new("");
    let dir = trusted_scratch();
    let version = dir.path().join("versions").join("probe-1");
    std::fs::create_dir_all(&version).unwrap();
    let binary = version.join(probe().file_name().unwrap());
    std::fs::copy(probe(), &binary).unwrap();
    foreign_writable(&version);
    let mut request = rig.request(&probe_hash(), LONG);
    request.binary = binary;
    request.installation.data_dir = dir.path().to_owned();
    assert_ne!(request.binary, request.installation.installed);
    let result = rig.driver.spawn(request, rig.registry.events()).await;
    assert!(
        matches!(result, Err(SpawnError::Untrusted(ref reason)) if reason.contains("probe-1") && reason.contains("write")),
        "{result:?}"
    );
    assert!(rig.registry.running().is_empty());
    assert!(
        !rig.dir.path().join("out").exists(),
        "probe was not started"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_an_unregistered_port_blocks_only_until_its_child_exits() {
    let mut rig = Rig::new("ignore");
    let request = rig.request(&probe_hash(), LONG);
    let id = request.id;
    let port = rig
        .driver
        .spawn(request, rig.registry.events())
        .await
        .unwrap();
    drop(port);
    let request = rig.request(&probe_hash(), LONG);
    assert!(matches!(
        rig.driver.spawn(request, rig.registry.events()).await,
        Err(SpawnError::SurvivorPending)
    ));
    // On a reverted pump, release the child by our tracked handle after the
    // bounded wait, so a failed test never intentionally leaves it running.
    let ended = tokio::time::timeout(Duration::from_secs(3), rig.until_exit(id)).await;
    if ended.is_err() {
        rig.registry.kill(id);
        rig.until_exit(id).await;
    }
    assert!(ended.is_ok(), "dropping the port left its child alive");
    assert!(rig.registry.survivors().is_empty());
    let second = rig.start().await.unwrap();
    rig.registry.kill(second);
    rig.until_exit(second).await;
}
