//! Not upstream's: design v3 3.3 and 6.1, a return that recovery carries on.
//! The supervisor is killed with a return open (J-R1 written, nothing after),
//! files on the disk are damaged, and a second supervisor recovers. The rung
//! recovery chooses must stay on the return's ladder: when it fails, the next
//! one tried is that rung again (its remaining tries) and then the installed
//! file, never the rung that already failed verification.
#![cfg(feature = "blue-green-test")]

mod under_test;
use under_test::*;

const BACKOFF_MS: u64 = 800;

fn timings() -> Tm {
    Tm {
        restart_first: BACKOFF_MS,
        restart_max: BACKOFF_MS,
        ..Tm::default()
    }
}

/// Switches A to B (settled: B is confirmed, A the `previous`, and the
/// installed file holds B's bytes), starts a switch to C whose worker exits at
/// `go`, and kills the supervisor once the return to B is journalled (J-R1)
/// and nothing else is. Every worker is gone when this returns.
fn killed_with_a_return_open(fx: &Fx) {
    fx.stage(B, "B");
    fx.stage(C, "C");
    // Kept versions no state names: a pin each, or start-up's clean-up
    // removes them.
    fx.pin(B);
    fx.pin(C);
    let mut sup = fx.start(&timings());
    let mut first = fx.update(B);
    first.accepted();
    assert_eq!(first.done().expect("a done result")["result"], "switched");
    fx.wait_settled();
    fx.wait_journal("the install", |j| j["install_pending"].is_null());
    fx.fault("exit-after", "go@C");
    fx.hold("journal.after:J-R1");
    let mut second = fx.update(C);
    second.accepted();
    fx.wait_arrived("journal.after:J-R1");
    // By its handle, held inside the write: nothing of its own runs afterwards.
    sup.kill();
    drop(second);
    let spawned = fx.driver_log().iter().filter(|v| v["k"] == "spawn").count();
    for n in 1..=spawned {
        fx.wait_dead(n);
    }
    fx.release("journal.after:J-R1");
    fx.clear_fault("exit-after");
}

/// The starts the driver was asked for after `from` lines of its log: a
/// `(kind, version, binary)` per `spawn` or `spawn_failed`.
fn starts_after(fx: &Fx, from: usize) -> Vec<(String, String, String)> {
    fx.driver_log()[from..]
        .iter()
        .filter(|v| v["k"] == "spawn" || v["k"] == "spawn_failed")
        .map(|v| {
            (
                v["k"].as_str().unwrap().to_owned(),
                v["version"].as_str().unwrap().to_owned(),
                v["binary"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Recovery's choice of `previous` (A) stays on the return's ladder. The return
/// to B was open when the supervisor died; B's kept copy and the installed file
/// are damaged, so recovery journals A (`previous`) as the target. A's first
/// start-up spawn then fails once, before J-R2, so the return is still open:
/// recovery has verified A, the `journal.after:J-L` barrier holds the start-up
/// count, and A's kept copy is damaged there and put right in the backoff
/// after the driver refused it. The next start is A again, its second of three
/// tries, and not a new cycle from B, which would be asked for (and refused by
/// the hash) first.
#[test]
fn recovery_keeps_the_fallback_it_chose_when_that_rungs_first_start_fails() {
    let fx = Fx::new();
    killed_with_a_return_open(&fx);
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["switch"]["return"]["target"]["version"], B);
    assert_eq!(journal["previous"]["version"], A);
    // The return's target no longer verifies, and neither does the installed file.
    fx.corrupt(B);
    fx.corrupt_installed();
    fx.hold("journal.after:J-L");
    let before = fx.driver_log().len();
    let _sup = fx.spawn(&timings());
    fx.wait_arrived("journal.after:J-L");
    assert_eq!(
        fx.journal().expect("a journal")["switch"]["return"]["target"]["version"],
        A
    );
    // Recovery has verified A; its spawn is refused by the hash.
    fx.corrupt(A);
    fx.release("journal.after:J-L");
    fx.wait_for("the first start from A to fail", || {
        fx.driver_log()[before..]
            .iter()
            .any(|v| v["k"] == "spawn_failed")
            .then_some(())
    });
    // The restart backoff leaves time to put A right before the next try.
    fx.corrupt(A);
    fx.wait_for("A's second start to run", || {
        let log = fx.driver_log();
        let run = log[before..].iter().find(|v| v["k"] == "spawn")?;
        alive(run["pid"].as_u64()? as u32).then_some(())
    });
    fx.wait_settled();
    let kept_a = fx.kept(A).display().to_string();
    let starts = starts_after(&fx, before);
    assert_eq!(
        starts,
        [
            ("spawn_failed".to_owned(), A.to_owned(), kept_a.clone()),
            ("spawn".to_owned(), A.to_owned(), kept_a),
        ],
        "{}",
        fx.dump()
    );
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["confirmed"]["version"], A, "{journal}");
    assert!(journal["switch"].is_null(), "{journal}");
}

/// The durable variant. The supervisor dies after recovery journals A as the
/// target (`journal.after:J-L`, the start-up count that stands the return on
/// the rung it tries). On the next start A's kept copy fails the hash and B's
/// kept copy and the installed file verify again. B is the rung that failed
/// before, so it is not journalled again in this cycle: A gets its remaining
/// tries (each refused by the hash) and then the installed file runs, by hash,
/// as B's bytes.
#[test]
fn a_fallback_that_fails_verification_after_a_crash_goes_to_the_installed_file() {
    let fx = Fx::new();
    killed_with_a_return_open(&fx);
    fx.corrupt(B);
    fx.corrupt_installed();
    fx.hold("journal.after:J-L");
    let mut second = fx.spawn(&timings());
    fx.wait_arrived("journal.after:J-L");
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["switch"]["return"]["target"]["version"], A);
    // Nothing of A has been started: the supervisor dies before its spawn.
    second.kill();
    fx.release("journal.after:J-L");
    // B and the installed file are whole again; A's kept copy is not.
    fx.corrupt(B);
    fx.corrupt_installed();
    fx.corrupt(A);
    let before = fx.driver_log().len();
    let _third = fx.spawn(&timings());
    fx.wait_for("the installed file to run", || {
        let log = fx.driver_log();
        let run = log[before..].iter().find(|v| v["k"] == "spawn")?;
        alive(run["pid"].as_u64()? as u32).then_some(())
    });
    let kept_a = fx.kept(A).display().to_string();
    let installed = fx.installed().display().to_string();
    let starts = starts_after(&fx, before);
    assert_eq!(
        starts,
        [
            ("spawn_failed".to_owned(), A.to_owned(), kept_a.clone()),
            ("spawn_failed".to_owned(), A.to_owned(), kept_a),
            ("spawn".to_owned(), B.to_owned(), installed),
        ],
        "{}",
        fx.dump()
    );
    let run = fx
        .driver_log()
        .into_iter()
        .rev()
        .find(|v| v["k"] == "spawn")
        .unwrap();
    assert_eq!(run["sha"], fx.sha(B), "the installed file holds B's bytes");
    fx.wait_settled();
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["ladder"]["current"]["installed"], true, "{journal}");
    assert_eq!(journal["ladder"]["spent"][0]["artifact"]["version"], A);
}

/// L9 round 6 item 2. When recovery's choice is the installed rung, start-up
/// runs the installed path, not the kept copy that holds the same bytes. The
/// kept copies of B and A are mode 0600, so a start from either one fails; the
/// installed file holds B's bytes and is executable. A return walks down its
/// ladder (B, A, then the installed file) and the supervisor is killed while
/// the return stands on the installed rung (`return.installed` in the
/// journal). Recovery then chooses the installed rung by hash, and every start
/// of the second supervisor must be from the installed path: the first fails
/// (`exit-after welcome@0.0.0`, which only the installed file's marker
/// matches) and is charged to the installed rung, so the next try is that rung
/// again, not a new cycle at the kept copy.
///
/// `without_the_record` takes the ladder record out of the journal before the
/// second start, as a journal written before ladder records has none. With it
/// the record names the rung and start-up's count follows it, so only without
/// it is what recovery chose the rung that runs.
#[cfg(unix)]
fn the_installed_rung_runs_from_the_installed_path(without_the_record: bool) {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.stage(C, "C");
    fx.pin(B);
    fx.pin(C);
    let mut sup = fx.start(&timings());
    let mut first = fx.update(B);
    first.accepted();
    assert_eq!(first.done().expect("a done result")["result"], "switched");
    fx.wait_settled();
    fx.wait_journal("the install", |j| j["install_pending"].is_null());
    // The installed file and B's kept copy are the same bytes; only the kept
    // copies (B, and A the `previous`) lose their mode.
    assert_eq!(
        std::fs::read(fx.kept(B)).unwrap(),
        std::fs::read(fx.installed()).unwrap()
    );
    for version in [A, B] {
        std::fs::set_permissions(fx.kept(version), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // The switch to C fails; its return starts with C's fault out of the way.
    fx.fault("exit-after", "go@C");
    fx.hold("journal.before:J-R1");
    let mut second = fx.update(C);
    second.accepted();
    fx.wait_arrived("journal.before:J-R1");
    fx.wait_dead(2);
    fx.wait_dead(3);
    fx.fault("exit-after", "welcome@0.0.0");
    fx.release("journal.before:J-R1");
    drop(second);
    fx.wait_journal("the return on the installed rung", |j| {
        j["switch"]["return"]["installed"] == true
    });
    // By its handle: nothing of its own runs afterwards.
    sup.kill();
    let spawned = fx.driver_log().iter().filter(|v| v["k"] == "spawn").count();
    for n in 1..=spawned {
        fx.wait_dead(n);
    }
    let mut journal = fx.journal().expect("a journal");
    assert_eq!(journal["switch"]["return"]["target"]["version"], B);
    assert_eq!(journal["switch"]["return"]["installed"], true);
    if without_the_record {
        assert!(journal["ladder"].is_object(), "{journal}");
        journal.as_object_mut().unwrap().remove("ladder");
        std::fs::write(
            fx.data().join("switch-journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }
    let installed = fx.installed().display().to_string();
    let before = fx.driver_log().len();
    let _third = fx.spawn(&timings());
    // The first start of the second supervisor fails at its welcome; the fault
    // goes before the backoff ends.
    fx.wait_for("the first start", || {
        (!starts_after(&fx, before).is_empty()).then_some(())
    });
    // Checked as soon as it is made: a start from the kept copy (which cannot
    // run) fails here and not by a timeout.
    let first_start = starts_after(&fx, before).remove(0);
    assert_eq!(
        first_start.2,
        installed,
        "the first start was not from the installed path\n{}",
        fx.dump()
    );
    fx.clear_fault("exit-after");
    let pid = fx.wait_for("the installed file to run", || {
        let log = fx.driver_log();
        let run = log[before..].iter().filter(|v| v["k"] == "spawn").nth(1)?;
        let pid = run["pid"].as_u64()? as u32;
        alive(pid).then_some(pid)
    });
    let starts = starts_after(&fx, before);
    assert_eq!(
        starts,
        [
            ("spawn".to_owned(), B.to_owned(), installed.clone()),
            ("spawn".to_owned(), B.to_owned(), installed.clone()),
        ],
        "{}",
        fx.dump()
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::canonicalize(format!("/proc/{pid}/exe")).unwrap(),
        std::fs::canonicalize(&installed).unwrap(),
        "the worker runs from another file than the installed one"
    );
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    fx.wait_settled();
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["ladder"]["current"]["installed"], true, "{journal}");
}

#[cfg(unix)]
#[test]
fn recovery_that_picks_the_installed_rung_runs_the_installed_path() {
    the_installed_rung_runs_from_the_installed_path(true);
}

#[cfg(unix)]
#[test]
fn the_installed_rung_the_ladder_record_names_runs_the_installed_path() {
    the_installed_rung_runs_from_the_installed_path(false);
}
