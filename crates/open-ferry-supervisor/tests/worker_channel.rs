//! Not upstream's: L2 and L3 of the blue-green plan. A real worker process
//! (the probe) takes its channel with `worker::take`, attaches, and the
//! supervisor side makes the handshake. Windows runs it over the named pipe,
//! Unix over the socketpair. Every process is held by a handle the test keeps.

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use open_ferry_supervisor::control::{HandshakeError, WelcomeParams, handshake, worker_args};
use open_ferry_supervisor::protocol::{
    Budgets, Installation, ListenerTopology, Message, Role, WindowsSocket,
};
use open_ferry_supervisor::worker::BoxedChannel;

const LONG: Duration = Duration::from_secs(30);

fn probe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_worker_probe"))
}

fn budgets() -> Budgets {
    Budgets {
        load_ms: 60_000,
        handover_ms: 60_000,
        ready_ms: 30_000,
        resume_ms: 10_000,
        watch_ms: 600_000,
        drain_ms: 1_800_000,
        stop_ms: 50_000,
    }
}

struct Started {
    worker: Worker,
    channel: BoxedChannel,
    dir: tempfile::TempDir,
    main: TcpListener,
    management: TcpListener,
    sockets: Vec<WindowsSocket>,
}

impl Started {
    fn params(&self, switch_id: Option<&str>) -> WelcomeParams {
        let topology = vec![
            ListenerTopology {
                name: "main".into(),
                address: self.main.local_addr().expect("address"),
                tls: false,
            },
            ListenerTopology {
                name: "management".into(),
                address: self.management.local_addr().expect("address"),
                tls: true,
            },
        ];
        WelcomeParams {
            role: Role::Active,
            installation: Installation {
                installed: PathBuf::from("installed"),
                data_dir: self.dir.path().to_owned(),
                receipt: serde_json::Value::Null,
            },
            budgets: budgets(),
            sockets: self.sockets.clone(),
            topology,
            switch_id: switch_id.map(str::to_owned),
        }
    }

    fn out(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("out")).unwrap_or_default()
    }

    fn stop(mut self) -> std::process::ExitStatus {
        self.worker.wait_exit()
    }
}

fn read_all(address: SocketAddr) -> String {
    let mut stream = TcpStream::connect_timeout(&address, LONG).expect("connect");
    stream.set_read_timeout(Some(LONG)).expect("timeout");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    text
}

fn listeners() -> (TcpListener, TcpListener) {
    #[cfg(windows)]
    {
        use open_ferry_supervisor::sockets::windows::{BindOptions, bind};
        let make = || {
            bind(
                "127.0.0.1:0".parse().expect("address"),
                BindOptions::named_host(),
            )
            .expect("bind")
        };
        (make(), make())
    }
    #[cfg(unix)]
    {
        let make = || TcpListener::bind("127.0.0.1:0").expect("bind");
        (make(), make())
    }
}

#[cfg(unix)]
struct Worker(open_ferry_supervisor::process::unix::WorkerProcess);

#[cfg(unix)]
impl Worker {
    fn pid(&self) -> u32 {
        self.0.pid()
    }
    fn wait_exit(&mut self) -> std::process::ExitStatus {
        self.0
            .wait_timeout(LONG)
            .expect("wait")
            .expect("the probe exits")
    }
}

#[cfg(windows)]
struct Worker(open_ferry_supervisor::process::windows::WorkerProcess);

#[cfg(windows)]
impl Worker {
    fn pid(&self) -> u32 {
        self.0.pid()
    }
    fn wait_exit(&mut self) -> std::process::ExitStatus {
        self.0
            .wait_timeout(LONG)
            .expect("wait")
            .expect("the probe exits")
    }
}

/// Starts the probe with its control channel and listeners, as the supervisor
/// does. `mode` goes to the probe's `mode` file.
async fn start(mode: &str) -> Started {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("mode"), mode).expect("mode");
    let (main, management) = listeners();
    #[cfg(unix)]
    {
        let mut command = std::process::Command::new(probe());
        command.args(worker_args(dir.path(), None));
        let (worker, control) = open_ferry_supervisor::process::unix::WorkerProcess::spawn(
            command,
            &[
                main.try_clone().expect("clone"),
                management.try_clone().expect("clone"),
            ],
        )
        .expect("the probe starts");
        let channel = open_ferry_supervisor::control::unix_channel(control).expect("channel");
        Started {
            worker: Worker(worker),
            channel,
            dir,
            main,
            management,
            sockets: Vec::new(),
        }
    }
    #[cfg(windows)]
    {
        use open_ferry_supervisor::control::ControlPipe;
        use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
        use open_ferry_supervisor::process::windows::{SpawnSpec, WorkerProcess};
        use open_ferry_supervisor::sockets::windows::duplicate_for_process;

        let pipe = ControlPipe::create(&Identity::current(ServiceKind::User).expect("identity"))
            .expect("pipe");
        let args = worker_args(dir.path(), Some(pipe.name()));
        let log = std::fs::File::create(dir.path().join("worker.log")).expect("log");
        let spec = SpawnSpec {
            exe: probe(),
            args: &args,
            cwd: None,
            stdout: Some(&log),
            stderr: Some(&log),
        };
        let worker = WorkerProcess::spawn(&spec).expect("the probe starts");
        let channel = tokio::time::timeout(LONG, pipe.accept(worker.pid()))
            .await
            .expect("the worker connects")
            .expect("the worker is the client");
        let sockets = vec![
            WindowsSocket {
                name: "main".into(),
                info: duplicate_for_process(&main, worker.pid()).expect("duplicate"),
            },
            WindowsSocket {
                name: "management".into(),
                info: duplicate_for_process(&management, worker.pid()).expect("duplicate"),
            },
        ];
        Started {
            worker: Worker(worker),
            channel,
            dir,
            main,
            management,
            sockets,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_attaches_and_gets_its_sockets_and_welcome() {
    // Not upstream's: L3's whole path. The worker sees the supervisor's welcome,
    // and its listeners are the supervisor's (a connection to the supervisor's
    // copy reaches the worker).
    let mut started = start("").await;
    let params = started.params(Some("sw-1"));
    let pid = started.worker.pid();
    let negotiated = handshake(&mut started.channel, pid, params)
        .await
        .expect("handshake");
    assert_eq!(negotiated.version, 1);
    assert_eq!(negotiated.hello.pid, pid);
    assert_eq!(negotiated.hello.version, "probe-1");
    assert_eq!(negotiated.hello.caps, vec!["probe".to_owned()]);
    let loaded = tokio::time::timeout(LONG, started.channel.receive::<Message>())
        .await
        .expect("loaded in time")
        .expect("receive")
        .expect("not closed");
    assert_eq!(loaded.message, Message::Loaded { ms: 1 });
    let out = started.out();
    assert!(
        out.contains("role=Active version=1 listeners=main,management"),
        "{out}"
    );
    assert!(out.contains("switch=Some(\"sw-1\")"), "{out}");
    assert!(out.contains("load_ms=60000 topology=2"), "{out}");
    assert_eq!(read_all(started.main.local_addr().unwrap()), "probe-main");
    assert_eq!(
        read_all(started.management.local_addr().unwrap()),
        "probe-management"
    );
    started
        .channel
        .send(&Message::Stop { budget_ms: 1000 })
        .await
        .expect("stop");
    assert!(started.stop().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_channel_stops_the_worker() {
    // Not upstream's: EOF means stop, on both transports. The runtime has its
    // own threads so a dropped Windows pipe can finish closing while the test waits.
    let mut started = start("").await;
    let params = started.params(None);
    let pid = started.worker.pid();
    handshake(&mut started.channel, pid, params)
        .await
        .expect("handshake");
    let loaded = tokio::time::timeout(LONG, started.channel.receive::<Message>())
        .await
        .expect("loaded in time")
        .expect("receive")
        .expect("open");
    assert_eq!(loaded.message, Message::Loaded { ms: 1 });
    let Started {
        worker,
        channel,
        dir: _dir,
        ..
    } = started;
    drop(channel);
    let mut worker = worker;
    let status = worker.wait_exit();
    let out = std::fs::read_to_string(_dir.path().join("out")).unwrap_or_default();
    assert!(status.success(), "{status}: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_that_needs_a_newer_supervisor_is_told_so() {
    // Not upstream's: negotiation fails closed with incompatible, and the worker
    // reports it instead of running.
    let mut started = start("min2").await;
    let params = started.params(None);
    let pid = started.worker.pid();
    match handshake(&mut started.channel, pid, params).await {
        Err(HandshakeError::Incompatible { needs_supervisor }) => assert_eq!(needs_supervisor, 2),
        other => panic!("expected incompatible, got {other:?}"),
    }
    let out = {
        let status = started.worker.wait_exit();
        assert!(!status.success());
        started.out()
    };
    assert!(out.contains("needs version 2"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hello_from_another_process_is_refused() {
    // Not upstream's: the supervisor checks hello's pid against the process it
    // started.
    let mut started = start("").await;
    let params = started.params(None);
    let wrong = started.worker.pid().wrapping_add(1);
    match handshake(&mut started.channel, wrong, params).await {
        Err(HandshakeError::PidMismatch { claimed, expected }) => {
            assert_eq!(claimed, started.worker.pid());
            assert_eq!(expected, wrong);
        }
        other => panic!("expected a pid mismatch, got {other:?}"),
    }
    // Hang up: the probe ends on EOF.
    let Started {
        worker, channel, ..
    } = started;
    drop(channel);
    let mut worker = worker;
    let _ = worker.wait_exit();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_message_before_welcome_is_answered_unsupported() {
    // Not upstream's: the worker answers a type it does not know with
    // unsupported, and still takes the welcome that follows.
    let mut started = start("").await;
    started
        .channel
        .send(&serde_json::json!({"type": "future_thing", "x": 1}))
        .await
        .expect("send");
    let params = started.params(None);
    let pid = started.worker.pid();
    handshake(&mut started.channel, pid, params)
        .await
        .expect("handshake");
    let mut saw_unsupported = false;
    for _ in 0..2 {
        let frame = tokio::time::timeout(LONG, started.channel.receive::<Message>())
            .await
            .expect("in time")
            .expect("receive")
            .expect("open");
        if frame.message
            == (Message::Unsupported {
                message_type: "future_thing".into(),
            })
        {
            saw_unsupported = true;
        }
    }
    assert!(saw_unsupported);
    started
        .channel
        .send(&Message::Stop { budget_ms: 1000 })
        .await
        .expect("stop");
    assert!(started.stop().success());
}

#[cfg(windows)]
mod windows {
    use open_ferry_supervisor::control::{ControlPipe, parse_pipe_name};
    use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
    use tokio::net::windows::named_pipe::ClientOptions;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_is_not_the_worker_is_dropped() {
        // Not upstream's: L2 checks the connecting process against the pid it
        // started. This test's own process connects, expecting another.
        let identity = Identity::current(ServiceKind::User).expect("identity");
        let pipe = ControlPipe::create(&identity).expect("pipe");
        let name = pipe.name().to_owned();
        let accept = tokio::spawn(pipe.accept(std::process::id().wrapping_add(7)));
        use tokio::io::AsyncReadExt;
        let mut client = ClientOptions::new().open(&name).expect("connect");
        let read = tokio::time::timeout(super::LONG, client.read(&mut [0u8; 1]))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)), "wrong peer was not dropped");
        assert!(
            !accept.is_finished(),
            "accept stopped after rejecting a peer"
        );
        accept.abort();
        assert!(matches!(accept.await, Err(error) if error.is_cancelled()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_pipe_has_its_own_name_and_the_name_has_the_documented_shape() {
        let identity = Identity::current(ServiceKind::User).expect("identity");
        let one = ControlPipe::create(&identity).expect("pipe");
        let two = ControlPipe::create(&identity).expect("pipe");
        assert_ne!(one.name(), two.name());
        assert_eq!(parse_pipe_name(one.name()).unwrap(), std::process::id());
        let random = one.name().rsplit('-').next().unwrap();
        assert_eq!(random.len(), 32);
    }

    #[test]
    fn a_malformed_pipe_name_is_refused_by_both_sides() {
        let good = format!(r"\\.\pipe\open-ferry-worker-123-{}", "ab".repeat(16));
        assert_eq!(parse_pipe_name(&good).unwrap(), 123);
        for bad in [
            r"\\.\pipe\open-ferry-supervisor-0123456789abcdef".to_owned(),
            format!(r"\\.\pipe\open-ferry-worker--{}", "ab".repeat(16)),
            format!(r"\\.\pipe\open-ferry-worker-12x-{}", "ab".repeat(16)),
            format!(r"\\.\pipe\open-ferry-worker-007-{}", "ab".repeat(16)),
            format!(r"\\.\pipe\open-ferry-worker-123-{}", "AB".repeat(16)),
            r"\\.\pipe\open-ferry-worker-123-abcd".to_owned(),
            format!(
                r"\\.\pipe\open-ferry-worker-99999999999-{}",
                "ab".repeat(16)
            ),
            format!(r"\\server\pipe\open-ferry-worker-123-{}", "ab".repeat(16)),
        ] {
            assert!(parse_pipe_name(&bad).is_err(), "{bad}");
            assert!(
                open_ferry_supervisor::worker::take(std::ffi::OsStr::new(&bad)).is_err(),
                "{bad}"
            );
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn a_wrong_control_peer_is_dropped_and_the_expected_peer_can_still_connect() {
    use open_ferry_supervisor::control::ControlPipe;
    use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
    use open_ferry_supervisor::process::windows::{SpawnSpec, WorkerProcess};
    use tokio::net::windows::named_pipe::ClientOptions;
    let dir = tempfile::tempdir().unwrap();
    let identity = Identity::current(ServiceKind::User).unwrap();
    let pipe = ControlPipe::create(&identity).unwrap();
    let name = pipe.name().to_owned();
    let args = worker_args(dir.path(), Some(&name));
    let wrong = WorkerProcess::spawn(&SpawnSpec {
        exe: probe(),
        args: &args,
        cwd: None,
        stdout: None,
        stderr: None,
    })
    .unwrap();
    let accept = tokio::spawn(async move { pipe.accept(std::process::id()).await });
    // The probe's attach fails after its connection has been rejected.
    let exited = tokio::task::spawn_blocking(move || wrong.wait_timeout(LONG).unwrap())
        .await
        .unwrap();
    assert!(exited.is_some(), "rejected peer was not disconnected");
    assert!(
        !accept.is_finished(),
        "wrong peer ended the accept instead of being dropped"
    );
    let client = ClientOptions::new().open(&name).unwrap();
    let channel = tokio::time::timeout(LONG, accept)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(channel);
    drop(client);
}
