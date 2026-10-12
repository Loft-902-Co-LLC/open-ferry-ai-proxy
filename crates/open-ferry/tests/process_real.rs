//! Not upstream's: process tests that only the real worker can answer, with
//! the real supervisor and the real disk. They use the supervisor crate's
//! `under_test` harness, included by path (see `process_supervise.rs`), and
//! its fault files and barriers. Nothing here sleeps to prove anything: each
//! test waits, with a bound, for an event, a barrier, a file or a process.
//!
//! A worker is held in loading by holding the workers' lease from the test:
//! an active worker waits for the lease up to the supervisor's load timeout,
//! after it has greeted and before it sends `loaded`.
#![cfg(feature = "blue-green-test")]

#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use serde_json::Value;
use under_test::*;

/// Runs a switch to `B` to its `done` and waits until the journal is settled.
fn switch_b(fx: &Fx) -> Value {
    let mut sw = fx.update(B);
    sw.accepted();
    let result = sw.done().expect("a done result");
    fx.wait_settled();
    result
}

/// A path under management's prefix: the "updating" layer wraps those, not the
/// server's own 404 for the rest.
const MANAGEMENT: &str = "/v0/management/config";

/// What the active-deadline kill may take beyond the deadline itself: the
/// supervisor's tick, the kill and the poll, on a loaded runner.
const KILL_MARGIN: std::time::Duration = std::time::Duration::from_secs(8);

/// The ladder's count of starts on the current rung, 0 without a record.
fn attempts(fx: &Fx) -> u64 {
    fx.journal()
        .and_then(|journal| journal["ladder"].get("attempts").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// Waits until the supervisor has an active worker that greeted, whether or
/// not it has loaded.
fn wait_active_greeted(fx: &Fx) {
    fx.wait_for("an active worker that greeted", || {
        let status = fx.status()?;
        status["workers"]
            .as_array()?
            .iter()
            .any(|w| w["role"] == "active")
            .then_some(())
    });
}

/// This checks the workers' events. What goes over the wire (one `loaded` per
/// worker, none on a take over) is covered by the unit tests in
/// `service/worker/tests/machine/switch.rs` (:455-462, also :280-290 and
/// :314-322).
#[test]
fn a_take_over_sends_one_loaded_per_worker() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    fx.stage(B, "B");
    let _sup = fx.start(&Tm::default());
    fx.wait_loaded();
    assert_eq!(switch_b(&fx)["result"], "switched");
    fx.wait_loaded();
    let events = fx.events();
    // The active worker reports its role's event; the standby's `loaded` comes
    // once, before its `standby`, and a take over adds no second one.
    assert_eq!(fx.count("A", "active"), 1, "{events:?}");
    assert_eq!(fx.count("A", "loaded"), 0, "{events:?}");
    assert_eq!(fx.count("B", "loaded"), 1, "{events:?}");
    assert_eq!(fx.count("B", "standby"), 1, "{events:?}");
    let at = |marker: &str, event: &str| {
        events
            .iter()
            .position(|e| e.marker == marker && e.event == event)
    };
    assert!(at("B", "loaded") < at("B", "standby"), "{events:?}");
    assert_eq!(attempts(&fx), 0, "the start is not counted once loaded");
}

#[test]
fn management_answers_503_during_the_hand_over_and_not_after_the_resume() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.hold("handover.gate_closing");
    let _sup = fx.start(&Tm::default());
    fx.wait_loaded();
    let before = fx.get_management(MANAGEMENT).expect("management answers");
    assert_ne!(before.0, 503, "before the hand over");
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("handover.gate_closing");
    // A is inside its hand over: management is "updating", from A.
    let during = fx.get_management(MANAGEMENT).expect("management answers");
    assert_eq!(during.0, 503, "during the hand over");
    assert_eq!(during.1.as_deref(), Some("A"), "A answered");
    let reply = fx.cancel();
    assert!(
        matches!(
            reply,
            open_ferry_supervisor::protocol::EndpointReply::Cancelled { .. }
        ),
        "{reply:?}"
    );
    assert_eq!(sw.done().expect("a done result")["result"], "cancelled");
    fx.release("handover.gate_closing");
    fx.wait_for("A to resume", || fx.logged("A", "resumed").then_some(()));
    // Ending the 503 is the last step before `resumed`: no wait is needed.
    let after = fx.get_management(MANAGEMENT).expect("management answers");
    assert_ne!(after.0, 503, "after the resume");
    assert_eq!(after.1.as_deref(), Some("A"), "A answered");
}

#[test]
fn a_failed_hand_over_resumes_a_and_the_next_peers_gone_turns_cleaning_on_once() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    fx.stage(B, "B");
    fx.fault("fail-before-ready", "@B");
    let _sup = fx.start(&Tm::default());
    // With no other worker running, the first `peers_gone` turns it on.
    fx.wait_for("the first peers_gone", || {
        (fx.count("A", "peers_gone") == 1).then_some(())
    });
    switch_b(&fx);
    fx.wait_for("A to resume", || fx.logged("A", "resumed").then_some(()));
    // The hand over turned cleaning off and reset the flag: one more
    // `peers_gone` turns it on again, and no further one logs.
    fx.wait_for("the peers_gone after the resume", || {
        (fx.count("A", "peers_gone") >= 2).then_some(())
    });
    fx.wait_dead(2);
    fx.wait_loaded();
    assert_eq!(fx.count("A", "peers_gone"), 2, "{:?}", fx.events());
}

#[test]
fn a_clean_stop_while_the_active_worker_is_loading_gives_the_start_back() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    let lease = fx.hold_lease();
    let mut sup = fx.spawn(&Tm::default());
    wait_active_greeted(&fx);
    fx.wait_for("the start to be counted", || {
        (attempts(&fx) == 1).then_some(())
    });
    assert!(!fx.logged("A", "active"), "{:?}", fx.events());
    let reply = fx.stop();
    assert!(
        matches!(
            reply,
            open_ferry_supervisor::protocol::EndpointReply::Stopping { .. }
        ),
        "{reply:?}"
    );
    let status = fx.wait_exit(&mut sup);
    assert!(status.success(), "{status}\n{}", fx.dump());
    drop(lease);
    assert_eq!(attempts(&fx), 0, "the stop refunded the start");
}

#[test]
fn an_active_worker_that_never_loads_is_killed_at_the_active_deadline() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    let lease = fx.hold_lease();
    // The worker waits for the lease for ten minutes, far past this test's
    // own bound; only the supervisor's deadline can end it sooner.
    let tm = Tm {
        load: 600_000,
        active_load: 1_500,
        ..Tm::default()
    };
    let _sup = fx.spawn(&tm);
    // The deadline counts from the worker's greeting, so the bound does too:
    // the active deadline plus a margin for the kill and the poll. LONG only
    // bounds the set-up. Measured on Windows: the kill came 2.0 s after the
    // greeting was seen (1.5 s deadline).
    wait_active_greeted(&fx);
    let greeted = std::time::Instant::now();
    let pid = fx.wait_for("worker 1 to be started", || fx.pid_of(1));
    let bound = std::time::Duration::from_millis(tm.active_load) + KILL_MARGIN;
    while alive(pid) {
        assert!(
            greeted.elapsed() < bound,
            "not killed within {bound:?} of the greeting\n{}",
            fx.dump()
        );
        std::thread::sleep(POLL);
    }
    eprintln!("active deadline kill after {:?}", greeted.elapsed());
    assert!(
        !fx.logged("A", "active"),
        "the worker loaded: {:?}",
        fx.events()
    );
    // The start stays counted, and the ladder goes on with another worker.
    assert!(attempts(&fx) >= 1, "{:?}", fx.journal());
    drop(lease);
    fx.wait_for("a worker that loads", || {
        fx.logged("A", "active").then_some(())
    });
    fx.wait_loaded();
    assert_eq!(attempts(&fx), 0, "loaded reset the count");
}

#[test]
fn the_workers_run_in_the_fixture_and_write_its_logs_not_the_package_roots() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    // A writable `logs` directory in the working directory is the one the
    // worker picks, before the auth directory's.
    let logs = fx.root().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let _sup = fx.start(&Tm::default());
    fx.wait_loaded();
    let found: Vec<_> = std::fs::read_dir(&logs)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        !found.is_empty(),
        "nothing in {}: the worker's working directory is not the fixture\n{}",
        logs.display(),
        fx.dump()
    );
}

#[test]
fn a_yaml_path_with_an_apostrophe_is_quoted() {
    assert_eq!(yaml_single_quoted("C:\\a\\it's"), "'C:\\a\\it''s'");
    assert_eq!(yaml_single_quoted("plain"), "'plain'");
    assert_eq!(yaml_single_quoted("o'b'"), "'o''b'''");
}

#[test]
fn b_reads_and_deletes_the_handover_file_a_wrote() {
    assert_eq!(kind(), Kind::Real);
    let fx = Fx::new();
    fx.stage(B, "B");
    // B reads it after this barrier, so the file is there while B waits.
    fx.hold("take_over.before_reload");
    let _sup = fx.start(&Tm::default());
    fx.wait_loaded();
    let file = fx.data().join("handover.json");
    assert!(!file.exists(), "nothing before the switch");
    let mut sw = fx.update(B);
    sw.accepted();
    fx.wait_arrived("take_over.before_reload");
    let text = std::fs::read_to_string(&file).expect("A wrote handover.json");
    let handover: Value = serde_json::from_str(&text).expect("it parses");
    let id = handover["id"].as_str().expect("an id");
    assert!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{text}"
    );
    // Only the shape: that the bans survive the hand over is the unit test
    // `a_take_over_restores_the_bans_less_the_time_since_the_hand_over`
    // (`service/worker/tests/machine/switch.rs:623`).
    assert!(handover["bans"].is_array(), "{text}");
    assert!(fx.logged("A", "handover.written"), "{:?}", fx.events());
    fx.release("take_over.before_reload");
    assert_eq!(sw.done().expect("a done result")["result"], "switched");
    fx.wait_settled();
    assert!(!file.exists(), "B deleted the file it read");
}
