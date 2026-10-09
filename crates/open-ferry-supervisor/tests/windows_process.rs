//! Not upstream's: the Windows process runner. Tests 13 (handle inheritance)
//! and 14 (the job object, a grandchild dies with it) of the blue-green plan,
//! plus waiting, exit status and the liveness check. Every process is the
//! probe, started by the test and held by a handle the test keeps.
#![cfg(windows)]

mod common;

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::*;
use open_ferry_supervisor::process::windows::{ProcessHandle, WorkerProcess, process_alive};
use open_ferry_supervisor::sockets::windows::{BindOptions, bind};
use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Networking::WinSock::{WSASocketW, closesocket};

fn log_in(dir: &Path) -> File {
    File::create(dir.join("worker.log")).expect("log")
}

fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("worker.log")).expect("read the log")
}

/// The probe's `<pid> <started>` line.
fn identity_in(path: &Path) -> (u32, u64) {
    let content = std::fs::read_to_string(path).expect("identity file");
    let mut parts = content.split_whitespace();
    let pid = parts.next().expect("pid").parse().expect("pid number");
    let started = parts
        .next()
        .expect("started")
        .parse()
        .expect("started number");
    (pid, started)
}

#[test]
fn a_worker_inherits_only_its_standard_handles() {
    // Not upstream's: a worker must not receive the supervisor's sockets,
    // control pipe, lease or journal handles, even when they are inheritable.
    let dir = tempfile::tempdir().expect("dir");
    // Makes sure Winsock is started, and gives a listener whose handle we can name.
    let listener = bind(
        "127.0.0.1:0".parse().expect("address"),
        BindOptions::named_host(),
    )
    .expect("bind");
    // A socket made without WSA_FLAG_NO_HANDLE_INHERIT: inheritable on purpose.
    // SAFETY: plain call; the socket is closed at the end of the test.
    let socket = unsafe { WSASocketW(2, 1, 6, std::ptr::null(), 0, 1) };
    assert_ne!(socket, usize::MAX, "WSASocketW failed");
    // A file made inheritable on purpose, standing for the lease and journal.
    let marker = File::create(dir.path().join("marker-lease.txt")).expect("marker");
    // SAFETY: the handle is the marker's, open for the whole test.
    let ok = unsafe {
        SetHandleInformation(
            marker.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    assert_ne!(ok, 0, "SetHandleInformation failed");

    let socket_text = socket.to_string();
    let file_text = (marker.as_raw_handle() as usize).to_string();
    let args = os(&[
        "report",
        "--socket",
        &socket_text,
        "--file",
        &file_text,
        "--name",
        "marker-lease.txt",
    ]);

    // The control: a plain std child inherits every inheritable handle, so the
    // probe sees both. Without this the negative result below would prove nothing.
    let control = Command::new(probe())
        .args(&args)
        .output()
        .expect("control run");
    let control = String::from_utf8_lossy(&control.stdout).into_owned();
    assert!(control.contains("socket=valid"), "control: {control}");
    assert!(control.contains("file=valid"), "control: {control}");

    // The runner: the same handles, in the same process, are not inherited.
    let log = log_in(dir.path());
    let worker = spawn_probe(&args, &log);
    let status = worker
        .wait_timeout(LONG)
        .expect("wait")
        .expect("the probe exits");
    assert!(status.success(), "{status}");
    let report = read_log(dir.path());
    assert!(
        report.contains(&format!("pid={}", worker.pid())),
        "{report}"
    );
    assert!(report.contains("socket=invalid"), "{report}");
    assert!(report.contains("file=invalid"), "{report}");
    // Its standard output and error did reach the log it was given.
    assert!(report.contains("stderr-ok"), "{report}");
    drop(listener);
    // SAFETY: closes the socket made above, once.
    unsafe { closesocket(socket) };
}

#[test]
fn a_grandchild_dies_when_its_worker_is_terminated() {
    // Not upstream's: terminate-by-handle takes the whole job, not just the worker.
    let dir = tempfile::tempdir().expect("dir");
    let (out, stop) = (dir.path().join("gc.out"), dir.path().join("gc.stop"));
    let log = log_in(dir.path());
    let worker = spawn_probe(
        &[
            os(&["grandchild", "--out"]),
            vec![text(&out)],
            os(&["--stop-file"]),
            vec![text(&stop)],
        ]
        .concat(),
        &log,
    );
    assert!(wait_for(&out), "the grandchild did not start");
    let (pid, started) = identity_in(&out);
    // The test's own handle on the grandchild, taken before anything is ended.
    let held = ProcessHandle::open(pid, started)
        .expect("open")
        .expect("the grandchild is running");
    assert!(process_alive(pid, started));
    assert!(worker.job().active_processes().expect("count") >= 2);

    worker.terminate().expect("terminate");
    assert!(
        held.wait_timeout(LONG).expect("wait").is_some(),
        "the grandchild outlived its worker's job"
    );
    assert!(!process_alive(pid, started));
    assert!(worker.wait_timeout(LONG).expect("wait").is_some());
    assert_eq!(worker.job().active_processes().expect("count"), 0);
}

#[test]
fn dropping_the_worker_closes_its_job_and_ends_a_grandchild() {
    // Not upstream's: KILL_ON_JOB_CLOSE is what ends workers when the supervisor dies.
    let dir = tempfile::tempdir().expect("dir");
    let (out, stop) = (dir.path().join("gc.out"), dir.path().join("gc.stop"));
    let log = log_in(dir.path());
    let worker = spawn_probe(
        &[
            os(&["grandchild", "--out"]),
            vec![text(&out)],
            os(&["--stop-file"]),
            vec![text(&stop)],
        ]
        .concat(),
        &log,
    );
    assert!(wait_for(&out), "the grandchild did not start");
    let (pid, started) = identity_in(&out);
    let held = ProcessHandle::open(pid, started)
        .expect("open")
        .expect("the grandchild is running");
    let parent = ProcessHandle::open(worker.pid(), worker.identity().started)
        .expect("open")
        .expect("the worker is running");

    drop(worker);
    assert!(held.wait_timeout(LONG).expect("wait").is_some());
    assert!(parent.wait_timeout(LONG).expect("wait").is_some());
    assert!(!process_alive(pid, started));
}

#[test]
fn exit_status_and_waits() {
    // Not upstream's: the status the supervisor sees is the worker's own code.
    let dir = tempfile::tempdir().expect("dir");
    let log = log_in(dir.path());
    let worker = spawn_probe(&os(&["exit", "--code", "7"]), &log);
    let status = worker.wait_timeout(LONG).expect("wait").expect("exits");
    assert_eq!(status.code(), Some(7));
    assert_eq!(
        worker.try_wait().expect("try").and_then(|s| s.code()),
        Some(7)
    );
    assert_eq!(worker.wait().expect("wait").code(), Some(7));
}

#[test]
fn terminating_by_a_recorded_identity_ends_the_worker() {
    // Not upstream's: a handle opened from a journal entry can end its process,
    // and the liveness check then says it is gone.
    let dir = tempfile::tempdir().expect("dir");
    let log = log_in(dir.path());
    let worker = spawn_probe(&os(&["sleep"]), &log);
    let identity = worker.identity();
    assert!(process_alive(identity.pid, identity.started));
    assert!(!process_alive(identity.pid, identity.started + 1));

    let other = ProcessHandle::open(identity.pid, identity.started)
        .expect("open")
        .expect("running");
    other.terminate(9).expect("terminate");
    let status = worker.wait_timeout(LONG).expect("wait").expect("exits");
    assert_eq!(status.code(), Some(9));
    assert!(!process_alive(identity.pid, identity.started));
    // Ending one that has already gone is not an error.
    worker.terminate().expect("terminate after exit");
}

#[test]
fn kill_and_wait_returns_the_exit() {
    // Not upstream's: the supervisor's last resort on a worker that lost its channel.
    let dir = tempfile::tempdir().expect("dir");
    let log = log_in(dir.path());
    let worker = spawn_probe(&os(&["sleep"]), &log);
    let status = worker.kill_and_wait(LONG).expect("killed");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn kill_reaps_without_consuming_and_can_repeat() {
    // Not upstream's: kill returns once the worker is gone, and again later.
    let dir = tempfile::tempdir().expect("dir");
    let stop = dir.path().join("stop");
    let log = log_in(dir.path());
    let worker = spawn_probe(
        &[os(&["sleep", "--stop-file"]), vec![text(&stop)]].concat(),
        &log,
    );
    assert!(worker.wait_timeout(Duration::ZERO).expect("poll").is_none());
    assert!(worker.try_wait().expect("try").is_none());
    let first = worker.kill().expect("killed");
    assert_eq!(first.code(), Some(1));
    assert!(worker.try_wait().expect("try").is_some());
    let second = worker.kill().expect("killed again");
    assert_eq!(second.code(), Some(1));
    assert!(worker.wait_timeout(Duration::ZERO).expect("poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_without_holding_a_thread() {
    // Not upstream's: wait_async sees the exit, and a cancelled wait leaks nothing.
    let dir = tempfile::tempdir().expect("dir");
    let stop = dir.path().join("stop");
    let log = log_in(dir.path());
    let worker: WorkerProcess = spawn_probe(
        &[os(&["sleep", "--stop-file"]), vec![text(&stop)]].concat(),
        &log,
    );
    // Still running: the wait is cancelled by the timeout, and can start again.
    for _ in 0..3 {
        assert!(
            tokio::time::timeout(Duration::from_millis(150), worker.wait_async())
                .await
                .is_err()
        );
    }
    write_atomic(&stop, "");
    let status = tokio::time::timeout(LONG, worker.wait_async())
        .await
        .expect("the exit was not seen")
        .expect("wait");
    assert!(status.success(), "{status}");
    // Already exited: answers at once.
    assert!(worker.wait_async().await.expect("wait").success());
}
