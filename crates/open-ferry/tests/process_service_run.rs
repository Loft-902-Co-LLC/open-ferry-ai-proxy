//! Not upstream's: the supervised form of the scheduled task, as processes.
//!
//! - On every platform: `open-ferry -supervise` loads the working
//!   directory's `.env` once, and the worker it starts sees what only the
//!   `.env` set.
//! - On Windows: `open-ferry service run -supervise`, the wrapper the task
//!   runs, starts the supervisor and its worker in a job that ends them with
//!   the wrapper, also when the wrapper itself is in an outer job; and the
//!   stop `service uninstall` makes first (`client::stop`) ends the
//!   supervisor, the worker and the wrapper, which exits 0.
//!
//! The harness is the supervisor crate's `under_test`, included by path as
//! `process_supervise.rs` does.
#![cfg(feature = "blue-green-test")]

#[path = "support/privilege.rs"]
mod privilege;

#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use under_test::*;

#[cfg(target_os = "linux")]
#[path = "support/parent_death.rs"]
mod parent_death;

/// Choose a candidate; launch retries only a logged address-in-use bind failure.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What starts the supervisor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    /// `open-ferry -supervise`, with the test flags for its data directory
    /// and endpoint.
    Directly,
    /// `open-ferry service run -supervise`, the task's wrapper, which passes
    /// `-supervise -config <path> -data-dir <path>` on.
    #[cfg(windows)]
    ThroughTheWrapper,
}

/// The binary, copied into `root/bin` where the data directory's trust check
/// accepts it, with the markers the worker answers with. Keep this immutable
/// across bind retries: Windows may still be unmapping a killed worker.
fn prepare(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(NAME);
    write_executable(&exe, &artifact_bytes(A));
    let kept = open_ferry_update::data_dir::DataDir::at(root.join("data"))
        .version_dir(env!("CARGO_PKG_VERSION"));
    for dir in [bin, kept] {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test-marker"),
            format!("{}\n", fixture_marker(root)),
        )
        .unwrap();
    }
    exe
}

/// Starts the supervisor in `root` as `how`, with `root` as its working
/// directory; its output goes to `root/out.log`.
fn spawn(root: &Path, exe: &Path, port: u16, how: How) -> Sup {
    std::fs::write(root.join("config.yaml"), config_text(root, port, "off")).unwrap();
    // The wrapper appends here: keep each attempt's bind evidence separate.
    std::fs::write(root.join("service.log"), b"").unwrap();
    let log = std::fs::File::create(root.join("out.log")).unwrap();
    let mut command = Command::new(exe);
    match how {
        How::Directly => {
            command
                .arg("-supervise")
                .arg("-config")
                .arg(root.join("config.yaml"))
                .arg("-test-data-dir")
                .arg(root.join("data"));
            if let Some(endpoint) = endpoint_flag(root) {
                command.arg("-test-endpoint").arg(endpoint);
            }
        }
        #[cfg(windows)]
        How::ThroughTheWrapper => {
            command
                .args(["service", "run", "-supervise", "-config"])
                .arg(root.join("config.yaml"))
                .arg("-data-dir")
                .arg(root.join("data"));
        }
    }
    isolate(&mut command);
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(target_os = "linux")]
    parent_death::on_test_exit(&mut command);
    Sup::adopt(command.spawn().expect("start open-ferry"), root)
}

/// What the supervisor and the wrapper printed.
fn dump(root: &Path) -> String {
    ["out.log", "service.log"]
        .iter()
        .filter_map(|name| std::fs::read_to_string(root.join(name)).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bind failure leaves the supervisor alive but degraded. Inspect its log
/// on every poll, and retry only that failure within the one startup budget.
fn launch(root: &Path, how: How) -> (Sup, u16) {
    launch_with_first_port(root, how, None, LONG)
}

fn launch_with_first_port(
    root: &Path,
    how: How,
    first_port: Option<u16>,
    budget: Duration,
) -> (Sup, u16) {
    let exe = prepare(root);
    let until = Instant::now() + budget;
    for attempt in 0..8 {
        assert!(
            Instant::now() < until,
            "no free port within the startup budget"
        );
        let port = if attempt == 0 {
            first_port.unwrap_or_else(free_port)
        } else {
            free_port()
        };
        let mut sup = spawn(root, &exe, port, how);
        loop {
            let exit = sup.try_exit();
            let log = dump(root);
            let conflict = log.lines().any(|line| {
                line.contains("failed to bind the main listener")
                    && ["(os error 98)", "(os error 48)", "(os error 10048)"]
                        .iter()
                        .any(|code| line.contains(code))
            });
            if conflict {
                assert!(attempt < 7, "all bind attempts failed:\n{log}");
                if attempt == 0 {
                    std::fs::write(root.join("first-bind-conflict.log"), &log).unwrap();
                }
                break;
            }
            if let Some(status) = exit {
                panic!("startup exited ({status}):\n{log}");
            }
            // A foreign answer is never accepted, even while our child lives.
            if let Some((_, marker)) = http_get(port, "/")
                && marker.as_deref() == Some(fixture_marker(root).as_str())
            {
                return (sup, port);
            }
            assert!(Instant::now() < until, "nothing answered GET /\n{log}");
            std::thread::sleep(POLL);
        }
        // Sup::drop ends this attempt's own group before another port is tried.
    }
    unreachable!("the last bind conflict fails above")
}

fn fixture_marker(root: &std::path::Path) -> String {
    format!("fixture-{}", root.file_name().unwrap().to_string_lossy())
}

/// Waits for the first answer to `GET /` on `port`, panicking with the logs
/// if the process exits first or nothing answers.
fn wait_answer(sup: &mut Sup, port: u16, root: &Path) -> u16 {
    let until = Instant::now() + LONG;
    loop {
        if let Some(status) = sup.try_exit() {
            panic!("it exited early ({status})\n{}", dump(root));
        }
        if let Some((status, marker)) = http_get(port, "/") {
            assert_eq!(marker.as_deref(), Some(fixture_marker(root).as_str()));
            return status;
        }
        assert!(
            Instant::now() < until,
            "nothing answered GET /\n{}",
            dump(root)
        );
        std::thread::sleep(POLL);
    }
}

/// Waits until nothing answers on `port`.
#[cfg(windows)]
fn wait_silent(port: u16, what: &str, root: &Path) {
    let until = Instant::now() + LONG;
    while http_get(port, "/").is_some() {
        assert!(
            Instant::now() < until,
            "{what}: something still answers on the port\n{}",
            dump(root)
        );
        std::thread::sleep(POLL);
    }
}

/// The management path: with no `MANAGEMENT_PASSWORD` the management API is
/// off and the server answers 404; with one it answers 401 without the key.
const MANAGEMENT: &str = "/v0/management/config";

/// Starts a supervisor in a fresh directory, with `dotenv` as its `.env` if
/// any, and the status the management path answers.
fn management_status(dotenv: Option<&str>) -> u16 {
    let _permit = Permit::take();
    let dir = scratch();
    let root = dir.path();
    if let Some(text) = dotenv {
        std::fs::write(root.join(".env"), text).unwrap();
    }
    let (mut sup, port) = launch(root, How::Directly);
    wait_answer(&mut sup, port, root);
    let (status, _) = http_get(port, MANAGEMENT)
        .unwrap_or_else(|| panic!("the management path did not answer\n{}", dump(root)));
    status
}

#[test]
fn a_setting_only_the_dotenv_has_reaches_the_worker() {
    if privilege::privileged() {
        eprintln!(
            "skipped a_setting_only_the_dotenv_has_reaches_the_worker: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    assert_eq!(kind(), Kind::Real);
    // Without the file the management API is off; with it, on: the worker
    // read nothing itself, it has the supervisor's environment.
    assert_eq!(management_status(None), 404);
    assert_eq!(
        management_status(Some("MANAGEMENT_PASSWORD=from-the-dotenv\n")),
        401
    );
}

#[cfg(windows)]
mod wrapper {
    use super::*;

    use open_ferry_supervisor::client::{self, StopOutcome};
    use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
    use open_ferry_supervisor::timings::STOP_BUDGET;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Whether the process `pid` is in `job`, or in any job when `job` is
    /// null. A process in a job nested in `job` is in `job` too.
    fn in_a_job(pid: u32, job: HANDLE) -> bool {
        // SAFETY: opens a handle to a process of this test's, for a query.
        let handle: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(
            !handle.is_null(),
            "OpenProcess: {}",
            std::io::Error::last_os_error()
        );
        let mut result = 0;
        // SAFETY: `handle` is open, and `job` is a job handle of ours or null.
        let asked = unsafe { IsProcessInJob(handle, job, &mut result) };
        let error = std::io::Error::last_os_error();
        // SAFETY: `handle` is ours and is not used again.
        unsafe { CloseHandle(handle) };
        assert_ne!(asked, 0, "IsProcessInJob: {error}");
        result != 0
    }

    /// The pid `supervisor.json` names, once it is there.
    fn supervisor_pid(root: &Path) -> u64 {
        let record = root.join("data").join("supervisor.json");
        let until = Instant::now() + LONG;
        loop {
            let named = std::fs::read_to_string(&record)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|v| v["pid"].as_u64());
            if let Some(named) = named {
                return named;
            }
            assert!(Instant::now() < until, "no supervisor.json\n{}", dump(root));
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn the_wrapper_runs_the_supervisor_and_ends_it_with_itself() {
        if privilege::privileged() {
            eprintln!(
                "skipped the_wrapper_runs_the_supervisor_and_ends_it_with_itself: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
            );
            return;
        }
        assert_eq!(kind(), Kind::Real);
        let _permit = Permit::take();
        let dir = scratch();
        let root = dir.path();
        let (mut sup, port) = launch(root, How::ThroughTheWrapper);
        let status = wait_answer(&mut sup, port, root);
        assert!((200..500).contains(&status), "GET / answered {status}");
        // The supervisor, not the plain server, is what runs: its record
        // names a process that is not the wrapper.
        let named = supervisor_pid(root);
        assert_ne!(
            named,
            u64::from(sup.pid()),
            "the wrapper is not the supervisor"
        );
        // Ending the wrapper without a word (as the scheduler's end of a
        // task does) ends the supervisor and the worker with it: its job
        // closes.
        sup.kill();
        wait_silent(port, "after the wrapper was ended", root);
    }

    /// `client::stop` as `Host::stop_supervisor` makes it (the uninstall
    /// plan's first step): a current-thread runtime, `STOP_BUDGET` from now.
    fn stop_as_uninstall_does(root: &Path) -> StopOutcome {
        let identity = Identity::current(ServiceKind::User).expect("this user's identity");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(client::stop(
            &root.join("data"),
            &identity,
            std::time::Instant::now() + STOP_BUDGET,
        ))
    }

    #[test]
    fn the_uninstall_stop_ends_the_wrappers_supervisor_and_the_wrapper_with_it() {
        if privilege::privileged() {
            eprintln!(
                "skipped the_uninstall_stop_ends_the_wrappers_supervisor_and_the_wrapper_with_it: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
            );
            return;
        }
        assert_eq!(kind(), Kind::Real);
        let _permit = Permit::take();
        let dir = scratch();
        let root = dir.path();
        let (mut sup, port) = launch(root, How::ThroughTheWrapper);
        wait_answer(&mut sup, port, root);
        let supervisor = u32::try_from(supervisor_pid(root)).unwrap();
        assert_ne!(supervisor, sup.pid(), "the wrapper is not the supervisor");
        // The worker's pid, from its own event log (as process_supervise.rs
        // reads it): the worker that went active is the one that answers.
        let events = root
            .join("data")
            .join(".blue-green-test")
            .join("events.log");
        let until = Instant::now() + LONG;
        let workers: Vec<u32> = loop {
            let pids: Vec<u32> = std::fs::read_to_string(&events)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|v| v["event"] == "active")
                .filter_map(|v| v["pid"].as_u64())
                .filter_map(|pid| u32::try_from(pid).ok())
                .collect();
            if !pids.is_empty() {
                break pids;
            }
            assert!(Instant::now() < until, "no active event\n{}", dump(root));
            std::thread::sleep(POLL);
        };
        for worker in &workers {
            assert_ne!(*worker, supervisor);
            assert_ne!(*worker, sup.pid());
            assert!(
                alive(*worker),
                "worker {worker} is not running before the stop"
            );
        }

        // The stop `service uninstall` makes before it ends the task.
        let started = Instant::now();
        let outcome = stop_as_uninstall_does(root);
        let first = started.elapsed();
        assert_eq!(outcome, StopOutcome::Stopped, "{}", dump(root));

        // The wrapper ends on its own, with nobody killing it.
        let until = Instant::now() + LONG;
        let status = loop {
            if let Some(status) = sup.try_exit() {
                break status;
            }
            assert!(
                Instant::now() < until,
                "the wrapper did not exit after the stop\n{}",
                dump(root)
            );
            std::thread::sleep(POLL);
        };
        // A nonzero exit would let the task's RestartOnFailure start it
        // again between the stop and the plan's end and delete.
        assert_eq!(status.code(), Some(0), "{}", dump(root));

        // The server is silent, and the supervisor and each worker ended.
        wait_silent(port, "after the uninstall stop", root);
        let until = Instant::now() + LONG;
        for (what, pid) in std::iter::once(("supervisor", supervisor))
            .chain(workers.iter().map(|worker| ("worker", *worker)))
        {
            while alive(pid) {
                assert!(
                    Instant::now() < until,
                    "the {what} {pid} still runs\n{}",
                    dump(root)
                );
                std::thread::sleep(POLL);
            }
        }

        // Nothing is left to stop, and that is found quickly.
        let started = Instant::now();
        let again = stop_as_uninstall_does(root);
        let second = started.elapsed();
        assert_eq!(again, StopOutcome::NotRunning, "{}", dump(root));
        assert!(
            second < std::time::Duration::from_secs(5),
            "NotRunning took {second:?}"
        );
        eprintln!(
            "uninstall stop: wrapper exit {:?}; workers {workers:?} alive before, gone after; first stop {first:?}; second stop {second:?}",
            status.code()
        );
    }

    #[test]
    fn the_wrapper_works_inside_an_outer_job() {
        if privilege::privileged() {
            eprintln!(
                "skipped the_wrapper_works_inside_an_outer_job: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
            );
            return;
        }
        assert_eq!(kind(), Kind::Real);
        let _permit = Permit::take();
        // This process joins a job of its own with no limits, so the wrapper
        // it starts is inside an outer job (nested jobs: Windows 8 and later)
        // whatever the harness it runs under did. Nothing is closed: the job
        // ends with this process.
        // SAFETY: an unnamed job with the default security.
        let outer = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!outer.is_null(), "{}", std::io::Error::last_os_error());
        // SAFETY: both handles are valid. Where this process is already in
        // a job, the new one nests in it (Windows 8 and later).
        let assigned = unsafe { AssignProcessToJobObject(outer, GetCurrentProcess()) };
        assert_ne!(
            assigned,
            0,
            "AssignProcessToJobObject: {}",
            std::io::Error::last_os_error()
        );
        let dir = scratch();
        let root = dir.path();
        let (mut sup, port) = launch(root, How::ThroughTheWrapper);
        let status = wait_answer(&mut sup, port, root);
        assert!((200..500).contains(&status), "GET / answered {status}");
        // The wrapper and the supervisor it started are in the outer job
        // (the wrapper's own job nests in it).
        assert!(
            in_a_job(sup.pid(), outer),
            "the wrapper is not in the outer job"
        );
        let supervisor = u32::try_from(supervisor_pid(root)).unwrap();
        assert!(
            in_a_job(supervisor, outer),
            "the supervisor is not in the outer job"
        );
        let log = dump(root);
        assert!(
            !log.contains("failed to make a job"),
            "the wrapper could not make its own job:\n{log}"
        );
        // Its own job still ends the supervisor and the worker with it.
        sup.kill();
        wait_silent(port, "after the wrapper was ended", root);
        // SAFETY: `outer` is ours and is not used again. This process is in
        // the job; with no limits, closing the handle ends nothing.
        unsafe { CloseHandle(outer) };
    }
}

/// Not upstream's: a live degraded supervisor must not strand a port retry.
#[test]
fn a_live_bind_conflict_retries_on_a_different_port() {
    // The non-root Linux job exercises this control.
    if privilege::privileged() {
        eprintln!(
            "skipped process_service_run::a_live_bind_conflict_retries_on_a_different_port: requires an unprivileged account"
        );
        return;
    }
    let _permit = Permit::take();
    let dir = scratch();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = held.local_addr().unwrap().port();
    let (_sup, port) = launch_with_first_port(dir.path(), How::Directly, Some(occupied), LONG);
    assert_ne!(port, occupied);
    let log = std::fs::read_to_string(dir.path().join("first-bind-conflict.log")).unwrap();
    assert!(log.contains("failed to bind the main listener"), "{log}");
    assert!(
        ["(os error 98)", "(os error 48)", "(os error 10048)"]
            .iter()
            .any(|code| log.contains(code)),
        "{log}"
    );
}
