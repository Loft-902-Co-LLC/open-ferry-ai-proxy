//! Not upstream's: design v3 13 item 11 and 5.3, mode `off`. The supervisor
//! stamps each admission from its own reading of the mode (here the host's
//! `host-mode` file, `Fx::mode`), re-reads it at every checkpoint and every
//! tick while a switch is open, and cancels when the mode is `off` now and
//! either it was not off at admission or its generation moved since.
#![cfg(feature = "blue-green-test")]

mod under_test;
use open_ferry_supervisor::protocol::{Admission, How};
use under_test::*;

/// The result of a cancelled switch: no record, nothing failed.
#[track_caller]
fn assert_cancelled(result: &serde_json::Value) {
    assert_eq!(result["result"], "cancelled", "{result}");
}

#[test]
fn off_during_loading_cancels_and_a_never_knew() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // B is spawned but cannot report loaded yet. The mode is read while
    // loading is held; the switch must end cancelled without telling A.
    fx.fault("hold-load", "@B");
    fx.hold("load.before_loaded");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("load.before_loaded");
    fx.mode(true, 1);
    fx.wait_mode_read(true, 1);
    fx.release("load.before_loaded");
    assert_cancelled(&sw.done().expect("a done result"));
    fx.wait_settled();
    fx.wait_dead(2);
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        gone: &[2],
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}

/// Design 13 item 11: "during `handing-over` (immediate `resume`)", and 5.3:
/// the mode is re-evaluated every second while a switch is open. A is held
/// inside its hand over the whole time, so `resume` must reach it while the
/// barrier is still held; a supervisor that acts only at checkpoint 2 (after
/// `handed_over`) never sends it, and this test fails.
#[test]
fn off_during_handing_over_resumes_a_at_once() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // A stops inside its hand over.
    fx.hold("handover.gate_closing");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("handover.gate_closing");
    fx.mode(true, 1);
    fx.wait_mode_read(true, 1);
    // Within a few seconds (the per-second re-evaluation, with margin) and
    // with the barrier still held: A itself says it resumed. The fake answers
    // `resume` while the barrier is held (it cancels the hand over), so its own
    // `resumed` event, from the pid it started with, is proof the command was
    // delivered and acted on, not merely queued by the supervisor.
    let a = fx.pid_of(1).expect("A's pid");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !fx
        .events()
        .iter()
        .any(|e| e.pid == a && e.event == "resumed")
    {
        assert!(
            std::time::Instant::now() < until,
            "A did not report `resumed` while its hand over was held
{}",
            fx.dump()
        );
        std::thread::sleep(POLL);
    }
    // The barrier is still held: this test has not released it yet.
    fx.release("handover.gate_closing");
    assert_cancelled(&sw.done().expect("a done result"));
    fx.wait_settled();
    fx.wait_dead(2);
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        gone: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

/// The window between `handed_over` and checkpoint 2, which is the only place
/// this can be acted on: the supervisor holds `handed_over` and has not sent
/// `take_over`, so no waiting loop is left to notice the mode (a hold at A's
/// `lease.release`, before A says `handed_over`, would let the per-tick
/// re-evaluation of the hand over do it instead).
#[test]
fn off_as_a_hands_over_cancels_at_checkpoint_two_and_a_resumes() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.hold("switch.before_checkpoint_two");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("switch.before_checkpoint_two");
    // A has handed over (and let go of the lease); B has not been told to take
    // over; A has not been told to resume. The fake sends `HandedOver` and then
    // logs the event, so the barrier can be reached first: wait, bounded, for the
    // event while the barrier is held, then make the negative assertions.
    fx.wait_for("A's `handed_over` event", || {
        fx.logged("A", "handed_over").then_some(())
    });
    assert!(!fx.sent_to(2).contains(&"send:take_over".to_owned()));
    assert!(!fx.sent_to(1).contains(&"send:resume".to_owned()));
    fx.mode(true, 1);
    fx.release("switch.before_checkpoint_two");
    assert_cancelled(&sw.done().expect("a done result"));
    fx.wait_settled();
    fx.wait_dead(2);
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        gone: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

#[test]
fn off_after_take_over_is_too_late() {
    let fx = Fx::new();
    fx.stage(B, "B");
    // B holds the lease and is reloading; the mode is off before it is ready.
    fx.hold("take_over.before_reload");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("take_over.before_reload");
    fx.mode(true, 1);
    fx.wait_mode_read(true, 1);
    fx.release("take_over.before_reload");
    let result = sw.done().expect("a done result");
    assert_eq!(result["result"], "switched", "{result}");
    fx.wait_settled();
    fx.wait_journal("the install", |j| j["install_pending"].is_null());
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

#[test]
fn a_manual_update_admitted_while_off_proceeds() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.mode(true, 1);
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    let result = sw.done().expect("a done result");
    assert_eq!(result["result"], "switched", "{result}");
    fx.wait_settled();
    fx.wait_journal("the install", |j| j["install_pending"].is_null());
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

#[test]
fn off_then_on_then_off_after_an_admission_while_off_cancels() {
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.mode(true, 1);
    // A stops inside its hand over, so the switch is open while the mode moves.
    fx.hold("handover.gate_closing");
    let _sup = fx.start(&Tm::default());
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("handover.gate_closing");
    // Each reading is waited for: a flip the supervisor never saw is not one.
    fx.mode(false, 2);
    fx.wait_mode_read(false, 2);
    fx.mode(true, 3);
    fx.wait_mode_read(true, 3);
    fx.release("handover.gate_closing");
    assert_cancelled(&sw.done().expect("a done result"));
    fx.wait_settled();
    fx.wait_dead(2);
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        spawns: Some(2),
        gone: &[2],
        running: &[1],
        resumed: &[1],
        installed: A,
        ..Columns::default()
    });
}

/// 5.3 and design 6.1 step 1: an automatic switch waits for pending sign-ins, and the
/// mode going off ends that wait as a cancel does. The `signins-pending` fault makes A
/// report 3 pending sign-ins on every answer, so the wait lasts until the mode changes,
/// not for a moment; the status says what the switch is doing before the mode is turned
/// off, so this does not rest on timing.
#[test]
fn off_while_waiting_for_sign_ins_cancels() {
    if lacks(Cap::SigninsPending) {
        return;
    }
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("signins-pending", "3@A");
    let _sup = fx.start(&Tm {
        signin_wait: 60_000,
        ..Tm::default()
    });
    let mut sw = fx.switch(
        B,
        How::Update,
        true,
        Admission {
            while_off: false,
            mode_gen: 0,
        },
    );
    sw.accepted();
    // The switch is in the sign-in wait, and A is the one holding it. The status
    // says the switch waits before A's next answer brings in its count, so this
    // waits for both.
    fx.wait_for("the sign-in wait, with A's pending sign-ins", || {
        let status = fx.status()?;
        let held = status["workers"]
            .as_array()?
            .iter()
            .any(|w| w["role"] == "active" && w["signins_pending"] == 3);
        (status["switch"]["status"] == "waiting-for-signins" && held).then_some(())
    });
    // B is not loaded while it waits.
    assert_eq!(fx.pid_of(2), None, "B was loaded during the sign-in wait");
    fx.mode(true, 1);
    // The wait ends at once (its 60 s limit is not what ends it).
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fx
        .status()
        .is_some_and(|s| s["switch"]["status"] == "waiting-for-signins")
    {
        assert!(
            std::time::Instant::now() < until,
            "the sign-in wait went on 10 s after the mode went off
{}",
            fx.dump()
        );
        std::thread::sleep(POLL);
    }
    assert_cancelled(&sw.done().expect("a done result"));
    fx.wait_settled();
    fx.assert_columns(Columns {
        state: Some(StateCol::NoRecord(&[])),
        // B is not loaded during the sign-in wait.
        spawns: Some(1),
        running: &[1],
        not_resumed: &[1],
        not_handed_over: &[1],
        installed: A,
        ..Columns::default()
    });
}
