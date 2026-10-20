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
    // A->B has settled; only C's confirmation waits for its exit to be seen.
    fx.hold("watch.before_confirm");
    fx.hold("journal.after:J-R1");
    let mut second = fx.update(C);
    second.accepted();
    fx.wait_arrived("journal.after:J-R1");
    fx.release("watch.before_confirm");
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
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["switch"]["return"]["target"]["version"], A);
    // The return is open, so recovery keeps `previous`: it still names A.
    assert_eq!(journal["previous"]["version"], A, "{journal}");
    // Recovery has verified A; its spawn is refused by the hash.
    fx.corrupt(A);
    // The next write of the count (the take-back of the refused start, or the
    // restart's count for its next try, whichever comes first) is held, so A
    // is put right while nothing can retry it: the repair does not race the
    // backoff.
    fx.hold("journal.before:J-L");
    fx.release("journal.after:J-L");
    fx.wait_arrived("journal.before:J-L");
    assert!(
        fx.driver_log()[before..]
            .iter()
            .any(|v| v["k"] == "spawn_failed"),
        "{}",
        fx.dump()
    );
    fx.corrupt(A);
    fx.release("journal.before:J-L");
    fx.wait_for("A's second start to run", || {
        let log = fx.driver_log();
        let run = log[before..].iter().find(|v| v["k"] == "spawn")?;
        alive(run["pid"].as_u64()? as u32).then_some(())
    });
    fx.wait_loaded();
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

/// A return open and recovery's J-L written with A as its target, then the
/// supervisor killed before it spawns A. B's kept copy is whole again and A's
/// kept copy is not, as the next start finds them; the installed file is whole
/// again too, unless `installed_whole` is false. Then the supervisor's own hash
/// is fixed to the installed file's whole one (`keep_own_hash`), so the damaged
/// installed file is not taken for the supervisor's own binary. Returns the
/// driver log's length and the new supervisor, started and not waited for.
fn the_return_stands_on_a_after_a_crash(fx: &Fx, installed_whole: bool) -> (usize, Sup) {
    killed_with_a_return_open(fx);
    if !installed_whole {
        fx.keep_own_hash();
    }
    fx.corrupt(B);
    fx.corrupt_installed();
    fx.hold("journal.after:J-L");
    let mut second = fx.spawn(&timings());
    fx.wait_arrived("journal.after:J-L");
    let journal = fx.journal().expect("a journal");
    assert_eq!(journal["switch"]["return"]["target"]["version"], A);
    assert_eq!(journal["previous"]["version"], A, "{journal}");
    // Nothing of A has been started: the supervisor dies before its spawn.
    second.kill();
    fx.release("journal.after:J-L");
    // B's kept copy is whole again; A's is not.
    fx.corrupt(B);
    if installed_whole {
        fx.corrupt_installed();
    }
    fx.corrupt(A);
    let before = fx.driver_log().len();
    if !installed_whole {
        // The third supervisor's start-up counts and its cycle boundary are
        // held from its first write (both barriers were released before, so they are
        // armed again).
        fx.rehold("journal.after:J-L");
        fx.rehold("journal.after:J-R1");
    }
    (before, fx.spawn(&timings()))
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
    let (before, _third) = the_return_stands_on_a_after_a_crash(&fx, true);
    fx.wait_for("the installed file to run", || {
        let log = fx.driver_log();
        let run = log[before..].iter().find(|v| v["k"] == "spawn")?;
        alive(run["pid"].as_u64()? as u32).then_some(())
    });
    fx.wait_loaded();
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

/// When a seam file was last written.
fn seam_mtime(fx: &Fx, name: &str, ext: &str) -> std::time::SystemTime {
    std::fs::metadata(fx.seams().join(format!("{}{ext}", name.replace(':', "_"))))
        .and_then(|m| m.modified())
        .expect("a seam file time")
}

/// Lets the held journal writes go on every path out of a test, a panic
/// included.
struct ReleaseOnDrop<'a>(&'a Fx, &'static [&'static str]);

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        for name in self.1 {
            self.0.release(name);
        }
    }
}

/// The durable variant with the installed file damaged too. The supervisor's own
/// hash is the whole file's (`keep_own_hash`), so the damaged installed file is
/// neither the supervisor's own binary nor any version the journal vouches for.
/// Nothing is repaired: A's kept copy and the installed file stay damaged.
///
/// The third supervisor is followed through its journal writes. Start-up
/// selects nothing and leaves the open return to the core's restart, whose
/// progress is the journalled ladder record (A at one try, B spent). The
/// core first restates the open return (a J-R1 write, still targeting A, held
/// and let go). Each
/// further try of A is counted (J-L, held at `journal.after:J-L`) before its
/// start, and the driver refuses A's kept copy by hash: `spawn_failed`, and
/// nothing started, B included. When A is spent the exhausted cycle is
/// journalled as one write (J-R1, held at `journal.after:J-R1`) that
/// retargets the return to the first permitted rung, B's kept copy, with the
/// ladder record cleared: the supervisor is `degraded` and has started
/// nothing. The new cycle begins one restart backoff later, with the count of
/// B's kept copy (J-L again), and B's kept copy, which is whole, starts
/// without any repair and loads.
#[test]
fn a_fallback_and_an_installed_file_that_both_fail_after_a_crash_are_degraded() {
    let fx = Fx::new();
    let _release = ReleaseOnDrop(
        &fx,
        &[
            "journal.after:J-L",
            "journal.before:J-L",
            "journal.after:J-R1",
        ],
    );
    let (before, _third) = the_return_stands_on_a_after_a_crash(&fx, false);
    let kept_a = fx.kept(A).display().to_string();
    let kept_b = fx.kept(B).display().to_string();
    let no_start_but_a_refusals = |what: &str| {
        let starts = starts_after(&fx, before);
        assert!(
            starts
                .iter()
                .all(|(kind, version, binary)| kind == "spawn_failed"
                    && version == A
                    && *binary == kept_a),
            "{what}: {starts:?}
{}",
            fx.dump()
        );
        assert!(
            fx.driver_log()[before..].iter().all(|v| v["k"] != "spawn"),
            "{what}: a worker started
{}",
            fx.dump()
        );
        starts.len()
    };
    // A's tries: each is counted, with A as the return's target, before the
    // driver refuses it. Two are left of A's three (the first was counted by the
    // supervisor that crashed).
    //
    // The J-L barrier is a pair, so that no count can pass a released barrier
    // however the parent is scheduled. Held at `after:J-L` (a count is written),
    // `before:J-L` is armed and `after:J-L` released: the next count stops at
    // `before:J-L`, where `after:J-L` is armed again and `before:J-L` released.
    // The core is blocked at the one barrier while the other is armed. After A's
    // last count the next stop is the cycle boundary, not `before:J-L`.
    let mut counts = 0;
    let mut at_after = true;
    loop {
        let (next, other) = if at_after {
            ("journal.after:J-L", "journal.before:J-L")
        } else {
            ("journal.before:J-L", "journal.after:J-L")
        };
        fx.wait_for("an A count or the cycle boundary", || {
            (fx.arrived(next) || fx.arrived("journal.after:J-R1")).then_some(())
        });
        if fx.arrived("journal.after:J-R1") {
            let journal = fx.journal().expect("a journal");
            if journal["switch"]["return"]["target"]["version"] == B {
                break;
            }
            // The write that restates the open return (target A) when the third
            // supervisor takes it over: it comes before any count, nothing has
            // started. Let it go; the next arrival is an A count, held, and the
            // barrier is armed again for the cycle boundary while the core is
            // blocked there.
            assert!(at_after && counts == 0, "{journal}");
            assert_eq!(
                journal["switch"]["return"]["target"]["version"], A,
                "{journal}"
            );
            assert_eq!(no_start_but_a_refusals("at the restated return"), 0);
            fx.release("journal.after:J-R1");
            fx.wait_arrived("journal.after:J-L");
            fx.rehold("journal.after:J-R1");
            continue;
        }
        if at_after {
            let journal = fx.journal().expect("a journal");
            assert_eq!(
                journal["switch"]["return"]["target"]["version"], A,
                "{journal}"
            );
            assert_eq!(
                journal["ladder"]["current"]["artifact"]["version"], A,
                "{journal}"
            );
            // Every refusal so far was an earlier try; this one is not made yet.
            assert_eq!(
                no_start_but_a_refusals("at an A count"),
                counts,
                "{}",
                fx.dump()
            );
            counts += 1;
        } else {
            // The next count is not written; the counted try was refused.
            assert_eq!(
                no_start_but_a_refusals("before the next A count"),
                counts,
                "{}",
                fx.dump()
            );
        }
        // Arm the other barrier while the core is held at this one, then let
        // this one go.
        fx.rehold(other);
        fx.release(next);
        at_after = !at_after;
    }
    assert_eq!(counts, 2, "{}", fx.dump());
    // The cycle boundary, written: the return is retargeted to B's kept copy and the
    // record ends with the cycle, in the one write. Nothing has started.
    assert_eq!(no_start_but_a_refusals("at the cycle boundary"), 2);
    let journal = fx.journal().expect("a journal");
    assert_eq!(
        journal["switch"]["return"]["target"]["version"], B,
        "{journal}"
    );
    assert_ne!(journal["switch"]["return"]["installed"], true, "{journal}");
    assert!(journal["ladder"].is_null(), "{journal}");
    assert_eq!(fx.status().expect("a status")["state"], "degraded");
    // B's count is held by the pair too: armed here, while the core is blocked.
    fx.rehold("journal.before:J-L");
    fx.rehold("journal.after:J-L");
    // The core restarts on its tick: `run` waits on `sleep_until(next_tick)`
    // with `next_tick` reset to a second on (`core::TICK`) before `on_tick`
    // runs, and this write is made inside `on_tick`. The tick is longer than the
    // backoff, so a restart due at once would still wait for the next tick, and
    // a core held here for less than a tick would reach it either way. Holding
    // the fence for more than a tick makes the tick overdue when the core goes
    // on, so only the backoff is what the new cycle waits for. This is a minimum
    // hold, not a wait for an outcome.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fx.release("journal.after:J-R1");
    fx.wait_arrived("journal.before:J-L");
    fx.release("journal.before:J-L");
    fx.wait_arrived("journal.after:J-L");
    // The new cycle's first count, B's kept copy, comes one backoff after the
    // boundary. The gap is read from the seam files' own times: the `.release`
    // this test wrote and the `.arrived` the child wrote at B's count, at the
    // `before` barrier, the first it reaches, so the parent's pace holds
    // nothing up. The child reads its clock for the backoff only after
    // `.release` exists, so the bound holds however the parent is scheduled,
    // and a slow parent cannot hide a missing backoff. The 20 ms is for
    // file-time granularity (a coarse clock on Linux, a few ms). The one
    // residual is a wall-clock step between the two writes. No `Instant` bound
    // is kept: it would measure when the parent noticed.
    let released = seam_mtime(&fx, "journal.after:J-R1", ".release");
    let arrived = seam_mtime(&fx, "journal.before:J-L", ".arrived");
    let waited = arrived
        .duration_since(released)
        .unwrap_or(std::time::Duration::ZERO);
    assert!(
        waited >= std::time::Duration::from_millis(BACKOFF_MS - 20),
        "the new cycle began {waited:?} after the boundary (the backoff is {BACKOFF_MS} ms)
{}",
        fx.dump()
    );
    assert_eq!(fx.status().expect("a status")["state"], "degraded");
    let journal = fx.journal().expect("a journal");
    assert_eq!(
        journal["switch"]["return"]["target"]["version"], B,
        "{journal}"
    );
    assert_eq!(
        journal["ladder"]["current"]["artifact"]["version"], B,
        "{journal}"
    );
    assert_eq!(
        journal["ladder"]["current"]["installed"], false,
        "{journal}"
    );
    assert_eq!(journal["ladder"]["attempts"], 1, "{journal}");
    assert_eq!(no_start_but_a_refusals("at B's count"), 2);
    fx.release("journal.after:J-L");
    let run = fx.wait_for("B's kept copy to run", || {
        let log = fx.driver_log();
        let run = log[before..].iter().find(|v| v["k"] == "spawn")?.clone();
        alive(run["pid"].as_u64()? as u32).then_some(run)
    });
    assert_eq!(run["version"], B, "{}", fx.dump());
    assert_eq!(run["binary"], kept_b, "{}", fx.dump());
    fx.wait_loaded();
    fx.wait_for("the status to leave degraded", || {
        (fx.status()?["state"] != "degraded").then_some(())
    });
}

/// L9 round 6 item 2. When recovery's choice is the installed rung, start-up
/// runs the installed path, not the kept copy that holds the same bytes. The
/// kept copies of B and A are mode 0600, so a start from either one fails; the
/// installed file holds B's bytes and is executable. A return walks down its
/// ladder (B, A, then the installed file) and the supervisor is killed while
/// the return stands on the installed rung (`return.installed` in the
/// journal). Recovery then chooses the installed rung by hash, and every start
/// of the second supervisor must be from the installed path: the first fails
/// (`exit-after welcome@<version>`, with the build's own version, which only
/// the installed file's marker matches) and is charged to the installed
/// rung, so the next try is that rung again, not a new cycle at the kept
/// copy.
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
    // A->B has settled; only C's confirmation waits for its exit to be seen.
    fx.hold("watch.before_confirm");
    fx.hold("journal.before:J-R1");
    let mut second = fx.update(C);
    second.accepted();
    fx.wait_arrived("journal.before:J-R1");
    fx.release("watch.before_confirm");
    fx.wait_dead(2);
    fx.wait_dead(3);
    fx.fault(
        "exit-after",
        &format!("welcome@{}", env!("CARGO_PKG_VERSION")),
    );
    fx.hold("journal.after:J-R1");
    fx.release("journal.before:J-R1");
    drop(second);
    // Stop at each choice before its spawn. Even an exit at welcome can follow
    // a successful handshake, whose J-R2 clears the return before a poll sees it.
    loop {
        fx.wait_arrived("journal.after:J-R1");
        if fx.journal().expect("a journal")["switch"]["return"]["installed"] == true {
            break;
        }
        fx.rehold("journal.before:J-R1");
        fx.release("journal.after:J-R1");
        fx.wait_arrived("journal.before:J-R1");
        fx.rehold("journal.after:J-R1");
        fx.release("journal.before:J-R1");
    }
    // By its handle: nothing of its own runs afterwards.
    sup.kill();
    fx.release("journal.after:J-R1");
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
    // The retry is gated by two barriers, as in the first recovery variant. The
    // first, `journal.after:J-L`, is start-up's count of the first try (written
    // before the spawn). At it the second, `journal.before:J-L`, is armed and the
    // first is released: the next J-L write is the restart's count of the second
    // try, which the core makes before it spawns it (no take-back comes between:
    // the first start is not refused as untrusted, it fails at its welcome). The
    // fault is cleared while that write is held, once the first worker is dead.
    fx.hold("journal.after:J-L");
    let _third = fx.spawn(&timings());
    fx.wait_arrived("journal.after:J-L");
    fx.hold("journal.before:J-L");
    fx.release("journal.after:J-L");
    // The first start of the second supervisor fails at its welcome.
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
    // The worker has read its fault and died; only then is the fault cleared.
    let first_pid = fx.driver_log()[before..]
        .iter()
        .find(|v| v["k"] == "spawn")
        .and_then(|v| v["pid"].as_u64())
        .expect("the first start's pid") as u32;
    fx.wait_for("the first worker to be gone", || {
        (!alive(first_pid)).then_some(())
    });
    fx.wait_arrived("journal.before:J-L");
    assert_eq!(
        starts_after(&fx, before).len(),
        1,
        "the retry was made before its count: {}",
        fx.dump()
    );
    fx.clear_fault("exit-after");
    fx.release("journal.before:J-L");
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
    fx.wait_loaded();
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
