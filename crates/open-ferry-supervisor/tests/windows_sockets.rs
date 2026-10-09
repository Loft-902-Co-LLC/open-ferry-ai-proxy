//! Not upstream's: test 1 of the blue-green plan on Windows. A listener the
//! supervisor made is served by worker processes that got it as protocol info;
//! two processes share one socket with one paused; and pause and resume repeat
//! with the real tokio listener. Loopback and ephemeral ports only.
#![cfg(windows)]

mod common;

use std::fs::File;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use common::*;
use open_ferry_supervisor::process::windows::WorkerProcess;
use open_ferry_supervisor::sockets::windows::{
    BindOptions, bind, duplicate_for_process, is_inheritable,
};

fn loopback() -> TcpListener {
    bind(
        "127.0.0.1:0".parse().expect("address"),
        BindOptions::named_host(),
    )
    .expect("bind")
}

/// One probe worker and the files that steer it.
struct Served {
    worker: WorkerProcess,
    pause: std::path::PathBuf,
    stop: std::path::PathBuf,
}

impl Served {
    fn start(dir: &std::path::Path, name: &str, listener: &TcpListener, paused: bool) -> Self {
        let file = |suffix: &str| dir.join(format!("{name}.{suffix}"));
        let (info, pause, stop, ready) = (file("info"), file("pause"), file("stop"), file("ready"));
        if paused {
            std::fs::write(&pause, "").expect("pause");
        }
        let log = File::create(file("log")).expect("log");
        let args = [
            os(&["serve", "--info-file"]),
            vec![text(&info)],
            os(&["--pause-file"]),
            vec![text(&pause)],
            os(&["--stop-file"]),
            vec![text(&stop)],
            os(&["--ready-file"]),
            vec![text(&ready)],
        ]
        .concat();
        let worker = spawn_probe(&args, &log);
        let hex = duplicate_for_process(listener, worker.pid()).expect("duplicate for the worker");
        write_atomic(&info, &hex);
        assert!(wait_for(&ready), "the worker did not start serving");
        Self {
            worker,
            pause,
            stop,
        }
    }

    fn state(&self) -> std::path::PathBuf {
        let mut name = self.pause.as_os_str().to_owned();
        name.push(".state");
        name.into()
    }

    fn pause(&self) {
        std::fs::write(&self.pause, "").expect("pause");
        assert!(wait_for_state(&self.state(), "paused"));
    }

    /// Pauses unless it is already paused, so a cycle can start with both paused.
    fn pause_if_serving(&self) {
        if !self.pause.exists() {
            self.pause();
        }
    }

    fn resume(&self) {
        std::fs::remove_file(&self.pause).expect("resume");
        assert!(wait_for_state(&self.state(), "serving"));
    }

    fn pid(&self) -> String {
        self.worker.pid().to_string()
    }

    fn stop(&self) {
        write_atomic(&self.stop, "");
        let status = self.worker.wait_timeout(LONG).expect("wait");
        let Some(status) = status else {
            let log = std::fs::read_to_string(self.stop.with_extension("log")).unwrap_or_default();
            panic!("the worker did not exit; its output:\n{log}");
        };
        assert!(status.success(), "the worker exited with {status}");
    }
}

#[test]
fn a_worker_serves_the_listener_it_was_sent() {
    // Not upstream's: the info made for the worker's pid opens the same listener.
    let dir = tempfile::tempdir().expect("dir");
    let listener = loopback();
    let address = listener.local_addr().expect("address");
    assert!(!is_inheritable(&listener).expect("flags"));

    let worker = Served::start(dir.path(), "a", &listener, false);
    for _ in 0..5 {
        assert_eq!(answer(connect(address)), worker.pid());
    }
    worker.stop();

    // The supervisor's own socket outlives the worker, and still has a backlog.
    let waiting = connect(address);
    listener.set_nonblocking(true).expect("nonblocking");
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((_, peer)) => {
                assert_eq!(peer, waiting.local_addr().expect("local"));
                break;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < end =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("the supervisor's listener stopped working: {error}"),
        }
    }
}

#[test]
fn two_processes_share_one_socket_and_the_paused_one_takes_nothing() {
    // Not upstream's: a paused worker leaves every connection to the other,
    // and connections made while both are paused wait for whichever resumes.
    let dir = tempfile::tempdir().expect("dir");
    let listener = loopback();
    let address = listener.local_addr().expect("address");

    let first = Served::start(dir.path(), "first", &listener, false);
    let second = Served::start(dir.path(), "second", &listener, true);
    assert!(wait_for_state(&second.state(), "paused"));

    for _ in 0..12 {
        assert_eq!(answer(connect(address)), first.pid());
    }

    first.pause();
    second.resume();
    for _ in 0..12 {
        assert_eq!(answer(connect(address)), second.pid());
    }

    // Both paused: the connections queue, then the one that resumes takes all.
    for cycle in 0..6 {
        first.pause_if_serving();
        second.pause_if_serving();
        let queued: Vec<_> = (0..3).map(|_| connect(address)).collect();
        let taker = if cycle % 2 == 0 { &first } else { &second };
        taker.resume();
        for stream in queued {
            assert_eq!(answer(stream), taker.pid(), "cycle {cycle}");
        }
    }

    first.stop();
    second.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_and_resume_repeat_on_the_real_tokio_listener() {
    // Not upstream's: dropping the tokio listener keeps the socket open through
    // the std clone, and a new tokio listener sees what queued while paused.
    let std_listener = loopback();
    std_listener.set_nonblocking(true).expect("nonblocking");
    let address = std_listener.local_addr().expect("address");

    for round in 0..25 {
        // Paused: no tokio listener exists, only the std one.
        let clients: Vec<_> = (0..4)
            .map(|_| std::net::TcpStream::connect(address).expect("connect while paused"))
            .collect();

        let resumed = tokio::net::TcpListener::from_std(std_listener.try_clone().expect("clone"))
            .expect("tokio");
        let mut wanted: Vec<_> = clients
            .iter()
            .map(|client| client.local_addr().expect("local"))
            .collect();
        let mut accepted = Vec::new();
        for _ in 0..clients.len() {
            let (stream, peer) = tokio::time::timeout(Duration::from_secs(10), resumed.accept())
                .await
                .unwrap_or_else(|_| panic!("round {round}: a queued connection was not accepted"))
                .expect("accept");
            accepted.push((stream, peer));
        }
        let mut peers: Vec<_> = accepted.iter().map(|(_, peer)| *peer).collect();
        peers.sort();
        wanted.sort();
        assert_eq!(peers, wanted, "round {round}");

        // Live: a connection made after the resume is accepted too.
        let late = std::net::TcpStream::connect(address).expect("connect after resume");
        let (_stream, peer) = tokio::time::timeout(Duration::from_secs(10), resumed.accept())
            .await
            .expect("the late connection was not accepted")
            .expect("accept");
        assert_eq!(peer, late.local_addr().expect("local"));
        drop(resumed);
    }
}
