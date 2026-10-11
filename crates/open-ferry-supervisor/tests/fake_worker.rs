#![cfg(feature = "blue-green-test")]

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_ferry_supervisor::driver::{Cmd, Driver, Event, Registry, SpawnRequest, WorkerPort};
use open_ferry_supervisor::endpoint::ServiceKind;
use open_ferry_supervisor::protocol::{Installation, ListenerTopology, Message, Role};
use open_ferry_supervisor::timings::Timings;
use open_ferry_supervisor::{driver::ProcessDriver, protocol::VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const ETXTBSY: i32 = 26;
const WAIT: Duration = Duration::from_secs(10);
const STOP_MESSAGE_MS: u64 = 250;
// Each test has at most two workers. Keep at most four test lifetimes (eight
// processes) alive, including panic cleanup, regardless of harness parallelism.
static TEST_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

// One implementation today; phase 2c can supply the real worker here.
struct WorkerUnderTest;
impl WorkerUnderTest {
    fn fake_only_faults() -> bool {
        true
    }

    fn prepare(root: &Path, marker: &str) -> (PathBuf, PathBuf) {
        let dir = root.join(format!("worker-{marker}"));
        std::fs::create_dir_all(&dir).unwrap();
        let source = PathBuf::from(env!("CARGO_BIN_EXE_fake_worker"));
        let binary = dir.join(source.file_name().unwrap());
        std::fs::copy(source, &binary).unwrap();
        std::fs::write(dir.join("test-marker"), format!(" {marker} ")).unwrap();
        let config = dir.join("config.yaml");
        std::fs::write(&config, "any config contents\n").unwrap();
        (binary, config)
    }
}

#[test]
fn welcome_stop_budget_matches_the_stop_message_budget() {
    let dir = tempfile::tempdir().unwrap();
    let t = Test::new(dir.path().to_owned(), Arc::new(Mutex::new(Vec::new())));
    assert_eq!(t.stop_ms(), STOP_MESSAGE_MS);
}

fn assert_marker(response: &str, marker: &str) {
    assert!(
        response.contains(&format!("\r\nx-open-ferry-test-marker: {marker}\r\n")),
        "{response}"
    );
}

struct Test {
    dir: PathBuf,
    registry: Registry,
    listeners: Arc<Vec<(String, TcpListener)>>,
    owned: Arc<Mutex<Vec<WorkerPort>>>,
    timings: Timings,
}

impl Test {
    fn new(dir: PathBuf, owned: Arc<Mutex<Vec<WorkerPort>>>) -> Self {
        let timings = Timings {
            load: Duration::from_millis(500),
            resume: Duration::from_millis(500),
            // stop_message_budget() reserves a 5 s margin for the handle kill.
            stop: Duration::from_millis(STOP_MESSAGE_MS) + Duration::from_secs(5),
            drain: Duration::from_millis(500),
            ..Timings::default()
        };
        Self {
            dir,
            registry: Registry::new(),
            listeners: Arc::new(vec![
                ("main".into(), TcpListener::bind("127.0.0.1:0").unwrap()),
                (
                    "management".into(),
                    TcpListener::bind("127.0.0.1:0").unwrap(),
                ),
            ]),
            owned,
            timings,
        }
    }

    fn stop_ms(&self) -> u64 {
        self.timings.budgets().stop_ms
    }

    fn root(&self) -> &Path {
        &self.dir
    }

    fn seam(&self, name: &str) -> PathBuf {
        self.root().join(".blue-green-test").join(name)
    }

    fn set(&self, name: &str, content: &str) {
        std::fs::create_dir_all(self.root().join(".blue-green-test")).unwrap();
        std::fs::write(self.seam(name), content).unwrap();
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.seam(name)).unwrap();
    }

    async fn spawn(&mut self, marker: &str, role: Role) -> u64 {
        let (binary, config) = WorkerUnderTest::prepare(self.root(), marker);
        let id = self.registry.next_id();
        let topology = self
            .listeners
            .iter()
            .map(|(name, listener)| ListenerTopology {
                name: name.clone(),
                address: listener.local_addr().unwrap(),
                tls: false,
            })
            .collect();
        let request = SpawnRequest {
            id,
            version: env!("CARGO_PKG_VERSION").into(),
            sha256: open_ferry_update::staging::hash_file(&binary).unwrap(),
            binary: binary.clone(),
            role,
            switch_id: Some("test-switch".into()),
            config,
            installation: Installation {
                installed: binary,
                data_dir: self.root().to_owned(),
                receipt: serde_json::json!({}),
            },
            topology,
            listeners: self.listeners.clone(),
            budgets: self.timings.budgets(),
            handshake_timeout: WAIT,
            log: Some(self.root().join(format!("{marker}.log"))),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let port = loop {
            let result = ProcessDriver::new(ServiceKind::User, Duration::from_millis(5))
                .spawn(request.clone(), self.registry.events())
                .await;
            if cfg!(target_os = "linux")
                && matches!(&result, Err(open_ferry_supervisor::driver::SpawnError::Start(error)) if error.raw_os_error() == Some(ETXTBSY))
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            break result.unwrap();
        };
        self.owned.lock().unwrap().push(port.clone());
        assert_eq!(port.reported_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(port.protocol, VERSION);
        self.registry.add(port);
        id
    }

    fn send(&self, id: u64, message: Message) {
        assert!(self.registry.send(id, message));
    }

    async fn message(&mut self, id: u64, matches: impl Fn(&Message) -> bool) -> Message {
        tokio::time::timeout(WAIT, async {
            loop {
                let (worker, event) = self.registry.next_event().await;
                match event {
                    Event::Message(message) if worker == id && matches(&message) => return message,
                    Event::Message(Message::Status { .. }) => {}
                    // An active start reports `loaded` too, when the test is
                    // waiting for something else (the test of that is
                    // `an_active_start_sends_loaded_before_its_first_active_status`).
                    Event::Message(Message::Loaded { .. }) => {}
                    Event::Message(_) if worker != id => {}
                    event => panic!("unexpected event {worker}: {event:?}"),
                }
            }
        })
        .await
        .expect("worker message timeout")
    }

    async fn loaded(&mut self, id: u64) {
        self.message(id, |m| matches!(m, Message::Loaded { .. }))
            .await;
    }

    async fn handover(&mut self, id: u64) -> String {
        self.send(id, Message::HandOver { budget_ms: 1000 });
        match self
            .message(id, |m| matches!(m, Message::HandedOver { .. }))
            .await
        {
            Message::HandedOver { state_id, .. } => state_id,
            _ => unreachable!(),
        }
    }

    async fn exited(&mut self, id: u64, success: bool) -> open_ferry_supervisor::driver::ExitInfo {
        tokio::time::timeout(WAIT, async {
            loop {
                let (worker, event) = self.registry.next_event().await;
                if let Event::Exited(exit) = event {
                    assert_eq!(worker, id);
                    assert_eq!(exit.success, success, "{exit:?}");
                    return exit;
                }
            }
        })
        .await
        .expect("exit timeout")
    }

    async fn logged(&self, marker: &str, event: &str) {
        tokio::time::timeout(WAIT, async {
            loop {
                if self
                    .events()
                    .iter()
                    .any(|line| line["marker"] == marker && line["event"] == event)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("event-log timeout");
    }

    fn events(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.seam("events.log"))
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str(line).expect("invalid event-log JSON line"))
            .collect()
    }

    async fn arrived(&self, barrier: &str) {
        tokio::time::timeout(WAIT, async {
            while !self.seam(&format!("{barrier}.arrived")).exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("barrier timeout");
    }

    fn lease(&self) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root().join("active-worker.lock"))
            .unwrap()
    }

    async fn lock_after_exit(&self) {
        tokio::time::timeout(WAIT, async {
            loop {
                if self.lease().try_lock().is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("lease not released");
    }

    async fn phase(&mut self, id: u64, expected: &str) {
        self.send(id, Message::Ping);
        self.message(
            id,
            |m| matches!(m, Message::Status { phase, .. } if phase == expected),
        )
        .await;
    }

    async fn ping(&mut self, id: u64, phase: &str) {
        self.phase(id, phase).await;
        self.ping_reply(id, phase).await;
    }

    async fn ping_reply(&mut self, id: u64, phase: &str) {
        // Drain all queued statuses before measuring the actual Ping round trip.
        self.send(id, Message::Unknown);
        self.fence(id, phase).await;
        for round in 0..2 {
            self.send(id, Message::Ping);
            self.send(id, Message::Unknown);
            let statuses = self.fence(id, phase).await;
            assert!(statuses >= 1, "ping had no status reply before its fence");
            // A periodic 5 s tick can overlap one millisecond-wide round, not
            // two consecutive ones. Redo once without assuming its epoch.
            if round == 0 && statuses == 2 {
                continue;
            }
            assert!(statuses <= 1, "extra ping status");
            break;
        }
    }

    async fn fence(&mut self, id: u64, phase: &str) -> usize {
        tokio::time::timeout(WAIT, async {
            let mut statuses = 0;
            loop {
                let (worker, event) = self.registry.next_event().await;
                if worker != id {
                    assert!(
                        matches!(event, Event::Message(Message::Status { .. })),
                        "{event:?}"
                    );
                    continue;
                }
                match event {
                    Event::Message(Message::Status { phase: p, .. }) => {
                        assert_eq!(p, phase);
                        statuses += 1;
                    }
                    Event::Message(Message::Unsupported { message_type }) => {
                        assert_eq!(message_type, "unknown");
                        return statuses;
                    }
                    event => panic!("unexpected ping event: {event:?}"),
                }
            }
        })
        .await
        .expect("ping fence timeout")
    }

    async fn live(&mut self, id: u64) {
        tokio::time::timeout(WAIT, async {
            loop {
                self.send(id, Message::Ping);
                let message = self
                    .message(id, |m| matches!(m, Message::Status { .. }))
                    .await;
                if matches!(message, Message::Status { live, .. } if live > 0) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn http(&self, index: usize) -> String {
        self.http_path(index, "/").await
    }

    async fn http_path(&self, index: usize, path: &str) -> String {
        tokio::time::timeout(WAIT, async {
            let mut stream = TcpStream::connect(self.listeners[index].1.local_addr().unwrap())
                .await
                .unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        })
        .await
        .expect("HTTP timeout")
    }
}

impl Drop for Test {
    fn drop(&mut self) {
        for id in self.registry.running() {
            self.registry.kill(id);
        }
    }
}

// A panic in the test task still leaves a runtime alive to deliver handle kills
// and await pump termination (which reaps the child). No pid-based cleanup.
fn run<F, Fut>(body: F)
where
    F: FnOnce(Test) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _permit = TEST_PERMITS.acquire().await.unwrap();
        let owned = Arc::new(Mutex::new(Vec::<WorkerPort>::new()));
        let dir = tempfile::tempdir().unwrap();
        let test = Test::new(dir.path().to_owned(), owned.clone());
        let result = tokio::spawn(body(test)).await;
        let ports = owned.lock().unwrap().clone();
        for port in &ports {
            let _ = port.cmd.send(Cmd::Kill);
        }
        let mut cleanup_failed = false;
        for port in ports {
            if tokio::time::timeout(WAIT, port.cmd.closed()).await.is_err() {
                eprintln!("cleanup did not reap worker");
                cleanup_failed = true;
            }
        }
        if cleanup_failed {
            eprintln!(
                "cleanup failed; keeping worker directory {}",
                dir.keep().display()
            );
        } else {
            drop(dir);
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error.into_panic());
        }
        assert!(!cleanup_failed, "cleanup did not reap all workers");
    });
}

#[test]
fn active_handshake_http_status_ping_unknown_stop_and_eof() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        assert!(t.lease().try_lock().is_err());
        let response = t.http(0).await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Connection: close\r\n"));
        assert_marker(&response, "A");
        let management = t.http_path(1, "/v0/management/config").await;
        assert!(!management.starts_with("HTTP/1.1 503 "));
        assert_marker(&management, "A");
        t.ping(a, "active").await;
        t.send(
            a,
            Message::Status {
                mode: "".into(),
                live: 0,
                signins_pending: 0,
                phase: "".into(),
            },
        );
        t.ping_reply(a, "active").await;
        t.send(a, Message::Unknown);
        assert_eq!(
            t.message(a, |m| matches!(m, Message::Unsupported { .. }))
                .await,
            Message::Unsupported {
                message_type: "unknown".into()
            }
        );
        let start = tokio::time::Instant::now();
        t.send(a, Message::Stop { budget_ms: 250 });
        t.exited(a, true).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        t.lock_after_exit().await;
        let b = t.spawn("B", Role::Active).await;
        t.logged("B", "active").await;
        let start = tokio::time::Instant::now();
        t.registry.close(b);
        t.exited(b, true).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        t.lock_after_exit().await;
    });
}

/// A worker started active reports `loaded` once it is serving, before its
/// first status as active (as a standby does before its hold).
#[test]
fn an_active_start_sends_loaded_before_its_first_active_status() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        let seen = tokio::time::timeout(WAIT, async {
            let mut seen = Vec::new();
            loop {
                let (worker, event) = t.registry.next_event().await;
                if worker != a {
                    continue;
                }
                match event {
                    Event::Message(Message::Loaded { .. }) => return seen,
                    Event::Message(Message::Status { phase, .. }) if phase == "active" => {
                        seen.push(phase);
                        return seen;
                    }
                    Event::Message(Message::Status { phase, .. }) => seen.push(phase),
                    event => panic!("unexpected event {event:?}"),
                }
            }
        })
        .await
        .expect("no loaded or active status");
        assert!(
            !seen.iter().any(|phase| phase == "active"),
            "the first active status came before `loaded`: {seen:?}"
        );
    });
}

#[test]
fn full_switch_lease_state_serving_peers_and_release() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        assert_marker(&t.http(0).await, "A");
        let id = t.handover(a).await;
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(t.root().join("handover.json")).unwrap())
                .unwrap();
        assert_eq!(state["id"], id);
        let lease = t.lease();
        lease.try_lock().unwrap();
        drop(lease);
        t.send(b, Message::TakeOver);
        t.message(b, |m| matches!(m, Message::Ready)).await;
        assert!(!t.root().join("handover.json").exists());
        assert!(t.lease().try_lock().is_err());
        assert_marker(&t.http(0).await, "B");
        t.send(b, Message::Go);
        t.logged("B", "go").await;
        let management = t.http_path(1, "/v0/management/config").await;
        assert!(!management.starts_with("HTTP/1.1 503 "));
        assert_marker(&management, "B");
        t.send(b, Message::PeersGone);
        t.ping(b, "watching").await;
        t.send(a, Message::Release);
        t.message(a, |m| matches!(m, Message::Drained)).await;
        t.exited(a, true).await;
        let events = t.events();
        let names: Vec<_> = events
            .iter()
            .filter(|line| line["marker"] == "A")
            .map(|line| line["event"].as_str().unwrap())
            .collect();
        for pair in [
            ["handover.gate_closing", "handover.gate_closed"],
            ["handover.written", "lease.release"],
            ["lease.release", "handed_over"],
            ["draining", "drained"],
        ] {
            assert!(
                names.iter().position(|name| *name == pair[0]).unwrap()
                    < names.iter().position(|name| *name == pair[1]).unwrap()
            );
        }
        assert!(
            events
                .iter()
                .all(|line| line["pid"].is_u64() && line["lease"].is_boolean())
        );
    });
}

#[test]
fn held_gate_closed_and_refresh_barriers_then_resume() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.set("fault.hold-refresh", "");
        t.set("refresh.before_save.hold", "");
        t.set("handover.gate_closed.hold", "");
        t.send(a, Message::HandOver { budget_ms: 1000 });
        t.arrived("refresh.before_save").await;
        t.ping(a, "handing-over").await;
        assert_marker(&t.http(0).await, "A");
        assert!(
            t.events()
                .iter()
                .any(|line| line["event"] == "handover.gate_closing")
        );
        t.set("refresh.before_save.release", "");
        t.arrived("handover.gate_closed").await;
        t.ping(a, "handing-over").await;
        let management = t.http_path(1, "/v0/management/config").await;
        assert!(management.starts_with("HTTP/1.1 503 "));
        assert_marker(&management, "A");
        assert!(t.lease().try_lock().is_err());
        assert!(!t.root().join("handover.json").exists());
        t.ping(a, "handing-over").await;
        assert!(t.lease().try_lock().is_err());
        t.set("handover.gate_closed.release", "");
        t.message(a, |m| matches!(m, Message::HandedOver { .. }))
            .await;
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        t.ping(a, "active").await;
        assert!(t.lease().try_lock().is_err());
        assert_marker(&t.http(0).await, "A");
        let management = t.http_path(1, "/v0/management/config").await;
        assert!(!management.starts_with("HTTP/1.1 503 "));
        assert_marker(&management, "A");
    });
}

#[test]
fn load_ready_slow_ignore_and_lease_conflict_faults() {
    run(|mut t| async move {
        t.set("fault.fail-load", "");
        let bad = t.spawn("bad", Role::Standby).await;
        t.message(bad, |m| matches!(m, Message::LoadFailed { .. }))
            .await;
        assert!(t.lease().try_lock().is_ok());
        assert_eq!(t.exited(bad, false).await.code, Some(1));
        t.remove("fault.fail-load");
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.set("fault.ignore-handover", "");
        t.send(a, Message::HandOver { budget_ms: 50 });
        t.ping(a, "active").await;
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        t.ping(a, "active").await;
        assert!(t.lease().try_lock().is_err());
        assert_marker(&t.http(0).await, "A");
        assert!(!t.root().join("handover.json").exists());
        t.remove("fault.ignore-handover");
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        t.handover(a).await;
        let lease = t.lease();
        lease.try_lock().unwrap();
        let handover = std::fs::read(t.root().join("handover.json")).unwrap();
        t.send(b, Message::TakeOver);
        assert!(
            matches!(t.message(b, |m| matches!(m, Message::TakeFailed { .. })).await, Message::TakeFailed { reason } if reason.starts_with("lease:"))
        );
        assert_eq!(
            std::fs::read(t.root().join("handover.json")).unwrap(),
            handover
        );
        t.send(b, Message::Stop { budget_ms: 100 });
        t.exited(b, true).await;
        drop(lease);
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        t.handover(a).await;
        let b = t.spawn("failed", Role::Standby).await;
        t.loaded(b).await;
        t.set("fault.fail-before-ready", "");
        t.send(b, Message::TakeOver);
        t.message(b, |m| matches!(m, Message::TakeFailed { .. }))
            .await;
        assert!(t.lease().try_lock().is_err());
        t.send(b, Message::Stop { budget_ms: 100 });
        t.exited(b, true).await;
        t.lock_after_exit().await;
        let b = t.spawn("C", Role::Standby).await;
        t.loaded(b).await;
        std::fs::write(
            t.root().join("handover.json"),
            serde_json::to_vec(&serde_json::json!({"id": "0".repeat(32)})).unwrap(),
        )
        .unwrap();
        t.remove("fault.fail-before-ready");
        t.set("fault.slow-ready", "200");
        let start = tokio::time::Instant::now();
        t.send(b, Message::TakeOver);
        t.message(b, |m| matches!(m, Message::Ready)).await;
        assert!(start.elapsed() >= Duration::from_millis(200));
    });
}

#[test]
fn exit_after_every_named_step_is_immediate_nonzero() {
    for step in ["welcome", "loaded", "handed_over", "ready", "go"] {
        run(move |mut t| async move {
            let a = t.spawn("A", Role::Active).await;
            t.logged("A", "active").await;
            t.set("fault.exit-after", step);
            if step == "handed_over" {
                t.send(a, Message::HandOver { budget_ms: 1000 });
                assert_eq!(t.exited(a, false).await.code, Some(42));
                t.logged("A", step).await;
                assert!(t.root().join("handover.json").exists());
            } else {
                let b = t.spawn("B", Role::Standby).await;
                if step != "welcome" && step != "loaded" {
                    t.loaded(b).await;
                    t.handover(a).await;
                    t.send(b, Message::TakeOver);
                    if step == "go" {
                        t.message(b, |m| matches!(m, Message::Ready)).await;
                        t.send(b, Message::Go);
                    }
                }
                assert_eq!(t.exited(b, false).await.code, Some(42));
                if step != "welcome" {
                    t.logged("B", step).await;
                }
            }
        });
    }
}

#[test]
fn dropped_channel_is_live_and_keeps_lease_until_handle_kill() {
    for phase in ["handing-over", "taking-over", "watching"] {
        run(move |mut t| async move {
            let a = t.spawn("A", Role::Active).await;
            t.logged("A", "active").await;
            let lost = if phase == "handing-over" {
                t.set("fault.drop-channel", phase);
                t.send(a, Message::HandOver { budget_ms: 1000 });
                a
            } else {
                let b = t.spawn("B", Role::Standby).await;
                t.loaded(b).await;
                t.handover(a).await;
                t.set("fault.drop-channel", phase);
                t.send(b, Message::TakeOver);
                if phase == "watching" {
                    t.message(b, |m| matches!(m, Message::Ready)).await;
                    t.send(b, Message::Go);
                }
                b
            };
            tokio::time::timeout(WAIT, async {
                loop {
                    let (id, event) = t.registry.next_event().await;
                    match event {
                        Event::Lost(_) if id == lost => break,
                        Event::Message(_) => {}
                        event => panic!("expected lost, got {id}: {event:?}"),
                    }
                }
            })
            .await
            .unwrap();
            assert!(t.lease().try_lock().is_err());
            t.registry.kill(lost);
            let exit = t.exited(lost, false).await;
            #[cfg(unix)]
            {
                assert_eq!(
                    exit.code, None,
                    "worker must be killed by a signal: {exit:?}"
                );
                assert!(
                    exit.text.contains("signal: 9"),
                    "expected SIGKILL: {exit:?}"
                );
            }
            // Windows' kill code (1) cannot be distinguished from an exit 1
            // initiated by the worker; exited() still checks unsuccessful exit.
            #[cfg(windows)]
            let _ = exit;
            t.lock_after_exit().await;
        });
    }
}

#[test]
fn release_finishes_an_open_request_before_drained() {
    run(|mut t| async move {
        t.timings.drain = Duration::from_secs(60);
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let mut stream = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
            .await
            .unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        t.live(a).await;
        t.send(a, Message::Release);
        t.logged("A", "draining").await;
        t.ping(a, "draining").await;
        assert!(!t.events().iter().any(|line| line["event"] == "drained"));
        stream.write_all(b"Host: localhost\r\n\r\n").await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(WAIT, stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_marker(&response, "A");
        t.message(a, |m| matches!(m, Message::Drained)).await;
        t.exited(a, true).await;
    });
}

#[test]
fn release_bounds_a_partial_request_by_the_welcome_drain_budget() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let mut stream = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
            .await
            .unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        t.live(a).await;
        let start = tokio::time::Instant::now();
        t.send(a, Message::Release);
        t.message(a, |m| matches!(m, Message::Drained)).await;
        assert_eq!(t.exited(a, true).await.code, Some(0));
        assert!(start.elapsed() >= Duration::from_millis(450));
        assert!(start.elapsed() < Duration::from_secs(2));
        t.lock_after_exit().await;
    });
}

#[test]
fn stop_bounds_an_unfinished_connection() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let mut stream = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
            .await
            .unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        t.live(a).await;
        let start = tokio::time::Instant::now();
        t.send(a, Message::Stop { budget_ms: 100 });
        t.exited(a, true).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        t.lock_after_exit().await;
    });
}

#[test]
fn stop_and_eof_interrupt_held_barriers_and_slow_ready() {
    for eof in [false, true] {
        for step in ["handover", "ready", "drain"] {
            run(move |mut t| async move {
                // Normal drain must not satisfy this test if stop/EOF is ignored.
                t.timings.drain = Duration::from_secs(60);
                let a = t.spawn("A", Role::Active).await;
                t.logged("A", "active").await;
                let mut held_connection = None;
                let stopped = match step {
                    "handover" => {
                        t.set("handover.gate_closed.hold", "");
                        t.send(a, Message::HandOver { budget_ms: 1000 });
                        t.arrived("handover.gate_closed").await;
                        a
                    }
                    "ready" => {
                        let b = t.spawn("B", Role::Standby).await;
                        t.loaded(b).await;
                        t.handover(a).await;
                        t.set("fault.slow-ready", "60000");
                        t.send(b, Message::TakeOver);
                        tokio::time::timeout(WAIT, async {
                            while t.root().join("handover.json").exists() {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        })
                        .await
                        .expect("take-over must enter slow-ready before stop");
                        t.ping(b, "taking-over").await;
                        b
                    }
                    "drain" => {
                        let mut stream = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
                            .await
                            .unwrap();
                        stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
                        held_connection = Some(stream);
                        t.live(a).await;
                        t.send(a, Message::Release);
                        t.logged("A", "draining").await;
                        t.ping(a, "draining").await;
                        assert!(!t.events().iter().any(|line| line["event"] == "drained"));
                        a
                    }
                    _ => unreachable!(),
                };
                let start = tokio::time::Instant::now();
                if eof {
                    t.registry.close(stopped);
                } else {
                    t.send(stopped, Message::Stop { budget_ms: 100 });
                }
                t.exited(stopped, true).await;
                assert!(start.elapsed() < Duration::from_secs(1));
                t.lock_after_exit().await;
                drop(held_connection);
            });
        }
    }
}

#[test]
fn lease_release_and_reload_barriers_and_barrier_failure() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        t.set("lease.release.hold", "");
        t.send(a, Message::HandOver { budget_ms: 1000 });
        t.arrived("lease.release").await;
        assert!(t.root().join("handover.json").exists());
        assert!(t.lease().try_lock().is_err());
        t.set("lease.release.release", "");
        t.message(a, |m| matches!(m, Message::HandedOver { .. }))
            .await;
        t.set("take_over.before_reload.hold", "");
        t.send(b, Message::TakeOver);
        t.arrived("take_over.before_reload").await;
        assert!(t.root().join("handover.json").exists());
        assert!(t.lease().try_lock().is_err());
        t.set("take_over.before_reload.release", "");
        t.message(b, |m| matches!(m, Message::Ready)).await;
        assert!(!t.root().join("handover.json").exists());
        t.send(b, Message::Go);
        t.logged("B", "go").await;
        t.set("handover.gate_closing.fail", "");
        t.send(b, Message::HandOver { budget_ms: 1000 });
        t.logged("B", "handover_failed").await;
        assert!(t.seam("handover.gate_closing.failed").exists());
        assert!(t.lease().try_lock().is_err());
        assert_marker(&t.http(0).await, "B");
        t.send(b, Message::Resume);
        t.message(b, |m| matches!(m, Message::Resumed)).await;
        t.ping(b, "active").await;
    });
}

#[test]
fn resume_cancels_a_held_handover_without_late_lease_release() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.set("handover.gate_closed.hold", "");
        t.send(a, Message::HandOver { budget_ms: 1000 });
        t.arrived("handover.gate_closed").await;
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        t.ping(a, "active").await;
        assert!(t.lease().try_lock().is_err());
        assert_marker(&t.http(0).await, "A");
        t.set("handover.gate_closed.release", "");
        t.ping(a, "active").await;
        assert!(t.lease().try_lock().is_err());
        assert!(!t.root().join("handover.json").exists());
        assert!(!t.events().iter().any(|line| line["event"] == "handed_over"));
        t.send(a, Message::Stop { budget_ms: 100 });
        t.exited(a, true).await;
        t.lock_after_exit().await;
        assert!(!t.root().join("handover.json").exists());
        assert!(!t.events().iter().any(|line| line["event"] == "handed_over"));
    });
}

#[test]
fn targeted_fault_only_affects_matching_worker() {
    run(|mut t| async move {
        t.set("fault.fail-load", "@B");
        let a = t.spawn("A", Role::Standby).await;
        t.loaded(a).await;
        let b = t.spawn("B", Role::Standby).await;
        assert_eq!(
            t.message(b, |m| matches!(m, Message::LoadFailed { .. }))
                .await,
            Message::LoadFailed {
                reason: "fail-load".into()
            }
        );
        t.exited(b, false).await;
        t.ping(a, "standby").await;
        t.send(a, Message::Stop { budget_ms: 100 });
        t.exited(a, true).await;
    });
}

#[test]
fn signins_pending_is_reported_while_its_fault_file_exists_and_0_after() {
    run(|mut t| async move {
        t.set("fault.signins-pending", "3@A");
        let a = t.spawn("A", Role::Standby).await;
        t.loaded(a).await;
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        // Every answer is read afresh, so wait for a ping's own answer.
        async fn pending(t: &mut Test, id: u64) -> u64 {
            t.send(id, Message::Unknown);
            t.fence(id, "standby").await;
            t.send(id, Message::Ping);
            match t.message(id, |m| matches!(m, Message::Status { .. })).await {
                Message::Status {
                    signins_pending, ..
                } => signins_pending,
                other => panic!("not a status: {other:?}"),
            }
        }
        assert_eq!(pending(&mut t, a).await, 3, "A reports the count");
        assert_eq!(pending(&mut t, b).await, 0, "B's marker does not match");
        t.remove("fault.signins-pending");
        assert_eq!(pending(&mut t, a).await, 0, "removing the file releases it");
        t.set("fault.signins-pending", "7");
        assert_eq!(
            pending(&mut t, b).await,
            7,
            "an untargeted count reaches all"
        );
    });
}

#[test]
fn spawn_grandchild_logs_the_child_pid_and_the_child_is_alive() {
    fn alive(pid: u64) -> bool {
        if cfg!(windows) {
            let out = std::process::Command::new("tasklist")
                .args(["/NH", "/FI", &format!("PID eq {pid}")])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
        } else {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        }
    }
    run(|mut t| async move {
        t.set("fault.spawn-grandchild", "@G");
        let other = t.spawn("other", Role::Active).await;
        t.logged("other", "active").await;
        assert!(
            t.events().iter().all(|line| line["event"] != "grandchild"),
            "the fault is targeted at G"
        );
        t.send(other, Message::Stop { budget_ms: 100 });
        t.exited(other, true).await;
        let g = t.spawn("G", Role::Active).await;
        t.logged("G", "grandchild").await;
        let events = t.events();
        let line = events
            .iter()
            .find(|line| line["event"] == "grandchild")
            .expect("the event");
        let child = line["child_pid"].as_u64().expect("a child_pid");
        assert_ne!(child, u64::from(std::process::id()));
        assert_ne!(Some(child), line["pid"].as_u64(), "a process of its own");
        assert!(alive(child), "the grandchild is not running");
        t.send(g, Message::Stop { budget_ms: 100 });
        t.exited(g, true).await;
    });
}

/// The grandchild is told its worker's pid and ends when its parent is any
/// other process, at once: a worker that died before the grandchild first
/// looked at its parent is noticed too. Started here with a pid that is not
/// this test's, so its parent differs from the one it was told.
#[cfg(unix)]
#[test]
fn a_grandchild_whose_parent_is_not_the_worker_exits_at_once() {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fake_worker"))
        .arg("-grandchild-sleep")
        .arg((std::process::id().wrapping_add(1)).to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if began.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the grandchild outlived a parent that is not the one it was told");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn targeted_value_faults_match_and_do_not_match_worker_markers() {
    run(|mut t| async move {
        t.set("fault.exit-after", "loaded@B");
        let a = t.spawn("A", Role::Standby).await;
        t.loaded(a).await;
        let b = t.spawn("B", Role::Standby).await;
        assert_eq!(t.exited(b, false).await.code, Some(42));
        t.logged("B", "loaded").await;
        t.ping(a, "standby").await;
        t.send(a, Message::Stop { budget_ms: 100 });
        t.exited(a, true).await;
        t.remove("fault.exit-after");
        let active = t.spawn("active", Role::Active).await;
        t.logged("active", "active").await;
        let slow = t.spawn("slow", Role::Standby).await;
        t.loaded(slow).await;
        t.handover(active).await;
        t.set("fault.slow-ready", "200@slow");
        let start = tokio::time::Instant::now();
        t.send(slow, Message::TakeOver);
        t.message(slow, |m| matches!(m, Message::Ready)).await;
        assert!(start.elapsed() >= Duration::from_millis(200));
        assert_marker(&t.http(0).await, "slow");
    });
}

#[test]
fn fail_load_exits_one_at_active_and_standby_loads() {
    for role in [Role::Active, Role::Standby] {
        run(move |mut t| async move {
            t.set("fault.fail-load", "");
            let a = t.spawn("A", role).await;
            assert_eq!(
                t.message(a, |m| matches!(m, Message::LoadFailed { .. }))
                    .await,
                Message::LoadFailed {
                    reason: "fail-load".into()
                }
            );
            assert_eq!(t.exited(a, false).await.code, Some(1));
            t.lock_after_exit().await;
            assert!(
                !t.events()
                    .iter()
                    .any(|e| e["event"] == "loaded" || e["event"] == "active")
            );
        });
    }
}

#[test]
#[should_panic(expected = "invalid event-log JSON line")]
fn malformed_event_log_is_not_silently_skipped() {
    run(|t| async move {
        t.set("events.log", "not-json\n");
        t.events();
    });
}

#[test]
fn transitions_clear_their_stale_barrier_arrivals() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.set("handover.gate_closed.hold", "");
        t.set("lease.release.arrived", "stale");
        t.send(a, Message::HandOver { budget_ms: 1000 });
        t.arrived("handover.gate_closed").await;
        assert!(!t.seam("lease.release.arrived").exists());
        t.set("handover.gate_closed.release", "");
        t.message(a, |m| matches!(m, Message::HandedOver { .. }))
            .await;
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        let lease = t.lease();
        lease.try_lock().unwrap();
        t.set("take_over.before_reload.arrived", "stale");
        t.send(b, Message::TakeOver);
        t.message(b, |m| matches!(m, Message::TakeFailed { .. }))
            .await;
        assert!(!t.seam("take_over.before_reload.arrived").exists());
        t.send(b, Message::Stop { budget_ms: 100 });
        t.exited(b, true).await;
        drop(lease);
    });
}

#[test]
fn missing_corrupt_handover_and_takeover_barrier_fail_without_exit() {
    for kind in ["missing", "corrupt", "barrier"] {
        run(move |mut t| async move {
            let b = t.spawn("B", Role::Standby).await;
            t.loaded(b).await;
            if kind == "corrupt" {
                std::fs::write(t.root().join("handover.json"), "not json").unwrap();
            } else if kind == "barrier" {
                t.set("take_over.before_reload.fail", "");
            }
            t.send(b, Message::TakeOver);
            let failed = t
                .message(b, |m| matches!(m, Message::TakeFailed { .. }))
                .await;
            assert!(matches!(failed, Message::TakeFailed { reason } if
                if kind == "barrier" { reason.contains("take_over.before_reload") } else { reason.starts_with("handover:") }));
            assert!(t.lease().try_lock().is_err());
            t.ping(b, "taking-over").await;
            t.send(b, Message::Stop { budget_ms: 100 });
            t.exited(b, true).await;
            t.lock_after_exit().await;
        });
    }
}

#[test]
fn every_failed_handover_step_preserves_lease_and_serving_until_resume() {
    for step in ["handover.gate_closed", "lease.release", "write"] {
        run(move |mut t| async move {
            let a = t.spawn("A", Role::Active).await;
            t.logged("A", "active").await;
            if step == "write" {
                std::fs::create_dir(t.root().join("handover.json")).unwrap();
            } else {
                t.set(&format!("{step}.fail"), "");
            }
            t.send(a, Message::HandOver { budget_ms: 1000 });
            t.logged("A", "handover_failed").await;
            assert!(t.lease().try_lock().is_err());
            assert_marker(&t.http(0).await, "A");
            t.send(a, Message::Resume);
            t.message(a, |m| matches!(m, Message::Resumed)).await;
            t.ping(a, "active").await;
            assert!(!t.events().iter().any(|e| e["event"] == "handed_over"));
        });
    }
}

#[test]
fn failed_active_handover_keeps_management_unavailable_until_resume() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.set("handover.gate_closed.fail", "");
        t.send(a, Message::HandOver { budget_ms: 1000 });
        t.logged("A", "handover_failed").await;
        assert_marker(&t.http(0).await, "A");
        let management = t.http_path(1, "/v0/management/config").await;
        assert_eq!(management.split_whitespace().nth(1), Some("503"));
        assert_marker(&management, "A");
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        let management = t.http_path(1, "/v0/management/config").await;
        let status = management.split_whitespace().nth(1).unwrap();
        assert_ne!(status, "503");
        assert_marker(&management, "A");
    });
}

#[test]
fn first_start_and_released_resume_wait_for_lease_and_expire_nonzero() {
    for resume in [false, true] {
        for release in [false, true] {
            run(move |mut t| async move {
                let mut a = None;
                if resume {
                    let id = t.spawn("A", Role::Active).await;
                    t.logged("A", "active").await;
                    t.handover(id).await;
                    a = Some(id);
                }
                let lease = t.lease();
                lease.try_lock().unwrap();
                let id = match a {
                    Some(id) => {
                        t.send(id, Message::Resume);
                        id
                    }
                    None => t.spawn("A", Role::Active).await,
                };
                let start = tokio::time::Instant::now();
                t.ping(id, if resume { "handed-over" } else { "loading" })
                    .await;
                assert!(!t.events().iter().any(|e| e["event"] == "resumed"));
                if release {
                    drop(lease);
                    if resume {
                        t.message(id, |m| matches!(m, Message::Resumed)).await;
                    } else {
                        t.logged("A", "active").await;
                    }
                    assert_marker(&t.http(0).await, "A");
                    assert!(t.lease().try_lock().is_err());
                } else {
                    assert_eq!(t.exited(id, false).await.code, Some(1));
                    assert!(start.elapsed() >= Duration::from_millis(400));
                    assert!(start.elapsed() < Duration::from_secs(2));
                }
            });
        }
    }
}

#[test]
fn invalid_standby_messages_are_ignored_and_status_is_periodic_during_holds() {
    run(|mut t| async move {
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        for message in [
            Message::Resume,
            Message::Go,
            Message::HandOver { budget_ms: 1 },
            Message::Release,
        ] {
            t.send(b, message);
        }
        t.ping(b, "standby").await;
        assert!(!t.root().join("handover.json").exists());
        assert!(t.lease().try_lock().is_ok());
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.set("handover.gate_closed.hold", "");
        t.send(a, Message::HandOver { budget_ms: 1000 });
        t.arrived("handover.gate_closed").await;
        t.ping(a, "handing-over").await;
        let message = tokio::time::timeout(
            open_ferry_supervisor::timings::STATUS_EVERY + Duration::from_secs(1),
            t.message(a, |m| matches!(m, Message::Status { .. })),
        )
        .await
        .unwrap();
        assert!(matches!(message, Message::Status { phase, .. } if phase == "handing-over"));
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        t.set("handover.gate_closed.release", "");
    });
}

#[test]
fn release_is_invalid_while_resume_waits_for_the_lease() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let mut admitted = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
            .await
            .unwrap();
        admitted.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        t.live(a).await;
        t.handover(a).await;
        let lease = t.lease();
        lease.try_lock().unwrap();
        t.send(a, Message::Resume);
        t.send(a, Message::Release);
        t.ping(a, "handed-over").await;
        drop(lease);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        admitted
            .write_all(b"Host: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(WAIT, admitted.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_marker(&response, "A");
        assert_marker(&t.http(0).await, "A");
        assert!(t.lease().try_lock().is_err());
    });
}

#[test]
fn handover_cancels_slow_takeover_and_accepts_ready_then_release() {
    for slow in [true, false] {
        run(move |mut t| async move {
            let a = t.spawn("A", Role::Active).await;
            t.logged("A", "active").await;
            let b = t.spawn("B", Role::Standby).await;
            t.loaded(b).await;
            t.handover(a).await;
            if slow {
                t.set("fault.slow-ready", "60000");
                t.set("handover.gate_closing.hold", "");
                t.set("handover.gate_closed.fail", "");
            }
            t.send(b, Message::TakeOver);
            if slow {
                tokio::time::timeout(WAIT, async {
                    while t.root().join("handover.json").exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
            } else {
                t.message(b, |m| matches!(m, Message::Ready)).await;
                assert_marker(&t.http(0).await, "B");
            }
            t.handover(b).await;
            if slow {
                assert!(!t.events().iter().any(|e| e["marker"] == "B"
                    && (e["event"] == "handover.gate_closing"
                        || e["event"] == "handover.gate_closed")));
                assert!(!t.seam("handover.gate_closing.arrived").exists());
                assert!(!t.seam("handover.gate_closed.failed").exists());
            }
            assert!(t.lease().try_lock().is_ok());
            t.ping(b, "handed-over").await;
            t.send(b, Message::Release);
            t.message(b, |m| matches!(m, Message::Drained)).await;
            assert_eq!(t.exited(b, true).await.code, Some(0));
            assert_eq!(
                t.events()
                    .iter()
                    .filter(|e| e["marker"] == "B" && e["event"] == "ready")
                    .count(),
                usize::from(!slow)
            );
        });
    }
}

#[test]
fn peers_gone_ignores_standby_and_resets_after_handover_resume() {
    run(|mut t| async move {
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        t.send(b, Message::PeersGone);
        t.ping(b, "standby").await;
        assert!(!t.events().iter().any(|e| e["event"] == "peers_gone"));
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        t.handover(a).await;
        t.send(b, Message::TakeOver);
        t.message(b, |m| matches!(m, Message::Ready)).await;
        t.send(b, Message::Go);
        t.logged("B", "go").await;
        for expected in [1, 2] {
            t.send(b, Message::PeersGone);
            t.send(b, Message::PeersGone);
            t.ping(b, if expected == 1 { "watching" } else { "active" })
                .await;
            assert_eq!(
                t.events()
                    .iter()
                    .filter(|e| e["marker"] == "B" && e["event"] == "peers_gone")
                    .count(),
                expected
            );
            if expected == 1 {
                t.handover(b).await;
                t.send(b, Message::Resume);
                t.message(b, |m| matches!(m, Message::Resumed)).await;
            }
        }
    });
}

#[test]
fn failed_ready_handover_preserves_accept_at_the_failed_step() {
    for step in ["lease.release", "handover.gate_closed"] {
        run(move |mut t| async move {
            let a = t.spawn("A", Role::Active).await;
            t.logged("A", "active").await;
            let b = t.spawn("B", Role::Standby).await;
            t.loaded(b).await;
            t.handover(a).await;
            t.send(b, Message::TakeOver);
            t.message(b, |m| matches!(m, Message::Ready)).await;
            t.set(&format!("{step}.fail"), "");
            t.send(b, Message::HandOver { budget_ms: 1000 });
            t.logged("B", "handover_failed").await;
            assert!(t.lease().try_lock().is_err());
            let mut pending = None;
            if step == "lease.release" {
                let mut probe = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
                    .await
                    .unwrap();
                probe
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
                t.send(b, Message::Resume);
                t.ping(b, "handing-over").await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), probe.read(&mut [0; 1]))
                        .await
                        .is_err(),
                    "ready B must not resume accept after the pause step failed"
                );
                pending = Some(probe);
            } else {
                t.send(b, Message::Resume);
                t.ping(b, "handing-over").await;
                assert_marker(&t.http(0).await, "B");
            }
            t.registry.kill(b);
            t.exited(b, false).await;
            t.lock_after_exit().await;
            t.send(a, Message::Resume);
            t.message(a, |m| matches!(m, Message::Resumed)).await;
            if let Some(mut probe) = pending {
                let mut response = String::new();
                tokio::time::timeout(WAIT, probe.read_to_string(&mut response))
                    .await
                    .expect("pending probe timeout")
                    .unwrap();
                assert_marker(&response, "A");
            }
            assert_marker(&t.http(0).await, "A");
        });
    }
}

#[test]
fn failed_never_ready_handover_ignores_resume_and_keeps_gate_closed() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        t.handover(a).await;
        t.set("fault.slow-ready", "60000@B");
        t.send(b, Message::TakeOver);
        tokio::time::timeout(WAIT, async {
            while t.root().join("handover.json").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        t.set("lease.release.fail", "");
        t.send(b, Message::HandOver { budget_ms: 1000 });
        t.logged("B", "handover_failed").await;
        t.send(b, Message::Resume);
        t.ping(b, "handing-over").await;
        // A closed gate cannot admit this request before B is dismissed.
        let mut probe = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
            .await
            .unwrap();
        probe
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        t.ping(b, "handing-over").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), probe.read(&mut [0; 1]))
                .await
                .is_err(),
            "neither worker may answer while B's gate is closed and A is handed over"
        );
        t.registry.kill(b);
        t.exited(b, false).await;
        t.lock_after_exit().await;
        t.send(a, Message::Resume);
        t.message(a, |m| matches!(m, Message::Resumed)).await;
        t.ping(a, "active").await;
        let mut response = String::new();
        tokio::time::timeout(WAIT, probe.read_to_string(&mut response))
            .await
            .expect("pending probe timeout")
            .unwrap();
        assert_marker(&response, "A");
        for _ in 0..16 {
            assert_marker(&t.http(0).await, "A");
        }
    });
}

fn scripted_ping(mode: &'static str) {
    run(move |mut t| async move {
        let id = t.registry.next_id();
        let (cmd, mut commands) = tokio::sync::mpsc::unbounded_channel();
        t.registry.add(WorkerPort {
            id,
            identity: open_ferry_supervisor::process::ProcessIdentity { pid: 0, started: 0 },
            reported_version: "scripted".into(),
            protocol: VERSION,
            caps: vec![],
            cmd,
        });
        let events = t.registry.events();
        tokio::spawn(async move {
            let status = Message::Status {
                mode: "scripted".into(),
                live: 0,
                signins_pending: 0,
                phase: "active".into(),
            };
            let mut pings = 0;
            while let Some(command) = commands.recv().await {
                match command {
                    Cmd::Send(Message::Ping) => {
                        pings += 1;
                        if pings == 1 || mode != "missing" {
                            events.send((id, Event::Message(status.clone()))).unwrap();
                        }
                        if (pings == 1 && mode == "missing")
                            || (pings == 2 && mode == "one-extra")
                            || (pings >= 2 && mode == "double")
                        {
                            events.send((id, Event::Message(status.clone()))).unwrap();
                        }
                    }
                    Cmd::Send(Message::Unknown) => {
                        events
                            .send((
                                id,
                                Event::Message(Message::Unsupported {
                                    message_type: "unknown".into(),
                                }),
                            ))
                            .unwrap();
                    }
                    Cmd::Kill => break,
                    command => panic!("unexpected scripted command: {command:?}"),
                }
            }
        });
        t.ping(id, "active").await;
    });
}

#[test]
fn ping_accepts_one_reply() {
    scripted_ping("none");
}

#[test]
fn ping_redoes_one_extra_status_on_second_ping() {
    scripted_ping("one-extra");
}

#[test]
#[should_panic(expected = "extra ping status")]
fn ping_rejects_double_reply_mid_interval() {
    scripted_ping("double");
}

#[test]
#[should_panic(expected = "ping had no status reply before its fence")]
fn ping_rejects_missing_reply_masked_by_queued_status() {
    scripted_ping("missing");
}

#[test]
fn failed_cleanup_preserves_the_directory_and_original_panic() {
    let path = Arc::new(Mutex::new(PathBuf::new()));
    let capture = path.clone();
    let result = std::panic::catch_unwind(move || {
        run(move |t| async move {
            *capture.lock().unwrap() = t.root().to_owned();
            let (cmd, commands) = tokio::sync::mpsc::unbounded_channel();
            t.owned.lock().unwrap().push(WorkerPort {
                id: 0,
                identity: open_ferry_supervisor::process::ProcessIdentity { pid: 0, started: 0 },
                reported_version: "unreaped-script".into(),
                protocol: VERSION,
                caps: vec![],
                cmd,
            });
            tokio::spawn(async move {
                let _commands = commands;
                std::future::pending::<()>().await;
            });
            panic!("original cleanup panic");
        });
    });
    let error = result.expect_err("original panic must survive cleanup");
    assert_eq!(
        error.downcast_ref::<&str>(),
        Some(&"original cleanup panic")
    );
    let path = path.lock().unwrap();
    let kept = path.exists();
    // The script owns no child; runtime teardown has now dropped its receiver.
    if kept {
        std::fs::remove_dir_all(&*path).unwrap();
    }
    assert!(kept, "an unreaped worker's directory must be kept");
}

#[test]
fn ping_check_ignores_another_workers_prompt_status() {
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let b = t.spawn("B", Role::Standby).await;
        t.loaded(b).await;
        t.phase(a, "active").await;
        t.ping(b, "standby").await;
        t.send(b, Message::Ping);
        t.ping_reply(a, "active").await;
    });
}

#[test]
fn incomplete_event_log_tail_is_ignored_until_newline() {
    run(|t| async move {
        t.set(
            "events.log",
            "{}
{\"event\":",
        );
        assert_eq!(t.events(), vec![serde_json::json!({})]);
        t.set(
            "events.log",
            "{}
{\"event\":\"ready\"}
",
        );
        assert_eq!(t.events().len(), 2);
    });
}

#[test]
fn send_failure_uses_eof_budget_and_success_exit() {
    // send-fail is fake-only; the real worker's machine tests cover send failures.
    if !WorkerUnderTest::fake_only_faults() {
        eprintln!("skipping fake-only send-fail injection for the real worker");
        return;
    }
    run(|mut t| async move {
        let a = t.spawn("A", Role::Active).await;
        t.logged("A", "active").await;
        let mut stream = TcpStream::connect(t.listeners[0].1.local_addr().unwrap())
            .await
            .unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        t.live(a).await;
        t.set("fault.send-fail", "@A");
        let start = tokio::time::Instant::now();
        t.send(a, Message::Ping);
        assert_eq!(t.exited(a, true).await.code, Some(0));
        assert!(start.elapsed() < Duration::from_secs(1));
        t.lock_after_exit().await;
    });
}
