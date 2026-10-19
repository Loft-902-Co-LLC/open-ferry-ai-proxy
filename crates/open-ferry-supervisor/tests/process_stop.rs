//! Not upstream's: design v3 13 item 14, the supervisor's part. A supervisor
//! stop stops its workers gracefully within the stop budget, a draining or
//! handed-over worker included; a supervisor that is killed outright leaves no
//! worker (EOF on Unix, the job object on Windows).
#![cfg(feature = "blue-green-test")]

mod under_test;
use std::time::{Duration, Instant};
use under_test::*;

/// A graceful stop never needs a handle: no `kill` was logged for `n`.
#[track_caller]
fn assert_not_killed(fx: &Fx, n: usize) {
    let sent = fx.sent_to(n);
    assert!(!sent.contains(&"kill".to_owned()), "worker {n}: {sent:?}");
}

#[test]
fn a_stop_ends_the_worker_gracefully_within_the_stop_budget() {
    let fx = Fx::new();
    let tm = Tm::default();
    let mut sup = fx.start(&tm);
    let began = Instant::now();
    fx.stop();
    let status = fx.wait_exit(&mut sup);
    fx.wait_dead(1);
    assert!(status.success(), "the supervisor exited with {status}");
    assert!(
        began.elapsed() < Duration::from_millis(tm.stop),
        "the stop took {:?}",
        began.elapsed()
    );
    let sent = fx.sent_to(1);
    assert!(sent.contains(&"send:stop".to_owned()), "{sent:?}");
    assert_not_killed(&fx, 1);
    // Four columns: nothing was switched, so no record, and A's file stays.
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(1),
        gone: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn a_stop_during_a_switch_ends_the_handed_over_a_and_the_taking_over_b() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let tm = Tm::default();
    // A has handed over and waits to be released; B holds the lease and has
    // not yet answered ready.
    fx.hold("take_over.before_reload");
    let mut sup = fx.start(&tm);
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("take_over.before_reload");
    let began = Instant::now();
    fx.stop();
    let status = fx.wait_exit(&mut sup);
    fx.wait_dead(1);
    fx.wait_dead(2);
    assert!(status.success(), "the supervisor exited with {status}");
    assert!(
        began.elapsed() < Duration::from_millis(tm.stop),
        "the stop took {:?}",
        began.elapsed()
    );
    assert_not_killed(&fx, 1);
    assert_not_killed(&fx, 2);
    drop(sw);
    // The switch was open when the supervisor stopped: nothing is installed or
    // recorded, and the journal keeps the switch for the next start (3.3).
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        gone: &[1, 2],
        installed: A,
        journal: JournalState::Open,
        ..Columns::default()
    });
}

#[test]
fn a_killed_supervisor_leaves_no_worker() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // Two workers are live: A, and B standing by.
    fx.hold("take_over.before_reload");
    let mut sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("take_over.before_reload");
    let a = fx.pid_of(1).unwrap();
    let b = fx.pid_of(2).unwrap();
    assert!(alive(a) && alive(b));
    // By its handle, with nothing run inside it: no stop, no cleanup.
    sup.kill();
    drop(sw);
    fx.wait_for("both workers to be gone", || {
        (!alive(a) && !alive(b)).then_some(())
    });
    // Nothing of the supervisor's own ran, so the switch is still open.
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        gone: &[1, 2],
        installed: A,
        journal: JournalState::Open,
        ..Columns::default()
    });
}

/// Owns a handle to the grandchild and ends it, and waits for it, when it drops:
/// a test whose assertion fails (a job object that did not take it) must not leave
/// a process that lives about ten minutes. It acts only on the drop, after the
/// assertion that tests the behaviour has run, so it rescues nothing.
#[cfg(windows)]
struct Grandchild(Option<open_ferry_supervisor::process::windows::ProcessHandle>);

#[cfg(windows)]
impl Drop for Grandchild {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.terminate(1);
            let _ = handle.wait_timeout(Duration::from_secs(10));
        }
    }
}

/// Design 13 item 14, the Windows part: the job object "includes a grandchild".
/// The fake worker starts one process of its own when it becomes active
/// (`spawn-grandchild`), and on Windows that process has no way to notice its
/// parent is gone, so only the job ends it. Returns the grandchild's pid, with
/// both it and A running, and the guard that ends it if the test fails (made
/// before any assertion that can panic).
#[cfg(windows)]
fn start_with_a_grandchild(fx: &Fx) -> (Sup, u32, u32, Grandchild) {
    fx.fault("spawn-grandchild", "");
    let sup = fx.start(&Tm::default());
    let a = fx.pid_of(1).expect("A's pid");
    let grandchild = fx.wait_for("A's grandchild", || fx.grandchildren().first().copied());
    let guard = Grandchild(
        open_ferry_supervisor::process::windows::ProcessHandle::open_pid(grandchild)
            .ok()
            .flatten(),
    );
    assert!(alive(a), "A is not running");
    assert!(alive(grandchild), "the grandchild is not running");
    assert_ne!(a, grandchild);
    (sup, a, grandchild, guard)
}

/// By its handle, with nothing run inside it: no stop, no cleanup. The job
/// object, closed with the supervisor's process, ends the worker and the
/// grandchild.
#[cfg(windows)]
#[test]
fn a_killed_supervisor_leaves_no_grandchild() {
    if lacks(Cap::SpawnGrandchild) {
        return;
    }
    let fx = Fx::new();
    let (mut sup, a, grandchild, _guard) = start_with_a_grandchild(&fx);
    sup.kill();
    fx.wait_for("the worker and its grandchild to be gone", || {
        (!alive(a) && !alive(grandchild)).then_some(())
    });
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(1),
        gone: &[1],
        installed: A,
        ..Columns::default()
    });
}

/// A graceful stop ends the worker by its own `stop`; the grandchild is not
/// the worker's to end, so it is the job that takes it when the supervisor's
/// process exits.
#[cfg(windows)]
#[test]
fn a_stopped_supervisor_leaves_no_grandchild() {
    if lacks(Cap::SpawnGrandchild) {
        return;
    }
    let fx = Fx::new();
    let (mut sup, a, grandchild, _guard) = start_with_a_grandchild(&fx);
    fx.stop();
    let status = fx.wait_exit(&mut sup);
    assert!(status.success(), "the supervisor exited with {status}");
    fx.wait_for("the worker and its grandchild to be gone", || {
        (!alive(a) && !alive(grandchild)).then_some(())
    });
    assert_not_killed(&fx, 1);
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(1),
        gone: &[1],
        installed: A,
        ..Columns::default()
    });
}

/// The guard may signal the supervisor's process group only while the
/// supervisor is unreaped: its zombie keeps the pid, and so the group id, from
/// being taken by an unrelated process. `kill` and `try_exit` therefore look at
/// the exit without reaping it, and only the guard's drop reaps.
#[cfg(unix)]
#[test]
fn a_killed_supervisor_stays_a_zombie_until_the_guard_drops() {
    let fx = Fx::new();
    let mut sup = fx.start(&Tm::default());
    let pid = sup.pid() as libc::pid_t;
    let exists = || {
        // SAFETY: signal 0 only checks that the id exists.
        unsafe { libc::kill(pid, 0) == 0 }
    };
    sup.kill();
    assert!(exists(), "the killed supervisor was reaped by `kill`");
    assert!(sup.try_exit().is_some(), "no exit status was seen");
    assert!(exists(), "the killed supervisor was reaped by `try_exit`");
    assert!(!alive(pid as u32), "the zombie was reported as running");
    drop(sup);
    assert!(!exists(), "the guard did not reap the supervisor");
}

/// L9.19: a worker whose drain is held open by a connection that never ends
/// is stopped with the rest. The whole stop is within the stop budget, and
/// neither worker needs its handle.
#[test]
fn a_stop_with_a_draining_worker_ends_everything_within_the_stop_budget() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let tm = Tm {
        watch: 60_000,
        ..Tm::default()
    };
    let mut sup = fx.start(&tm);
    // A request that never ends, accepted by A before the switch.
    let _held = fx.hold_partial();
    fx.wait_live(1);
    let mut sw = fx.update(B);
    sw.accepted();
    let result = sw.done().expect("a done result");
    assert_eq!(result["result"], "switched", "{result}");
    fx.wait_for("A to drain with the held connection", || {
        let status = fx.status()?;
        (status["drain"]["live"].as_u64()? >= 1).then_some(())
    });
    let began = Instant::now();
    fx.stop();
    let status = fx.wait_exit(&mut sup);
    fx.wait_dead(1);
    fx.wait_dead(2);
    let took = began.elapsed();
    assert!(status.success(), "the supervisor exited with {status}");
    assert!(
        took < Duration::from_millis(tm.stop),
        "the stop took {took:?}, the budget is {} ms",
        tm.stop
    );
    assert_not_killed(&fx, 1);
    assert_not_killed(&fx, 2);
    // B's window was open: the install is still A's, the journal keeps the
    // switch, and the state has B's record, unconfirmed.
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        gone: &[1, 2],
        installed: A,
        journal: JournalState::Open,
        ..Columns::default()
    });
}
