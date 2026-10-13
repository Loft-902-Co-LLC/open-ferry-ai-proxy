//! Not upstream's: the supervisor client's detection and stop, and the
//! supervisor's start-up locks, against supervisors run as processes (test 16
//! of the 2c-2a plan).
//!
//! The supervisor is the harness's child (`Fx::spawn`): the real `supervise()`
//! in a process of its own, with the real `open-ferry` as its worker. Its
//! installed file is a plain file in the fixture, so a test can replace it
//! while the supervisor waits, as the update CLI's switch does (a running
//! `open-ferry.exe` could not be replaced on Windows). What a test waits for
//! is a barrier, never a sleep: the supervisor's warning in `child.log`
//! (written by the harness's subscriber), a lock's state, the workers' event
//! log. The one wait that is a duration is case (a)'s 40 s hold, which asks
//! whether a wait outlasts the 30 s between its warnings.
//!
//! The harness is the supervisor crate's `under_test`, included by path (see
//! `process_supervise.rs`).
#![cfg(feature = "blue-green-test")]

#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use std::path::Path;
use std::time::{Duration, Instant};

use open_ferry_supervisor::client::{self, Detection, StopOutcome};
use open_ferry_supervisor::endpoint::{Identity, Server, ServiceKind};
use open_ferry_supervisor::process::SupervisorLock;
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::install_lock::InstallLock;
use open_ferry_update::staging::hash_file;
use under_test::*;

/// What the supervisor says while it waits for each lock.
const UPDATE_WAIT: &str = "waiting for the update lock";
const INSTALL_WAIT: &str = "waiting for the install lock";

fn identity() -> Identity {
    Identity::current(ServiceKind::User).unwrap()
}

/// `client::detect` as the update CLI calls it, for `data` and the binary
/// `installed`.
fn detect(data: &Path, installed: &Path) -> Detection {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(client::detect(
            data,
            &identity(),
            installed,
            Instant::now() + Duration::from_secs(10),
        ))
}

/// A data directory that is not the fixture's, in a scratch directory of its
/// own: another user's, or another profile's.
fn other_data_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = scratch();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    (dir, data)
}

/// Waits, bounded, for the child to have logged `text`; the child exiting
/// first is a failure.
fn wait_logged(fx: &Fx, sup: &mut Sup, text: &str) {
    fx.wait_for(&format!("the child to log {text:?}"), || {
        if let Some(status) = sup.try_exit() {
            panic!("the supervisor exited early ({status})\n{}", fx.dump());
        }
        fx.child_log().contains(text).then_some(())
    });
}

/// Waits, bounded, until the supervisor serves a status with an active worker
/// and that worker has logged its own `active` event. The status lists a
/// worker from its handshake, and the worker writes the event only after it has
/// loaded (`on_started`, after its `Loaded` message), so the status alone
/// can come first.
fn wait_active(fx: &Fx, sup: &mut Sup) {
    fx.wait_for("the supervisor's active worker", || {
        if let Some(status) = sup.try_exit() {
            panic!("the supervisor exited early ({status})\n{}", fx.dump());
        }
        let status = fx.status()?;
        let served = status["state"] == "idle"
            && status["workers"]
                .as_array()
                .is_some_and(|w| w.iter().any(|w| w["role"] == "active"));
        (served && fx.events().iter().any(|event| event.event == "active")).then_some(())
    });
}

/// The supervisor has started no worker and published no endpoint.
#[track_caller]
fn assert_nothing_started(fx: &Fx) {
    let spawned = fx
        .driver_log()
        .iter()
        .filter(|line| line["k"] == "spawn")
        .count();
    assert_eq!(spawned, 0, "a worker was started\n{}", fx.dump());
    assert!(fx.events().is_empty(), "a worker ran\n{}", fx.dump());
    assert!(
        !fx.data().join("supervisor.json").exists(),
        "an endpoint was published\n{}",
        fx.dump()
    );
}

/// After the replacement, the supervisor ran the replaced binary: its active
/// worker is `B`'s, whose copy under `versions/` is the installed file's
/// bytes, and it was started after `released` (milliseconds since the epoch).
#[track_caller]
fn assert_adopted_the_replacement(fx: &Fx, released: u64) {
    assert_eq!(
        hash_file(&fx.kept(B)).unwrap(),
        hash_file(&fx.installed()).unwrap(),
        "the kept copy is not the installed file's bytes"
    );
    let active: Vec<Ev> = fx
        .events()
        .into_iter()
        .filter(|event| event.event == "active")
        .collect();
    assert!(!active.is_empty(), "no active worker\n{}", fx.dump());
    assert!(
        active.iter().all(|event| event.marker == "B"),
        "a worker ran the old binary: {active:?}\n{}",
        fx.dump()
    );
    let first = fx
        .driver_log()
        .into_iter()
        .find(|line| line["k"] == "spawn")
        .expect("a spawn line");
    let at = first["at"].as_u64().unwrap();
    assert!(
        at >= released,
        "the first worker started at {at}, before the release at {released}"
    );
}

/// The update CLI's switch, as far as this test needs: `B`'s bytes become the
/// installed file.
fn replace_installed(fx: &Fx) {
    write_executable(&fx.installed(), &artifact_bytes(B));
}

/// (a) A supervisor that starts first waits for the update lock and holds its
/// supervisor lock meanwhile: detection answers `Supervised`, for as long as
/// the wait lasts (past the 30 s between its warnings), and again, with the
/// supervisor's pid, once it runs.
#[test]
fn a_supervisor_waiting_for_the_update_lock_is_detected_as_supervised_for_40_seconds_and_after() {
    let fx = Fx::new();
    let update = DataDir::at(fx.data())
        .try_lock()
        .unwrap()
        .expect("the update lock is free");
    let mut sup = fx.spawn(&Tm::default());
    wait_logged(&fx, &mut sup, UPDATE_WAIT);

    let began = Instant::now();
    let installed_before = std::fs::read(fx.installed()).unwrap();
    loop {
        let detection = detect(&fx.data(), &fx.installed());
        let Detection::Supervised(running) = detection else {
            panic!("{detection:?}\n{}", fx.dump());
        };
        assert_eq!(running.pid, None, "no endpoint answers during the wait");
        assert!(sup.try_exit().is_none(), "the supervisor gave up waiting");
        assert_nothing_started(&fx);
        if began.elapsed() >= Duration::from_secs(40) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let warnings = fx.child_log().matches(UPDATE_WAIT).count();
    assert!(
        warnings >= 2,
        "the wait was not warned about again after 30 s ({warnings})\n{}",
        fx.dump()
    );

    drop(update);
    wait_active(&fx, &mut sup);
    let detection = detect(&fx.data(), &fx.installed());
    let Detection::Supervised(running) = detection else {
        panic!("{detection:?}\n{}", fx.dump());
    };
    assert_eq!(running.pid, Some(sup.pid()), "{}", fx.dump());
    assert_eq!(std::fs::read(fx.installed()).unwrap(), installed_before);
}

/// (b) The interleaving: detection clears the binary and keeps its lock; a
/// supervisor then starts and waits; the binary is replaced; once both locks
/// go, the supervisor runs the replaced bytes, and only then.
#[test]
fn a_supervisor_that_starts_after_detection_cleared_the_binary_runs_the_replaced_bytes() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let update = DataDir::at(fx.data())
        .try_lock()
        .unwrap()
        .expect("the update lock is free");
    let Detection::Clear(install) = detect(&fx.data(), &fx.installed()) else {
        panic!("the binary was not clear");
    };

    let mut sup = fx.spawn(&Tm::default());
    wait_logged(&fx, &mut sup, UPDATE_WAIT);
    assert_nothing_started(&fx);

    replace_installed(&fx);
    let released = now_ms();
    // The CLI lets the install lock go before the update lock.
    drop(install);
    drop(update);

    wait_active(&fx, &mut sup);
    assert_adopted_the_replacement(&fx, released);
}

/// (c) A supervisor with no endpoint can't be asked, but holds the binary's
/// install lock: another data directory's detection says the binary is busy,
/// naming the lock.
#[test]
fn a_supervisor_without_an_endpoint_makes_another_data_directorys_detection_busy() {
    let fx = Fx::new();
    // The endpoint's record can't be written: start-up drops the endpoint and
    // disables switching, and the worker runs on. (The real supervisor's
    // `-test-endpoint` flag would do the same at an unbindable location; this
    // fixture child takes its location from the harness, and a record that
    // can't be written fails the same way on every platform.)
    std::fs::create_dir(fx.data().join("supervisor.json")).unwrap();
    let mut sup = fx.spawn(&Tm::default());
    fx.wait_for("the active worker", || {
        if let Some(status) = sup.try_exit() {
            panic!("the supervisor exited early ({status})\n{}", fx.dump());
        }
        fx.events()
            .iter()
            .any(|event| event.event == "active")
            .then_some(())
    });
    assert!(
        fx.child_log().contains("could not write supervisor.json"),
        "the endpoint was not the failure\n{}",
        fx.dump()
    );

    let (_other, data_b) = other_data_dir();
    let detection = detect(&data_b, &fx.installed());
    let Detection::Busy(why) = detection else {
        panic!("{detection:?}\n{}", fx.dump());
    };
    let lock = InstallLock::path_for(&fx.installed()).unwrap();
    assert!(why.contains(&lock.display().to_string()), "{why}");
    assert!(sup.try_exit().is_none());
}

/// (d) Across data directories: another data directory's update holds the
/// binary's install lock, so a supervisor of this one waits for it, holding
/// this directory's locks in order, and runs the bytes that replace the
/// binary meanwhile.
#[test]
fn a_supervisor_waiting_for_another_data_directorys_install_runs_the_replaced_bytes() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let (_other, data_b) = other_data_dir();
    let update_b = DataDir::at(data_b.clone())
        .try_lock()
        .unwrap()
        .expect("the other update lock is free");
    let install = InstallLock::try_acquire(&fx.installed()).unwrap();

    let mut sup = fx.spawn(&Tm::default());
    wait_logged(&fx, &mut sup, INSTALL_WAIT);
    assert_nothing_started(&fx);
    // The order of the locks: this data directory's update lock is held by
    // the supervisor while it waits for the install lock.
    assert!(
        DataDir::at(fx.data()).try_lock().unwrap().is_none(),
        "the supervisor waits for the install lock without its update lock"
    );
    // And the other data directory's detection finds the binary taken.
    assert!(matches!(
        detect(&data_b, &fx.installed()),
        Detection::Busy(_) | Detection::Supervised(_)
    ));

    replace_installed(&fx);
    let released = now_ms();
    drop(install);
    drop(update_b);

    wait_active(&fx, &mut sup);
    assert_adopted_the_replacement(&fx, released);
}

/// (e) A stop that no supervisor answers with `Stopping` ends at its
/// deadline as `TimedOut`, and not before it.
#[test]
fn a_stop_that_is_never_answered_ends_at_its_deadline_as_timed_out() {
    let dir = scratch();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        // A supervisor that holds its lifetime lock and accepts every
        // connection, and never replies.
        let lock = SupervisorLock::try_acquire(&data).unwrap();
        let location = client_location(dir.path()).unwrap();
        let mut server = Server::bind(&location, &identity(), &lock).unwrap();
        let record = serde_json::json!({
            "pid": std::process::id(),
            "started": 1,
            "endpoint": location.to_record(),
            "version": "9.9.9",
            "protocol": 1,
        });
        std::fs::write(data.join("supervisor.json"), record.to_string()).unwrap();
        let held = tokio::spawn(async move {
            let mut connections = Vec::new();
            while let Ok(connection) = server.accept().await {
                connections.push(connection);
            }
        });

        let wait = Duration::from_secs(2);
        let began = Instant::now();
        let outcome = client::stop(&data, &identity(), began + wait).await;
        let took = began.elapsed();
        assert!(matches!(outcome, StopOutcome::TimedOut(_)), "{outcome:?}");
        assert!(took >= wait, "ended after {took:?}, before the deadline");
        assert!(
            took < wait + Duration::from_secs(8),
            "ended after {took:?}, long after the deadline"
        );
        held.abort();
        drop(lock);
    });
}

/// (e) A stop while the supervisor waits for the update lock ends the wait:
/// it exits having taken and started nothing.
#[test]
fn a_stop_during_the_update_lock_wait_ends_the_supervisor_with_nothing_taken() {
    let fx = Fx::new();
    let update = DataDir::at(fx.data())
        .try_lock()
        .unwrap()
        .expect("the update lock is free");
    let mut sup = fx.spawn(&Tm::default());
    wait_logged(&fx, &mut sup, UPDATE_WAIT);

    fx.request_stop();
    let status = fx.wait_exit(&mut sup);
    assert!(status.success(), "{status}\n{}", fx.dump());
    assert_eq!(fx.child_exit().as_deref(), Some("Ok(Stopped)"));
    assert_nothing_started(&fx);
    drop(SupervisorLock::try_acquire(&fx.data()).unwrap());
    drop(InstallLock::try_acquire(&fx.installed()).unwrap());
    drop(update);
}

/// (e) A stop while the supervisor waits for the install lock ends the wait,
/// and lets go of the locks it held while it waited.
#[test]
fn a_stop_during_the_install_lock_wait_ends_the_supervisor_and_frees_its_locks() {
    let fx = Fx::new();
    let install = InstallLock::try_acquire(&fx.installed()).unwrap();
    let mut sup = fx.spawn(&Tm::default());
    wait_logged(&fx, &mut sup, INSTALL_WAIT);

    fx.request_stop();
    let status = fx.wait_exit(&mut sup);
    assert!(status.success(), "{status}\n{}", fx.dump());
    assert_eq!(fx.child_exit().as_deref(), Some("Ok(Stopped)"));
    assert_nothing_started(&fx);
    drop(SupervisorLock::try_acquire(&fx.data()).unwrap());
    assert!(
        DataDir::at(fx.data()).try_lock().unwrap().is_some(),
        "the stopped supervisor still holds the update lock"
    );
    drop(install);
}
