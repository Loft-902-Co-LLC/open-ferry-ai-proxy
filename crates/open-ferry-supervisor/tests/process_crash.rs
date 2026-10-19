//! Not upstream's: design v3 13 item 10. The supervisor runs in a child
//! process and is held at a journal write (`journal.before|after:<name>`),
//! killed by its handle, and a second supervisor starts on the same data
//! directory. The recovery table of 3.3 decides what the second does; each case
//! asserts that action, that the outcome is replayed once (one history entry for the operation, the
//! older ones kept, and a third supervisor after a stop changes nothing), and that no worker
//! of the killed supervisor survives.
#![cfg(feature = "blue-green-test")]

mod under_test;
use under_test::*;

/// How the switch under test goes before the crash.
#[derive(Clone, Copy)]
enum Run {
    /// A switch to B that works (the watch window is short).
    Good,
    /// B sends `load_failed`: the failure is an outcome (J7, state, J8).
    FailLoad,
    /// B exits inside the watch window: an automatic return (J-R1, J-R2).
    Return,
}

/// What the recovery must leave.
#[derive(Clone, Copy)]
struct Want {
    /// The version the second supervisor runs.
    runs: &'static str,
    /// The state record, if the switch leaves one.
    record: Option<Rec<'static>>,
    failed: &'static [&'static str],
}

const ENV: &str = "environment";
const NEW: &str = "new-version";

const fn rec(
    result: &'static str,
    phase: &'static str,
    blame: &'static str,
    returned_to: &'static str,
) -> Option<Rec<'static>> {
    Some(Rec {
        result,
        phase,
        blame,
        returned_to,
        confirmed: false,
    })
}

fn crash_at(seam: &str, run: Run, want: Want) {
    let fx = Fx::new();
    fx.stage(B, "B");
    // An older record, which every recovery must keep.
    let old = fx.seed_history();
    match run {
        Run::Good => {}
        Run::FailLoad => fx.fault("fail-load", "@B"),
        Run::Return => {
            fx.fault("exit-after", "go@B");
            // Keep confirmation pending until the exit opens the return.
            fx.hold("watch.before_confirm");
        }
    }
    fx.hold(seam);
    let mut first = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    let id = sw.accepted();
    fx.wait_arrived(seam);
    let before = fx.driver_log().iter().filter(|v| v["k"] == "spawn").count();
    // By its handle, held inside the write: nothing of its own runs afterwards.
    first.kill();
    if matches!(run, Run::Return) {
        fx.release("watch.before_confirm");
    }
    drop(sw);
    // The workers die with it (the job object, or EOF on their channels).
    let pids: Vec<u32> = (1..=before).filter_map(|n| fx.pid_of(n)).collect();
    fx.wait_for("the killed supervisor's workers to be gone", || {
        pids.iter().all(|pid| !alive(*pid)).then_some(())
    });
    // The same write must not hold the second supervisor, which hits it too.
    fx.release(seam);
    let second = fx.start(&Tm::default());
    fx.wait_settled();
    let log = fx.driver_log();
    let last = log
        .iter()
        .rev()
        .find(|v| v["k"] == "spawn")
        .expect("a worker");
    assert_eq!(last["sha"], fx.sha(want.runs), "it runs {}", want.runs);
    assert!(alive(last["pid"].as_u64().unwrap() as u32));
    assert!(
        log.iter()
            .filter(|v| v["k"] == "spawn")
            .skip(before)
            .all(|v| v["sha"] == fx.sha(want.runs)),
        "the second supervisor starts only {}",
        want.runs
    );
    for pid in &pids {
        assert!(!alive(*pid), "worker {pid} of the killed supervisor runs");
    }
    let record_id = want.record.map(|_| id.as_str());
    match want.record {
        Some(record) => {
            let state = fx.state();
            let last = state.last_switch.as_ref().expect("a record");
            assert_eq!(last.id, id, "the record is the switch's own");
            fx.assert_columns(Columns {
                state: Some(StateCol::Record(record, want.failed)),
                installed: want.runs,
                ..Columns::default()
            });
        }
        None => {
            // No record of this switch: the state still ends in the older one.
            fx.assert_columns(Columns {
                state: Some(StateCol::Older(&old, want.failed)),
                installed: want.runs,
                ..Columns::default()
            });
        }
    }
    // Replayed once: one entry for the operation, the older one kept.
    fx.assert_history(record_id, &old);
    // And recovery is settled: stop the second supervisor and start a third.
    // It runs the same version and changes neither the state nor the journal.
    fx.assert_third_start_changes_nothing(second, want.runs, record_id, &old);
}

macro_rules! crash {
    ($name:ident, $seam:expr, $run:expr, $want:expr) => {
        #[test]
        fn $name() {
            crash_at($seam, $run, $want);
        }
    };
}

const FLOW_A: Want = Want {
    runs: A,
    record: None,
    failed: &[],
};

fn reverted(phase: &'static str) -> Want {
    Want {
        runs: A,
        record: rec("failed-to-load", phase, ENV, ""),
        failed: &[],
    }
}

fn reverted_in_window() -> Want {
    Want {
        runs: A,
        record: rec("failed-after-switch", "watching", ENV, A),
        failed: &[],
    }
}

// ---- the forward switch: J1 to J6 ---------------------------------------------------

crash!(j1_before, "journal.before:J1", Run::Good, FLOW_A);

#[test]
fn j1_after() {
    crash_at("journal.after:J1", Run::Good, reverted("loading"));
}
#[test]
fn j2_before() {
    crash_at("journal.before:J2", Run::Good, reverted("loading"));
}
#[test]
fn j2_after() {
    crash_at("journal.after:J2", Run::Good, reverted("handing-over"));
}
#[test]
fn j3_before() {
    crash_at("journal.before:J3", Run::Good, reverted("handing-over"));
}
#[test]
fn j3_after() {
    crash_at("journal.after:J3", Run::Good, reverted("taking-over"));
}
#[test]
fn j4_before() {
    crash_at("journal.before:J4", Run::Good, reverted("taking-over"));
}
#[test]
fn j4_after() {
    crash_at("journal.after:J4", Run::Good, reverted_in_window());
}
#[test]
fn j5_before() {
    crash_at("journal.before:J5", Run::Good, reverted_in_window());
}

/// The confirm is on disk: `confirmed` is B, so B runs, and the switch is not
/// reverted (3.3, `switch` null: run `confirmed`).
const CONFIRMED_B: Want = Want {
    runs: B,
    record: Some(Rec {
        result: "switched",
        phase: "",
        blame: "",
        returned_to: "",
        confirmed: true,
    }),
    failed: &[],
};

#[test]
fn j5_after() {
    crash_at("journal.after:J5", Run::Good, CONFIRMED_B);
}
#[test]
fn j6_before() {
    crash_at("journal.before:J6", Run::Good, CONFIRMED_B);
}
#[test]
fn j6_after() {
    crash_at("journal.after:J6", Run::Good, CONFIRMED_B);
}

// ---- a failed switch: its outcome (J7, the state write, J8) ----------------------------

/// Before J7 nothing says why B failed; recovery blames the environment.
#[test]
fn j7_before() {
    crash_at("journal.before:J7", Run::FailLoad, reverted("loading"));
}

/// J7 landed: the outcome is replayed (blame and `mark_failed` as the
/// supervisor decided), once.
const OUTCOME: Want = Want {
    runs: A,
    record: rec("failed-to-load", "loading", NEW, ""),
    failed: &[B],
};

#[test]
fn j7_after_before_the_state_write() {
    crash_at("journal.after:J7", Run::FailLoad, OUTCOME);
}
/// The state is written; the journal still has the outcome: replay is
/// idempotent by `id`.
#[test]
fn j8_before_after_the_state_write() {
    crash_at("journal.before:J8", Run::FailLoad, OUTCOME);
}
#[test]
fn j8_after() {
    crash_at("journal.after:J8", Run::FailLoad, OUTCOME);
}

// ---- an automatic return: J-R1, J-R2 ----------------------------------------------------

/// B has exited but nothing was decided: a window in progress reverts.
#[test]
fn j_r1_before() {
    crash_at("journal.before:J-R1", Run::Return, reverted_in_window());
}

/// The decision to return is journalled, so it is carried out and the outcome
/// has the blame the decision stored.
const RETURNED: Want = Want {
    runs: A,
    record: rec("failed-after-switch", "watching", NEW, A),
    failed: &[B],
};

#[test]
fn j_r1_after() {
    crash_at("journal.after:J-R1", Run::Return, RETURNED);
}
#[test]
fn j_r2_before() {
    crash_at("journal.before:J-R2", Run::Return, RETURNED);
}
/// J-R2 is down with the outcome set and the state not yet written.
#[test]
fn j_r2_after_before_the_state_write() {
    crash_at("journal.after:J-R2", Run::Return, RETURNED);
}
/// The state is written, the outcome not yet cleared.
#[test]
fn j8_before_after_the_state_write_of_a_return() {
    crash_at("journal.before:J8", Run::Return, RETURNED);
}
#[test]
fn j8_after_a_return() {
    crash_at("journal.after:J8", Run::Return, RETURNED);
}
