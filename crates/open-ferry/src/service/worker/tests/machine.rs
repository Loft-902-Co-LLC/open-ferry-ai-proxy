//! Not upstream's: the worker's state machine, message by message, with a
//! scripted supervisor on a `tokio::io::duplex` channel and loopback
//! `127.0.0.1` sockets.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use open_ferry_supervisor::protocol::{Budgets, Channel, Installation, Message, Role};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::logging::LogLevel;
use crate::service::Options;
use crate::service::worker::{
    Context, Hooks, Launch, Lease, Timing, Welcome, lease_slot, prepare, run_with,
};

const WAIT: Duration = Duration::from_secs(10);

/// What a stop that took its budget must finish well inside, on a busy
/// machine too. The budgets these tests give are 100 to 500 ms; the
/// wrong behaviours they guard take the 5 s default budgets of [`Setup`], a
/// response's own long delay, or never end (and are cut by [`WAIT`]). So
/// this is far above the budget and below each of those.
const STOPPED_BY: Duration = Duration::from_secs(4);

fn succeeded(code: ExitCode) -> bool {
    format!("{code:?}") == format!("{:?}", ExitCode::SUCCESS)
}

/// The supervisor's end of the channel.
struct Fake {
    channel: Channel<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>,
}

impl Fake {
    async fn send(&mut self, message: Message) {
        self.channel.send(&message).await.unwrap();
    }

    async fn send_raw(&mut self, json: serde_json::Value) {
        self.channel.send(&json).await.unwrap();
    }

    async fn recv(&mut self) -> Message {
        tokio::time::timeout(WAIT, self.channel.receive::<Message>())
            .await
            .expect("the worker sent nothing in time")
            .unwrap()
            .expect("the worker closed the channel")
            .message
    }

    /// After exit, inspect every buffered frame through channel EOF.
    async fn assert_no_load_failed(&mut self) {
        while let Some(frame) = tokio::time::timeout(WAIT, self.channel.receive::<Message>())
            .await
            .expect("the exited worker kept the channel open")
            .unwrap()
        {
            assert!(
                !matches!(frame.message, Message::LoadFailed { .. }),
                "failure cleanup completed instead of being cancelled"
            );
        }
    }

    /// What a worker started active sends first: `loaded`, then its first
    /// status, in the `active` phase.
    async fn recv_active_start(&mut self) {
        assert!(matches!(self.recv().await, Message::Loaded { .. }));
        assert!(matches!(
            self.recv().await,
            Message::Status { phase, .. } if phase == "active"
        ));
    }

    /// The next message that isn't a `status`.
    async fn recv_other(&mut self) -> Message {
        loop {
            match self.recv().await {
                Message::Status { .. } => {}
                other => return other,
            }
        }
    }

    /// The mode, live count and phase of the next `status`.
    async fn recv_status(&mut self) -> (String, u64, String) {
        loop {
            if let Message::Status {
                mode, live, phase, ..
            } = self.recv().await
            {
                return (mode, live, phase);
            }
        }
    }

    /// Pings, and gives the phase of the first status that follows.
    async fn ping(&mut self) -> String {
        self.send(Message::Ping).await;
        self.recv_status().await.2
    }

    /// Pings until the worker counts `live` connections.
    async fn wait_live(&mut self, live: u64) {
        for _ in 0..200 {
            self.send(Message::Ping).await;
            if self.recv_status().await.1 >= live {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the worker never counted {live} connection(s)");
    }
}

#[derive(Default)]
struct WriteProbe {
    stall: std::sync::atomic::AtomicBool,
    partial: std::sync::atomic::AtomicBool,
    arrived: tokio::sync::Notify,
    writes: std::sync::atomic::AtomicUsize,
    /// When the first stalled write began, which is when its send timeout
    /// started.
    first_stall: std::sync::OnceLock<tokio::time::Instant>,
}

struct ProbedWriter {
    writer: WriteHalf<DuplexStream>,
    probe: Option<Arc<WriteProbe>>,
}

impl tokio::io::AsyncWrite for ProbedWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if let Some(probe) = self.probe.clone() {
            probe
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if probe.stall.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = probe.first_stall.set(tokio::time::Instant::now());
                probe.arrived.notify_one();
                return std::task::Poll::Pending;
            }
            if probe.partial.load(std::sync::atomic::Ordering::SeqCst) {
                let written =
                    std::pin::Pin::new(&mut self.writer).poll_write(cx, &data[..data.len().min(3)]);
                if matches!(written, std::task::Poll::Ready(Ok(_))) {
                    probe
                        .partial
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    probe.stall.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                return written;
            }
        }
        std::pin::Pin::new(&mut self.writer).poll_write(cx, data)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

struct Setup {
    role: Role,
    /// Config lines after `host`, `port` and `auth-dir`.
    config: String,
    /// The port the config names, when not the main socket's.
    config_port: Option<u16>,
    /// Whether to write the config file.
    write_config: bool,
    management: bool,
    drain_ms: u64,
    load_ms: u64,
    stop_ms: u64,
    timing: Timing,
    channel_capacity: usize,
    write_probe: Option<Arc<WriteProbe>>,
    resume_ms: u64,
    /// The worker's test seams, marked so, in its data directory.
    #[cfg(feature = "blue-green-test")]
    marker: Option<&'static str>,
    /// Fault files, written before the worker starts: a kind and its content.
    #[cfg(feature = "blue-green-test")]
    faults: Vec<(&'static str, &'static str)>,
}

impl Setup {
    fn new(role: Role) -> Self {
        Self {
            role,
            config: String::new(),
            config_port: None,
            write_config: true,
            management: false,
            drain_ms: 5_000,
            load_ms: 5_000,
            stop_ms: 5_000,
            timing: Timing {
                failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                // A status comes on a ping or a change, unless a test asks.
                status_every: Duration::from_secs(3600),
                lease_retry: Duration::from_millis(20),
                send_timeout: Timing::default().send_timeout,
                shutdown_reserve: Duration::from_millis(10_100),
                routes: axum::Router::new(),
                wedge_service: None,
                wedge_startup: None,
                wedge_lease: None,
                wedge_watcher: None,
                fail_server: None,
                stop_took_the_take_over: None,
                updates: None,
                fail_resume_accept: false,
                fail_listener_when_sync_queued: false,
                resume_error: None,
                watcher_error: None,
                hold_server: None,
                fail_listener: None,
                activation_stop_budget: Duration::from_secs(5),
                hooks: Some(Hooks::default()),
            },
            channel_capacity: 1 << 16,
            write_probe: None,
            resume_ms: 5_000,
            #[cfg(feature = "blue-green-test")]
            marker: None,
            #[cfg(feature = "blue-green-test")]
            faults: Vec::new(),
        }
    }

    fn start(self) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let (main, main_address) = loopback();
        let mut topology = vec![entry(MAIN, main_address, false)];
        let mut listeners = vec![(MAIN.to_owned(), main)];
        let mut management_address = None;
        if self.management {
            let (listener, address) = loopback();
            topology.push(entry(MANAGEMENT, address, false));
            listeners.push((MANAGEMENT.to_owned(), listener));
            management_address = Some(address);
        }
        let config_path = dir.path().join("config.yaml");
        if self.write_config {
            let separate = management_address
                .map(|address| format!("remote-management:\n  separate-address: '{address}'\n"))
                .unwrap_or_default();
            std::fs::write(
                &config_path,
                format!(
                    "host: 127.0.0.1\nport: {}\nauth-dir: '{}'\n{separate}{}",
                    self.config_port.unwrap_or(main_address.port()),
                    dir.path().join("auth").display(),
                    self.config
                ),
            )
            .unwrap();
        }
        let welcome = Welcome {
            role: self.role,
            installation: Installation {
                installed: dir.path().join("open-ferry"),
                data_dir: data.clone(),
                receipt: serde_json::Value::Null,
            },
            budgets: Budgets {
                load_ms: self.load_ms,
                handover_ms: 5_000,
                ready_ms: 5_000,
                resume_ms: self.resume_ms,
                watch_ms: 5_000,
                drain_ms: self.drain_ms,
                stop_ms: self.stop_ms,
            },
            topology,
            switch_id: None,
        };
        let (ours, theirs) = duplex(self.channel_capacity);
        let (reader, writer) = split(ours);
        #[cfg_attr(not(feature = "blue-green-test"), allow(unused_mut))]
        let mut timing = self.timing;
        #[cfg(feature = "blue-green-test")]
        let hooks = {
            // A test's hold on a step is not a seam file, and survives.
            let step_hold = timing
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.step_hold.clone());
            let mut hooks = match self.marker {
                Some(marker) => Hooks::in_data_dir_marked(&data, marker),
                None if self.faults.is_empty() => Hooks::default(),
                None => Hooks::in_data_dir_marked(&data, "test"),
            };
            for (kind, value) in &self.faults {
                std::fs::write(
                    data.join(".blue-green-test").join(format!("fault.{kind}")),
                    value,
                )
                .unwrap();
            }
            hooks.step_hold = step_hold;
            timing.hooks = Some(hooks.clone());
            hooks
        };
        let log = LogLevel::detached_supervised();
        let worker = tokio::spawn(run_with(
            config_path,
            log.clone(),
            Options::default(),
            Channel::new(
                reader,
                ProbedWriter {
                    writer,
                    probe: self.write_probe,
                },
            ),
            welcome,
            listeners,
            timing,
        ));
        let (reader, writer) = split(theirs);
        Rig {
            _dir: dir,
            data,
            main: main_address,
            management: management_address,
            fake: Some(Fake {
                channel: Channel::new(reader, writer),
            }),
            worker: Some(worker),
            log,
            #[cfg(feature = "blue-green-test")]
            hooks,
        }
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    data: PathBuf,
    main: SocketAddr,
    management: Option<SocketAddr>,
    fake: Option<Fake>,
    worker: Option<JoinHandle<ExitCode>>,
    /// The worker's log, for the state of its rotation and cleaning.
    log: LogLevel,
    /// The worker's seams, to see where it would have exited.
    #[cfg(feature = "blue-green-test")]
    hooks: Hooks,
}

impl Rig {
    fn fake(&mut self) -> &mut Fake {
        self.fake.as_mut().unwrap()
    }

    /// The worker's exit code, once it has exited.
    async fn exit(&mut self) -> ExitCode {
        tokio::time::timeout(WAIT, self.worker.take().unwrap())
            .await
            .expect("the worker didn't exit in time")
            .unwrap()
    }

    /// A file in the worker's data directory.
    fn path(&self, name: &str) -> PathBuf {
        self.data.join(name)
    }

    /// Whether the worker's log rotation is on.
    fn rotating(&self) -> bool {
        self.log.file_log().controls().rotation_enabled()
    }

    /// Whether the worker's log cleaning is on.
    fn cleaning(&self) -> bool {
        self.log.file_log().controls().cleaning_enabled()
    }

    /// Whether another handle can take the lease.
    fn lease_is_free(&self) -> bool {
        Lease::try_take(&self.data).unwrap().is_some()
    }
}

/// The answer to a `GET`.
async fn get(address: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut answer = String::new();
    tokio::time::timeout(WAIT, stream.read_to_string(&mut answer))
        .await
        .expect("no answer in time")
        .unwrap();
    answer
}

/// Captures what the worker logs on this thread's runtime.
fn capture_here() -> (tracing::subscriber::DefaultGuard, Logged) {
    let logged = Logged::default();
    let guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logged.clone()));
    (guard, logged)
}

#[tokio::test]
async fn a_standby_loads_and_says_so() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    let (mode, live, phase) = rig.fake().recv_status().await;
    assert_eq!(
        (mode.as_str(), live, phase.as_str()),
        ("auto", 0, "standby")
    );
    assert!(rig.lease_is_free(), "a standby doesn't take the lease");
}

// A standby holds its sockets and doesn't accept on them: a client that
// connects waits in the backlog, and gets no answer.
#[tokio::test]
async fn a_standby_does_not_answer_on_its_socket() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    let mut stream = TcpStream::connect(rig.main).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(rig.fake().recv_status().await.2, "standby");
    for _ in 0..5 {
        assert_eq!(rig.fake().ping().await, "standby");
    }
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(WAIT, stream.read_to_end(&mut answer))
        .await
        .expect("standby socket stayed open after stop");
    assert!(answer.is_empty(), "a standby answered a request");
}

#[tokio::test]
async fn a_failed_load_is_reported() {
    let mut setup = Setup::new(Role::Standby);
    setup.write_config = false;
    let mut rig = setup.start();
    let Message::LoadFailed { reason } = rig.fake().recv().await else {
        panic!("expected load_failed");
    };
    assert!(reason.contains("failed to load config"), "{reason}");
    assert_eq!(rig.exit().await, ExitCode::FAILURE);
}

#[tokio::test]
async fn status_and_ping_are_answered_while_standby() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    assert_eq!(rig.fake().recv_status().await.2, "standby");
    assert_eq!(rig.fake().ping().await, "standby");
    // A status from the supervisor is ignored: nothing comes back.
    rig.fake()
        .send(Message::Status {
            mode: "auto".into(),
            live: 0,
            signins_pending: 0,
            phase: "active".into(),
        })
        .await;
    rig.fake().send(Message::Ping).await;
    rig.fake()
        .send_raw(serde_json::json!({"type":"after_ping"}))
        .await;
    assert!(matches!(rig.fake().recv().await, Message::Status { phase, .. } if phase == "standby"));
    assert!(
        matches!(rig.fake().recv().await, Message::Unsupported { message_type } if message_type == "after_ping")
    );
}

// While it waits for the lease, a worker is loading, and answers.
#[tokio::test]
async fn status_and_ping_are_answered_while_loading() {
    let mut setup = Setup::new(Role::Active);
    setup.load_ms = 60_000;
    let mut rig = setup.start();
    let held = Lease::try_take(&rig.data).unwrap().unwrap();
    assert_eq!(rig.fake().ping().await, "loading");
    drop(held);
    // It takes the lease on a later try, and serves.
    assert_eq!(rig.fake().recv_status().await.2, "active");
    assert_eq!(rig.fake().ping().await, "active");
}

#[tokio::test]
async fn an_unknown_message_is_unsupported_in_every_state() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    rig.fake()
        .send_raw(serde_json::json!({"type": "mystery_message", "x": 1}))
        .await;
    let Message::Unsupported { message_type } = rig.fake().recv_other().await else {
        panic!("expected unsupported");
    };
    assert_eq!(message_type, "mystery_message");

    let mut rig = Setup::new(Role::Active).start();
    assert_eq!(rig.fake().recv_status().await.2, "active");
    rig.fake()
        .send_raw(serde_json::json!({"type": "another_one"}))
        .await;
    let Message::Unsupported { message_type } = rig.fake().recv_other().await else {
        panic!("expected unsupported");
    };
    assert_eq!(message_type, "another_one");
    let _idle = TcpStream::connect(rig.main).await.unwrap();
    rig.fake().wait_live(1).await;
    rig.fake().send(Message::Release).await;
    assert_eq!(rig.fake().recv_status().await.2, "draining");
    rig.fake()
        .send_raw(serde_json::json!({"type": "while_draining"}))
        .await;
    assert!(
        matches!(rig.fake().recv().await, Message::Unsupported { message_type } if message_type == "while_draining")
    );
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));

    let mut setup = Setup::new(Role::Active);
    setup.load_ms = 60_000;
    let mut rig = setup.start();
    let _held = Lease::try_take(&rig.data).unwrap().unwrap();
    assert_eq!(rig.fake().ping().await, "loading");
    rig.fake()
        .send_raw(serde_json::json!({"type": "while_loading"}))
        .await;
    assert!(
        matches!(rig.fake().recv().await, Message::Unsupported { message_type } if message_type == "while_loading")
    );
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
}

// Stop and EOF must not erase a resume error whose cleanup is still waiting
// for an admitted operation. No LoadFailed frame is promised on a lost channel.
#[tokio::test]
async fn stop_or_eof_during_resume_failure_cleanup_exits_with_failure() {
    for eof in [false, true] {
        let (observed, observation) = tokio::sync::oneshot::channel();
        let observed = Mutex::new(Some(observed));
        let operation = Arc::new(Mutex::new(None));
        let held = operation.clone();
        let mut setup = Setup::new(Role::Active);
        setup.timing.fail_resume_accept = true;
        setup.timing.resume_error = Some(Arc::new(move |gate| {
            *held.lock().unwrap() = Some(gate.begin_operation().unwrap());
            observed.lock().unwrap().take().unwrap().send(()).unwrap();
        }));
        let mut rig = setup.start();
        tokio::time::timeout(WAIT, observation)
            .await
            .unwrap()
            .unwrap();
        assert!(operation.lock().unwrap().is_some());
        if eof {
            drop(rig.fake.take());
        } else {
            rig.fake().send(Message::Stop { budget_ms: 100 }).await;
        }
        assert!(
            !succeeded(rig.exit().await),
            "resume failure was lost (EOF: {eof})"
        );
        if !eof {
            rig.fake().assert_no_load_failed().await;
        }
        drop(operation.lock().unwrap().take());
    }
}

// Stop and EOF must not erase a watcher error while its cleanup is held.
#[tokio::test]
async fn stop_or_eof_during_watcher_failure_cleanup_exits_with_failure() {
    for eof in [false, true] {
        let mut setup = Setup::new(Role::Active);
        setup.timing.wedge_watcher = Some(Arc::new(move |_, config| {
            config.auth_dir = PathBuf::from(&config.auth_dir)
                .join("missing-watcher-directory")
                .to_string_lossy()
                .into_owned();
        }));
        let (observed, observation) = tokio::sync::oneshot::channel();
        let observed = Mutex::new(Some(observed));
        setup.timing.watcher_error = Some(Arc::new(move || {
            observed.lock().unwrap().take().unwrap().send(()).unwrap();
            Box::pin(std::future::pending())
        }));
        let mut rig = setup.start();
        tokio::time::timeout(WAIT, observation)
            .await
            .unwrap()
            .unwrap();
        if eof {
            drop(rig.fake.take());
        } else {
            rig.fake().send(Message::Stop { budget_ms: 100 }).await;
        }
        assert!(
            !succeeded(rig.exit().await),
            "watcher failure was lost (EOF: {eof})"
        );
        if !eof {
            rig.fake().assert_no_load_failed().await;
        }
    }
}

// A message that makes no sense in the state is ignored, with no reply.
#[tokio::test]
async fn a_message_that_makes_no_sense_in_the_state_is_ignored() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    assert_eq!(rig.fake().recv_status().await.2, "standby");
    rig.fake().send(Message::Loaded { ms: 1 }).await;
    rig.fake().send(Message::Drained).await;
    rig.fake().send(Message::Ready).await;
    // What comes next is the answer to the ping, not an `unsupported`.
    rig.fake().send(Message::Ping).await;
    assert!(matches!(rig.fake().recv().await, Message::Status { .. }));
    rig.fake()
        .send_raw(serde_json::json!({"type":"after_invalid"}))
        .await;
    assert!(
        matches!(rig.fake().recv().await, Message::Unsupported { message_type } if message_type == "after_invalid")
    );
}

#[tokio::test]
async fn an_active_worker_takes_the_lease_and_serves() {
    let mut rig = Setup::new(Role::Active).start();
    rig.fake().recv_active_start().await;
    assert!(!rig.lease_is_free(), "the active worker holds the lease");
    let answer = get(rig.main, "/healthz").await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    rig.fake().send(Message::Stop { budget_ms: 2_000 }).await;
    assert!(succeeded(rig.exit().await));
    assert!(!rig.lease_is_free(), "run returning must retain the lease");
}

// The supervisor counts a worker started active as not yet started until
// `loaded`: it comes once, holding the lease, before the first status.
#[tokio::test]
async fn an_active_start_says_loaded_once_before_its_first_status() {
    let mut rig = Setup::new(Role::Active).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    assert!(
        !rig.lease_is_free(),
        "`loaded` came before the worker held the lease"
    );
    assert!(matches!(rig.fake().recv().await, Message::Status { phase, .. } if phase == "active"));
    // Two more status rounds, and no second `loaded` among them.
    for _ in 0..2 {
        rig.fake().send(Message::Ping).await;
        assert!(matches!(rig.fake().recv().await, Message::Status { .. }));
    }
    rig.fake().send(Message::Stop { budget_ms: 1_000 }).await;
    assert!(succeeded(rig.exit().await));
    while let Some(frame) = rig.fake().channel.receive::<Message>().await.unwrap() {
        assert!(
            !matches!(frame.message, Message::Loaded { .. }),
            "a second `loaded`: {:?}",
            frame.message
        );
    }
}

// A worker held in loading answers a ping with a loading status; once it has
// loaded, `loaded` comes before its first active status.
#[tokio::test]
async fn loaded_follows_a_loading_status_and_comes_before_the_active_one() {
    let mut setup = Setup::new(Role::Active);
    setup.load_ms = 60_000;
    let mut rig = setup.start();
    let held = Lease::try_take(&rig.data).unwrap().unwrap();
    rig.fake().send(Message::Ping).await;
    assert!(matches!(rig.fake().recv().await, Message::Status { phase, .. } if phase == "loading"));
    drop(held);
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    assert!(matches!(rig.fake().recv().await, Message::Status { phase, .. } if phase == "active"));
}

// The server task is first polled only when it is scheduled, so activation
// can get ahead of the loops: a listener that fails then is a failed start.
async fn a_listener_fails_after_activation_resumed(name: &'static str, management: bool) {
    let mut setup = Setup::new(Role::Active);
    setup.management = management;
    setup.timing.hold_server = Some(Arc::new(tokio::sync::Notify::new()));
    setup.timing.fail_listener = Some(name);
    let mut rig = setup.start();
    let first = rig.fake().recv().await;
    assert!(
        matches!(first, Message::LoadFailed { .. }),
        "{name}: {first:?}"
    );
    assert!(!succeeded(rig.exit().await), "{name}");
    while let Some(frame) = rig.fake().channel.receive::<Message>().await.unwrap() {
        assert!(
            !matches!(frame.message, Message::Loaded { .. }),
            "{name}: `loaded` after the failure"
        );
    }
}

#[tokio::test]
async fn a_main_listener_that_fails_after_activation_resumed_is_a_failed_load() {
    a_listener_fails_after_activation_resumed(MAIN, false).await;
}

#[tokio::test]
async fn a_management_listener_that_fails_after_activation_resumed_is_a_failed_load() {
    a_listener_fails_after_activation_resumed(MANAGEMENT, true).await;
}

#[tokio::test]
async fn an_active_worker_that_cannot_take_the_lease_exits_in_the_load_budget() {
    let mut setup = Setup::new(Role::Active);
    setup.load_ms = 300;
    let mut rig = setup.start();
    let held = Lease::try_take(&rig.data).unwrap().unwrap();
    let started = Instant::now();
    assert!(!succeeded(rig.exit().await));
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(held);
}

#[tokio::test]
async fn a_worker_serves_the_management_socket_it_was_given() {
    let mut setup = Setup::new(Role::Active);
    setup.management = true;
    let mut rig = setup.start();
    rig.fake().recv_active_start().await;
    let main = get(rig.main, "/healthz").await;
    assert!(main.starts_with("HTTP/1.1 200"), "{main}");
    // The management address serves the management API alone: the proxy's
    // `/healthz` isn't there.
    let management = get(rig.management.unwrap(), "/healthz").await;
    assert!(management.starts_with("HTTP/1.1 404"), "{management}");
}

// The supervisor's sockets win over a config that disagrees, with one
// warning; a config that agrees warns of nothing.
#[tokio::test]
async fn a_worker_serves_the_welcome_topology_and_warns_once() {
    let (_guard, logged) = capture_here();
    let mut rig = Setup::new(Role::Active).start();
    let first = rig.fake().channel.receive::<Message>().await;
    assert!(
        matches!(&first, Ok(Some(frame)) if matches!(frame.message, Message::Loaded { .. })),
        "{first:?}: {:?}",
        logged.at_least(Level::WARN)
    );
    assert!(matches!(rig.fake().recv().await, Message::Status { .. }));
    let disagreements = |logged: &Logged| -> Vec<String> {
        logged
            .at_least(Level::WARN)
            .into_iter()
            .filter(|line| line.contains("disagrees"))
            .collect()
    };
    let warned = disagreements(&logged);
    assert!(warned.is_empty(), "a config that agrees warns: {warned:?}");
    rig.fake().send(Message::Stop { budget_ms: 1_000 }).await;
    assert!(succeeded(rig.exit().await));

    // A config that names another port than the welcome's.
    let (other, other_address) = loopback();
    // Keep this port reserved throughout; do not race another bind(0).
    let (_guard, logged) = capture_here();
    let mut setup = Setup::new(Role::Active);
    setup.config_port = Some(other_address.port());
    let mut rig = setup.start();
    let first = rig.fake().channel.receive::<Message>().await;
    assert!(
        matches!(&first, Ok(Some(frame)) if matches!(frame.message, Message::Loaded { .. })),
        "{first:?}: {:?}",
        logged.at_least(Level::WARN)
    );
    assert!(matches!(rig.fake().recv().await, Message::Status { .. }));
    let answer = get(rig.main, "/healthz").await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    drop(other);
    let warned = disagreements(&logged);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("the main address is"), "{warned:?}");
}

#[tokio::test]
async fn release_of_a_standby_is_ignored() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    assert_eq!(rig.fake().recv_status().await.2, "standby");
    rig.fake().send(Message::Release).await;
    rig.fake().send(Message::Ping).await;
    let reply = rig.fake().recv().await;
    assert!(matches!(reply, Message::Status { phase, .. } if phase == "standby"));
    assert!(!rig.worker.as_ref().unwrap().is_finished());
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
}

#[tokio::test]
async fn release_of_an_idle_active_worker_drains_and_exits() {
    let mut rig = Setup::new(Role::Active).start();
    rig.fake().recv_active_start().await;
    rig.fake().send(Message::Release).await;
    assert_eq!(rig.fake().recv_status().await.2, "draining");
    assert!(matches!(rig.fake().recv_other().await, Message::Drained));
    assert!(succeeded(rig.exit().await));
    assert!(!rig.lease_is_free());
}

// After the release, a connection that has carried no request gets the
// drain budget of the welcome to carry one, and is closed at its end.
#[tokio::test]
async fn release_closes_a_connection_with_no_request_at_the_drain_budget() {
    let mut setup = Setup::new(Role::Active);
    setup.drain_ms = 400;
    let mut rig = setup.start();
    rig.fake().recv_active_start().await;
    let mut idle = TcpStream::connect(rig.main).await.unwrap();
    rig.fake().wait_live(1).await;
    let released = Instant::now();
    rig.fake().send(Message::Release).await;
    assert!(matches!(rig.fake().recv_other().await, Message::Drained));
    let waited = released.elapsed();
    assert!(
        waited >= Duration::from_millis(300),
        "closed early: {waited:?}"
    );
    assert!(waited < Duration::from_secs(5), "closed late: {waited:?}");
    let mut byte = [0u8; 1];
    assert_eq!(idle.read(&mut byte).await.unwrap_or(0), 0);
    assert!(succeeded(rig.exit().await));
}

/// A request whose body never arrives: the handler waits for it, so the
/// connection stays in a request until it is cut.
async fn stuck_request(address: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n{\"model\":",
        )
        .await
        .unwrap();
    stream
}

#[tokio::test]
async fn graceful_stop_completes_a_response_inside_the_request_window() {
    response_during_stop(Duration::from_millis(150), Duration::from_millis(50), true).await;
}

#[tokio::test]
async fn graceful_stop_cuts_a_response_outside_the_request_window() {
    // The delay is far past the 400 ms budget and past [`STOPPED_BY`], so a
    // stop that waited for the response, rather than cutting it, is late.
    response_during_stop(Duration::from_secs(8), Duration::from_millis(50), false).await;
}

#[tokio::test]
async fn graceful_stop_cuts_a_response_inside_the_budget_but_outside_the_reserve() {
    response_during_stop(
        Duration::from_millis(300),
        Duration::from_millis(200),
        false,
    )
    .await;
}

async fn response_during_stop(delay: Duration, reserve: Duration, completes: bool) {
    let arrived = Arc::new(tokio::sync::Notify::new());
    let mut setup = Setup::new(Role::Active);
    setup.timing.shutdown_reserve = reserve;
    setup.timing.routes = axum::Router::new().route(
        "/worker-test-response",
        axum::routing::get({
            let arrived = arrived.clone();
            move || {
                let arrived = arrived.clone();
                async move {
                    arrived.notify_one();
                    tokio::time::sleep(delay).await;
                    "complete response"
                }
            }
        }),
    );
    let mut rig = setup.start();
    assert_eq!(rig.fake().recv_status().await.2, "active");
    let mut request = TcpStream::connect(rig.main).await.unwrap();
    request
        .write_all(b"GET /worker-test-response HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::timeout(WAIT, arrived.notified())
        .await
        .unwrap();
    let started = Instant::now();
    rig.fake().send(Message::Stop { budget_ms: 400 }).await;
    assert!(succeeded(rig.exit().await));
    assert!(
        started.elapsed() < STOPPED_BY,
        "stop late: {:?}",
        started.elapsed()
    );
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, request.read_to_end(&mut rest))
        .await
        .unwrap();
    if completes {
        read.unwrap();
        let answer = String::from_utf8(rest).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.ends_with("\r\n\r\ncomplete response"), "{answer}");
        assert!(started.elapsed() >= Duration::from_millis(100));
    } else {
        assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset);
        assert!(
            rest.is_empty(),
            "cut response was partially served: {rest:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(300).saturating_sub(reserve));
    }
    assert!(!rig.lease_is_free());
}

fn wedge_response(setup: &mut Setup) -> Arc<tokio::sync::Notify> {
    let arrived = Arc::new(tokio::sync::Notify::new());
    setup.timing.routes = axum::Router::new().route(
        "/worker-test-wedge",
        axum::routing::get({
            let arrived = arrived.clone();
            move || {
                let arrived = arrived.clone();
                async move {
                    arrived.notify_one();
                    std::future::pending::<&str>().await
                }
            }
        }),
    );
    arrived
}

async fn request_to_wedge(address: SocketAddr, arrived: &tokio::sync::Notify) -> TcpStream {
    let mut request = TcpStream::connect(address).await.unwrap();
    request
        .write_all(b"GET /worker-test-wedge HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::timeout(WAIT, arrived.notified())
        .await
        .unwrap();
    request
}

// The graceful stop waits for a request in flight until the budget, and
// then the connection is cut.
#[tokio::test]
async fn stop_cuts_what_is_left_at_the_budget() {
    let mut setup = Setup::new(Role::Active);
    let arrived = wedge_response(&mut setup);
    let mut rig = setup.start();
    rig.fake().recv_active_start().await;
    let mut stuck = request_to_wedge(rig.main, &arrived).await;
    rig.fake().wait_live(1).await;
    let started = Instant::now();
    rig.fake().send(Message::Stop { budget_ms: 400 }).await;
    assert!(succeeded(rig.exit().await));
    let took = started.elapsed();
    assert!(took < STOPPED_BY, "cut late: {took:?}");
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, stuck.read_to_end(&mut rest))
        .await
        .expect("stop left the connection open");
    assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset);
    assert!(
        rest.is_empty(),
        "stop answered the wedged request: {rest:?}"
    );
    assert!(!rig.lease_is_free());
}

// EOF is a stop with the welcome's stop budget, and the lease is only given
// up by exiting.
#[tokio::test]
async fn eof_stops_within_the_stop_budget_and_keeps_the_lease_until_exit() {
    let mut setup = Setup::new(Role::Active);
    setup.stop_ms = 500;
    let arrived = wedge_response(&mut setup);
    let mut rig = setup.start();
    rig.fake().recv_active_start().await;
    let mut stuck = request_to_wedge(rig.main, &arrived).await;
    rig.fake().wait_live(1).await;
    let started = Instant::now();
    rig.fake = None;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !rig.lease_is_free(),
        "EOF gave the lease up before the exit"
    );
    assert!(succeeded(rig.exit().await));
    let took = started.elapsed();
    assert!(took < STOPPED_BY, "stop late: {took:?}");
    assert!(!rig.lease_is_free());
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, stuck.read_to_end(&mut rest))
        .await
        .expect("stop left the connection open");
    assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset);
    assert!(rest.is_empty(), "EOF answered the wedged request: {rest:?}");
}

#[tokio::test]
async fn eof_stops_a_standby_at_once() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    rig.fake = None;
    assert!(succeeded(rig.exit().await));
}

#[tokio::test]
async fn a_stop_while_standby_exits() {
    let mut rig = Setup::new(Role::Standby).start();
    assert!(matches!(rig.fake().recv().await, Message::Loaded { .. }));
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
}

#[tokio::test]
async fn status_is_sent_every_interval_and_on_a_change() {
    let mut setup = Setup::new(Role::Standby);
    setup.timing.status_every = Duration::from_millis(50);
    let mut rig = setup.start();
    // Startup now runs independently; periodic loading statuses may precede
    // loaded when the shared blocking pool is busy.
    assert!(matches!(
        rig.fake().recv_other().await,
        Message::Loaded { .. }
    ));
    // The first status after `loaded` is the phase change; the rest are
    // timed, with nothing asked for.
    for _ in 0..3 {
        assert_eq!(rig.fake().recv_status().await.2, "standby");
    }
}

// A worker whose service loop is stuck in a call that waits (a reload behind
// a persist, v3 7.6) still answers the supervisor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_service_loop_does_not_keep_the_worker_from_answering() {
    let (release, wedged) = std::sync::mpsc::channel::<()>();
    struct Unwedge(std::sync::mpsc::Sender<()>);
    impl Drop for Unwedge {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Unwedge(release);
    let wedged = Mutex::new(wedged);
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let arrived = Mutex::new(Some(arrived));
    let mut setup = Setup::new(Role::Active);
    setup.timing.wedge_service = Some(Arc::new(move || {
        if let Some(arrived) = arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = wedged.lock().unwrap().recv();
    }));
    let mut rig = setup.start();
    rig.fake().recv_active_start().await;
    tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
    assert_eq!(rig.fake().ping().await, "active");
    assert_eq!(rig.fake().ping().await, "active");
    let mut stuck = stuck_request(rig.main).await;
    rig.fake().wait_live(1).await;
    let started = Instant::now();
    rig.fake().send(Message::Stop { budget_ms: 200 }).await;
    let address = rig.main;
    let probe = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let Ok(Ok(mut connection)) =
            tokio::time::timeout(Duration::from_millis(75), TcpStream::connect(address)).await
        else {
            return false;
        };
        if connection
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .is_err()
        {
            return false;
        }
        let mut byte = [0];
        matches!(
            tokio::time::timeout(Duration::from_millis(75), connection.read(&mut byte)).await,
            Ok(Ok(1))
        )
    });
    assert!(succeeded(rig.exit().await));
    assert!(started.elapsed() < STOPPED_BY);
    assert!(
        !probe.await.unwrap(),
        "stop kept accepting while the service loop was wedged"
    );
    assert!(!rig.lease_is_free());
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, stuck.read_to_end(&mut rest)).await;
    // Always release the test's blocked thread, including after a failure.
    drop(release);
    assert_eq!(
        read.expect("wedged stop left the connection open").unwrap(),
        0
    );
    assert!(rest.is_empty());
}

#[tokio::test]
async fn a_poisoned_partial_channel_attempts_no_further_write() {
    use crate::service::worker::{State, Worker};
    let (ours, mut peer) = duplex(4096);
    let (_, writer) = split(ours);
    let probe = Arc::new(WriteProbe::default());
    probe
        .partial
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut worker = Worker {
        stop_deadline: None,
        failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        channel: Some(Channel::new(
            tokio::io::empty(),
            ProbedWriter {
                writer,
                probe: Some(probe.clone()),
            },
        )),
        channel_lost: false,
        send_timeout: Duration::from_millis(50),
        budgets: Budgets {
            load_ms: 100,
            handover_ms: 100,
            ready_ms: 100,
            resume_ms: 100,
            watch_ms: 100,
            drain_ms: 100,
            stop_ms: 100,
        },
        control: crate::service::serve_loop::ServeControl::new(),
        started_at: Instant::now(),
        mode: "auto".into(),
        peers_gone: false,
        state: State::Loading,
        hooks: Hooks::default(),
        data_dir: PathBuf::new(),
        config_path: PathBuf::new(),
        timing: Timing::default(),
    };
    worker.send_status().await;
    assert!(worker.channel_lost);
    let mut prefix = [0; 3];
    tokio::time::timeout(WAIT, peer.read_exact(&mut prefix))
        .await
        .unwrap()
        .unwrap();
    let writes = probe.writes.load(std::sync::atomic::Ordering::SeqCst);
    assert!(writes >= 2, "no partial write was attempted");
    worker.send_status().await;
    worker
        .send(&Message::Unsupported {
            message_type: "test".into(),
        })
        .await;
    assert_eq!(
        probe.writes.load(std::sync::atomic::Ordering::SeqCst),
        writes
    );
}

#[tokio::test]
async fn a_stalled_send_stops_with_the_eof_budget() {
    for role in [Role::Standby, Role::Active] {
        let mut setup = Setup::new(role);
        setup.channel_capacity = 1;
        setup.timing.send_timeout = Duration::from_millis(50);
        setup.stop_ms = 100;
        let started = Instant::now();
        let mut rig = setup.start();
        // Never consume the frame: even the first send cannot finish.
        assert!(succeeded(rig.exit().await));
        // Start-up is in the time. The welcome's other budgets are 5 s.
        assert!(started.elapsed() < STOPPED_BY);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicked_service_loop_stops_the_worker_and_cuts_connections() {
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    struct Release(std::sync::mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(release);
    let blocked = Mutex::new(blocked);
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let arrived = Mutex::new(Some(arrived));
    let mut setup = Setup::new(Role::Active);
    setup.stop_ms = 200;
    setup.timing.send_timeout = Duration::from_millis(300);
    setup.timing.wedge_service = Some(Arc::new(move || {
        if let Some(arrived) = arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = blocked.lock().unwrap().recv();
        panic!("synchronized service panic");
    }));
    let mut rig = setup.start();
    assert_eq!(rig.fake().recv_status().await.2, "active");
    tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
    let mut request = stuck_request(rig.main).await;
    rig.fake().wait_live(1).await;
    drop(release);
    assert_eq!(rig.exit().await, ExitCode::FAILURE);
    assert!(!rig.lease_is_free());
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, request.read_to_end(&mut rest))
        .await
        .unwrap();
    assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset);
    assert!(rest.is_empty(), "panic answered the request: {rest:?}");
}

mod lease {
    use super::*;

    #[test]
    fn a_second_handle_cannot_take_the_lease_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("not-yet-made");
        let first = Lease::try_take(&data).unwrap();
        assert!(first.is_some());
        assert!(data.join("active-worker.lock").exists());
        assert!(Lease::try_take(&data).unwrap().is_none());
        drop(first);
        assert!(Lease::try_take(&data).unwrap().is_some());
    }
}

mod prepared {
    use open_ferry_core::gate::GateState;

    use super::*;
    use crate::service::worker::Prepared;

    pub(super) fn prepared(standby: bool) -> (tempfile::TempDir, Prepared) {
        let dir = tempfile::tempdir().unwrap();
        let (main, address) = loopback();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            format!(
                "host: 127.0.0.1\nport: {}\nauth-dir: '{}'\n",
                address.port(),
                dir.path().join("auth").display()
            ),
        )
        .unwrap();
        let prepared = prepare(
            &path,
            LogLevel::detached_supervised(),
            Options::default(),
            &[entry(MAIN, address, false)],
            vec![(MAIN.to_owned(), main)],
            standby,
            context(dir.path()),
        )
        .unwrap();
        (dir, prepared)
    }

    /// What `prepare` gets besides the config: no seams, a fresh control.
    pub(super) fn context(dir: &std::path::Path) -> Context {
        Context {
            data_dir: dir.join("data"),
            installed: dir.join("bin").join(if cfg!(windows) {
                "open-ferry.exe"
            } else {
                "open-ferry"
            }),
            hooks: Hooks::default(),
            control: crate::service::serve_loop::ServeControl::new(),
            updates: None,
        }
    }

    /// The handles of a prepared service, with `control` as theirs.
    fn handles_for(
        control: &crate::service::serve_loop::ServeControl,
    ) -> crate::service::worker::Handles {
        let (_dir, prepared) = prepared(false);
        let mut handles = prepared.handles.clone();
        handles.control = control.clone();
        handles
    }

    #[tokio::test]
    async fn peers_gone_counts_only_in_active() {
        use crate::service::worker::{Draining, Launch, ProcessLease, State, Worker, activate};
        use open_ferry_supervisor::protocol::Frame;
        let (_guard, logged) = capture_here();
        let (ours, mut peer) = duplex(4096);
        let (reader, writer) = split(ours);
        let mut worker = Worker {
            channel: Some(Channel::new(reader, writer)),
            channel_lost: false,
            stop_deadline: None,
            failed: Timing::default().failed,
            send_timeout: WAIT,
            budgets: Budgets {
                load_ms: 100,
                handover_ms: 100,
                ready_ms: 100,
                resume_ms: 100,
                watch_ms: 100,
                drain_ms: 100,
                stop_ms: 100,
            },
            control: crate::service::serve_loop::ServeControl::new(),
            started_at: Instant::now(),
            mode: "auto".into(),
            peers_gone: false,
            state: State::Loading,
            hooks: Hooks::default(),
            data_dir: PathBuf::new(),
            config_path: PathBuf::new(),
            timing: Timing::default(),
        };
        async fn send(worker: &mut Worker<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>) {
            worker
                .on_message(Frame {
                    message_type: "peers_gone".into(),
                    message: Message::PeersGone,
                })
                .await;
        }
        send(&mut worker).await;
        assert!(!worker.peers_gone, "Loading counted peers_gone");
        let (_standby_dir, standby) = prepared(true);
        worker.state = State::Standby(Box::new(standby));
        send(&mut worker).await;
        assert!(!worker.peers_gone, "Standby counted peers_gone");
        let (dir, serving) = prepared(false);
        let active = activate(
            serving,
            &dir.path().join("config.yaml"),
            lease_slot(Some(ProcessLease(Some(
                Lease::try_take(&dir.path().join("data")).unwrap().unwrap(),
            )))),
            &Timing::default(),
            Launch::First,
        )
        .await
        .unwrap();
        worker.state = State::Draining(Box::new(Draining {
            active,
            drained: Box::pin(std::future::pending()),
        }));
        send(&mut worker).await;
        assert!(!worker.peers_gone, "Draining counted peers_gone");
        let State::Draining(draining) = std::mem::replace(&mut worker.state, State::Loading) else {
            unreachable!()
        };
        worker.state = State::Active(Box::new(draining.active));
        send(&mut worker).await;
        assert!(worker.peers_gone, "first Active peers_gone did not count");
        send(&mut worker).await;
        assert_eq!(
            logged
                .at_least(Level::INFO)
                .iter()
                .filter(|line| line.contains("no other worker"))
                .count(),
            1
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err(),
            "peers_gone sent a reply"
        );
        let State::Active(active) = std::mem::replace(&mut worker.state, State::Loading) else {
            unreachable!()
        };
        active
            .stop(tokio::time::Instant::now() + Duration::from_secs(1))
            .await;
    }

    // A standby is a closed gate with the cooldown store held; the first
    // activation opens the gate and thaws the store.
    #[test]
    fn a_standby_is_closed_and_held_until_the_first_activation() {
        let (dir, mut prepared) = prepared(true);
        assert!(
            !dir.path().join("auth").exists(),
            "standby preparation wrote on disk"
        );
        assert_eq!(prepared.service.gate.state(), GateState::Closed);
        assert!(prepared.service.cooldown_held);
        prepared
            .service
            .first_activation(&prepared.key)
            .ok()
            .unwrap();
        assert_eq!(prepared.service.gate.state(), GateState::Closed);
        prepared.service.gate.reopen();
        assert_eq!(prepared.service.gate.state(), GateState::Open);
        assert!(dir.path().join("auth").is_dir());
        assert!(!prepared.service.cooldown_held);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn activation_attaches_updates_before_routes_and_starts_checks_after_reopen() {
        use crate::service::worker::{ProcessLease, activate};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, mut prepared) = prepared(false);
        Arc::make_mut(&mut prepared.config)
            .remote_management
            .secret_key = "test-password".into();
        prepared.service.config = prepared.config.clone();
        prepared
            .service
            .management
            .set_loaded_config(prepared.config.clone(), None);
        prepared.service.management = prepared
            .service
            .management
            .clone()
            .with_local_password("test-password");
        let gate = prepared.service.gate.clone();
        let address = prepared.topology.main.listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let checked = Arc::new(tokio::sync::Notify::new());
        // Creating an HTTP downloader performs no IO. No check ever receives
        // it: the injected factory counts attempts and refuses without network.
        let fetch = open_ferry_update::background::http_fetch()("").unwrap();
        let updater = open_ferry_update::Updater::for_this_binary(
            fetch,
            open_ferry_update::DataDir::at(dir.path().join("updates")),
        )
        .unwrap();
        let updates = open_ferry_update::UpdateService::new(
            updater,
            Arc::new({
                let calls = calls.clone();
                let checked = checked.clone();
                let gate = gate.clone();
                move |_| {
                    assert_eq!(gate.state(), GateState::Open);
                    calls.fetch_add(1, Ordering::SeqCst);
                    checked.notify_one();
                    Err(open_ferry_update::FetchError::Network(
                        "network-free test".into(),
                    ))
                }
            }),
            &prepared.config,
        );
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        // The updater is attached the way `prepare` does it for a test.
        prepared.service.management = prepared
            .service
            .management
            .clone()
            .with_updates(updates.clone());
        prepared.handles.management = prepared.service.management.clone();
        prepared.handles.first_check = Duration::ZERO;
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        struct Release(std::sync::mpsc::Sender<()>);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let release = Release(release);
        let blocked = Mutex::new(blocked);
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        let arrived = Mutex::new(Some(arrived));
        let timing = Timing {
            wedge_watcher: Some(Arc::new(move |_, _| {
                if let Some(arrived) = arrived.lock().unwrap().take() {
                    let _ = arrived.send(());
                }
                let _ = blocked.lock().unwrap().recv();
            })),
            ..Timing::default()
        };
        let task = tokio::spawn({
            let path = dir.path().join("config.yaml");
            async move { activate(prepared, &path, lease, &timing, Launch::First).await }
        });
        tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
        assert_eq!(gate.state(), GateState::Closed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(updates.status().next_check.is_none());
        drop(release);
        let active = task.await.unwrap().ok().unwrap();
        tokio::time::timeout(WAIT, checked.notified())
            .await
            .unwrap();
        let mut request = TcpStream::connect(address).await.unwrap();
        request
            .write_all(
                b"GET /open-ferry/api/v1/update HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-password\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut answer = String::new();
        tokio::time::timeout(WAIT, request.read_to_string(&mut answer))
            .await
            .unwrap()
            .unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(!answer.contains("updates_unavailable"), "{answer}");
        assert!(
            !active
                .stop(tokio::time::Instant::now() + Duration::from_millis(100))
                .await
        );
    }

    #[tokio::test]
    async fn a_partial_resume_failure_settles_an_owned_management_write() {
        partial_resume_failure_settles_an_owned_management_write(false).await;
    }

    #[tokio::test]
    async fn a_partial_resume_failure_settles_an_owned_management_write_with_a_held_server() {
        partial_resume_failure_settles_an_owned_management_write(true).await;
    }

    async fn partial_resume_failure_settles_an_owned_management_write(hold_server: bool) {
        use crate::service::worker::{ProcessLease, activate};
        let dir = tempfile::tempdir().unwrap();
        let (main, main_address) = loopback();
        let (management, management_address) = loopback();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, format!(
            "host: 127.0.0.1\nport: {}\nauth-dir: '{}'\nmanagement:\n  secret-key: test-password\n  allow-remote: true\n  separate-address: '127.0.0.1:{}'\n",
            main_address.port(), dir.path().join("auth").display(), management_address.port(),
        )).unwrap();
        let options = Options {
            local_password: "test-password".into(),
            ..Options::default()
        };
        let prepared = prepare(
            &path,
            LogLevel::detached_supervised(),
            options,
            &[
                entry(MAIN, main_address, false),
                entry(MANAGEMENT, management_address, false),
            ],
            vec![(MAIN.into(), main), (MANAGEMENT.into(), management)],
            false,
            context(dir.path()),
        )
        .unwrap();
        let gate = prepared.service.gate.clone();
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        let control = prepared.handles.control.clone();
        let resume_failed = Arc::new(tokio::sync::Notify::new());
        let failure_at = Arc::new(std::sync::OnceLock::new());
        let activation = tokio::spawn({
            let resume_failed = resume_failed.clone();
            let failure_at = failure_at.clone();
            async move {
                activate(
                    prepared,
                    &path,
                    lease,
                    &Timing {
                        fail_listener_when_sync_queued: true,
                        // Cover both the ordinary ordering and resume_accept
                        // settling before the server's first poll.
                        hold_server: hold_server.then(|| Arc::new(tokio::sync::Notify::new())),
                        resume_error: Some(Arc::new(move |_| {
                            failure_at.set(tokio::time::Instant::now()).unwrap();
                            resume_failed.notify_one();
                        })),
                        activation_stop_budget: Duration::from_millis(200),
                        ..Timing::default()
                    },
                    Launch::First,
                )
                .await
            }
        });
        let mut request = TcpStream::connect(management_address).await.unwrap();
        let body = r#"{"type":"codex","access_token":"local-test"}"#;
        request.write_all(format!(
            "POST /v0/management/auth-files?name=test.json HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-password\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
        ).as_bytes()).await.unwrap();
        // Watcher startup does blocking filesystem work before the injected
        // listener failure. Give that precondition the fixture's setup wait;
        // the two-second guard measures settling the owned write afterward.
        tokio::time::timeout(WAIT, resume_failed.notified())
            .await
            .expect("activation did not reach the queued-write listener failure");
        let result = tokio::time::timeout_at(
            *failure_at.get().unwrap() + Duration::from_secs(2),
            activation,
        )
        .await
        .expect("activation hung on the owned management write")
        .unwrap();
        assert!(result.is_err());
        assert_eq!(gate.state(), GateState::Closed);
        assert!(control.resume_accept().await.is_err());
        assert!(Lease::try_take(&data).unwrap().is_none());
    }

    #[tokio::test]
    async fn dropping_active_records_failure_and_takes_the_emergency_cut() {
        use crate::service::worker::{ProcessLease, activate, failure_code};
        let (dir, prepared) = prepared(false);
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        let control = prepared.handles.control.clone();
        let timing = Timing::default();
        let active = activate(
            prepared,
            &dir.path().join("config.yaml"),
            lease,
            &timing,
            Launch::First,
        )
        .await
        .ok()
        .unwrap();
        drop(active);
        assert_eq!(
            failure_code(ExitCode::SUCCESS, &timing.failed),
            ExitCode::FAILURE
        );
        assert!(control.resume_accept().await.is_err());
        assert!(Lease::try_take(&data).unwrap().is_none());
    }

    #[test]
    fn an_emergency_exit_cannot_wait_on_a_blocked_flusher_past_the_cap() {
        use crate::service::worker::emergency_exit_with;
        let (arrived, arrival) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (finished, done) = std::sync::mpsc::channel();
        let start = Instant::now();
        emergency_exit_with(move |cap| {
            assert!(cap <= Duration::from_millis(250));
            arrived.send(()).unwrap();
            blocked.recv().unwrap();
            finished.send(()).unwrap();
        });
        let elapsed = start.elapsed();
        arrival
            .recv_timeout(Duration::from_secs(1))
            .expect("the fake flusher never started");
        assert!(done.try_recv().is_err(), "the fake flusher wasn't blocked");
        assert!(
            elapsed >= Duration::from_millis(200),
            "returned without waiting for flush"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "flush exceeded the cap: {elapsed:?}"
        );
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[tokio::test]
    async fn cancelling_stop_keeps_the_emergency_guard_armed() {
        use crate::service::serve_loop::ServeControl;
        use crate::service::worker::{Active, ProcessLease, Stage};
        let dir = tempfile::tempdir().unwrap();
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control = ServeControl::new();
        let (stop, stopping) = tokio::sync::oneshot::channel();
        let (_sender, ended) = tokio::sync::mpsc::unbounded_channel();
        let active = Active {
            armed: true,
            failed: failed.clone(),
            control: control.clone(),
            stop: Some(stop),
            ended,
            task: Some(tokio::spawn(std::future::pending::<()>())),
            file_log: crate::file_log::FileLog::default(),
            stop_timeout_hook: None,
            stage: Stage::Active,
            handles: handles_for(&control),
            commands: tokio::sync::mpsc::unbounded_channel().0,
            lease: lease_slot(Some(ProcessLease(Some(
                Lease::try_take(dir.path()).unwrap().unwrap(),
            )))),
        };
        let stopping_task = tokio::spawn(active.stop(tokio::time::Instant::now() + WAIT));
        stopping.await.unwrap();
        stopping_task.abort();
        assert!(stopping_task.await.unwrap_err().is_cancelled());
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(control.resume_accept().await.is_err());
        assert!(Lease::try_take(dir.path()).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_panic_completed_just_before_the_deadline_is_failure() {
        use crate::service::serve_loop::ServeControl;
        use crate::service::worker::{Active, ProcessLease, Stage};
        let dir = tempfile::tempdir().unwrap();
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control = ServeControl::new();
        let (stop, _receiver) = tokio::sync::oneshot::channel();
        let (_sender, ended) = tokio::sync::mpsc::unbounded_channel();
        let panic_now = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let panic_now = panic_now.clone();
            async move {
                panic_now.notified().await;
                panic!("panic between the first wait and the abort recheck");
            }
        });
        let task_state = task.abort_handle();
        let timed_out = Arc::new(tokio::sync::Notify::new());
        let observe = Arc::new(tokio::sync::Notify::new());
        let active = Active {
            armed: true,
            failed,
            control: control.clone(),
            stop: Some(stop),
            ended,
            task: Some(task),
            file_log: crate::file_log::FileLog::default(),
            stop_timeout_hook: Some((timed_out.clone(), observe.clone())),
            stage: Stage::Active,
            handles: handles_for(&control),
            commands: tokio::sync::mpsc::unbounded_channel().0,
            lease: lease_slot(Some(ProcessLease(Some(
                Lease::try_take(dir.path()).unwrap().unwrap(),
            )))),
        };
        let stopping = tokio::spawn(active.stop(tokio::time::Instant::now()));
        tokio::time::timeout(WAIT, timed_out.notified())
            .await
            .unwrap();
        // The initial wait has already timed out. Complete the panic before
        // allowing the post-abort observation, which has no fresh grace time.
        panic_now.notify_one();
        tokio::time::timeout(WAIT, async {
            while !task_state.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        observe.notify_one();
        assert!(tokio::time::timeout(WAIT, stopping).await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn a_service_cancelled_before_stop_is_failure() {
        use crate::service::serve_loop::ServeControl;
        use crate::service::worker::{Active, ProcessLease, Stage};
        let dir = tempfile::tempdir().unwrap();
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control = ServeControl::new();
        let (stop, _receiver) = tokio::sync::oneshot::channel();
        let (_sender, ended) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        // A cancellation observed before our own abort is a service failure.
        let deadline = tokio::time::Instant::now();
        let active = Active {
            armed: true,
            failed,
            control: control.clone(),
            stop: Some(stop),
            ended,
            task: Some(task),
            file_log: crate::file_log::FileLog::default(),
            stop_timeout_hook: None,
            stage: Stage::Active,
            handles: handles_for(&control),
            commands: tokio::sync::mpsc::unbounded_channel().0,
            lease: lease_slot(Some(ProcessLease(Some(
                Lease::try_take(dir.path()).unwrap().unwrap(),
            )))),
        };
        assert!(active.stop(deadline).await);
    }

    #[tokio::test]
    async fn dropping_the_stop_sender_is_an_explicit_failure_and_cut() {
        use crate::service::worker::{ProcessLease, activate};
        let (dir, prepared) = prepared(false);
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        let control = prepared.handles.control.clone();
        let mut active = activate(
            prepared,
            &dir.path().join("config.yaml"),
            lease,
            &Timing::default(),
            Launch::First,
        )
        .await
        .ok()
        .unwrap();
        drop(active.stop.take());
        assert_eq!(
            tokio::time::timeout(WAIT, active.ended.recv())
                .await
                .unwrap(),
            Some(false)
        );
        assert!(active.stop(tokio::time::Instant::now() + WAIT).await);
        assert!(
            control.resume_accept().await.is_err(),
            "emergency drop did not cut"
        );
        assert!(Lease::try_take(&data).unwrap().is_none());
    }

    // Diagnostic for a rare Windows hang: report the stuck stage or slow runtime shutdown.
    // Remove this diagnostic once the cause is fixed.
    #[test]
    fn a_resume_accept_failure_closes_the_gate_and_retains_the_lease() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        use std::time::{Duration, Instant};

        use crate::service::worker::{ProcessLease, activate};

        const WATCHDOG: Duration = Duration::from_secs(120);
        const SHUTDOWN: Duration = Duration::from_secs(60);
        const STAGES: [&str; 10] = [
            "setup",
            "activate",
            "activate returned",
            "error asserted",
            "gate asserted",
            "lease asserted",
            "drop temp dir",
            "temp dir dropped",
            "runtime shutdown",
            "runtime shut down",
        ];
        let stage = Arc::new(AtomicUsize::new(0));
        let watchdog_stage = stage.clone();
        let (done, receiver) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if matches!(
                receiver.recv_timeout(WATCHDOG),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let message = format!(
                    "resume-accept test hung at stage: {}\n",
                    STAGES[watchdog_stage.load(Ordering::SeqCst)]
                );
                let mut stderr = std::io::stderr();
                let _ = std::io::Write::write_all(&mut stderr, message.as_bytes());
                let _ = std::io::Write::flush(&mut stderr);
                std::process::abort();
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (dir, prepared) = prepared(false);
            let gate = prepared.service.gate.clone();
            let data = dir.path().join("data");
            let lease = lease_slot(Some(ProcessLease(Some(
                Lease::try_take(&data).unwrap().unwrap(),
            ))));
            let timing = Timing {
                fail_resume_accept: true,
                ..Timing::default()
            };
            stage.store(1, Ordering::SeqCst);
            let error = activate(
                prepared,
                &dir.path().join("config.yaml"),
                lease,
                &timing,
                Launch::First,
            )
            .await;
            stage.store(2, Ordering::SeqCst);
            let error = error.err().unwrap();
            assert!(error.contains("failed to resume accepting"), "{error}");
            stage.store(3, Ordering::SeqCst);
            assert_eq!(gate.state(), GateState::Closed);
            stage.store(4, Ordering::SeqCst);
            assert!(Lease::try_take(&data).unwrap().is_none());
            stage.store(5, Ordering::SeqCst);
            // The original dropped the gate before the temp dir.
            drop(gate);
            stage.store(6, Ordering::SeqCst);
            drop(dir);
            stage.store(7, Ordering::SeqCst);
        });
        stage.store(8, Ordering::SeqCst);
        let shutdown_started = Instant::now();
        rt.shutdown_timeout(SHUTDOWN);
        let shutdown_elapsed = shutdown_started.elapsed();
        stage.store(9, Ordering::SeqCst);
        drop(done);
        watchdog.join().unwrap();
        if shutdown_elapsed >= Duration::from_secs(30) {
            panic!(
                "runtime shutdown waited {}s on a blocking task",
                shutdown_elapsed.as_secs()
            );
        }
    }

    #[tokio::test]
    async fn a_failed_watcher_never_opens_the_gate_and_cuts_the_started_server() {
        use crate::service::worker::{ProcessLease, activate};
        let (dir, prepared) = prepared(false);
        let gate = prepared.service.gate.clone();
        let address = prepared.topology.main.listener.local_addr().unwrap();
        let data = dir.path().join("data");
        let lease = lease_slot(Some(ProcessLease(Some(
            Lease::try_take(&data).unwrap().unwrap(),
        ))));
        let error = activate(
            prepared,
            &dir.path().join("missing.yaml"),
            lease,
            &Timing::default(),
            Launch::First,
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("failed to create watcher"), "{error}");
        assert_eq!(gate.state(), GateState::Closed);
        assert!(Lease::try_take(&data).unwrap().is_none());
        if let Ok(mut connection) = TcpStream::connect(address).await {
            let mut response = Vec::new();
            let _ = tokio::time::timeout(WAIT, connection.read_to_end(&mut response))
                .await
                .expect("activation failure left the listener running");
            assert!(response.is_empty());
        }
    }

    #[test]
    fn an_active_worker_does_not_hold_the_cooldown_store() {
        let (_dir, prepared) = prepared(false);
        assert_eq!(prepared.service.gate.state(), GateState::Closed);
        assert!(!prepared.service.cooldown_held);
    }
}

#[tokio::test]
async fn an_active_load_failure_is_reported_and_retains_the_lease() {
    let mut setup = Setup::new(Role::Active);
    setup.write_config = false;
    let mut rig = setup.start();
    assert!(matches!(
        rig.fake().recv().await,
        Message::LoadFailed { .. }
    ));
    assert_eq!(rig.exit().await, ExitCode::FAILURE);
    assert!(!rig.lease_is_free());
}

#[tokio::test]
async fn a_stop_behind_a_stalled_send_fits_the_send_timeout_plus_stop_budget() {
    let probe = Arc::new(WriteProbe::default());
    let mut setup = Setup::new(Role::Active);
    setup.stop_ms = 100;
    setup.write_probe = Some(probe.clone());
    // Paused time is free. Leave real blocking work, such as the ledger's
    // SQLite open/join, 30 s of wall headroom while retaining a bound below
    // a broken stop's full send-timeout sleep on the runtime thread.
    let send_timeout = Duration::from_secs(30);
    setup.timing.send_timeout = send_timeout;
    let mut rig = setup.start();
    assert_eq!(rig.fake().recv_status().await.2, "active");
    // Startup uses real sockets and disk I/O. Freeze time only once active,
    // so this checks the send and stop deadlines without counting delays in
    // scheduling the worker or waking the task that observes its exit.
    tokio::time::pause();
    let wall_started = Instant::now();
    probe.stall.store(true, std::sync::atomic::Ordering::SeqCst);
    rig.fake().send(Message::Ping).await;
    tokio::time::timeout(WAIT, probe.arrived.notified())
        .await
        .unwrap();
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    // Waits longer than `exit` does, so a faulty run fails the bound below
    // and not the exit wait.
    let code = tokio::time::timeout(send_timeout * 3, rig.worker.take().unwrap())
        .await
        .expect("the worker didn't exit in time")
        .unwrap();
    let exited = tokio::time::Instant::now();
    let wall_elapsed = wall_started.elapsed();
    assert!(succeeded(code));
    // Keep the former 3 s margin for the 100 ms stop budget and remaining
    // cleanup. A second full send timeout takes at least 60 s of Tokio time
    // and fails this 33 s bound; the exit wait above lets it reach this check.
    let stalled_at = *probe.first_stall.get().unwrap();
    let elapsed = exited.duration_since(stalled_at);
    assert!(
        elapsed < send_timeout + Duration::from_secs(3),
        "the stalled send and stop took {elapsed:?} of Tokio time"
    );
    // spawn_blocking inhibits this paused runtime's clock auto-advance, so
    // legitimate ledger I/O may outlast the 100 ms stop budget in wall time.
    // Keep that work below the send timeout, while rejecting a full timeout
    // spent blocking the runtime. An interruptible Stop remains valid: this
    // combined send/stop path has no lower bound on either clock.
    assert!(
        wall_elapsed < send_timeout,
        "the stalled send and stop took {wall_elapsed:?} of wall time (Tokio time: {elapsed:?})"
    );
    assert!(!rig.lease_is_free());
}

#[tokio::test]
async fn a_stalled_status_does_not_mask_an_api_server_failure() {
    let probe = Arc::new(WriteProbe::default());
    let failure = Arc::new(tokio::sync::Notify::new());
    let mut setup = Setup::new(Role::Active);
    setup.stop_ms = 200;
    setup.timing.send_timeout = Duration::from_millis(300);
    let seen = Arc::new(tokio::sync::Notify::new());
    setup.timing.fail_server = Some((failure.clone(), seen.clone()));
    setup.write_probe = Some(probe.clone());
    let mut rig = setup.start();
    assert_eq!(rig.fake().recv_status().await.2, "active");
    probe.stall.store(true, std::sync::atomic::Ordering::SeqCst);
    rig.fake().send(Message::Ping).await;
    tokio::time::timeout(WAIT, probe.arrived.notified())
        .await
        .unwrap();
    failure.notify_one();
    tokio::time::timeout(WAIT, seen.notified()).await.unwrap();
    assert_eq!(rig.exit().await, ExitCode::FAILURE);
    assert!(!rig.lease_is_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_status_does_not_mask_a_concurrent_service_panic() {
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    struct Release(std::sync::mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(release);
    let blocked = Mutex::new(blocked);
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let arrived = Mutex::new(Some(arrived));
    let probe = Arc::new(WriteProbe::default());
    let mut setup = Setup::new(Role::Active);
    setup.stop_ms = 200;
    setup.timing.send_timeout = Duration::from_millis(300);
    setup.write_probe = Some(probe.clone());
    setup.timing.wedge_service = Some(Arc::new(move || {
        if let Some(arrived) = arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = blocked.lock().unwrap().recv();
        panic!("synchronized service panic");
    }));
    let mut rig = setup.start();
    assert_eq!(rig.fake().recv_status().await.2, "active");
    tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
    let mut request = stuck_request(rig.main).await;
    rig.fake().wait_live(1).await;
    probe.stall.store(true, std::sync::atomic::Ordering::SeqCst);
    rig.fake().send(Message::Ping).await;
    tokio::time::timeout(WAIT, probe.arrived.notified())
        .await
        .unwrap();
    drop(release);
    assert_eq!(rig.exit().await, ExitCode::FAILURE);
    assert!(!rig.lease_is_free());
    let mut rest = Vec::new();
    let read = tokio::time::timeout(WAIT, request.read_to_end(&mut rest))
        .await
        .unwrap();
    assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset);
    assert!(rest.is_empty(), "panic answered the request: {rest:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_lease_still_answers_and_stops_while_loading() {
    blocked_startup_stage_answers_and_stops(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_watcher_still_answers_and_stops_while_loading() {
    blocked_startup_stage_answers_and_stops(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_does_not_accept_management_before_the_gate_reopens() {
    let (unblock, blocked) = std::sync::mpsc::channel::<()>();
    struct Release(std::sync::mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(unblock);
    let blocked = Mutex::new(blocked);
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let arrived = Mutex::new(Some(arrived));
    let mut setup = Setup::new(Role::Active);
    setup.management = true;
    setup.timing.wedge_watcher = Some(Arc::new(move |control, _| {
        assert!(
            control.accepting_is_paused(),
            "accept loops were not paused before watcher creation"
        );
        if let Some(arrived) = arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = blocked.lock().unwrap().recv();
    }));
    let mut rig = setup.start();
    tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
    let mut request = TcpStream::connect(rig.management.unwrap()).await.unwrap();
    request
        .write_all(b"GET /v0/management/config HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), request.read(&mut byte))
            .await
            .is_err(),
        "management answered before activation finished"
    );
    assert_eq!(rig.fake().ping().await, "loading");
    drop(release);
    assert_eq!(rig.fake().recv_status().await.2, "active");
    let mut answer = String::new();
    tokio::time::timeout(WAIT, request.read_to_string(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert!(answer.starts_with("HTTP/1.1"), "{answer}");
    // The deterministic hook asserted paused mode and zero accepting loops.
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
}

async fn blocked_startup_stage_answers_and_stops(lease: bool) {
    let (unblock, blocked) = std::sync::mpsc::channel::<()>();
    struct Release(std::sync::mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(unblock);
    let blocked = Mutex::new(blocked);
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let arrived = Mutex::new(Some(arrived));
    let wedge = Arc::new(move || {
        if let Some(arrived) = arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = blocked.lock().unwrap().recv();
    });
    let mut setup = Setup::new(Role::Active);
    if lease {
        setup.timing.wedge_lease = Some(wedge);
    } else {
        setup.timing.wedge_watcher = Some(Arc::new(move |_, _| wedge()));
    }
    let mut rig = setup.start();
    tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
    assert_eq!(rig.fake().ping().await, "loading");
    let started = Instant::now();
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
    assert!(started.elapsed() < STOPPED_BY);
    if !lease {
        assert!(!rig.lease_is_free());
    }
    drop(release);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_preparation_still_answers_and_stops_while_loading() {
    let (unblock, blocked) = std::sync::mpsc::channel::<()>();
    struct Release(std::sync::mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(unblock);
    let blocked = Mutex::new(blocked);
    let (arrived, at_prepare) = tokio::sync::oneshot::channel();
    let arrived = Mutex::new(Some(arrived));
    let mut setup = Setup::new(Role::Active);
    setup.timing.wedge_startup = Some(Arc::new(move || {
        if let Some(arrived) = arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = blocked.lock().unwrap().recv();
    }));
    let mut rig = setup.start();
    tokio::time::timeout(WAIT, at_prepare)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rig.fake().ping().await, "loading");
    assert_eq!(rig.fake().ping().await, "loading");
    let started = Instant::now();
    rig.fake().send(Message::Stop { budget_ms: 100 }).await;
    assert!(succeeded(rig.exit().await));
    assert!(started.elapsed() < STOPPED_BY);
    assert!(!rig.lease_is_free());
    drop(release);
}

#[tokio::test]
async fn finish_flushes_a_queued_line_when_time_remains() {
    use tracing_subscriber::prelude::*;
    let dir = tempfile::tempdir().unwrap();
    let (layer, file_log) = crate::file_log::init();
    file_log.configure(dir.path(), true, 0);
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || tracing::info!("worker final diagnostic"));
    assert_eq!(
        super::super::finish(
            ExitCode::FAILURE,
            tokio::time::Instant::now() + Duration::from_secs(1),
            &file_log
        )
        .await,
        1
    );
    assert!(
        std::fs::read_to_string(dir.path().join(crate::file_log::MAIN_LOG))
            .unwrap()
            .contains("worker final diagnostic")
    );
}

#[tokio::test]
async fn finish_skips_flush_when_the_stop_deadline_has_passed() {
    let file_log = crate::file_log::FileLog::default();
    let started = Instant::now();
    assert_eq!(
        super::super::finish(
            ExitCode::SUCCESS,
            tokio::time::Instant::now() - Duration::from_millis(1),
            &file_log
        )
        .await,
        0
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn finish_calls_flush_before_return_with_only_the_time_left() {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flushed = called.clone();
    // A long deadline: a loaded blocking pool can start the flush late, and
    // a short one would pass before it ran.
    let deadline = tokio::time::Instant::now() + WAIT;
    let result = super::super::finish_with(ExitCode::SUCCESS, deadline, move |timeout| {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(timeout <= left + Duration::from_millis(1));
        // All of the time left, measured before this call, and not a fixed
        // timeout: one that fits this long deadline would overrun a short one.
        assert!(timeout >= left, "{timeout:?} with {left:?} left");
        assert!(timeout <= WAIT);
        flushed.store(true, std::sync::atomic::Ordering::SeqCst);
    })
    .await;
    assert_eq!(result, 0);
    assert!(called.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn finish_does_not_call_flush_without_time_left() {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flushed = called.clone();
    let code =
        super::super::finish_with(ExitCode::FAILURE, tokio::time::Instant::now(), move |_| {
            flushed.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
    assert_eq!(code, 1);
    assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn finish_cannot_be_extended_by_a_blocked_flusher() {
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let finishing = tokio::spawn(super::super::finish_with(
        ExitCode::SUCCESS,
        deadline,
        move |timeout| {
            assert!(timeout <= Duration::from_millis(100));
            let _ = arrived.send(());
            let _ = blocked.recv();
        },
    ));
    tokio::time::timeout(WAIT, arrival).await.unwrap().unwrap();
    let code = tokio::time::timeout_at(deadline + STOPPED_BY, finishing).await;
    // Release only after finish returned: no sleep or early release can mask
    // an unbounded flush. Release before assertions so a failure can't orphan it.
    drop(release);
    assert_eq!(code.unwrap().unwrap(), 0);
}

#[tokio::test]
async fn finish_cannot_be_extended_by_a_held_log_writer() {
    use tracing_subscriber::prelude::*;
    let dir = tempfile::tempdir().unwrap();
    let (layer, file_log) = crate::file_log::init();
    file_log.configure(dir.path(), true, 0);
    let (release, held) = std::sync::mpsc::channel();
    file_log.hold(held);
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("queued behind the held writer");
    });
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let code = tokio::time::timeout_at(
        deadline + STOPPED_BY,
        super::super::finish(ExitCode::FAILURE, deadline, &file_log),
    )
    .await;
    // finish's own timeout would hide a flush that ignores its time left, so
    // check that bound too while the writer is still held.
    // The time is taken inside the closure, so a slow blocking pool is not
    // counted against the flush.
    let log = file_log.clone();
    let flushed = tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        log.flush(Duration::from_millis(50));
        started.elapsed()
    });
    let bounded = tokio::time::timeout(Duration::from_secs(2), flushed).await;
    // Release before assertions so a failure can't leave the writer held.
    drop(release);
    assert_eq!(code.unwrap(), 1);
    let took = bounded
        .expect("a held writer kept flush past its timeout")
        .expect("the flush task panicked");
    assert!(
        took < Duration::from_millis(1500),
        "a 50ms flush took {took:?}"
    );
    file_log.flush(WAIT);
    let text = std::fs::read_to_string(dir.path().join(crate::file_log::MAIN_LOG)).unwrap();
    assert!(text.contains("queued behind the held writer"), "{text:?}");
}

#[tokio::test]
async fn an_unobserved_api_server_error_sets_the_failure_record() {
    use crate::service::worker::{failure_code, record_server};
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn(record_server(
        async { Err(std::io::Error::other("unobserved API failure")) },
        failed.clone(),
    ));
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    assert_eq!(failure_code(ExitCode::SUCCESS, &failed), ExitCode::FAILURE);
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn an_unobserved_api_server_panic_sets_the_failure_record() {
    use crate::service::worker::{failure_code, record_server};
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn(record_server(
        async {
            panic!("unobserved API panic");
        },
        failed.clone(),
    ));
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    assert_eq!(failure_code(ExitCode::SUCCESS, &failed), ExitCode::FAILURE);
    assert!(task.await.unwrap_err().is_panic());
}

mod switch;
