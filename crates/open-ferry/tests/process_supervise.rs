//! Not upstream's: the real `open-ferry -supervise` (the real `Host`, not the
//! process tests' own), run as a process on its own data directory and its
//! own endpoint: its first start, a request to the worker it started, and a
//! stop through the endpoint.
//!
//! The harness is the supervisor crate's `under_test`, included by path: a
//! test crate cannot use another's modules, a shared crate would be a new
//! workspace member for test code only, and the harness needs the including
//! crate's own binaries (`CARGO_BIN_EXE_*`), which a crate of its own would
//! not have. The feature gate is the same on both sides.
#![cfg(feature = "blue-green-test")]

#[path = "support/privilege.rs"]
mod privilege;

#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use open_ferry_supervisor::protocol::{EndpointReply, EndpointRequest, VERSION};
use open_ferry_supervisor::timings::STOP_BUDGET;
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

/// Starts `open-ferry -supervise` from a copy of the worker binary in `root`
/// (its `bin/`), with a config on `port` and self-update off, on a data
/// directory and an endpoint of its own. The worker answers with the marker
/// unique to this fixture.
fn spawn(root: &std::path::Path, port: u16) -> Sup {
    // The binary runs from the scratch directory, where the data directory's
    // trust check accepts it, not from the target directory.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(NAME);
    write_executable(&exe, &artifact_bytes(A));
    // The worker marks every response with the `test-marker` beside the
    // binary it runs from. Which copy that is, the installed one or the
    // supervisor's kept one under `versions/` (named by the build's version),
    // is the supervisor's business, so both carry the marker: the answer then
    // proves it came from a copy of this binary, not from a leftover.
    let kept = open_ferry_update::data_dir::DataDir::at(root.join("data"))
        .version_dir(env!("CARGO_PKG_VERSION"));
    for dir in [bin.clone(), kept] {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test-marker"),
            format!("{}\n", fixture_marker(root)),
        )
        .unwrap();
    }
    // Self-update is off: nothing here may switch or update.
    std::fs::write(root.join("config.yaml"), config_text(root, port, "off")).unwrap();

    let log = std::fs::File::create(root.join("supervisor.log")).unwrap();
    let mut command = Command::new(&exe);
    command
        .arg("-supervise")
        .arg("-config")
        .arg(root.join("config.yaml"))
        .arg("-test-data-dir")
        .arg(root.join("data"));
    if let Some(endpoint) = endpoint_flag(root) {
        command.arg("-test-endpoint").arg(endpoint);
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
    Sup::adopt(command.spawn().expect("start open-ferry -supervise"), root)
}

/// A bind failure leaves the supervisor alive but degraded. Inspect its log
/// on every poll, and retry only that failure within the one startup budget.
fn launch(root: &std::path::Path) -> (Sup, u16) {
    launch_with_first_port(root, None, LONG)
}

fn launch_with_first_port(
    root: &std::path::Path,
    first_port: Option<u16>,
    budget: Duration,
) -> (Sup, u16) {
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
        let mut sup = spawn(root, port);
        loop {
            let exit = sup.try_exit();
            let log = std::fs::read_to_string(root.join("supervisor.log")).unwrap_or_default();
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

/// Waits for the first answer to `GET /` on `port`, panicking with the
/// supervisor's log if it exits first or nothing answers. (Until the worker
/// accepts, a request waits in the listener's queue and the read times out;
/// the next try asks again.)
fn wait_answer(sup: &mut Sup, port: u16, dump: &dyn Fn() -> String) -> (u16, Option<String>) {
    let until = Instant::now() + LONG;
    loop {
        if let Some(status) = sup.try_exit() {
            panic!(
                "the supervisor exited early ({status})
{}",
                dump()
            );
        }
        if let Some(answer) = http_get(port, "/") {
            return answer;
        }
        assert!(
            Instant::now() < until,
            "nothing answered GET /
{}",
            dump()
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn the_real_supervisor_starts_serves_and_stops_through_its_endpoint() {
    if privilege::privileged() {
        eprintln!(
            "skipped the_real_supervisor_starts_serves_and_stops_through_its_endpoint: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    assert_eq!(kind(), Kind::Real);
    let _permit = Permit::take();
    let dir = scratch();
    let root = dir.path();
    let (mut sup, port) = launch(root);
    let dump = || std::fs::read_to_string(root.join("supervisor.log")).unwrap_or_default();

    // First start: the worker serves GET / on the supervisor's own listener.
    let started = Instant::now();
    let (status, marker) = wait_answer(&mut sup, port, &dump);
    eprintln!("first answer after {:?}", started.elapsed());
    assert!((200..500).contains(&status), "GET / answered {status}");
    assert_eq!(
        marker.as_deref(),
        Some(fixture_marker(root).as_str()),
        "the answer did not carry the binary's test marker"
    );

    // The endpoint is this supervisor's: its record names this process.
    let record = root.join("data").join("supervisor.json");
    let until = Instant::now() + LONG;
    loop {
        let named = std::fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|v| v["pid"].as_u64());
        if named == Some(u64::from(sup.pid())) {
            break;
        }
        assert!(
            Instant::now() < until,
            "supervisor.json never named the child\n{}",
            dump()
        );
        std::thread::sleep(POLL);
    }

    let reply = endpoint_ask(root, EndpointRequest::Stop { v: VERSION });
    assert!(
        matches!(reply, Some(EndpointReply::Stopping { .. })),
        "stop answered {reply:?}\n{}",
        dump()
    );
    let stopped = Instant::now();
    let status = loop {
        if let Some(status) = sup.try_exit() {
            break status;
        }
        assert!(
            stopped.elapsed() < STOP_BUDGET,
            "the supervisor did not exit within {STOP_BUDGET:?}\n{}",
            dump()
        );
        std::thread::sleep(POLL);
    };
    eprintln!("exited {:?} after the stop request", stopped.elapsed());
    assert!(status.success(), "exit status {status}\n{}", dump());
    // The listener is closed with the supervisor and its worker.
    assert!(
        http_get(port, "/").is_none(),
        "something still answers on the port"
    );
}

/// A SIGTERM to the supervisor (what a service manager sends) stops it and
/// its worker within the stop budget, with success. Unix only: a Windows
/// console process gets Ctrl-C only through a shared console, which a test
/// that must not touch its parent's cannot give it reliably, so there the
/// stop is the endpoint's, tested above.
#[cfg(unix)]
#[test]
fn a_terminate_signal_stops_the_supervisor_and_its_worker() {
    if privilege::privileged() {
        eprintln!(
            "skipped a_terminate_signal_stops_the_supervisor_and_its_worker: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    assert_eq!(kind(), Kind::Real);
    let _permit = Permit::take();
    let dir = scratch();
    let root = dir.path();
    let (mut sup, port) = launch(root);
    let dump = || std::fs::read_to_string(root.join("supervisor.log")).unwrap_or_default();
    let (status, _) = wait_answer(&mut sup, port, &dump);
    assert!((200..500).contains(&status), "GET / answered {status}");
    // The worker's pid, from its own event log.
    let events = root
        .join("data")
        .join(".blue-green-test")
        .join("events.log");
    let until = Instant::now() + LONG;
    let worker = loop {
        let pid = std::fs::read_to_string(&events)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|v| v["event"] == "active")
            .and_then(|v| v["pid"].as_u64());
        if let Some(pid) = pid {
            break u32::try_from(pid).unwrap();
        }
        assert!(
            Instant::now() < until,
            "no active event
{}",
            dump()
        );
        std::thread::sleep(POLL);
    };
    assert_ne!(worker, sup.pid());
    assert!(alive(worker));
    // SAFETY: `kill` with the pid of a child this test holds.
    let sent = unsafe { libc::kill(i32::try_from(sup.pid()).unwrap(), libc::SIGTERM) };
    assert_eq!(sent, 0, "kill: {}", std::io::Error::last_os_error());
    let stopped = Instant::now();
    let status = loop {
        if let Some(status) = sup.try_exit() {
            break status;
        }
        assert!(
            stopped.elapsed() < STOP_BUDGET,
            "the supervisor did not exit within {STOP_BUDGET:?}
{}",
            dump()
        );
        std::thread::sleep(POLL);
    };
    eprintln!("exited {:?} after SIGTERM", stopped.elapsed());
    assert!(
        status.success(),
        "exit status {status}
{}",
        dump()
    );
    assert!(!alive(worker), "the worker outlived the supervisor");
    assert!(
        http_get(port, "/").is_none(),
        "something still answers on the port"
    );
}

/// Not upstream's: a live degraded supervisor must not strand a port retry.
#[test]
fn a_live_bind_conflict_retries_on_a_different_port() {
    // The non-root Linux job exercises this control.
    if privilege::privileged() {
        eprintln!(
            "skipped process_supervise::a_live_bind_conflict_retries_on_a_different_port: requires an unprivileged account"
        );
        return;
    }
    let _permit = Permit::take();
    let dir = scratch();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = held.local_addr().unwrap().port();
    let (_sup, port) = launch_with_first_port(dir.path(), Some(occupied), LONG);
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
