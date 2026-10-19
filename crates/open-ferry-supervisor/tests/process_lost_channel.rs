//! Not upstream's: design v3 13 item 9 and 6.2. A lost channel is not a dead
//! process: the worker's channel closes while it keeps running and keeps what
//! it holds (the fake sleeps with its lease). The supervisor waits up to the
//! stop budget for it to exit, terminates it by its handle, and starts the
//! replacement only after it is gone. The fallback is a hash-checked artifact.
#![cfg(feature = "blue-green-test")]

mod under_test;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use under_test::*;

/// The stop budget these tests run with. A worker whose channel is lost gets
/// this long to exit by itself, then the supervisor uses its handle (the
/// worker's `kill_at`, set when the loss is seen). Measured here: the kill
/// comes after the 10 s budget, allowing for the supervisor's tick.
const STOP: u64 = 10_000;

/// The least time that may pass between the loss and the process being gone:
/// half the budget, checked against a conservative lower bound (below). A
/// correct kill comes at about `STOP` (10 s) after the loss, an immediate one
/// within about 0.5 s of it. The bound is the gap between the latest the loss
/// can have been seen (the receive of the first status poll that showed it)
/// and the earliest the exit can have happened (the start of the last liveness
/// check that found the process alive), so it is short of the true gap by at
/// most three amounts: the loss's window between two polls, the exit's
/// window between two checks, and the lag of the status. Their sum must fit
/// `UNCERTAINTY_MAX` (set-up fails otherwise). The lag is the time from the
/// command that caused the loss to the poll that showed it, so it holds the
/// supervisor's own delay in noticing the loss (seen at up to about 1 s on
/// Windows) and the journal write `worker_lost` (core.rs) makes before the
/// status can show it.
/// A correct run has a bound of at least `STOP - UNCERTAINTY_MAX` = 6 s,
/// less the allowed wall-clock slack, still above `LOSS_MIN` = 5 s.
/// An immediate kill has a true gap under 0.5 s, so a bound under 0.5 s, 4.5 s
/// below `LOSS_MIN`. The windows are taken in the test process from `Instant`,
/// so a step of the wall clock cannot move them. Only the lag is measured
/// against the wall clock (the supervisor's log), and a step there fails the
/// set-up (it is checked against the `Instant`s) and cannot make the test pass.
const LOSS_MIN: Duration = Duration::from_millis(STOP / 2);

/// Spend the available measurement margin as a whole. A 1.5 s status poll
/// under load is harmless when the other windows are small; rejecting every
/// round trip over 500 ms rejected such valid measurements. The cap follows
/// from the unchanged stop budget and lower bound, rather than machine speed.
/// Four seconds leaves one second above `LOSS_MIN`, including the 200 ms
/// allowed wall-clock slack, instead of spending the entire five-second gap.
const UNCERTAINTY_MAX: Duration = Duration::from_secs(4);

fn tm(watch: u64) -> Tm {
    Tm {
        stop: STOP,
        watch,
        ..Tm::default()
    }
}

/// The `at` (wall clock, from the supervisor's log) of the first command `cmd`
/// logged for worker `n`. Used only to order what the supervisor did, never to
/// time a budget.
fn cmd_at(fx: &Fx, n: usize, cmd: &str) -> u64 {
    let pid = fx.pid_of(n).expect("the worker was started");
    fx.driver_log()
        .iter()
        .find(|v| v["k"] == "cmd" && v["pid"] == pid && v["cmd"] == cmd)
        .unwrap_or_else(|| {
            panic!(
                "no {cmd} for worker {n}
{}",
                fx.dump()
            )
        })["at"]
        .as_u64()
        .unwrap()
}

fn spawns(fx: &Fx) -> Vec<Value> {
    fx.driver_log()
        .into_iter()
        .filter(|v| v["k"] == "spawn")
        .collect()
}

/// Stops the watcher thread when the test ends, by a panic too.
struct Done<'a>(&'a AtomicBool);
impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// How far the wall clock may drift from the `Instant`s over the measurement
/// before a step of the clock is suspected.
const CLOCK_SLACK_MS: i64 = 200;

/// Arms the fake's `drop-channel` fault for `phase_at` (`taking-over@B`,
/// `watching@B`, `handing-over@A`) and returns the command the supervisor logs
/// when it sends the message that makes the fake drop its channel (the fake
/// drops it on receiving that message): `take_over`, `go` and `hand_over`.
fn arm(fx: &Fx, phase_at: &str) -> &'static str {
    fx.fault("drop-channel", phase_at);
    match phase_at.split('@').next().unwrap() {
        "taking-over" => "send:take_over",
        "watching" => "send:go",
        "handing-over" => "send:hand_over",
        other => panic!("no command is known to cause a drop in {other}"),
    }
}

/// How late the loss shows in the status. The supervisor sets the worker's
/// `kill_at` when it sees the loss, then writes its journal before the status
/// can show it, so a slow write moves the loss in the status later while every
/// poll returns quickly. The loss cannot be earlier than the command that
/// caused it, so the lag is at most the time from that command's log line to
/// the poll that showed the loss. The reference is that one command for that
/// worker, not the last command sent (a later one, such as `peers_gone`, would
/// make a slow loss look fresh). A missing line, a repeated one, or a poll
/// instant before the command (a step of the clock) fails the set-up, as does a
/// lag that, with the sampling windows, exceeds `UNCERTAINTY_MAX`.
fn loss_lag(log: &[Value], pid: u32, cause: &str, shown_ms: u64) -> Result<Duration, String> {
    let lines: Vec<u64> = log
        .iter()
        .filter(|v| v["k"] == "cmd" && v["pid"] == pid && v["cmd"] == cause)
        .filter_map(|v| v["at"].as_u64())
        .collect();
    let [caused_ms] = lines[..] else {
        return Err(format!(
            "the log has {} `{cause}` lines for worker pid {pid}, not one",
            lines.len()
        ));
    };
    let Some(lag) = shown_ms.checked_sub(caused_ms) else {
        return Err(format!(
            "the status showed the loss at {shown_ms}, before its cause `{cause}` at {caused_ms}: the clock stepped"
        ));
    };
    Ok(Duration::from_millis(lag))
}

fn check_uncertainty(loss: Duration, lag: Duration, exit: Duration) -> Result<(), String> {
    let total = loss + lag + exit;
    if total > UNCERTAINTY_MAX {
        return Err(format!(
            "the measurement uncertainty is {total:?} (loss window {loss:?}, status lag {lag:?}, exit window {exit:?}), over {UNCERTAINTY_MAX:?}"
        ));
    }
    Ok(())
}

#[test]
fn the_lag_is_taken_from_the_command_that_caused_the_loss() {
    let cmd = |cmd: &str, at: u64| json!({"k": "cmd", "pid": 7, "cmd": cmd, "at": at});
    // `go` at 1000, then a later `peers_gone` at 2600: the loss shows at 3200,
    // 2200 ms after its cause, which is slow, though 600 ms after the last send.
    let log = [cmd("send:go", 1000), cmd("send:peers_gone", 2600)];
    let lag = loss_lag(&log, 7, "send:go", 3200).unwrap();
    assert_eq!(lag, Duration::from_millis(2200));
    // A slow status is usable while the total uncertainty fits the budget.
    assert!(check_uncertainty(Duration::from_millis(1530), lag, POLL).is_ok());
    // The later command must not make an over-budget measurement look fresh.
    let slow = check_uncertainty(Duration::from_secs(3), lag, POLL).unwrap_err();
    assert!(slow.contains("over"), "{slow}");
    assert_eq!(
        loss_lag(&log, 7, "send:go", 1300).unwrap(),
        Duration::from_millis(300)
    );
    // Another worker's command is not this worker's.
    assert!(loss_lag(&log, 8, "send:go", 1300).is_err());
    // No line, and two lines, for the cause.
    assert!(loss_lag(&log, 7, "send:take_over", 1300).is_err());
    let twice = [cmd("send:go", 1000), cmd("send:go", 1100)];
    assert!(loss_lag(&twice, 7, "send:go", 1300).is_err());
    // The status instant before the cause is a step of the clock.
    let stepped = loss_lag(&log, 7, "send:go", 900).unwrap_err();
    assert!(stepped.contains("clock stepped"), "{stepped}");
}

/// What the watcher found: the conservative lower bound of the time between
/// the loss and the exit, or why the measurement cannot be trusted.
type Measured = Result<Duration, String>;

/// From a watcher thread, worker `n`'s loss and exit, with the uncertainty
/// kept. Every status poll records the `Instant`s it was sent and received.
/// The loss lies between the send of the last poll that did not show worker
/// `n` lost and the receive of the first that did. The exit is treated alike:
/// it lies between the start of the last liveness check that found the process
/// alive and the end of the first that did not. The result is the lower bound
/// of the gap, the earliest exit minus the latest loss. `ready` is set once a
/// poll has come back, so the fault is released only into a watching test.
/// `cause` is the logged command that makes the fake drop its channel (see
/// `arm`). `None` if `done` is set first.
fn measure(
    fx: &Fx,
    n: usize,
    cause: &str,
    ready: &AtomicBool,
    done: &AtomicBool,
) -> Option<Measured> {
    let pause = || std::thread::sleep(POLL);
    let base = (Instant::now(), now_ms());
    let mut clear_sent: Option<Instant> = None;
    let (loss_lo, loss_hi, shown_ms) = loop {
        if done.load(Ordering::Relaxed) {
            return None;
        }
        let sent = Instant::now();
        let status = fx.status();
        let received = Instant::now();
        let received_ms = now_ms();
        if let Some(status) = status {
            let lost = fx.pid_of(n).is_some_and(|pid| {
                status["workers"].as_array().is_some_and(|workers| {
                    workers.iter().any(|w| w["pid"] == pid && w["lost"] == true)
                })
            });
            if lost {
                match clear_sent {
                    Some(lo) => break (lo, received, received_ms),
                    None => {
                        return Some(Err(
                            "the first poll already showed the loss, so its start is unknown"
                                .to_owned(),
                        ));
                    }
                }
            }
            clear_sent = Some(sent);
            ready.store(true, Ordering::Release);
        }
        pause();
    };
    let pid = fx.pid_of(n).expect("a lost worker has a pid");
    // How late the loss shows (see `loss_lag`), in the wall clock of the
    // supervisor's log. A step of that clock since the watcher began would
    // move it, so the wall clock must agree with the `Instant`s over the same
    // span, or the set-up fails.
    let mono_ms = loss_hi.duration_since(base.0).as_millis() as i64;
    let wall_ms = shown_ms as i64 - base.1 as i64;
    if (wall_ms - mono_ms).abs() > CLOCK_SLACK_MS {
        return Some(Err(format!(
            "the wall clock moved {wall_ms} ms while the monotonic clock moved {mono_ms} ms, so the loss's lag cannot be told"
        )));
    }
    let lag = match loss_lag(&fx.driver_log(), pid, cause, shown_ms) {
        Ok(lag) => lag,
        Err(why) => return Some(Err(why)),
    };
    // The process was alive at the loss (the fixture's other tests say so), so
    // the earliest exit is no earlier than the loss's start; a check that finds
    // it gone at once leaves the bound at zero, which the test then fails.
    let mut alive_from = loss_hi;
    loop {
        if done.load(Ordering::Relaxed) {
            return None;
        }
        let before = Instant::now();
        let up = alive(pid);
        let after = Instant::now();
        if up {
            alive_from = before;
        } else {
            if let Err(why) = check_uncertainty(
                loss_hi.duration_since(loss_lo),
                lag,
                after.duration_since(alive_from),
            ) {
                return Some(Err(why));
            }
            return Some(Ok(alive_from.saturating_duration_since(loss_hi)));
        }
        pause();
    }
}

/// Runs `body` while a watcher times worker `n` from its loss to its exit, and
/// asserts the lower bound of the gap kept the budget. A trouble with the
/// measurement itself (combined uncertainty over the cap, or a watcher that
/// was not ready before `body` released the fault) fails the set-up, in words
/// that say so, and not the property.
fn budgeted<R>(fx: &Fx, n: usize, cause: &str, body: impl FnOnce() -> R) -> R {
    let done = AtomicBool::new(false);
    let ready = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| measure(fx, n, cause, &ready, &done));
        // Sets `done` when this closure ends, by a panic too, so a failing
        // body never leaves the watcher polling.
        let _stop = Done(&done);
        let until = Instant::now() + Duration::from_secs(10);
        while !ready.load(Ordering::Acquire) {
            assert!(
                Instant::now() < until && !watcher.is_finished(),
                "SET-UP: the watcher was not ready before the fault was released
{}",
                fx.dump()
            );
            std::thread::sleep(POLL);
        }
        let result = body();
        // The body returned: the kill and the exit are over by now, or about
        // to be. Let the watcher finish, bounded by the fixture's wait.
        let until = Instant::now() + LONG;
        while !watcher.is_finished() && Instant::now() < until {
            std::thread::sleep(POLL);
        }
        done.store(true, Ordering::Relaxed);
        let measured = watcher.join().unwrap().unwrap_or_else(|| {
            panic!(
                "never saw worker {n} lost and gone
{}",
                fx.dump()
            )
        });
        let bound = measured.unwrap_or_else(|why| panic!("SET-UP: worker {n}: {why}"));
        assert!(
            bound >= LOSS_MIN,
            "worker {n} was gone at least {bound:?} after it was lost (a lower bound); the supervisor must wait the {STOP} ms budget first (at least {LOSS_MIN:?})"
        );
        result
    })
}

#[test]
fn a_lost_b_in_taking_over_gets_the_budget_then_its_handle_then_a_resumes() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let cause = arm(&fx, "taking-over@B");
    let _sup = fx.start(&tm(1_500));
    // B's channel goes at its take_over; its process must stay the budget.
    let result = budgeted(&fx, 2, cause, || {
        let mut sw = fx.update(B);
        sw.accepted();
        let result = sw.done().expect("a done result");
        fx.wait_settled();
        result
    });
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["phase"], "taking-over", "{result}");
    // A (it holds nothing now) is resumed only once B is gone: B's lease is
    // the one A needs.
    let b = fx.pid_of(2).unwrap();
    let resume = fx
        .driver_log()
        .into_iter()
        .find(|v| v["k"] == "cmd" && v["pid"] == fx.pid_of(1).unwrap() && v["cmd"] == "send:resume")
        .expect("A was resumed");
    assert!(resume["at"].as_u64().unwrap() >= cmd_at(&fx, 2, "kill"));
    assert!(
        !resume["alive"].as_array().unwrap().iter().any(|p| *p == b),
        "A was resumed while B ran: {resume}"
    );
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
}

/// Row f after a lost B in `watching`, with `change` run while the decision
/// to return is held at J-R1 (B is gone by then, so is A).
fn lost_b_watching(fx: &Fx, change: impl FnOnce()) -> Watching {
    let cause = arm(fx, "watching@B");
    fx.hold("journal.before:J-R1");
    let sup = fx.start(&tm(60_000));
    // A exits well before B's lost-channel budget ends. Keep its process
    // object until the caller finishes asserting: reopening its numeric pid
    // later can find another process, making `gone: &[1]` report a live A.
    #[cfg(windows)]
    let original = open_ferry_supervisor::process::windows::ProcessHandle::open_pid(
        fx.pid_of(1).expect("the original worker was started"),
    )
    .expect("open the original worker")
    .expect("the original worker still runs");
    budgeted(fx, 2, cause, || {
        let mut sw = fx.update(B);
        sw.accepted();
        fx.wait_arrived("journal.before:J-R1");
        // The supervisor reaches J-R1 only after B has exited: the replacement
        // does not run, and nothing is decided, while B holds anything.
        fx.wait_dead(2);
        fx.wait_dead(1);
        change();
        fx.release("journal.before:J-R1");
        sw.done().expect("a done result");
    });
    Watching {
        _supervisor: sup,
        #[cfg(windows)]
        _original: original,
    }
}

struct Watching {
    _supervisor: Sup,
    #[cfg(windows)]
    _original: open_ferry_supervisor::process::windows::ProcessHandle,
}

#[test]
fn a_lost_b_in_watching_is_terminated_after_the_budget_and_a_fresh_from_runs() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let sha_a = fx.sha(A);
    let _sup = lost_b_watching(&fx, || {});
    fx.wait_settled();
    let all = spawns(&fx);
    assert_eq!(all.len(), 3, "{all:#?}");
    let b = fx.pid_of(2).unwrap();
    assert!(
        !all[2]["alive"].as_array().unwrap().iter().any(|p| *p == b),
        "the fresh from started while B still ran: {}",
        all[2]
    );
    assert_eq!(all[2]["sha"], sha_a);
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
        killed: &[2],
        gone: &[1],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn a_lost_b_whose_fallback_is_corrupt_runs_the_installed_file_by_hash() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let sha_a = fx.sha(A);
    let _sup = lost_b_watching(&fx, || fx.corrupt(A));
    fx.wait_for("the installed file to run", || {
        fx.pid_of(3).filter(|pid| alive(*pid))
    });
    fx.wait_settled();
    let all = spawns(&fx);
    assert_eq!(all.len(), 3, "{all:#?}");
    assert_eq!(all[2]["binary"], fx.installed().display().to_string());
    assert_eq!(all[2]["sha"], sha_a);
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
        killed: &[2],
        gone: &[1],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
}

/// 6.2 step 4 (line 320) sends a lost B in `watching` to row f; row f (line 304)
/// makes the forward switch's own record `failed-after-switch` (phase `watching`,
/// where it failed), blame the new version (B misbehaved: it is the one marked failed,
/// `mark_failed(to)`), `returned_to` set only when the fresh `from` ran, so absent
/// here; `confirmed` stays false. Row g (line 305) says the outcome is recorded
/// already when nothing can start; 3.3's `switch.return` row records it with the
/// blame stored in `return`, and the decision stays journalled. (This one test holds
/// what was two: nothing starts, and the record.)
#[test]
fn a_lost_b_with_every_fallback_corrupt_records_the_outcome() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = lost_b_watching(&fx, || {
        fx.corrupt(A);
        let mut bytes = std::fs::read(fx.installed()).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(fx.installed(), bytes).unwrap();
    });
    fx.wait_for("degraded", || {
        (fx.status()?["state"] == "degraded").then_some(())
    });
    assert_eq!(spawns(&fx).len(), 2, "{}", fx.dump());
    assert_eq!(fx.status().unwrap()["state"], "degraded");
    // The installed file was damaged on purpose, so its bytes are named, not
    // A's. Nothing runs, B was ended by its handle, and the decision to return
    // is still in the journal.
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
        killed: &[2],
        gone: &[1],
        installed_bytes: Some(&damaged_artifact(A)),
        journal: JournalState::Open,
        ..Columns::default()
    });
}

#[test]
fn a_lost_a_in_handing_over_is_terminated_after_the_budget_and_a_fresh_from_runs() {
    let fx = Fx::new();
    fx.stage(B, "B");
    let cause = arm(&fx, "handing-over@A");
    let sha_a = fx.sha(A);
    let _sup = fx.start(&tm(1_500));
    let result = budgeted(&fx, 1, cause, || {
        let mut sw = fx.update(B);
        sw.accepted();
        let result = sw.done().expect("a done result");
        fx.wait_settled();
        result
    });
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["phase"], "handing-over", "{result}");
    let all = spawns(&fx);
    assert_eq!(all.len(), 3, "{all:#?}");
    let a = fx.pid_of(1).unwrap();
    assert!(
        !all[2]["alive"].as_array().unwrap().iter().any(|p| *p == a),
        "the replacement started while the lost A still ran: {}",
        all[2]
    );
    assert_eq!(all[2]["sha"], sha_a);
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
        killed: &[1],
        gone: &[2],
        running: &[3],
        installed: A,
        ..Columns::default()
    });
}

/// The fixture's clean-up (item 1): the guard ends the workers too, not only
/// the supervisor. B's channel is lost, so B (it sleeps with its lease) does
/// not exit on EOF; when the guard is dropped it must still be gone, and soon,
/// without the supervisor's own kill (the supervisor is killed, not stopped).
#[test]
fn dropping_the_guard_ends_a_worker_that_lost_its_channel() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("drop-channel", "taking-over@B");
    let sup = fx.start(&tm(60_000));
    let mut sw = fx.update(B);
    sw.accepted();
    let b = fx.wait_for("B to be started", || fx.pid_of(2));
    fx.wait_for("B to show as lost", || {
        let status = fx.status()?;
        status["workers"]
            .as_array()?
            .iter()
            .any(|w| w["pid"] == b && w["lost"] == true)
            .then_some(())
    });
    assert!(alive(b), "B is lost but runs");
    let a = fx.pid_of(1).unwrap();
    drop(sw);
    drop(sup);
    // On Unix the guard itself waits for them (bounded), so both are gone when
    // it returns. On Windows the job object ends them as the supervisor dies.
    #[cfg(unix)]
    {
        assert!(!alive(b), "B outlived the guard");
        assert!(!alive(a), "A outlived the guard");
    }
    fx.wait_for("both workers to be gone", || {
        (!alive(a) && !alive(b)).then_some(())
    });
}
