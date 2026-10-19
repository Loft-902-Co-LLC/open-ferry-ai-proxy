//! Not upstream's: design v3 13 item 8, the failure branches of 6.1, with the
//! real supervisor, the real disk and journal, and fake worker processes. Each
//! test drives one row with fault kinds and barriers and then asserts the
//! table's four columns with `Fx::assert_columns`.
#![cfg(feature = "blue-green-test")]

mod under_test;
use under_test::*;

/// Runs a switch to `B` to its `done` and waits until the journal is settled.
fn switch_b(fx: &Fx) -> serde_json::Value {
    let mut sw = fx.update(B);
    sw.accepted();
    let result = sw.done().expect("a done result");
    fx.wait_settled();
    result
}

/// Worker `a` was sent `resume` only when worker `b` was no longer running: the
/// supervisor's log of what ran at that moment does not list it.
#[track_caller]
fn assert_resumed_after_gone(fx: &Fx, a: usize, b: usize) {
    let (a, b) = (fx.pid_of(a).unwrap(), fx.pid_of(b).unwrap());
    let log = fx.driver_log();
    let resume = log
        .iter()
        .find(|v| v["k"] == "cmd" && v["pid"] == a && v["cmd"] == "send:resume")
        .unwrap_or_else(|| panic!("worker {a} was never resumed\n{}", fx.dump()));
    assert!(
        !resume["alive"].as_array().unwrap().iter().any(|p| *p == b),
        "A was resumed while B ran: {resume}"
    );
}

// ---- row a: B does not load -------------------------------------------------

#[test]
fn row_a_b_sends_load_failed() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("fail-load", "@B");
    let _sup = fx.start(&Tm::default());
    let result = switch_b(&fx);
    assert_eq!(result["result"], "failed-to-load");
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "loading",
                blame: "new-version",
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(2),
        killed: &[2],
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_a_b_exits_before_it_loads() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("exit-after", "welcome@B");
    let _sup = fx.start(&Tm::default());
    switch_b(&fx);
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "loading",
                blame: "new-version",
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(2),
        gone: &[2],
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_a_b_never_greets_within_the_load_timeout() {
    if lacks(Cap::Probe) {
        return;
    }
    let fx = Fx::new();
    fx.stage_probe(B, "stall");
    let tm = Tm {
        load: 1_500,
        ..Tm::default()
    };
    let _sup = fx.start(&tm);
    // Start before sending: Accepted passes through the fixture's reader,
    // which can lag behind the supervisor's load clock. This interval must
    // contain the whole load budget, even when the reply is delivered late.
    let requested = std::time::Instant::now();
    let mut sw = fx.update(B);
    sw.accepted();
    let result = sw.done().expect("a done result");
    let elapsed = requested.elapsed();
    fx.wait_settled();
    assert!(
        [
            "the new version did not start within the load timeout",
            "the worker did not connect in time",
        ]
        .contains(&result["why"].as_str().unwrap_or_default()),
        "{result}"
    );
    // The load budget covers process startup too. Under load the deadline
    // can cancel the spawn before the probe reaches main and writes its pid.
    // A probe that did start must be gone; an absent pid is not a failed test
    // precondition when startup itself used the budget.
    let probe_gone = match std::fs::read_to_string(fx.root().join("cfg").join("pid")) {
        Ok(pid) => pid.trim().parse::<u32>().is_ok_and(|pid| pid != 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("could not read the probe's pid: {error}\n{}", fx.dump()),
    };
    if !probe_gone {
        assert!(
            elapsed >= std::time::Duration::from_millis(tm.load),
            "the probe left no complete pid, but the switch settled in {elapsed:?}, before its {} ms load budget\n{}",
            tm.load,
            fx.dump()
        );
    }
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "loading",
                blame: "new-version",
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(1),
        probe_gone,
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_a_a_missing_binary_is_the_environment() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = fx.start(&Tm::default());
    // The pin is checked and J1 is written first; the file goes after.
    fx.hold("journal.before:J1");
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("journal.before:J1");
    std::fs::remove_file(fx.data().join("versions").join(B).join(NAME)).unwrap();
    fx.release("journal.before:J1");
    sw.done().expect("a done result");
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "loading",
                blame: "environment",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(1),
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

// ---- row b: B is incompatible -----------------------------------------------

#[test]
fn row_b_b_needs_a_newer_supervisor() {
    if lacks(Cap::Probe) {
        return;
    }
    let fx = Fx::new();
    fx.stage_probe(B, "min2");
    let _sup = fx.start(&Tm::default());
    switch_b(&fx);
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "loading",
                blame: "environment",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(1),
        spawn_failed: Some(1),
        probe_gone: true,
        running: &[1],
        not_resumed: &[1],
        installed: A,
        ..Columns::default()
    });
    assert!(fx.state().last_switch.unwrap().restart_needed);
    assert_eq!(fx.status().unwrap()["needs_restart"], true);
}

/// Design 11.2: "State gains ... `needs_restart`", and 6.1 row b: `needs_restart`
/// is set against the supervisor's version. The supervisor sets it in the status
/// snapshot, on the switch record (`restart_needed`) and, in the same write as
/// the record, in the state file (statefile.rs `apply_locked_by`), stamped with
/// the version of the supervisor that decided it.
#[test]
fn row_b_the_state_file_records_needs_restart() {
    if lacks(Cap::Probe) {
        return;
    }
    let fx = Fx::new();
    fx.stage_probe(B, "min2");
    let _sup = fx.start(&Tm::default());
    switch_b(&fx);
    assert!(fx.state().needs_restart);
    // ... against the supervisor's own version (the fixture's host is A's).
    assert_eq!(fx.state().supervisor_version.as_deref(), Some(A));
}

// ---- row c: A cannot quiesce --------------------------------------------------

#[test]
fn row_c_a_does_not_quiesce_within_the_handover_budget() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("ignore-handover", "@A");
    let tm = Tm {
        handover: 1_500,
        ..Tm::default()
    };
    let _sup = fx.start(&tm);
    switch_b(&fx);
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "handing-over",
                blame: "environment",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        killed: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_c_a_cancel_while_a_is_quiescing_is_not_a_result() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // A stops inside its hand over, as when a refresh is in flight.
    fx.hold("handover.gate_closing");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("handover.gate_closing");
    let reply = fx.cancel();
    assert!(
        matches!(
            reply,
            open_ferry_supervisor::protocol::EndpointReply::Cancelled { .. }
        ),
        "{reply:?}"
    );
    let result = sw.done().expect("a done result");
    assert_eq!(result["result"], "cancelled", "{result}");
    fx.release("handover.gate_closing");
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        killed: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

// ---- row d: A exits during hand over -------------------------------------------

#[test]
fn row_d_a_exits_during_the_hand_over() {
    if lacks(Cap::SendFail) {
        return;
    }
    let fx = Fx::new();
    fx.stage(B, "B");
    // The fake's `exit-after=handed_over` exits after it sent `handed_over`, so
    // A is made to end before it can answer: its next send fails and it stops
    // (the fake-only `send-fail`), set while it is held inside the hand over.
    fx.hold("handover.gate_closing");
    fx.hold("journal.before:J-R1");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("handover.gate_closing");
    fx.fault("send-fail", "@A");
    fx.release("handover.gate_closing");
    // The decision to return is journalled before the fresh A starts: the
    // fault goes now, so the fresh A (also marked A) can send.
    fx.wait_arrived("journal.before:J-R1");
    fx.clear_fault("send-fail");
    fx.release("journal.before:J-R1");
    sw.done().expect("a done result");
    fx.wait_settled();
    // A went, B was not promoted, and a fresh A runs from versions/1.0.0.
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "handing-over",
                blame: "environment",
                returned_to: A,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(3),
        killed: &[2],
        gone: &[1],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
    let log = fx.driver_log();
    let fresh = log
        .iter()
        .find(|v| v["k"] == "spawn" && v["n"] == 3)
        .unwrap();
    assert_eq!(fresh["version"], A);
    assert_eq!(fresh["role"], "active");
    assert!(
        fx.logged("B", "loaded") && !fx.logged("B", "ready"),
        "B was not promoted"
    );
}

/// The other side of row d: A that exits only after it said `handed_over` has
/// handed over, so the switch goes on (nothing here is a failure).
#[test]
fn row_d_a_exit_after_handed_over_is_not_a_failure() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("exit-after", "handed_over@A");
    let _sup = fx.start(&Tm::default());
    let result = switch_b(&fx);
    assert_eq!(result["result"], "switched", "{result}");
    // The window confirms B, and the install and the record follow it. The
    // install may wait for a retry (A had just gone).
    fx.wait_journal("the install to land", |j| j["install_pending"].is_null());
    // Its record update rides in the journal until the state has it.
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                confirmed: true,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        gone: &[1],
        running: &[2],
        not_resumed: &[1],
        installed: B,
        ..Columns::default()
    });
}

// ---- row e: B fails to take over ------------------------------------------------

/// The columns every row e case shares: B released or gone, A resumed (it
/// still has its listener and gets the lease back), nothing replaced.
fn row_e(fx: &Fx, blame: &str, failed: &[&str]) {
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "taking-over",
                blame,
                ..Rec::default()
            },
            failed,
        )),
        spawns: Some(2),
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_e_b_reports_take_failed() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("fail-before-ready", "@B");
    let _sup = fx.start(&Tm::default());
    switch_b(&fx);
    // L9.1 and L9.16: after `take_failed` B's gate never opened, so there is
    // nothing to hand over; it is dismissed with `stop`, then A resumes.
    let sent = fx.sent_to(2);
    assert!(sent.contains(&"send:stop".to_owned()), "{sent:?}");
    assert!(!sent.contains(&"send:hand_over".to_owned()), "{sent:?}");
    fx.wait_dead(2);
    row_e(&fx, "new-version", &[B]);
}

#[test]
fn row_e_b_exits_before_ready() {
    if lacks(Cap::SendFail) {
        return;
    }
    let fx = Fx::new();
    fx.stage(B, "B");
    // The fake's `exit-after=ready` exits after it sent `ready`. B is made to
    // end inside its take over instead: its `ready` cannot be sent, so it stops.
    fx.hold("take_over.before_reload");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("take_over.before_reload");
    fx.fault("send-fail", "@B");
    fx.release("take_over.before_reload");
    sw.done().expect("a done result");
    fx.wait_settled();
    fx.wait_dead(2);
    // B was already gone, so it was sent nothing to hand over.
    assert!(!fx.sent_to(2).contains(&"send:hand_over".to_owned()));
    row_e(&fx, "new-version", &[B]);
}

#[test]
fn row_e_a_lease_conflict_is_the_environment() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = fx.start(&Tm::default());
    // A released the lease at `handed_over`; before B's `take_over` another
    // process takes it, so B's take over fails with a lease conflict.
    fx.hold("journal.before:J3");
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("journal.before:J3");
    let lease = fx.hold_lease();
    fx.release("journal.before:J3");
    // B is dismissed (`stop`) before A resumes; only then is the lease let go,
    // so that A gets it back.
    fx.wait_for("B to be dismissed", || {
        fx.sent_to(2)
            .contains(&"send:stop".to_owned())
            .then_some(())
    });
    drop(lease);
    sw.done().expect("a done result");
    fx.wait_settled();
    fx.wait_dead(2);
    // A lease conflict is the environment's, so B is not marked failed.
    row_e(&fx, "environment", &[]);
}

#[test]
fn row_e_b_is_not_ready_within_the_ready_timeout() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("slow-ready", "60000@B");
    let tm = Tm {
        ready: 1_500,
        ..Tm::default()
    };
    let _sup = fx.start(&tm);
    switch_b(&fx);
    fx.wait_dead(2);
    let log = fx.driver_log();
    let b = fx.pid_of(2).unwrap();
    let sent: Vec<&str> = log
        .iter()
        .filter(|v| v["k"] == "cmd" && v["pid"] == b)
        .map(|v| v["cmd"].as_str().unwrap())
        .collect();
    assert!(sent.contains(&"send:hand_over"), "{sent:?}");
    assert!(sent.contains(&"send:release"), "{sent:?}");
    row_e(&fx, "new-version", &[B]);
}

/// Rows e and k: B is not ready within the ready timeout, and then ignores the
/// clean-up hand over as well. It is killed by its handle and waited for, and
/// only then is A resumed (B's lease is the one A needs).
#[test]
fn row_e_k_b_not_ready_that_ignores_the_clean_up_is_killed_before_a_resumes() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("slow-ready", "60000@B");
    fx.fault("ignore-handover", "@B");
    let tm = Tm {
        ready: 1_500,
        handover: 1_500,
        ..Tm::default()
    };
    let _sup = fx.start(&tm);
    switch_b(&fx);
    assert!(fx.sent_to(2).contains(&"send:hand_over".to_owned()));
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "taking-over",
                blame: "new-version",
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(2),
        killed: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
    assert_resumed_after_gone(&fx, 1, 2);
}

// ---- row f: B exits inside the watch window -------------------------------------

#[test]
fn row_f_b_exits_inside_the_watch_window() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("exit-after", "go@B");
    // Exit handling stays live while confirmation is fenced off.
    fx.hold("watch.before_confirm");
    let _sup = fx.start(&Tm::default());
    let result = switch_b(&fx);
    fx.release("watch.before_confirm");
    eprintln!("{result}");
    // A may still be draining; it is left to finish and is not resumed. The
    // fresh A runs from versions/1.0.0, and the forward switch's own record
    // says it failed after the switch and went back to 1.0.0.
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "new-version",
                returned_to: A,
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(3),
        gone: &[2],
        running: &[3],
        not_resumed: &[1],
        installed: A,
        ..Columns::default()
    });
    let log = fx.driver_log();
    let fresh = log
        .iter()
        .find(|v| v["k"] == "spawn" && v["n"] == 3)
        .unwrap();
    assert_eq!(fresh["version"], A);
    assert_eq!(fresh["role"], "active");
    assert_eq!(fresh["sha"], fx.sha(A));
}

/// L9.19: a connection that never ends keeps A draining when B fails in its
/// window. A is not resumed (it handed over), the fresh `from` starts beside it,
/// and A goes on draining until its connection ends.
#[test]
fn row_f_a_still_draining_is_not_resumed_while_the_fresh_from_starts() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("exit-after", "go@B");
    // Exit handling stays live while confirmation is fenced off.
    fx.hold("watch.before_confirm");
    let tm = Tm {
        drain: 60_000,
        ..Tm::default()
    };
    let _sup = fx.start(&tm);
    let held = fx.hold_partial();
    fx.wait_live(1);
    switch_b(&fx);
    fx.release("watch.before_confirm");
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "new-version",
                returned_to: A,
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(3),
        gone: &[2],
        running: &[1, 3],
        not_resumed: &[1],
        installed: A,
        ..Columns::default()
    });
    let log = fx.driver_log();
    let fresh = log
        .iter()
        .find(|v| v["k"] == "spawn" && v["n"] == 3)
        .unwrap();
    assert_eq!(fresh["version"], A);
    assert_eq!(fresh["role"], "active");
    // A has not gone: its connection is still open. It ends with the connection.
    drop(held);
    fx.wait_dead(1);
}

// ---- row g: the fresh `from` does not start -------------------------------------

/// Row f, with `change` run while the decision to return is held at J-R1
/// (after B is dead and A is gone, before the fresh `from` is tried).
fn row_f_then(fx: &Fx, change: impl FnOnce()) -> Sup {
    fx.fault("exit-after", "go@B");
    // Exit handling stays live while confirmation is fenced off.
    fx.hold("watch.before_confirm");
    fx.hold("journal.before:J-R1");
    let sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("journal.before:J-R1");
    fx.release("watch.before_confirm");
    // A's binary is a running image on Windows until A has exited.
    fx.wait_dead(1);
    fx.wait_dead(2);
    change();
    fx.release("journal.before:J-R1");
    sw.done().expect("a done result");
    sup
}

fn flip_last_byte(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn row_g_the_fresh_from_fails_its_hash_check_and_the_installed_file_runs_by_hash() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let sha_a = fx.sha(A);
    // The kept copy of `from` is damaged; the installed file still has its bytes.
    let _sup = row_f_then(&fx, || fx.corrupt(A));
    fx.wait_for("the installed file to run", || {
        fx.pid_of(3).filter(|pid| alive(*pid))
    });
    fx.wait_settled();
    let log = fx.driver_log();
    let spawns: Vec<&serde_json::Value> = log.iter().filter(|v| v["k"] == "spawn").collect();
    // A, B, then one worker, started from the installed path, never the damaged copy.
    assert_eq!(spawns.len(), 3, "{spawns:#?}");
    assert_eq!(
        spawns[2]["binary"],
        fx.installed().display().to_string(),
        "{spawns:#?}"
    );
    assert_eq!(spawns[2]["sha"], sha_a, "run by its hash");
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "new-version",
                returned_to: A,
                ..Rec::default()
            },
            &[B],
        )),
        gone: &[1, 2],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
    // The status is a snapshot published every `status_every`: it may still
    // show the degraded state of the rung that failed its hash, so it is
    // waited for, not read once.
    fx.wait_for("the status to stop saying degraded", || {
        (fx.status()?["state"] != "degraded").then_some(())
    });
}

/// Design 6.1 g: when the fresh `from` of d or f cannot start, the supervisor is
/// `degraded` and "the outcome of d or f is already recorded"; row f (line 304) gives
/// the record: the forward switch's own record becomes `failed-after-switch`, blame
/// `new-version` (B exited), `mark_failed(to)` unless `how = rollback`. Nothing ran, so
/// there is no `returned_to` (it is added by J-R2 once a worker does, 6.1 f). 3.3's
/// `switch.return` row: the outcome is recorded with the blame and why stored in
/// `return`, and the decision stays journalled; 6.2: it is written before anything is
/// tried and a path is never trusted, only a hash. (This one test holds what was two:
/// the degraded case and its record.)
#[test]
fn row_g_the_outcome_is_recorded_even_when_nothing_starts() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // Both the kept copy and the installed file fail their hash: a path is
    // never trusted, only a hash.
    let _sup = row_f_then(&fx, || {
        fx.corrupt(A);
        flip_last_byte(&fx.installed());
    });
    fx.wait_for("degraded", || {
        (fx.status()?["state"] == "degraded").then_some(())
    });
    // The decision to return stays journalled, to be carried out when a
    // version can start.
    let journal = fx.journal().unwrap();
    assert_eq!(
        journal["switch"]["return"]["target"]["version"], A,
        "{journal}"
    );
    let log = fx.driver_log();
    assert_eq!(
        log.iter().filter(|v| v["k"] == "spawn").count(),
        2,
        "no worker may start from a file that fails its hash: {log:#?}"
    );
    // 6.1 row f: B exited, so it is the new version's fault and it is marked
    // failed; nothing started, so there is no `returned_to`. The file was damaged
    // on purpose, so its bytes are named, not A's.
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "new-version",
                returned_to: "",
                confirmed: false,
            },
            &[B],
        )),
        spawns: Some(2),
        gone: &[1, 2],
        installed_bytes: Some(&damaged_artifact(A)),
        journal: JournalState::Open,
        ..Columns::default()
    });
}

// ---- row h: the supervisor crashes ----------------------------------------------

#[test]
fn row_h_a_crash_inside_the_watch_window_reverts_to_from() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let mut first = fx.start(&Tm {
        watch: 60_000,
        ..Tm::default()
    });
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_journal("the watch window", |j| j["switch"]["phase"] == "watching");
    // Nothing runs inside the supervisor after this: no stop, no cleanup.
    first.kill();
    drop(sw);
    // Everything of it dies with it (the job object, or EOF on its channels).
    fx.wait_dead(1);
    fx.wait_dead(2);
    let _second = fx.start(&Tm::default());
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "environment",
                returned_to: A,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(3),
        gone: &[1, 2],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
    let log = fx.driver_log();
    let last = log.iter().rev().find(|v| v["k"] == "spawn").unwrap();
    assert_eq!(last["sha"], fx.sha(A), "the revert runs `from`");
}

// ---- row i: the install step fails ----------------------------------------------

/// The seam that fails the install. On Windows the install keeps an aside, and
/// `install.between_renames` fails after the installed file was moved aside, which
/// covers the put-back as well (design 3.9, install.rs). On Unix the install is one
/// rename and that seam is not reached, so it is `install.before_replace` (install.rs,
/// reached on every OS before anything is changed; `.fail` fails the install there and
/// leaves a `.failed` receipt). Row i's columns are the same either way.
const INSTALL_FAILS: &str = if cfg!(windows) {
    "install.between_renames"
} else {
    "install.before_replace"
};

#[test]
fn row_i_a_failed_install_stays_pending_and_is_retried() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fail(INSTALL_FAILS);
    let _sup = fx.start(&Tm {
        watch: 1_000,
        ..Tm::default()
    });
    let result = switch_b(&fx);
    assert_eq!(result["result"], "switched");
    // The record the confirm writes names the failed install.
    fx.wait_for("the failed install in the record", || {
        fx.state()
            .last_switch
            .and_then(|r| r.installed_binary)
            .filter(|text| text.starts_with("failed"))
    });
    assert!(
        fx.failed(INSTALL_FAILS) >= 1,
        "the install seam never fired"
    );
    // Status shows what is left to do: the attempts so far and the next try
    // (it is published every `status_every`, so it is waited for).
    let pending = fx.wait_for("the install in the status", || {
        let view = fx.status()?["install_pending"].clone();
        (view["attempts"].as_u64()? >= 1).then_some(view)
    });
    assert_eq!(pending["target"], B, "{pending}");
    assert!(!pending["next_try"].is_null(), "{pending}");
    // The switch stays switched and confirmed; the installed file is not B's.
    let journal = fx.journal().unwrap();
    assert!(!journal["install_pending"].is_null(), "{journal}");
    assert_eq!(journal["confirmed"]["version"], B);
    assert!(journal["switch"].is_null());
    assert_eq!(std::fs::read(fx.installed()).unwrap(), artifact_bytes(A));
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                confirmed: true,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        running: &[2],
        gone: &[1],
        installed: A,
        ..Columns::default()
    });
    // Once the fault is gone the timer retries it, with no new switch.
    fx.unfail(INSTALL_FAILS);
    fx.wait_journal("the install to be retried", |j| {
        j["install_pending"].is_null()
    });
    assert_eq!(std::fs::read(fx.installed()).unwrap(), artifact_bytes(B));
}

/// The pending install is also retried when a supervisor starts, with no
/// switch and no timer of the first one.
#[test]
fn row_i_a_pending_install_is_retried_at_the_next_supervisor_start() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fail(INSTALL_FAILS);
    let mut first = fx.start(&Tm {
        watch: 1_000,
        ..Tm::default()
    });
    switch_b(&fx);
    fx.wait_for("the failed install in the record", || {
        fx.state()
            .last_switch
            .and_then(|r| r.installed_binary)
            .filter(|text| text.starts_with("failed"))
    });
    assert_eq!(std::fs::read(fx.installed()).unwrap(), artifact_bytes(A));
    // The first supervisor ends; the fault goes; the second finds the pending
    // install in the journal and lands it at its start.
    fx.stop();
    fx.wait_exit(&mut first);
    fx.unfail(INSTALL_FAILS);
    // The second supervisor is held where start-up is about to run the pending
    // install. Nothing else can land it while it is held (the core's timer does not
    // run until start-up is over), so reaching the barrier with the install still
    // pending, and the install landing after the release, is start-up's own run.
    // Start-up writes the journal once the install is placed (`J6`, in
    // `execute`, after the placement and before the core is built), so that write's
    // `journal.after:J6` fence is held too: reaching it, with the install done and
    // journalled, is start-up's own landing, and no core timer can have run yet.
    fx.hold("startup.before_install");
    fx.hold("journal.after:J6");
    let mut second = fx.spawn(&Tm::default());
    fx.wait_arrived("startup.before_install");
    assert!(
        !fx.journal().unwrap()["install_pending"].is_null(),
        "the install had landed before start-up ran it"
    );
    assert_eq!(std::fs::read(fx.installed()).unwrap(), artifact_bytes(A));
    fx.release("startup.before_install");
    fx.wait_arrived("journal.after:J6");
    assert!(
        fx.journal().unwrap()["install_pending"].is_null(),
        "start-up's journal write did not clear the pending install: {}",
        fx.dump()
    );
    assert_eq!(
        std::fs::read(fx.installed()).unwrap(),
        artifact_bytes(B),
        "start-up did not land the install: {}",
        fx.dump()
    );
    fx.release("journal.after:J6");
    fx.wait_for("the second supervisor's active worker", || {
        if let Some(status) = second.try_exit() {
            panic!(
                "the second supervisor exited early ({status})
{}",
                fx.dump()
            );
        }
        let status = fx.status()?;
        status["workers"]
            .as_array()?
            .iter()
            .any(|w| w["role"] == "active")
            .then_some(())
    });
    assert_eq!(std::fs::read(fx.installed()).unwrap(), artifact_bytes(B));
    fx.wait_settled();
    let log = fx.driver_log();
    let last = log.iter().rev().find(|v| v["k"] == "spawn").unwrap();
    assert_eq!(last["sha"], fx.sha(B), "the second runs the confirmed B");
    // The record says the install is done now, and the columns agree.
    fx.wait_for("the record to say the install is done", || {
        fx.state()
            .last_switch
            .and_then(|r| r.installed_binary)
            .filter(|text| text == "replaced")
    });
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                confirmed: true,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(3),
        gone: &[1, 2],
        running: &[3],
        installed: B,
        ..Columns::default()
    });
}

// ---- row j: a manual rollback's target exits inside its window ---------------------

#[test]
fn row_j_the_rollback_target_exits_and_no_automatic_switch_follows() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = fx.start(&Tm {
        watch: 1_500,
        ..Tm::default()
    });
    let forward = switch_b(&fx);
    assert_eq!(forward["result"], "switched");
    fx.wait_journal("the forward install", |j| j["install_pending"].is_null());
    fx.wait_settled();
    // The rollback target is A again (worker 3); it exits as soon as it is live.
    fx.fault("exit-after", "go@A");
    // The forward watch has ended; fence only the rollback's confirmation.
    fx.hold("watch.before_confirm");
    let mut sw = fx.rollback(A);
    sw.accepted();
    sw.done().expect("a done result");
    fx.wait_settled();
    fx.release("watch.before_confirm");
    // The fresh worker of the version the rollback came from, and no more.
    fx.wait_for("the fresh B", || {
        (fx.status()?["workers"]
            .as_array()?
            .iter()
            .any(|w| w["role"] == "active")
            && fx.driver_log().iter().filter(|v| v["k"] == "spawn").count() == 4)
            .then_some(())
    });
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "new-version",
                returned_to: B,
                ..Rec::default()
            },
            // The rollback does not mark its target failed.
            &[],
        )),
        spawns: Some(4),
        gone: &[1, 2, 3],
        running: &[4],
        installed: B,
        ..Columns::default()
    });
    let log = fx.driver_log();
    let fourth = log.iter().filter(|v| v["k"] == "spawn").nth(3).unwrap();
    assert_eq!(fourth["sha"], fx.sha(B));
    let record = fx.state().last_switch.unwrap();
    assert_eq!(record.how, "rollback");
    // No oscillation: A went live once (the rollback's try), and no fifth
    // worker came. A bounded dwell (several status ticks and B's whole watch
    // window of 1.5 s) lets a later automatic attempt show; only a window this
    // long is shown, not "never".
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert_eq!(fx.count("A", "go"), 1, "{:?}", fx.events());
    assert_eq!(
        fx.driver_log().iter().filter(|v| v["k"] == "spawn").count(),
        4
    );
}

// ---- row m: a journal write fails (3.6) ---------------------------------------------

/// A switch whose journal write `point` always fails (all three tries).
fn row_m_early(point: &str, phase: &str, columns: impl FnOnce(&Fx) -> Columns<'static>) {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fail(&format!("journal.fail:{point}"));
    let _sup = fx.start(&Tm::default());
    let result = switch_b(&fx);
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["phase"], phase, "{result}");
    assert_eq!(result["blame"], "environment", "{result}");
    assert!(
        result["why"]
            .as_str()
            .is_some_and(|why| why.contains("journal")),
        "{result}"
    );
    assert!(fx.failed(&format!("journal.fail:{point}")) >= 3);
    fx.assert_columns(columns(&fx));
}

#[test]
fn row_m_j1_nothing_was_started() {
    row_m_early("J1", "loading", |_| Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "loading",
                blame: "environment",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(1),
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_m_j2_b_is_killed_and_a_was_not_told() {
    row_m_early("J2", "handing-over", |_| Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "handing-over",
                blame: "environment",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        killed: &[2],
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_m_j3_b_is_killed_and_a_resumes() {
    row_m_early("J3", "taking-over", |_| Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-to-load",
                phase: "taking-over",
                blame: "environment",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        killed: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn row_m_j4_rolls_forward_in_memory_and_the_retry_lands() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // The retry on the status tick is its own write, J-retry, and the worker
    // list's write (J-workers) carries the whole journal: both fail too, or
    // they would land the stale phase.
    for point in ["J4", "J-retry", "J-workers"] {
        fx.fail(&format!("journal.fail:{point}"));
    }
    let _sup = fx.start(&Tm {
        watch: 60_000,
        ..Tm::default()
    });
    let mut sw = fx.update(B);
    sw.accepted();
    // B is live and A has released: the switch rolls forward in the watch
    // window although the journal on disk still says `taking-over`.
    fx.wait_for("the failed J4 write", || {
        (fx.failed("journal.fail:J4") >= 3).then_some(())
    });
    fx.wait_for("B to be ready", || fx.logged("B", "go").then_some(()));
    let journal = fx.journal().unwrap();
    assert_eq!(journal["switch"]["phase"], "taking-over", "{journal}");
    assert_eq!(fx.status().unwrap()["journal"], "stale");
    fx.wait_for("A to drain", || fx.logged("A", "drained").then_some(()));
    // Not a failure: no failure was recorded and B still runs.
    assert!(
        fx.state()
            .last_switch
            .is_none_or(|r| r.result.is_none_or(|result| !result.starts_with("failed")))
    );
    assert!(alive(fx.pid_of(2).unwrap()));
    // The four columns while the journal is stale: B runs, A is gone, and the
    // installed file is still A's (the install follows the confirm, which is
    // a minute away). The journal keeps the switch.
    fx.wait_dead(1);
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        gone: &[1],
        running: &[2],
        installed: A,
        journal: JournalState::Open,
        ..Columns::default()
    });
    // The retry on the status tick lands once the write can.
    fx.unfail("journal.fail:J-retry");
    fx.unfail("journal.fail:J-workers");
    fx.wait_journal("the stale write to land", |j| {
        j["switch"]["phase"] == "watching"
    });
    // The status clears its `stale` flag just after the write lands, so wait
    // for it rather than read it once.
    fx.wait_for("the journal to stop being stale", || {
        (fx.status()?["journal"] != "stale").then_some(())
    });
}

#[test]
fn row_m_j_r1_the_return_is_acted_on_after_three_failed_writes() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("exit-after", "go@B");
    // Exit handling stays live while confirmation is fenced off.
    fx.hold("watch.before_confirm");
    fx.fail("journal.fail:J-R1");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    sw.done().expect("a done result");
    fx.wait_settled();
    fx.release("watch.before_confirm");
    assert!(fx.failed("journal.fail:J-R1") >= 3);
    // The worker that failed is replaced all the same: row f's result.
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "new-version",
                returned_to: A,
                ..Rec::default()
            },
            &[B],
        )),
        spawns: Some(3),
        gone: &[1, 2],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
}

// ---- row m, the rest: J5 to J8, J-R2 and the state writes S1 to S3 (3.6) ----------------

/// J5: the confirm cannot be written. B keeps running and is not installed
/// (the journal still says `watching`, so a restart now would revert), the
/// status says `stale`, and the retry lands the confirm and then the install.
#[test]
fn row_m_j5_b_keeps_running_uninstalled_and_the_retry_lands_the_confirm() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // The retry on the status tick and the workers write carry the whole
    // journal, so they fail too, or they would land the stale phase.
    let points = [
        "journal.fail:J5",
        "journal.fail:J-retry",
        "journal.fail:J-workers",
    ];
    for point in points {
        fx.fail(point);
    }
    let _sup = fx.start(&Tm {
        watch: 1_000,
        ..Tm::default()
    });
    let mut sw = fx.update(B);
    sw.accepted();
    sw.done().expect("a done result");
    fx.wait_for("the failed J5 write", || {
        (fx.failed("journal.fail:J5") >= 3).then_some(())
    });
    fx.wait_for("the journal to be stale", || {
        (fx.status()?["journal"] == "stale").then_some(())
    });
    let journal = fx.journal().unwrap();
    assert!(
        !journal["switch"].is_null(),
        "still watching on disk: {journal}"
    );
    assert_eq!(journal["confirmed"]["version"], A, "{journal}");
    assert!(alive(fx.pid_of(2).unwrap()), "B keeps running");
    assert_eq!(std::fs::read(fx.installed()).unwrap(), artifact_bytes(A));
    // The write can land again.
    for point in points {
        fx.unfail(point);
    }
    fx.wait_journal("the confirm to land", |j| {
        j["switch"].is_null() && j["confirmed"]["version"] == B
    });
    fx.wait_journal("the install to land", |j| j["install_pending"].is_null());
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                confirmed: true,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(2),
        gone: &[1],
        running: &[2],
        installed: B,
        ..Columns::default()
    });
}

/// J6: the install result cannot be written. The install itself is done (the
/// file is B's), the journal on disk still says it is pending, and the next
/// start sees the file's hash equal to the target and finishes the record.
#[test]
fn row_m_j6_the_install_is_done_and_the_next_start_finishes_the_record() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let old = fx.seed_history();
    // J8 (the outcome cleared) writes the whole journal after J6 and would
    // save `installed = B` and clear `install_pending`; the retry and the
    // workers write do the same. Every write after J6 fails, so the journal on
    // disk is the one J6 left behind.
    let points = [
        "journal.fail:J6",
        "journal.fail:J8",
        "journal.fail:J-retry",
        "journal.fail:J-workers",
    ];
    for point in points {
        fx.fail(point);
    }
    let mut first = fx.start(&Tm {
        watch: 1_000,
        ..Tm::default()
    });
    let mut sw = fx.update(B);
    let id = sw.accepted();
    sw.done().expect("a done result");
    fx.wait_for("the file to be B's", || {
        (std::fs::read(fx.installed()).ok()? == artifact_bytes(B)).then_some(())
    });
    // The writes follow the rename, so they are waited for, not assumed: J6
    // used all its tries, and J8 (the last write of the flow) was tried.
    fx.wait_for("the J6 write to fail", || {
        (fx.failed("journal.fail:J6") >= 3).then_some(())
    });
    fx.wait_for("the J8 write to fail", || {
        (fx.failed("journal.fail:J8") >= 3).then_some(())
    });
    // The journal on disk, exactly: the install is still pending, nothing
    // says it is done (`installed` is A), and B is the confirmed version.
    let journal = fx.journal().unwrap();
    assert!(journal["switch"].is_null(), "{journal}");
    assert_eq!(
        journal["install_pending"]["target"]["version"], B,
        "{journal}"
    );
    assert_eq!(journal["installed"]["version"], A, "{journal}");
    assert_eq!(journal["confirmed"]["version"], B, "{journal}");
    // Killed outright, right now: nothing of its own runs afterwards, so the
    // next start must find the install done by the file's hash alone (3.9,
    // "hash = new").
    first.kill();
    drop(sw);
    let pids: Vec<u32> = (1..=2).filter_map(|n| fx.pid_of(n)).collect();
    fx.wait_for("the killed supervisor's workers to be gone", || {
        pids.iter().all(|pid| !alive(*pid)).then_some(())
    });
    for point in points {
        fx.unfail(point);
    }
    let before = file_identity(&fx.installed());
    let second = fx.start(&Tm::default());
    fx.wait_journal("the record to be finished", |j| {
        j["install_pending"].is_null() && j["installed"]["version"] == B
    });
    fx.wait_settled();
    // Done by the hash, not by a second copy: the file is the same one.
    assert_eq!(
        file_identity(&fx.installed()),
        before,
        "the second supervisor replaced a file that already held B"
    );
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "switched",
                confirmed: true,
                ..Rec::default()
            },
            &[],
        )),
        installed: B,
        ..Columns::default()
    });
    fx.assert_history(Some(&id), &old);
    fx.assert_third_start_changes_nothing(second, B, Some(&id), &old);
}

/// Which file this is: the inode and write time on Unix, the volume serial
/// number and file index on Windows (times alone do not identify a file: a
/// copy can keep them). A replaced file differs; the same file does not. On
/// Windows the file is opened fresh for each reading and closed before it
/// returns, so no handle is held across the recovery.
///
/// The limit: this detects a copy that replaces the file (a new file object,
/// so a new inode or file index). It does not detect an in-place overwrite
/// that keeps the file object, which leaves the identity the same (on Unix the
/// write time changes, on Windows nothing here does). The test's byte check
/// of the installed file stands beside it for that.
fn file_identity(path: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap();
        format!("{} {} {}", meta.ino(), meta.mtime(), meta.mtime_nsec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let file = std::fs::File::open(path).unwrap();
        // SAFETY: the handle is open for the call and the struct is plain data.
        let info = unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            assert!(
                GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) != 0,
                "GetFileInformationByHandle failed: {}",
                std::io::Error::last_os_error()
            );
            info
        };
        format!(
            "{:08x} {:08x}{:08x}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        )
    }
}

/// How the switch of a late-write case goes.
#[derive(Clone, Copy)]
enum Flow {
    /// B sends `load_failed`.
    FailLoad,
    /// B exits inside the watch window.
    Return,
    /// A switch that works.
    Good,
}

/// A switch with the writes in `points` failing, to the end of what the first
/// supervisor can do; the first stops; the seams go; a second starts. The
/// state ends with the operation's record once and the older one kept
/// (3.6: the change stays in the journal or in memory, and is replayed by
/// `id`). `lands_first` says the state has the record before the restart (a
/// write that failed in the journal goes to the state from memory).
fn late_write(
    points: &[&str],
    flow: Flow,
    lands_first: bool,
    record: Rec<'static>,
    failed: &'static [&'static str],
    installed: &'static str,
) {
    let fx = Fx::new();
    fx.stage(B, "B");
    let old = fx.seed_history();
    match flow {
        Flow::FailLoad => fx.fault("fail-load", "@B"),
        Flow::Return => {
            fx.fault("exit-after", "go@B");
            fx.hold("watch.before_confirm");
        }
        Flow::Good => {}
    }
    for point in points {
        fx.fail(point);
    }
    let mut first = fx.start(&Tm {
        watch: 1_000,
        ..Tm::default()
    });
    let mut sw = fx.update(B);
    let id = sw.accepted();
    sw.done().expect("a done result");
    fx.wait_for("a failed write", || {
        points.iter().any(|p| fx.failed(p) >= 1).then_some(())
    });
    match flow {
        Flow::Good => fx.wait_for("the confirm", || {
            (fx.journal()?["confirmed"]["version"] == B).then_some(())
        }),
        Flow::Return => {
            fx.wait_for("the fresh from", || fx.pid_of(3).filter(|p| alive(*p)));
            fx.release("watch.before_confirm");
        }
        Flow::FailLoad => fx.wait_dead(2),
    };
    if lands_first {
        fx.wait_for("the state record before any restart", || {
            fx.state()
                .last_switch
                .filter(|r| r.id == id && r.result.is_some())
        });
    }
    drop(sw);
    fx.stop();
    fx.wait_exit(&mut first);
    for point in points {
        fx.unfail(point);
    }
    let second = fx.start(&Tm::default());
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(record, failed)),
        installed,
        ..Columns::default()
    });
    fx.assert_history(Some(&id), &old);
    // A third start finds nothing left to replay: the record is there once.
    fx.assert_third_start_changes_nothing(second, installed, Some(&id), &old);
}

const LOAD_FAILED: Rec<'static> = Rec {
    result: "failed-to-load",
    phase: "loading",
    blame: "new-version",
    returned_to: "",
    confirmed: false,
};

const SWITCHED: Rec<'static> = Rec {
    result: "switched",
    phase: "",
    blame: "",
    returned_to: "",
    confirmed: true,
};

/// J7: the outcome cannot be put in the journal; the state is written from
/// memory, directly.
#[test]
fn row_m_j7_the_outcome_goes_to_the_state_from_memory() {
    late_write(
        &["journal.fail:J7"],
        Flow::FailLoad,
        true,
        LOAD_FAILED,
        &[B],
        A,
    );
}

/// J8: the state has the outcome, the journal still holds it; replay is
/// idempotent by `id`.
#[test]
fn row_m_j8_the_outcome_stays_in_the_journal_and_replays_once() {
    late_write(
        &["journal.fail:J8"],
        Flow::FailLoad,
        true,
        LOAD_FAILED,
        &[B],
        A,
    );
}

/// J-R2: the return's outcome cannot be put in the journal.
#[test]
fn row_m_j_r2_the_return_outcome_is_written_and_replayed_once() {
    late_write(
        &["journal.fail:J-R2"],
        Flow::Return,
        true,
        Rec {
            result: "failed-after-switch",
            phase: "watching",
            blame: "new-version",
            returned_to: A,
            confirmed: false,
        },
        &[B],
        A,
    );
}

/// S1: the state upsert at `ready` fails; the record is in memory and lands
/// later (at the confirm, or at the next start). The confirmation is held back
/// by the watch window, and A's drain by a request that never ends (a finished
/// drain would patch the record into the state without S1). While S1 fails the
/// status says the state write is pending and the state lacks the record; once
/// the write can land, the ready-time fields (`gap_ms` included) are the ones
/// that were in memory.
#[test]
fn row_m_s1_the_upsert_at_ready_is_replayed_once() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let old = fx.seed_history();
    fx.fail("state.fail:S1");
    let _sup = fx.start(&Tm {
        watch: 6_000,
        drain: 30_000,
        ..Tm::default()
    });
    let held_request = fx.hold_partial();
    fx.wait_live(1);
    let mut sw = fx.update(B);
    let id = sw.accepted();
    sw.done().expect("a done result");
    fx.wait_for("the failed S1 write", || {
        (fx.failed("state.fail:S1") >= 1).then_some(())
    });
    // Inside the watch window, before the confirm: the write is pending in the
    // status, and the state has not got this operation's record.
    fx.wait_for("the pending state write in the status", || {
        let status = fx.status()?;
        (status["state"] == "watching"
            && status["state_write"] == "pending"
            && status["state_write_pending"] == true)
            .then_some(())
    });
    let kept = fx.state().last_switch.expect("the older record");
    assert_ne!(kept.id, id, "the record landed while S1 was failing");
    assert!(
        fx.journal().unwrap()["switch"].is_object(),
        "the window is over: {}",
        fx.dump()
    );
    fx.unfail("state.fail:S1");
    let record = fx.wait_for("the record to land", || {
        fx.state().last_switch.filter(|r| r.id == id)
    });
    assert!(
        record.gap_ms.is_some(),
        "the ready-time gap was lost: {record:?}"
    );
    assert_eq!(record.result.as_deref(), Some("switched"), "{record:?}");
    drop(held_request);
    fx.wait_settled();
    fx.wait_journal("the install", |j| j["install_pending"].is_null());
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(SWITCHED, &[])),
        spawns: Some(2),
        gone: &[1],
        running: &[2],
        installed: B,
        ..Columns::default()
    });
    let last = fx.state().last_switch.expect("the record");
    assert_eq!(last.gap_ms, record.gap_ms, "the gap changed: {last:?}");
    fx.assert_history(Some(&id), &old);
}

/// S1 across a hard kill: the failed S1 payload lives only in memory (design
/// 3.6, line 164: "the change stays in the journal `outcome` (or in memory for
/// S1)"), and A's drain is held open so that no other write (the drain's S2)
/// can carry it to the state. The supervisor is killed inside the watch window
/// with the state still lacking the record. The memory is lost, so nothing is
/// replayed: the next start finds the journal in `watching` and reverts, by
/// design 3.3 line 136, recording `failed-after-switch`, `returned_to = from`,
/// blame `environment`, and creating the record by `id`, once. It has no
/// `gap_ms`: that lived only in the memory that died.
#[test]
fn row_m_s1_a_hard_kill_loses_the_memory_and_the_next_start_records_the_revert() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let old = fx.seed_history();
    fx.fail("state.fail:S1");
    let mut first = fx.start(&Tm {
        watch: 60_000,
        drain: 30_000,
        ..Tm::default()
    });
    let held_request = fx.hold_partial();
    fx.wait_live(1);
    let mut sw = fx.update(B);
    let id = sw.accepted();
    fx.wait_journal("the watch window", |j| j["switch"]["phase"] == "watching");
    fx.wait_for("the failed S1 write", || {
        (fx.failed("state.fail:S1") >= 1).then_some(())
    });
    // From disk, just before the kill: the state has no record for the id.
    let state = fx.state();
    let has = |state: &open_ferry_update::state::State| {
        state
            .last_switch
            .iter()
            .chain(state.history.iter())
            .any(|r| r.id == id)
    };
    assert!(
        !has(&state),
        "the record is already in the state: {state:?}"
    );
    first.kill();
    drop(sw);
    drop(held_request);
    fx.wait_dead(1);
    fx.wait_dead(2);
    fx.unfail("state.fail:S1");
    let second = fx.start(&Tm::default());
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::Record(
            Rec {
                result: "failed-after-switch",
                phase: "watching",
                blame: "environment",
                returned_to: A,
                ..Rec::default()
            },
            &[],
        )),
        spawns: Some(3),
        gone: &[1, 2],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
    let record = fx.state().last_switch.expect("the record");
    assert_eq!(record.id, id, "{record:?}");
    assert_eq!(
        record.gap_ms, None,
        "the gap was in memory only and cannot come back: {record:?}"
    );
    fx.assert_history(Some(&id), &old);
    fx.assert_third_start_changes_nothing(second, A, Some(&id), &old);
}

/// S2: the confirm's state write fails; the confirmed record stays in the
/// journal and the next start replays it.
#[test]
fn row_m_s2_the_confirmed_record_stays_in_the_journal_and_replays_once() {
    late_write(&["state.fail:S2"], Flow::Good, false, SWITCHED, &[], B);
}

/// S3: the failure's state write fails; the record and `mark_failed` stay in
/// the journal and the next start replays them.
#[test]
fn row_m_s3_the_failure_stays_in_the_journal_and_replays_once() {
    late_write(
        &["state.fail:S3"],
        Flow::FailLoad,
        false,
        LOAD_FAILED,
        &[B],
        A,
    );
}
