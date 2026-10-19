//! Not upstream's: design v3 6.1 g and 3.3, the return ladder with real worker
//! processes. When the fresh `from` of a failed switch exits before it reports
//! `loaded`, the supervisor gives its rung three tries, then moves to
//! `previous` (a kept copy), then to the installed file by hash. A worker that
//! exits before `loaded` did not start: it is a failed try of its rung, not a
//! restart of the same one. Every wait between tries is the restart backoff,
//! so a ladder that fails everywhere is slow, never a spin.
#![cfg(feature = "blue-green-test")]

mod under_test;
use under_test::*;

/// The gap the tests give the supervisor between two starts: the backoff is
/// fixed at 150 ms, and a start is never sooner than 100 ms after the last
/// (the margin is for the clock the driver log uses).
const BACKOFF_MS: u64 = 150;
const AT_LEAST_MS: u64 = 100;

fn timings() -> Tm {
    Tm {
        restart_first: BACKOFF_MS,
        restart_max: BACKOFF_MS,
        ..Tm::default()
    }
}

/// Which rung a spawn ran from, with its hash checked against what that rung
/// must hold: "B" and "A" are the kept copies, "installed" is the installed
/// path, which holds B's bytes after the first switch.
fn rung_of(fx: &Fx, spawn: &serde_json::Value) -> String {
    let binary = spawn["binary"].as_str().unwrap();
    let (name, sha) = if binary == fx.kept(B).display().to_string() {
        ("B", fx.sha(B))
    } else if binary == fx.kept(A).display().to_string() {
        ("A", fx.sha(A))
    } else if binary == fx.installed().display().to_string() {
        ("installed", fx.sha(B))
    } else {
        return format!("unknown:{binary}");
    };
    assert_eq!(spawn["sha"], sha, "a spawn from {name} ran other bytes");
    name.to_owned()
}

/// The spawns made after the failed switch to C (the first three are A, B and
/// C): the rung and the time of each.
fn returns(fx: &Fx) -> Vec<(String, u64)> {
    fx.driver_log()
        .iter()
        .filter(|v| v["k"] == "spawn" && v["n"].as_u64().unwrap() > 3)
        .map(|v| (rung_of(fx, v), v["at"].as_u64().unwrap()))
        .collect()
}

#[track_caller]
fn assert_spaced(fx: &Fx, spawns: &[(String, u64)]) {
    for pair in spawns.windows(2) {
        let gap = pair[1].1.saturating_sub(pair[0].1);
        assert!(
            gap >= AT_LEAST_MS,
            "{} was started {gap} ms after {} (the backoff is {BACKOFF_MS} ms): {spawns:?}\n{}",
            pair[1].0,
            pair[0].0,
            fx.dump()
        );
    }
}

/// Switches A to B and lets it settle, so B is the confirmed version, A the
/// `previous`, and the installed file holds B's bytes; then starts a switch to
/// C whose worker exits at `go` (row f), holds the decision to return at J-R1
/// and, once B and C are gone, runs `before_return`. Releases it and waits for
/// the switch's result.
fn failed_switch_to_c(fx: &Fx, before_return: impl FnOnce()) -> Sup {
    fx.stage(C, "C");
    // Two versions are kept that no state names: a pin each keeps start-up's
    // clean-up from removing them before the switches that use them.
    fx.pin(B);
    fx.pin(C);
    let sup = fx.start(&timings());
    let mut first = fx.update(B);
    first.accepted();
    assert_eq!(first.done().expect("a done result")["result"], "switched");
    fx.wait_settled();
    fx.wait_journal("the install", |j| j["install_pending"].is_null());
    fx.fault("exit-after", "go@C");
    // A->B has settled; only C's confirmation waits for its exit to be seen.
    fx.hold("watch.before_confirm");
    fx.hold("journal.before:J-R1");
    let mut second = fx.update(C);
    second.accepted();
    fx.wait_arrived("journal.before:J-R1");
    fx.release("watch.before_confirm");
    fx.wait_dead(2);
    fx.wait_dead(3);
    before_return();
    fx.release("journal.before:J-R1");
    second.done().expect("a done result");
    sup
}

/// The state after the failed switch to C, whichever rung ends up running:
/// 6.1 f/g, `failed-after-switch` with the new version to blame, C marked
/// failed. `returned_to` is what J-R2 wrote when the first start of the return
/// was made (B's kept copy); the rungs after it belong to the restart ladder,
/// which does not rewrite the record.
fn the_record() -> StateCol<'static> {
    StateCol::Record(
        Rec {
            result: "failed-after-switch",
            phase: "watching",
            blame: "new-version",
            returned_to: B,
            ..Rec::default()
        },
        &[C],
    )
}

/// The rungs of the starts made so far are exactly `expected`, in order.
#[track_caller]
fn assert_rungs(fx: &Fx, expected: &[&str]) -> Vec<(String, u64)> {
    let spawns = returns(fx);
    let rungs: Vec<&str> = spawns.iter().map(|(rung, _)| rung.as_str()).collect();
    assert_eq!(rungs, expected, "{spawns:?}\n{}", fx.dump());
    spawns
}

fn wait_for_returns(fx: &Fx, count: usize) {
    fx.wait_for(&format!("{count} starts after the failed switch"), || {
        (returns(fx).len() >= count).then_some(())
    });
}

/// Row g: the kept copy of `from` (B) never loads. It gets exactly three tries,
/// each one a failed start, and then `previous` (A) is tried and loads. The
/// installed file is never reached.
#[test]
fn row_g_the_kept_copy_gets_three_tries_then_previous() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = failed_switch_to_c(&fx, || fx.fault("fail-load", "@B"));
    // Checked as soon as they are made, so a ladder that never moves on fails
    // here and not by a timeout.
    wait_for_returns(&fx, 4);
    assert_rungs(&fx, &["B", "B", "B", "A"]);
    fx.wait_for("the worker from `previous` to run", || {
        fx.pid_of(7).filter(|pid| alive(*pid))
    });
    // Its own evidence that it loaded, and its try uncounted.
    fx.wait_loaded();
    fx.wait_settled();
    // And nothing was started after it.
    let spawns = assert_rungs(&fx, &["B", "B", "B", "A"]);
    assert_spaced(&fx, &spawns);
    fx.assert_columns(Columns {
        state: Some(the_record()),
        running: &[7],
        installed: B,
        ..Columns::default()
    });
}

/// The same, with the kept copy of `previous` failing to load as well: three
/// tries on B, three on A, and the installed file (B's bytes, at its own
/// path) is the rung that loads.
#[test]
fn row_g_previous_gets_three_tries_then_the_installed_file_by_hash() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // A's kept copy carries B's marker, so the one fault fails both copies.
    fx.stage(A, "B");
    let _sup = failed_switch_to_c(&fx, || fx.fault("fail-load", "@B"));
    let all = ["B", "B", "B", "A", "A", "A", "installed"];
    wait_for_returns(&fx, 7);
    assert_rungs(&fx, &all);
    fx.wait_for("the installed file to run", || {
        fx.pid_of(10).filter(|pid| alive(*pid))
    });
    fx.wait_loaded();
    fx.wait_settled();
    let spawns = assert_rungs(&fx, &all);
    assert_spaced(&fx, &spawns);
    fx.assert_columns(Columns {
        state: Some(the_record()),
        running: &[10],
        installed: B,
        ..Columns::default()
    });
}

/// Every rung fails: nine starts, three on each, then the supervisor is
/// `degraded`, and the next cycle starts again from the first rung only after
/// the backoff. It is never a spin. When the fault is gone the next cycle
/// loads.
#[test]
fn row_g_every_rung_failing_is_degraded_and_the_next_cycle_waits() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = failed_switch_to_c(&fx, || fx.fault("fail-load", ""));
    // Nine starts, and the first of the next cycle.
    wait_for_returns(&fx, 10);
    fx.wait_for("degraded", || {
        (fx.status()?["state"] == "degraded").then_some(())
    });
    let spawns = returns(&fx);
    let rungs: Vec<&str> = spawns[..9].iter().map(|(rung, _)| rung.as_str()).collect();
    assert_eq!(
        rungs,
        [
            "B",
            "B",
            "B",
            "A",
            "A",
            "A",
            "installed",
            "installed",
            "installed"
        ],
        "{spawns:?}\n{}",
        fx.dump()
    );
    assert_eq!(spawns[9].0, "B", "the next cycle starts at the first rung");
    assert_spaced(&fx, &spawns);
    // A pause of a few backoffs makes only a few more starts, not a flood.
    let before = returns(&fx).len();
    std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS * 6));
    let after = returns(&fx);
    assert!(
        after.len() - before <= 8,
        "{} starts in {} ms: {after:?}",
        after.len() - before,
        BACKOFF_MS * 6
    );
    assert_spaced(&fx, &after);
    // The fault goes. A worker whose handshake ended after that reads no
    // fault, so the first of those loads and stays.
    fx.clear_fault("fail-load");
    // The worker that stays is the newest one with its own `loaded` evidence
    // (no worker loaded while the fault was in, so none of the earlier starts
    // can be it) and its try uncounted; no timestamp picks it.
    fx.wait_loaded();
    let kept = fx
        .driver_log()
        .iter()
        .rev()
        .find(|v| v["k"] == "spawn")
        .unwrap()["n"]
        .as_u64()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS * 4));
    let last = fx
        .driver_log()
        .into_iter()
        .rev()
        .find(|v| v["k"] == "spawn")
        .unwrap();
    assert_eq!(last["n"].as_u64().unwrap(), kept, "{}", fx.dump());
    assert!(alive(last["pid"].as_u64().unwrap() as u32), "{}", fx.dump());
    fx.wait_for("the status to stop saying degraded", || {
        (fx.status()?["state"] != "degraded").then_some(())
    });
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(the_record()),
        installed: B,
        ..Columns::default()
    });
}
