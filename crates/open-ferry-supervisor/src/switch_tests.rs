//! Not upstream's: the switch's unit tests (v3 13, "Unit tests"; 3.3 and 6.1).
//! Each test is named after the row it covers. Time is paused; workers are
//! scripted; the disk is in memory.

use std::time::Duration;

use open_ferry_update::journal::{Phase, WorkerRole};
use tokio::sync::mpsc::unbounded_channel;

use crate::core::{Command, Exit};
use crate::protocol::EndpointReply;
use open_ferry_update::journal::Outcome;
use serde_json::Value;

use crate::testkit::{
    Rig, SpawnFail, Step, core_of, done_result, drain, handed_over, loaded, update,
};

fn phases(replies: &[EndpointReply]) -> Vec<String> {
    replies
        .iter()
        .filter_map(|reply| match reply {
            EndpointReply::Progress { phase, .. } => Some(phase.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_happy_path_switches_watches_confirms_and_installs() {
    let mut rig = Rig::new().await;
    let command_tx = rig.command_tx.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;

    let replies = drain(&mut rx);
    assert_eq!(phases(&replies), ["loading", "handing-over", "taking-over"]);
    let result = done_result(&replies);
    assert_eq!(result["result"], "switched");
    let id = result["id"].as_str().unwrap().to_owned();

    // The sequence on disk and on the wire (5.1 steps 2 to 9).
    assert_eq!(
        rig.disk.order(),
        ["J1", "pin:pin", "J2", "J3", "J4", "S1"].map(String::from)
    );
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        rig.driver.log(),
        [
            "spawn 1.0.0 active",
            "spawn 2.0.0 standby",
            "send 1.0.0 hand_over",
            "send 2.0.0 take_over",
            "send 2.0.0 go",
            "send 1.0.0 release",
        ]
        .map(String::from)
    );
    let journal = core.rt.journal.clone().unwrap();
    let switch = journal.switch.expect("the switch is open for the window");
    assert_eq!(switch.phase, Phase::Watching);
    assert_eq!(journal.active.version, "2.0.0");
    assert_eq!(journal.confirmed.version, "1.0.0");
    assert!(core.rt.watch.is_some());
    assert!(!core.rt.switching);
    let b = core.rt.active.unwrap();
    assert_eq!(core.rt.workers[&b].role, WorkerRole::Active);
    let record = rig.disk.record(&id).expect("S1 gave the state a record");
    assert_eq!(record.result.as_deref(), Some("switched"));
    assert!(!record.confirmed);

    // The window ends, the install runs, and a stop ends the run.
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(700)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, ()) = tokio::join!(core.run(), stopper);
    assert_eq!(exit, Exit::Stopped);

    let journal = core.rt.journal.clone().unwrap();
    assert!(journal.switch.is_none());
    assert_eq!(journal.confirmed.version, "2.0.0");
    assert_eq!(journal.previous.unwrap().version, "1.0.0");
    assert!(journal.install_pending.is_none());
    assert_eq!(journal.installed.unwrap().version, "2.0.0");
    assert!(
        journal.outcomes.is_empty(),
        "every outcome reached the state"
    );
    let record = rig.disk.record(&id).unwrap();
    assert!(record.confirmed);
    assert_eq!(record.installed_binary.as_deref(), Some("replaced"));
    assert!(record.drained_ms.is_some());
    assert_eq!(rig.disk.state.lock().unwrap().install_calls, ["2.0.0"]);
    // B is told once nobody is draining (T7), after A exited.
    assert!(
        rig.driver
            .log()
            .contains(&"send 2.0.0 peers_gone".to_owned())
    );
}

/// Runs one switch to its answer; the core is kept for looking at.
macro_rules! go {
    ($rig:ident, $request:expr) => {{
        let (tx, mut rx) = unbounded_channel();
        let mut core = core_of!($rig);
        core.handle_switch($request, tx).await;
        let replies = drain(&mut rx);
        (core, replies)
    }};
}

fn stated(disk: &crate::testkit::FakeDisk, id: &str) -> Outcome {
    disk.state
        .lock()
        .unwrap()
        .state
        .iter()
        .find(|outcome| outcome.record.id == id)
        .cloned()
        .unwrap_or_else(|| panic!("the state has no record {id}"))
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

fn failed(result: &Value, phase: &str, blame: &str) {
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["phase"], phase, "{result}");
    assert_eq!(result["blame"], blame, "{result}");
}

fn active_version<D: crate::driver::Driver, H: crate::host::Host, S: crate::disk::Disk>(
    core: &crate::core::Core<'_, D, H, S>,
) -> Option<String> {
    core.rt
        .active
        .and_then(|id| core.rt.workers.get(&id))
        .map(|w| w.artifact.version.clone())
}

// ---- 6.1 row a: B does not load ---------------------------------------------

#[tokio::test(start_paused = true)]
async fn row_a_load_failed_kills_b_and_marks_the_release_failed() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "bad config".to_owned(),
        })],
    );
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "new-version");
    let id = result["id"].as_str().unwrap();
    settle().await;
    assert!(rig.driver.log().contains(&"kill 2.0.0".to_owned()));
    assert!(
        rig.driver
            .log_of("send 1.0.0")
            .iter()
            .all(|line| line.ends_with("peers_gone")),
        "A never knew"
    );
    assert_eq!(
        rig.disk.order(),
        ["J1", "pin:pin", "J7", "S3", "J8"].map(String::from)
    );
    let outcome = stated(&rig.disk, id);
    assert_eq!(outcome.mark_failed.unwrap().version, "2.0.0");
    assert!(core.rt.journal.as_ref().unwrap().switch.is_none());
    assert!(core.rt.journal.as_ref().unwrap().outcomes.is_empty());
    assert!(rig.disk.on_disk().unwrap().outcomes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn row_a_b_exits_while_loading() {
    let mut rig = Rig::new().await;
    rig.driver
        .on("2.0.0", "spawn", vec![Step::Exit(false, "crashed")]);
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "new-version");
    assert!(result["why"].as_str().unwrap().contains("exited"));
}

#[tokio::test(start_paused = true)]
async fn row_a_no_loaded_within_the_load_timeout() {
    let mut rig = Rig::new().await;
    rig.driver.on("2.0.0", "spawn", vec![]);
    let started = tokio::time::Instant::now();
    let (_core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "loading", "new-version");
    let elapsed = started.elapsed();
    assert!(elapsed >= rig.cfg.timings.load);
    // The standby gets `load`, not the active worker's `active_load`.
    assert!(
        elapsed < rig.cfg.timings.active_load,
        "standby waited {elapsed:?}, the active load timeout is {:?}",
        rig.cfg.timings.active_load
    );
    settle().await;
    assert!(rig.driver.log().contains(&"kill 2.0.0".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn row_a_a_missing_file_is_the_environment_and_marks_nothing() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Missing);
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "environment");
    assert!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn row_a_a_worker_that_does_not_start_is_the_new_versions_fault() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Start);
    let (_core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "loading", "new-version");
}

#[tokio::test(start_paused = true)]
async fn row_a_a_spawn_that_hangs_ends_at_the_load_timeout() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Hang);
    let (_core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "loading", "new-version");
}

#[tokio::test(start_paused = true)]
async fn row_a_a_worker_refused_for_a_missing_capability_is_the_new_versions_fault() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::MissingCap);
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "new-version");
    assert!(
        result["why"]
            .as_str()
            .is_some_and(|why| why.contains("missing required capabilities")),
        "{result}"
    );
    // New-version blame is what marks the version failed.
    assert!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .is_some()
    );
}

// ---- row b: incompatible ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn row_b_incompatible_is_the_environment_and_asks_for_a_restart() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::Incompatible {
            needs_supervisor: 5,
        })],
    );
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "environment");
    let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
    assert!(outcome.record.restart_needed);
    assert!(outcome.mark_failed.is_none());
    settle().await;
    assert!(rig.driver.log().contains(&"kill 2.0.0".to_owned()));
}

// Row b also tells the state file itself (11.2): `needs_restart`, noted against
// the running supervisor's version, written with the outcome so that it rides
// the owed-outcome path like the record does.
#[tokio::test(start_paused = true)]
async fn row_b_the_state_file_records_needs_restart_and_the_supervisors_version() {
    use open_ferry_update::data_dir::DataDir;
    use open_ferry_update::journal::JournalStore;
    use open_ferry_update::state::State;
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::at(temp.path());
    let mut rig = Rig::new().await;
    rig.disk.real = Some(
        crate::disk::RealDisk::new(
            data.clone(),
            temp.path().join("installed"),
            "open-ferry".to_owned(),
            JournalStore::new(temp.path()),
        )
        .with_supervisor_version("1.0.0".to_owned()),
    );
    rig.disk.real_state = true;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::Incompatible {
            needs_supervisor: 5,
        })],
    );
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "environment");
    let state = State::load_strict(&data.state_file()).unwrap();
    assert!(state.needs_restart, "{state:?}");
    assert_eq!(state.supervisor_version.as_deref(), Some("1.0.0"));
    assert_eq!(state.history.len(), 1);
    assert!(state.history[0].restart_needed);
    assert!(
        state.failed.is_empty(),
        "no mark_failed for the environment"
    );
}

#[tokio::test(start_paused = true)]
async fn row_b_incompatible_at_the_handshake_is_the_same() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Incompatible);
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "environment");
    assert!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .record
            .restart_needed
    );
}

// ---- row c: A does not quiesce, or the switch is cancelled --------------------

#[tokio::test(start_paused = true)]
async fn row_c_a_that_cannot_quiesce_in_the_budget_resumes() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    let started = tokio::time::Instant::now();
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "handing-over", "environment");
    assert!(started.elapsed() >= rig.cfg.timings.handover);
    settle().await;
    let log = rig.driver.log();
    let kill = log.iter().position(|l| l == "kill 2.0.0").unwrap();
    let resume = log.iter().position(|l| l == "send 1.0.0 resume").unwrap();
    assert!(kill < resume, "B is gone before A resumes: {log:?}");
    assert!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .is_none()
    );
    assert!(rig.disk.state.lock().unwrap().handover_removed >= 1);
    assert!(core.rt.journal.as_ref().unwrap().switch.is_none());
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
}

#[tokio::test(start_paused = true)]
async fn a_standby_exit_queued_after_loaded_is_caught_before_hand_over() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            crate::testkit::loaded(),
            Step::Exit(false, "exited after loaded"),
        ],
    );
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "new-version");
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    assert_eq!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .unwrap()
            .version,
        "2.0.0"
    );
    assert!(
        !rig.driver
            .log()
            .contains(&"send 1.0.0 hand_over".to_owned())
    );
    assert!(
        !rig.driver
            .log()
            .contains(&"send 2.0.0 take_over".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn a_standby_failure_keeps_its_blame_when_a_cannot_resume() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            crate::testkit::loaded(),
            Step::Exit(false, "exited after loaded"),
        ],
    );
    rig.driver
        .on("1.0.0", "resume", vec![Step::Exit(false, "resume failed")]);
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "new-version");
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    assert_eq!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .unwrap()
            .version,
        "2.0.0"
    );
}

async fn standby_failure_during_handover(lost: bool) {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    let (tx, mut rx) = unbounded_channel();
    let shared = rig.shared.clone();
    let started = tokio::time::Instant::now();
    let inject = async {
        let mut replies = Vec::new();
        while let Some(reply) = rx.recv().await {
            let handing_over =
                matches!(&reply, EndpointReply::Progress { phase, .. } if phase == "handing-over");
            replies.push(reply);
            if handing_over {
                // Snapshot progress is the event that synchronizes the fault,
                // not a sleep racing the hand-over budget.
                assert_eq!(shared.snapshot().switch.unwrap().phase, "handing-over");
                rig.driver.emit(
                    "2.0.0",
                    if lost {
                        crate::driver::Event::Lost("standby channel lost".to_owned())
                    } else {
                        crate::driver::Event::Exited(crate::driver::ExitInfo {
                            code: Some(42),
                            success: false,
                            text: "standby crashed".to_owned(),
                        })
                    },
                );
                break;
            }
        }
        while let Some(reply) = rx.recv().await {
            replies.push(reply);
        }
        replies
    };
    let mut core = core_of!(rig);
    let ((), replies) = tokio::join!(core.handle_switch(update(false), tx), inject);
    let result = done_result(&replies);
    failed(&result, "handing-over", "new-version");
    assert!(
        started.elapsed() < rig.cfg.timings.handover,
        "B's failure must not wait for A's hand-over deadline"
    );
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    assert_eq!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .unwrap()
            .version,
        "2.0.0"
    );
    assert!(rig.driver.log().contains(&"send 1.0.0 resume".to_owned()));
    assert!(
        !rig.driver
            .log()
            .contains(&"send 2.0.0 take_over".to_owned())
    );
    assert!(rig.disk.state.lock().unwrap().handover_removed > 0);
}

#[tokio::test(start_paused = true)]
async fn a_standby_exit_during_handover_resumes_a_without_waiting_for_the_budget() {
    standby_failure_during_handover(false).await;
}

#[tokio::test(start_paused = true)]
async fn a_standby_loss_during_handover_waits_for_b_exit_then_resumes_a() {
    standby_failure_during_handover(true).await;
}

#[tokio::test(start_paused = true)]
async fn row_c_a_cancel_while_quiescing_resumes_a_and_adds_no_record() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let canceller = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), canceller);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    let result = done_result(&drain(&mut rx));
    assert_eq!(result["result"], "cancelled");
    settle().await;
    assert!(rig.driver.log().contains(&"send 1.0.0 resume".to_owned()));
    assert!(
        rig.disk.state.lock().unwrap().state.is_empty(),
        "a cancel adds no record"
    );
    assert!(core.rt.journal.as_ref().unwrap().switch.is_none());
    assert!(core.rt.journal.as_ref().unwrap().outcomes.is_empty());
}

// ---- row d: A exits during the hand over ---------------------------------------

#[tokio::test(start_paused = true)]
async fn row_d_a_exits_during_the_hand_over_and_the_fresh_from_runs() {
    let mut rig = Rig::new().await;
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "handing-over", "environment");
    assert_eq!(result["returned"], true);
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()));
    // B is not promoted: the fresh `from` is a second start of 1.0.0.
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 2);
    let order = rig.disk.order();
    let r1 = order.iter().position(|n| n == "J-R1").unwrap();
    let r2 = order.iter().position(|n| n == "J-R2").unwrap();
    assert!(r1 < r2);
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert_eq!(journal.active.version, "1.0.0");
    assert_eq!(journal.confirmed.version, "1.0.0");
    let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
    assert_eq!(outcome.record.returned_to.as_deref(), Some("1.0.0"));
    assert!(
        outcome.mark_failed.is_none(),
        "the environment, not the release"
    );
    assert!(core.rt.returning.is_none());
}

// ---- row e: B does not take over ------------------------------------------------

#[tokio::test(start_paused = true)]
async fn row_e_only_a_lease_prefix_is_the_environments_blame() {
    for reason in [
        "handover: handover.json is missing",
        "fail-before-ready",
        "the lease is odd",
    ] {
        let mut rig = Rig::new().await;
        rig.driver.on(
            "2.0.0",
            "take_over",
            vec![Step::Msg(crate::protocol::Message::TakeFailed {
                reason: reason.to_owned(),
            })],
        );
        let (_core, replies) = go!(rig, update(false));
        failed(&done_result(&replies), "taking-over", "new-version");
    }
}

#[tokio::test(start_paused = true)]
async fn a_worker_ping_is_ignored() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    let id = *core.rt.workers.keys().next().expect("a worker");
    let before = core.rt.workers[&id].status_seq;
    core.note_message(id, &crate::protocol::Message::Ping);
    assert_eq!(core.rt.workers[&id].status_seq, before);
}

#[tokio::test(start_paused = true)]
async fn row_e_take_failed_on_the_lease_hands_back_and_resumes_a() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "take_over",
        vec![Step::Msg(crate::protocol::Message::TakeFailed {
            reason: "lease: held by another process".to_owned(),
        })],
    );
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "taking-over", "environment");
    settle().await;
    let log = rig.driver.log();
    // L9.16: B's gate never opened, so it is stopped, not handed over, and
    // gone before A resumes.
    assert!(!log.contains(&"send 2.0.0 hand_over".to_owned()), "{log:?}");
    let stop = log.iter().position(|l| l == "send 2.0.0 stop").unwrap();
    let resume = log.iter().position(|l| l == "send 1.0.0 resume").unwrap();
    assert!(stop < resume, "{log:?}");
    assert!(!log.contains(&"kill 2.0.0".to_owned()), "it exited on stop");
    assert!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .is_none()
    );
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
}

#[tokio::test(start_paused = true)]
async fn row_e_b_exits_while_taking_over() {
    let mut rig = Rig::new().await;
    rig.driver
        .on("2.0.0", "take_over", vec![Step::Exit(false, "crashed")]);
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "taking-over", "new-version");
    settle().await;
    let log = rig.driver.log();
    assert!(
        !log.contains(&"send 2.0.0 hand_over".to_owned()),
        "B is already gone"
    );
    assert!(log.contains(&"send 1.0.0 resume".to_owned()));
    assert_eq!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .unwrap()
            .version,
        "2.0.0"
    );
}

#[tokio::test(start_paused = true)]
async fn row_e_no_ready_in_the_ready_timeout() {
    let mut rig = Rig::new().await;
    rig.driver.on("2.0.0", "take_over", vec![]);
    let (_core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "taking-over", "new-version");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"send 2.0.0 hand_over".to_owned()));
    assert!(log.contains(&"send 1.0.0 resume".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn row_e_a_b_that_will_not_answer_is_killed_before_a_resumes() {
    let mut rig = Rig::new().await;
    rig.driver.on("2.0.0", "take_over", vec![]);
    rig.driver.on("2.0.0", "hand_over", vec![]);
    let (_core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "taking-over", "new-version");
    settle().await;
    let log = rig.driver.log();
    let stop = log.iter().position(|l| l == "send 2.0.0 stop").unwrap();
    let resume = log.iter().position(|l| l == "send 1.0.0 resume").unwrap();
    assert!(stop < resume, "A resumes only once B is gone: {log:?}");
}

// ---- L9.16: a B that cannot hand over is dismissed, never resumed -----------------

#[tokio::test(start_paused = true)]
async fn row_e_a_failed_hand_over_dismisses_b_with_stop_then_its_handle_and_resumes_a() {
    let mut rig = Rig::new().await;
    rig.driver.on("2.0.0", "take_over", vec![]);
    rig.driver.on("2.0.0", "hand_over", vec![]);
    rig.driver.on("2.0.0", "stop", vec![]);
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "taking-over", "new-version");
    settle().await;
    let log = rig.driver.log();
    let hand = log
        .iter()
        .position(|l| l == "send 2.0.0 hand_over")
        .unwrap();
    let stop = log.iter().position(|l| l == "send 2.0.0 stop").unwrap();
    let kill = log.iter().position(|l| l == "kill 2.0.0").unwrap();
    let resume = log.iter().position(|l| l == "send 1.0.0 resume").unwrap();
    assert!(hand < stop && stop < kill && kill < resume, "{log:?}");
    assert!(
        log.iter().all(|l| l != "send 2.0.0 resume"),
        "B is never resumed"
    );
    assert_eq!(
        stated(&rig.disk, result["id"].as_str().unwrap())
            .mark_failed
            .unwrap()
            .version,
        "2.0.0"
    );
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    // handover.json is deleted only once A is back.
    let order = rig.disk.order();
    assert_eq!(
        rig.disk.state.lock().unwrap().handover_removed,
        1,
        "{order:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn row_e_then_row_l_keeps_bs_blame_and_mark() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "take_over",
        vec![Step::Msg(crate::protocol::Message::TakeFailed {
            reason: "fail-before-ready".to_owned(),
        })],
    );
    rig.driver.on("1.0.0", "resume", vec![]);
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "taking-over", "new-version");
    assert_eq!(result["returned"], true);
    let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
    assert_eq!(
        outcome
            .mark_failed
            .expect("B's release stays marked")
            .version,
        "2.0.0"
    );
    assert!(core.rt.active.is_some());
}

// ---- row l: A does not resume ---------------------------------------------------

#[tokio::test(start_paused = true)]
async fn row_l_a_that_does_not_resume_is_replaced_by_the_fresh_from() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver.on("1.0.0", "resume", vec![]);
    rig.driver.on("1.0.0", "stop", vec![]);
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    assert_eq!(result["result"], "failed-to-load");
    assert_eq!(result["returned"], true);
    settle().await;
    let log = rig.driver.log();
    assert!(
        log.contains(&"send 1.0.0 stop".to_owned()),
        "A is told to stop before its handle is used"
    );
    assert!(
        log.contains(&"kill 1.0.0".to_owned()),
        "A is stopped by its handle"
    );
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 2);
    assert!(core.rt.active.is_some());
}

// ---- the window: rows f, g, i, j, k ------------------------------------------------

/// Runs the core's loop for `secs`, then stops it.
macro_rules! window {
    ($core:ident, $tx:ident, $secs:expr) => {{
        let stopper = async {
            tokio::time::sleep(Duration::from_secs($secs)).await;
            let _ = $tx.send(Command::Stop);
        };
        let (exit, ()) = tokio::join!($core.run(), stopper);
        assert_eq!(exit, Exit::Stopped);
    }};
}

/// Row j has no later automatic attempt: the return, restart and watch state
/// are cleared, and with the paused clock moved past every deadline the core
/// has, no worker is started. The longest is `install_retry_max` (one hour);
/// the others are the drain cap (30 minutes), the watch window (10 minutes)
/// and the restart backoff cap (60 seconds). Two hours passes them all.
macro_rules! no_later_attempt {
    ($core:ident, $rig:ident) => {{
        assert!($core.rt.returning.is_none(), "the return is over");
        assert!($core.rt.restart.is_none(), "no restart is scheduled");
        assert!($core.rt.watch.is_none(), "no watch is running");
        let spawns = $rig.driver.places();
        for _ in 0..24 {
            tokio::time::advance(Duration::from_secs(5 * 60)).await;
            $core.on_tick().await;
        }
        assert_eq!(
            $rig.driver.places(),
            spawns,
            "no worker starts after the return"
        );
        assert!($core.rt.returning.is_none());
        assert!($core.rt.restart.is_none());
        assert!($core.rt.watch.is_none());
    }};
}

/// Runs the core's loop for `secs` and leaves it running (no stop), so what it
/// does afterwards is its own doing.
macro_rules! window_running {
    ($core:ident, $secs:expr) => {{
        tokio::select! {
            exit = $core.run() => panic!("the core exited: {exit:?}"),
            () = tokio::time::sleep(Duration::from_secs($secs)) => {}
        }
    }};
}

/// Runs the core in one-second steps until the return has ended (the journal
/// has no open switch), so the state is read before the next tick could clear
/// a restart that was scheduled by mistake.
macro_rules! until_returned {
    ($core:ident) => {{
        let mut steps = 0;
        while $core
            .rt
            .journal
            .as_ref()
            .is_none_or(|journal| journal.switch.is_some())
        {
            window_running!($core, 1);
            steps += 1;
            assert!(steps < 200, "the return never ended");
        }
    }};
}

fn rollback_to(version: &str, sha: &str) -> crate::core::SwitchRequest {
    crate::core::SwitchRequest {
        to: version.to_owned(),
        sha256: sha.to_owned(),
        pin: String::new(),
        how: open_ferry_update::journal::How::Rollback,
        auto: false,
        admitted: crate::protocol::Admission {
            while_off: false,
            mode_gen: 0,
        },
    }
}

fn with_previous(rig: &mut Rig) {
    let rt = rig.rt.as_mut().unwrap();
    let journal = rt.journal.as_mut().unwrap();
    journal.previous = Some(crate::testkit::artifact("0.9.0"));
    rig.disk.state.lock().unwrap().on_disk = Some(journal.clone());
}

#[tokio::test(start_paused = true)]
async fn row_f_b_exits_inside_the_window_and_the_fresh_from_runs() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "go",
        vec![
            Step::Sleep(Duration::from_secs(60)),
            Step::Exit(false, "crashed"),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    window!(core, command_tx, 120);

    let order = rig.disk.order();
    let r1 = order.iter().position(|n| n == "J-R1").unwrap();
    let r2 = order.iter().position(|n| n == "J-R2").unwrap();
    assert!(r1 < r2, "{order:?}");
    assert!(
        !order.contains(&"J5".to_owned()),
        "the window never ended cleanly"
    );
    assert!(rig.disk.state.lock().unwrap().install_calls.is_empty());
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert_eq!(journal.active.version, "1.0.0");
    assert_eq!(journal.confirmed.version, "1.0.0");
    let outcome = stated(&rig.disk, &id);
    assert_eq!(
        outcome.record.result.as_deref(),
        Some("failed-after-switch")
    );
    assert_eq!(outcome.record.returned_to.as_deref(), Some("1.0.0"));
    assert_eq!(outcome.mark_failed.unwrap().version, "2.0.0");
    let log = rig.driver.log();
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 2);
}

#[tokio::test(start_paused = true)]
async fn row_g_a_fresh_from_that_will_not_start_is_retried_then_previous_is_tried() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    assert_eq!(result["returned"], false);
    assert!(core.rt.degraded.is_some());
    assert!(core.rt.returning.is_some());
    window!(core, command_tx, 200);

    let log = rig.driver.log();
    let from_tries = log.iter().filter(|l| *l == "spawn 1.0.0 active").count() - 1;
    assert_eq!(from_tries, 3, "three tries of `from` first: {log:?}");
    assert!(log.contains(&"spawn 0.9.0 active".to_owned()));
    let journal = core.rt.journal.as_ref().unwrap();
    assert_eq!(journal.active.version, "0.9.0");
    assert_eq!(journal.confirmed.version, "0.9.0");
    assert!(core.rt.degraded.is_none());
    // The choice of `previous` was journalled before it was tried (J-R1 twice).
    let order = rig.disk.order();
    assert_eq!(
        order.iter().filter(|n| *n == "J-R1").count(),
        2,
        "{order:?}"
    );
}

// 6.1 g: when no rung of the return can start, the status is `degraded` and
// "the outcome of d or f is already recorded". The forward switch's own record
// gets its row d or f result (with `mark_failed` for f's blame on the new
// version), `returned_to` stays absent, the decision stays journalled for the
// retries, and a replay of the journal's outcome adds nothing.
#[tokio::test(start_paused = true)]
async fn row_g_the_outcome_is_recorded_when_no_rung_can_start() {
    use open_ferry_update::data_dir::DataDir;
    use open_ferry_update::journal::JournalStore;
    use open_ferry_update::state::State;
    for case in ["d: A exits while handing over", "f: B exits in the window"] {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDir::at(temp.path());
        let mut rig = Rig::new().await;
        with_previous(&mut rig);
        rig.disk.real = Some(crate::disk::RealDisk::new(
            data.clone(),
            temp.path().join("installed"),
            "open-ferry".to_owned(),
            JournalStore::new(temp.path()),
        ));
        rig.disk.real_state = true;
        if case.starts_with('d') {
            rig.driver
                .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
        } else {
            rig.driver.on(
                "2.0.0",
                "go",
                vec![
                    Step::Sleep(Duration::from_secs(60)),
                    Step::Exit(false, "crashed"),
                ],
            );
        }
        // Every rung of the return fails: `from`, then `previous`.
        rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
        rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
        let command_tx = rig.command_tx.clone();
        let (mut core, replies) = go!(rig, update(false));
        let first = done_result(&replies);
        if case.starts_with('d') {
            assert_eq!(first["returned"], false, "{case}: {first}");
        }
        let id = first["id"].as_str().unwrap().to_owned();
        window!(core, command_tx, 400);
        assert!(core.rt.degraded.is_some(), "{case}: nothing started");
        assert!(core.rt.returning.is_some(), "{case}: still being retried");
        let journal = core.rt.journal.as_ref().unwrap();
        assert!(
            journal
                .switch
                .as_ref()
                .is_some_and(|switch| switch.return_.is_some()),
            "{case}: the decision stays journalled"
        );
        let state = State::load_strict(&data.state_file()).unwrap();
        assert_eq!(state.history.len(), 1, "{case}: {state:?}");
        let record = &state.history[0];
        assert_eq!(record.id, id, "{case}");
        if case.starts_with('d') {
            assert_eq!(record.result.as_deref(), Some("failed-to-load"), "{case}");
            assert_eq!(record.phase.as_deref(), Some("handing-over"), "{case}");
            assert!(state.failed.is_empty(), "{case}: environment blame");
        } else {
            assert_eq!(
                record.result.as_deref(),
                Some("failed-after-switch"),
                "{case}"
            );
            assert_eq!(record.phase.as_deref(), Some("watching"), "{case}");
            assert_eq!(state.failed, ["2.0.0"], "{case}: mark_failed(to)");
        }
        assert_eq!(record.returned_to, None, "{case}: nothing started");
        // Written once, however long the retries ran.
        let order = rig.disk.order();
        assert_eq!(
            order.iter().filter(|step| *step == "S3").count(),
            1,
            "{case}: {order:?}"
        );
        // Delivered: nothing is owed, so a restart's replay has nothing to add.
        assert!(
            core.rt.journal.as_ref().unwrap().outcomes.is_empty(),
            "{case}"
        );
        assert!(core.rt.pending.is_empty(), "{case}");
    }
}

// A return whose first rung can't start records its outcome without
// `returned_to`; when a later rung does start, that same record (by id) is
// given `returned_to`, and the history still holds one entry.
#[tokio::test(start_paused = true)]
async fn row_g_a_later_rung_that_starts_gives_the_same_record_its_returned_to() {
    use open_ferry_update::data_dir::DataDir;
    use open_ferry_update::journal::JournalStore;
    use open_ferry_update::state::State;
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::at(temp.path());
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.disk.real = Some(crate::disk::RealDisk::new(
        data.clone(),
        temp.path().join("installed"),
        "open-ferry".to_owned(),
        JournalStore::new(temp.path()),
    ));
    rig.disk.real_state = true;
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    // `from` never starts; `previous` does.
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let first = State::load_strict(&data.state_file()).unwrap();
    assert_eq!(first.history.len(), 1, "{first:?}");
    assert_eq!(first.history[0].returned_to, None, "nothing had started");
    window!(core, command_tx, 200);
    assert!(core.rt.degraded.is_none());
    assert!(core.rt.returning.is_none());
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert_eq!(journal.confirmed.version, "0.9.0");
    let state = State::load_strict(&data.state_file()).unwrap();
    assert_eq!(state.history.len(), 1, "one entry: {state:?}");
    assert_eq!(state.history[0].id, id);
    assert_eq!(state.history[0].result.as_deref(), Some("failed-to-load"));
    assert_eq!(state.history[0].returned_to.as_deref(), Some("0.9.0"));
}

// The same, when the state write fails: the outcome is in the journal on disk,
// shown as owed, and a replay (the next start's first act, by id) gives the
// state the record once, however often it is replayed.
#[tokio::test(start_paused = true)]
async fn row_g_an_owed_unstarted_outcome_is_replayed_once() {
    use open_ferry_update::data_dir::DataDir;
    use open_ferry_update::state::State;
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::at(temp.path());
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
    rig.disk.fail_state("S3", 100);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    window!(core, command_tx, 60);
    assert!(core.rt.degraded.is_some());
    let on_disk = rig.disk.on_disk().expect("the journal was written");
    let owed = on_disk
        .outcomes
        .iter()
        .find(|outcome| outcome.record.id == id)
        .expect("the outcome is in the journal on disk");
    assert_eq!(owed.record.result.as_deref(), Some("failed-to-load"));
    assert_eq!(owed.record.returned_to, None);
    assert!(
        on_disk
            .switch
            .as_ref()
            .is_some_and(|switch| switch.return_.is_some())
    );
    // A restart replays it; a second restart before the journal is cleared
    // replays it again.
    for _ in 0..2 {
        crate::statefile::apply_locked(&data, "replay", owed).unwrap();
        let state = State::load_strict(&data.state_file()).unwrap();
        assert_eq!(state.history.len(), 1, "{state:?}");
        assert_eq!(state.history[0].id, id);
        assert_eq!(state.history[0].result.as_deref(), Some("failed-to-load"));
    }
}

#[tokio::test(start_paused = true)]
async fn row_i_a_failed_install_is_retried_and_never_undoes_the_switch() {
    let mut rig = Rig::new().await;
    rig.disk.state.lock().unwrap().install_fail = 1;
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    // Just after the window: J5 landed, the install failed once.
    let first = async {
        tokio::time::sleep(Duration::from_secs(620)).await;
        rig.disk.state.lock().unwrap().install_calls.len()
    };
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(700)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, calls, ()) = tokio::join!(core.run(), first, stopper);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(calls, 1);
    assert_eq!(rig.disk.state.lock().unwrap().install_calls.len(), 2);
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.install_pending.is_none());
    assert_eq!(journal.confirmed.version, "2.0.0");
    let record = stated(&rig.disk, &id).record;
    assert!(record.confirmed);
    assert_eq!(record.installed_binary.as_deref(), Some("replaced"));
    // The failed attempt was recorded when it happened.
    let tried = rig.disk.state.lock().unwrap().state_writes.clone();
    assert!(tried.iter().any(|(_, o, _)| {
        o.record
            .installed_binary
            .as_deref()
            .is_some_and(|text| text.starts_with("failed"))
    }));
}

#[tokio::test(start_paused = true)]
async fn row_j_a_rollback_target_that_exits_returns_to_the_version_it_came_from() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver.on(
        "0.9.0",
        "go",
        vec![
            Step::Sleep(Duration::from_secs(30)),
            Step::Exit(false, "crashed"),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, rollback_to("0.9.0", crate::testkit::SHA_3));
    let result = done_result(&replies);
    assert_eq!(result["result"], "rolled-back");
    let id = result["id"].as_str().unwrap().to_owned();
    let _ = command_tx;
    until_returned!(core);
    assert!(core.rt.returning.is_none(), "the return is over");
    assert!(core.rt.restart.is_none(), "no restart is scheduled");
    assert!(core.rt.watch.is_none(), "no watch is running");
    window_running!(core, 120);

    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert_eq!(journal.active.version, "1.0.0");
    assert_eq!(journal.confirmed.version, "1.0.0");
    let outcome = stated(&rig.disk, &id);
    assert_eq!(
        outcome.record.result.as_deref(),
        Some("failed-after-switch")
    );
    assert_eq!(outcome.record.how, "rollback");
    assert_eq!(outcome.record.returned_to.as_deref(), Some("1.0.0"));
    assert!(outcome.mark_failed.is_none(), "a rollback marks nothing");
    no_later_attempt!(core, rig);
}

#[tokio::test(start_paused = true)]
async fn a_rollback_to_previous_confirms_and_swaps_the_two() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, rollback_to("0.9.0", crate::testkit::SHA_3));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    window!(core, command_tx, 700);
    let journal = core.rt.journal.as_ref().unwrap();
    assert_eq!(journal.confirmed.version, "0.9.0");
    assert_eq!(journal.previous.as_ref().unwrap().version, "1.0.0");
    let record = stated(&rig.disk, &id).record;
    assert_eq!(record.result.as_deref(), Some("rolled-back"));
    assert!(record.confirmed);
}

#[tokio::test(start_paused = true)]
async fn row_k_a_lost_b_is_stopped_by_its_handle_before_the_fresh_from_starts() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "go",
        vec![Step::Sleep(Duration::from_secs(10)), Step::Lost],
    );
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    window!(core, command_tx, 200);
    let log = rig.driver.log();
    let close = log.iter().position(|l| l == "close 2.0.0").unwrap();
    let kill = log.iter().position(|l| l == "kill 2.0.0").unwrap();
    let fresh = log.iter().rposition(|l| l == "spawn 1.0.0 active").unwrap();
    assert!(close < kill && kill < fresh, "{log:?}");
    let outcome = stated(&rig.disk, &id);
    assert_eq!(
        outcome.record.result.as_deref(),
        Some("failed-after-switch")
    );
}

#[tokio::test(start_paused = true)]
async fn row_6_2_a_lost_a_is_stopped_by_its_handle_then_the_fresh_from_runs() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![Step::Lost]);
    let started = tokio::time::Instant::now();
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "handing-over", "environment");
    assert!(
        started.elapsed() >= rig.cfg.timings.stop,
        "waited the stop budget"
    );
    settle().await;
    let log = rig.driver.log();
    let kill_a = log.iter().position(|l| l == "kill 1.0.0").unwrap();
    let fresh = log.iter().rposition(|l| l == "spawn 1.0.0 active").unwrap();
    assert!(kill_a < fresh, "{log:?}");
}

// ---- admission, cancel and mode ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_automatic_request_is_refused_while_updates_are_off() {
    let mut rig = Rig::new().await;
    rig.host.set_mode(true, 1);
    let (core, replies) = go!(rig, update(true));
    assert!(
        matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason == "updates are off")
    );
    assert!(!core.rt.switching);
    assert!(rig.disk.order().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_request_with_no_live_pin_or_a_wrong_hash_or_the_running_version_is_refused() {
    let mut no_pin = update(false);
    no_pin.pin = "nope".to_owned();
    let mut rig = Rig::new().await;
    let (_c, replies) = go!(rig, no_pin);
    assert!(
        matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason.contains("pin"))
    );
    let mut wrong = update(false);
    wrong.sha256 = crate::testkit::SHA_3.to_owned();
    let mut rig = Rig::new().await;
    let (_c, replies) = go!(rig, wrong);
    assert!(
        matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason.contains("hashes"))
    );
    let mut same = update(false);
    same.to = "1.0.0".to_owned();
    same.sha256 = crate::testkit::SHA_1.to_owned();
    let mut rig = Rig::new().await;
    let (_c, replies) = go!(rig, same);
    assert!(
        matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason.contains("already"))
    );
    let mut rig = Rig::new().await;
    let (_c, replies) = go!(rig, rollback_to("0.9.0", crate::testkit::SHA_3));
    assert!(
        matches!(&replies[..], [EndpointReply::Error { reason, .. }] if reason.contains("previous"))
    );
    assert!(rig.disk.order().is_empty(), "nothing was written");
}

#[tokio::test(start_paused = true)]
async fn a_second_switch_is_refused_while_one_runs_and_an_update_while_watching() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(5)),
            crate::testkit::loaded(),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (tx, mut rx) = unbounded_channel();
    let (tx2, mut rx2) = unbounded_channel();
    let mut core = core_of!(rig);
    let other = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = command_tx.send(Command::Switch {
            request: update(false),
            replies: tx2,
        });
    };
    tokio::join!(core.handle_switch(update(false), tx), other);
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    let refused = drain(&mut rx2);
    assert!(
        matches!(&refused[..], [EndpointReply::Error { reason, .. }] if reason == "busy: switching"),
        "{refused:?}"
    );
    // The window is open now, so a further update is refused as busy.
    let (tx3, mut rx3) = unbounded_channel();
    core.handle_switch(update(false), tx3).await;
    assert!(
        matches!(&drain(&mut rx3)[..], [EndpointReply::Error { reason, .. }] if reason == "busy: watching")
    );
}

#[tokio::test(start_paused = true)]
async fn checkpoint_one_cancels_when_the_mode_was_turned_off_after_admission() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            crate::testkit::loaded(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let off = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 1);
    };
    // Not `auto`: the sign-in wait would race the test's timer (A-N1).
    tokio::join!(core.handle_switch(update(false), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()));
    assert!(
        log.iter().all(|l| !l.starts_with("send 1.0.0 hand_over")),
        "A was never told"
    );
    assert!(rig.disk.state.lock().unwrap().state.is_empty());
}

#[tokio::test(start_paused = true)]
async fn checkpoint_two_cancels_after_the_hand_over_and_resumes_a() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            crate::testkit::handed_over(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let off = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 1);
    };
    tokio::join!(core.handle_switch(update(false), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()));
    assert!(log.contains(&"send 1.0.0 resume".to_owned()), "{log:?}");
    assert!(!log.contains(&"send 2.0.0 take_over".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_manual_switch_admitted_while_off_proceeds_unless_the_mode_changed_after() {
    // Off at admission, and unchanged: the manual switch goes through.
    let mut rig = Rig::new().await;
    rig.host.set_mode(true, 4);
    let (_core, replies) = go!(rig, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");

    // Off at admission, then the mode changes during the hand over: the
    // supervisor's own mode_gen moved, so checkpoint 2 cancels.
    let mut rig = Rig::new().await;
    rig.host.set_mode(true, 4);
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            crate::testkit::handed_over(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let change = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 5);
    };
    tokio::join!(core.handle_switch(update(false), tx), change);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
}

// ---- mode off acts at once in a cancellable wait (5.3) ------------------------------

#[tokio::test(start_paused = true)]
async fn mode_off_during_a_held_hand_over_resumes_a_at_once_without_a_record() {
    let mut rig = Rig::new().await;
    // A's hand over is held for 6 s (a fake worker plays one script at a time,
    // so its `resumed` follows).
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(6)),
            crate::testkit::handed_over(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let off = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 1);
        // Before A's `handed_over` (6 s): B is already killed, from the tick
        // after the change (A is told to resume once B's exit is seen).
        tokio::time::sleep(Duration::from_secs(3)).await;
        let log = rig.driver.log();
        assert!(log.contains(&"kill 2.0.0".to_owned()), "at 4 s: {log:?}");
        // A is told to resume while its hand over is still held (it ends at
        // 6 s), not after it: `received` notes a message when it is sent,
        // where `log` shows it once A has finished the earlier script.
        let sent = rig.driver.received();
        assert!(
            sent.contains(&"send 1.0.0 resume".to_owned()),
            "no resume while the hand over is held: {sent:?}"
        );
        assert!(
            sent.iter()
                .all(|line| !line.starts_with("send 2.0.0 take_over")),
            "{sent:?}"
        );
    };
    tokio::join!(core.handle_switch(update(false), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()), "{log:?}");
    assert!(log.contains(&"send 1.0.0 resume".to_owned()), "{log:?}");
    assert!(
        log.iter().all(|l| !l.starts_with("send 2.0.0 take_over")),
        "{log:?}"
    );
    assert!(rig.disk.state.lock().unwrap().state.is_empty(), "a record");
}

/// The barrier `switch.before_checkpoint_two` sits between `handed_over` and
/// checkpoint 2: the mode turned off exactly there is acted on at checkpoint 2.
#[cfg(feature = "blue-green-test")]
#[tokio::test(start_paused = true)]
async fn mode_off_at_the_barrier_before_checkpoint_two_cancels_there() {
    use open_ferry_update::data_dir::DataDir;
    let dir = tempfile::tempdir().unwrap();
    let seams = dir.path().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("switch.before_checkpoint_two.hold"), []).unwrap();
    let mut rig = Rig::new().await;
    rig.cfg.data = DataDir::at(dir.path().to_path_buf());
    rig.driver
        .on("1.0.0", "hand_over", vec![crate::testkit::handed_over()]);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let off = async {
        let mut turns = 0_u32;
        while !seams.join("switch.before_checkpoint_two.arrived").exists() {
            turns += 1;
            assert!(turns < 2_000_000, "the barrier was never reached");
            tokio::task::yield_now().await;
        }
        // `handed_over` was received: A has handed over, `take_over` was not sent.
        assert!(
            rig.driver
                .log()
                .contains(&"send 1.0.0 hand_over".to_owned())
        );
        assert!(
            rig.driver
                .log()
                .iter()
                .all(|l| !l.starts_with("send 2.0.0 take_over"))
        );
        rig.host.set_mode(true, 1);
        std::fs::write(seams.join("switch.before_checkpoint_two.release"), []).unwrap();
    };
    tokio::join!(core.handle_switch(update(false), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()), "{log:?}");
    assert!(log.contains(&"send 1.0.0 resume".to_owned()), "{log:?}");
    assert!(
        log.iter().all(|l| !l.starts_with("send 2.0.0 take_over")),
        "{log:?}"
    );
}

// ---- item 14: the supervisor stamps the admission ---------------------------------

#[tokio::test(start_paused = true)]
async fn the_supervisor_stamps_the_admission_and_ignores_the_clients() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            crate::testkit::handed_over(),
        ],
    );
    // The client claims it was admitted while off, at the generation the host
    // will reach. The mode is on, so the real admission was not while off.
    let mut request = update(false);
    request.admitted = crate::protocol::Admission {
        while_off: true,
        mode_gen: 1,
    };
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let off = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 1);
    };
    tokio::join!(core.handle_switch(request, tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
}

// ---- item 13: the mode is re-read while a switch is open --------------------------

#[tokio::test(start_paused = true)]
async fn a_mode_flip_off_and_back_on_cancels_a_switch_admitted_while_off() {
    let mut rig = Rig::new().await;
    rig.host.set_mode(true, 4);
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(6)),
            crate::testkit::handed_over(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    // On at 1.5s, off again at 4s, with the same generation the host reported:
    // only the once-a-second re-reading can see the flip. At checkpoint 2 the
    // mode is off, as at admission.
    let flip = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        rig.host.set_mode(false, 4);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        rig.host.set_mode(true, 4);
    };
    tokio::join!(core.handle_switch(update(false), tx), flip);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
}

#[tokio::test(start_paused = true)]
async fn an_explicit_cancel_is_too_late_once_take_over_was_sent() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "take_over",
        vec![
            Step::Sleep(Duration::from_secs(5)),
            Step::Msg(crate::protocol::Message::Ready),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let canceller = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), canceller);
    assert_eq!(answer, crate::snapshot::CancelAnswer::TooLate);
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
}

#[tokio::test(start_paused = true)]
async fn an_automatic_switch_waits_for_pending_sign_ins_and_a_manual_one_does_not() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "ping",
        vec![Step::Msg(crate::protocol::Message::Status {
            mode: "active".to_owned(),
            live: 0,
            signins_pending: 2,
            phase: "serving".to_owned(),
        })],
    );
    let started = tokio::time::Instant::now();
    let (_core, replies) = go!(rig, update(true));
    let result = done_result(&replies);
    assert_eq!(result["result"], "switched", "{result}");
    let waited = started.elapsed();
    assert!(
        waited >= rig.cfg.timings.signin_wait,
        "waited the whole cap: {waited:?}"
    );

    let mut rig2 = Rig::new().await;
    rig2.driver.on(
        "1.0.0",
        "ping",
        vec![Step::Msg(crate::protocol::Message::Status {
            mode: "active".to_owned(),
            live: 0,
            signins_pending: 2,
            phase: "serving".to_owned(),
        })],
    );
    let started = tokio::time::Instant::now();
    let (_core, replies) = go!(rig2, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(rig2.driver.log_of("send 1.0.0 ping").is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stop_during_a_switch_ends_it_without_touching_the_journal_further() {
    let mut rig = Rig::new().await;
    rig.driver.on("2.0.0", "spawn", vec![]);
    let command_tx = rig.command_tx.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let stop = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let _ = command_tx.send(Command::Stop);
    };
    tokio::join!(core.handle_switch(update(false), tx), stop);
    assert!(core.rt.stop_requested);
    assert!(
        matches!(&drain(&mut rx).last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("stopping"))
    );
    // Item 17: before take over a stop reaches the standby (`stop` at once,
    // then its handle) and clears the journal as a cancel would, with no
    // outcome. The fake standby exits on the `stop`.
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"send 2.0.0 stop".to_owned()), "{log:?}");
    assert!(
        core.rt
            .workers
            .values()
            .all(|w| w.artifact.version != "2.0.0")
    );
    assert!(rig.disk.on_disk().unwrap().switch.is_none());
    assert!(rig.disk.on_disk().unwrap().outcomes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stop_after_take_over_began_leaves_the_journal_for_recovery() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "take_over",
        vec![
            Step::Sleep(Duration::from_secs(30)),
            Step::Msg(crate::protocol::Message::Ready),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (tx, _rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let stop = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let _ = command_tx.send(Command::Stop);
    };
    tokio::join!(core.handle_switch(update(false), tx), stop);
    assert!(core.rt.stop_requested);
    assert!(rig.disk.on_disk().unwrap().switch.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_rollback_during_the_window_supersedes_the_forward_record() {
    let mut rig = Rig::new().await;
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let forward = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(rollback_to("1.0.0", crate::testkit::SHA_1), tx)
        .await;
    let back = done_result(&drain(&mut rx))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let record = stated(&rig.disk, &forward).record;
    assert_eq!(record.superseded_by.as_deref(), Some(back.as_str()));
    assert_eq!(record.result.as_deref(), Some("switched"));
    assert!(!record.confirmed);
    window!(core, command_tx, 700);
    let journal = core.rt.journal.as_ref().unwrap();
    assert_eq!(journal.confirmed.version, "1.0.0");
    let rolled = stated(&rig.disk, &back).record;
    assert_eq!(rolled.result.as_deref(), Some("rolled-back"));
    assert!(rolled.confirmed);
    assert!(!stated(&rig.disk, &forward).record.confirmed);
}

#[tokio::test(start_paused = true)]
async fn a_failed_rollback_restores_a_fresh_forward_window() {
    let mut rig = Rig::new().await;
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let forward = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let old_deadline = core.rt.watch.as_ref().unwrap().deadline;
    tokio::time::advance(Duration::from_secs(20)).await;
    rig.driver.on(
        "1.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "rollback load failed".to_owned(),
        })],
    );
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(rollback_to("1.0.0", crate::testkit::SHA_1), tx)
        .await;
    failed(&done_result(&drain(&mut rx)), "loading", "new-version");
    let watch = core
        .rt
        .watch
        .as_ref()
        .expect("B needs a confirmation window");
    assert_eq!(watch.open.id, forward);
    assert!(watch.deadline > old_deadline);
    assert_eq!(
        watch.deadline,
        tokio::time::Instant::now() + rig.cfg.timings.watch
    );
    let journal = rig.disk.on_disk().unwrap();
    let switch = journal.switch.unwrap();
    assert_eq!(switch.id, forward);
    assert_eq!(switch.phase, Phase::Watching);
    assert!(switch.watch_until.is_some());
    assert_eq!(journal.active.version, "2.0.0");
    window!(core, command_tx, 700);
    assert_eq!(rig.disk.on_disk().unwrap().confirmed.version, "2.0.0");
    assert!(stated(&rig.disk, &forward).record.confirmed);
}

#[tokio::test(start_paused = true)]
async fn row_j_after_a_forward_window_confirms_the_returned_to_version_per_t4() {
    let mut rig = Rig::new().await;
    let command_tx = rig.command_tx.clone();
    let (mut core, _) = go!(rig, update(false));
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(rollback_to("1.0.0", crate::testkit::SHA_1), tx)
        .await;
    let back = done_result(&drain(&mut rx))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.driver.emit(
        "1.0.0",
        crate::driver::Event::Exited(crate::driver::ExitInfo {
            success: false,
            code: Some(42),
            text: "rollback target crashed".to_owned(),
        }),
    );
    let _ = command_tx;
    until_returned!(core);
    assert!(core.rt.returning.is_none(), "the return is over");
    assert!(core.rt.restart.is_none(), "no restart is scheduled");
    assert!(core.rt.watch.is_none(), "no watch is running");
    window_running!(core, 120);
    let journal = rig.disk.on_disk().unwrap();
    assert_eq!(journal.active.version, "2.0.0");
    assert_eq!(journal.confirmed.version, "2.0.0");
    assert!(
        journal.switch.is_none(),
        "T4 resolves row j through J-R2, not a restored watch"
    );
    assert_eq!(
        stated(&rig.disk, &back).record.returned_to.as_deref(),
        Some("2.0.0")
    );
    no_later_attempt!(core, rig);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_rollback_restores_a_fresh_forward_window() {
    let mut rig = Rig::new().await;
    let (mut core, replies) = go!(rig, update(false));
    let forward = done_result(&replies)["id"].as_str().unwrap().to_owned();
    tokio::time::advance(Duration::from_secs(20)).await;
    rig.driver.on("1.0.0", "spawn", vec![]);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let cancel = async {
        while let Some(reply) = rx.recv().await {
            if matches!(reply, EndpointReply::Progress { .. }) {
                assert_eq!(
                    shared.request_cancel(),
                    crate::snapshot::CancelAnswer::Cancelled
                );
                return;
            }
        }
        panic!("rollback closed before its loading progress");
    };
    tokio::join!(
        core.handle_switch(rollback_to("1.0.0", crate::testkit::SHA_1), tx),
        cancel
    );
    let watch = core
        .rt
        .watch
        .as_ref()
        .expect("cancel must restore B's window");
    assert_eq!(watch.open.id, forward);
    assert_eq!(
        watch.deadline,
        tokio::time::Instant::now() + rig.cfg.timings.watch
    );
    let journal = rig.disk.on_disk().unwrap();
    assert_eq!(journal.switch.unwrap().id, forward);
    assert_eq!(journal.active.version, "2.0.0");
}

// A cancel is bound to the operation it was accepted for. The end of a
// cancelled rollback that ran during a forward watch is held between retiring
// its cancel window and the picture that follows; a cancel asked for then finds
// the old rollback's picture, and must not be accepted, nor leave a flag that
// marks the restored watch as cancelled.
#[tokio::test(start_paused = true)]
async fn a_cancel_asked_for_after_an_operation_ended_does_not_mark_the_restored_watch() {
    let mut rig = Rig::new().await;
    let (mut core, replies) = go!(rig, update(false));
    let forward = done_result(&replies)["id"].as_str().unwrap().to_owned();
    tokio::time::advance(Duration::from_secs(20)).await;
    rig.driver.on("1.0.0", "spawn", vec![]);
    let shared = rig.shared.clone();
    let late = std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let shared = shared.clone();
        let late = late.clone();
        rig.shared.on_operation_end(move || {
            *late.lock().unwrap() = Some(shared.request_cancel());
        });
    }
    let (tx, mut rx) = unbounded_channel();
    let cancel = async {
        while let Some(reply) = rx.recv().await {
            if matches!(reply, EndpointReply::Progress { .. }) {
                assert_eq!(
                    shared.request_cancel(),
                    crate::snapshot::CancelAnswer::Cancelled
                );
                return;
            }
        }
        panic!("rollback closed before its loading progress");
    };
    tokio::join!(
        core.handle_switch(rollback_to("1.0.0", crate::testkit::SHA_1), tx),
        cancel
    );
    let answer = late.lock().unwrap().take().expect("the end hook ran");
    assert_ne!(
        answer,
        crate::snapshot::CancelAnswer::Cancelled,
        "no cancel is accepted for an operation that has ended"
    );
    let shown = rig.shared.snapshot().switch.expect("the restored watch");
    assert_eq!(shown.id, forward);
    assert_eq!(shown.status, "watching", "{shown:?}");
    assert!(!rig.shared.cancel_requested(), "no cancel is left over");
    core.publish();
    let shown = rig.shared.snapshot().switch.expect("the restored watch");
    assert_eq!(shown.status, "watching", "{shown:?}");
}

// ---- the journal write sequences (J1-J8, J-R1, J-R2, S1-S3) -------------------------

/// What `plan_startup` would do with the journal as the disk holds it now.
fn plan_from(disk: &crate::testkit::FakeDisk) -> open_ferry_update::recovery::RecoveryPlan {
    use open_ferry_update::journal::JournalLoad;
    use open_ferry_update::recovery::{Facts, plan_startup};
    let journal = disk.on_disk().expect("a journal is on disk");
    let versions = [
        ("1.0.0", crate::testkit::SHA_1),
        ("2.0.0", crate::testkit::SHA_2),
        ("0.9.0", crate::testkit::SHA_3),
    ]
    .into_iter()
    .map(|(v, s)| (v.to_owned(), s.to_owned()))
    .collect();
    let facts = Facts {
        now: chrono::Utc::now(),
        self_sha256: None,
        self_version: "1.0.0".to_owned(),
        installed_sha256: disk.state.lock().unwrap().installed.clone(),
        installed_size: 1,
        installed_path: std::path::PathBuf::from("/unit-test-bin/open-ferry"),
        versions,
    };
    plan_startup(&JournalLoad::Loaded(journal), &facts)
}

fn runs(plan: &open_ferry_update::recovery::RecoveryPlan, version: &str) {
    match &plan.run {
        open_ferry_update::recovery::RunChoice::Version(artifact) => {
            assert_eq!(artifact.version, version, "{plan:?}");
        }
        other => panic!("expected to run {version}, the plan runs {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn journal_j1_failing_ends_the_switch_before_anything_starts() {
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J1", 99);
    let (_core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "environment");
    // The failure is recorded in the state file; the journal never opened.
    assert_eq!(rig.disk.order(), ["J1!", "S3"].map(String::from));
    assert!(
        rig.disk.state.lock().unwrap().consumed.is_empty(),
        "no pin spent"
    );
    assert!(rig.disk.on_disk().unwrap().switch.is_none());
    settle().await;
    let log: Vec<String> = rig
        .driver
        .log()
        .into_iter()
        .filter(|line| !line.ends_with("peers_gone"))
        .collect();
    assert_eq!(log, ["spawn 1.0.0 active"].map(String::from));
    runs(&plan_from(&rig.disk), "1.0.0");
}

#[tokio::test(start_paused = true)]
async fn journal_j2_failing_returns_to_a_and_recovery_runs_confirmed() {
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J2", 99);
    let (core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "handing-over", "environment");
    settle().await;
    assert!(
        !rig.driver
            .log()
            .contains(&"send 1.0.0 hand_over".to_owned())
    );
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    let plan = plan_from(&rig.disk);
    runs(&plan, "1.0.0");
    assert!(plan.install == open_ferry_update::recovery::InstallAction::None);
}

#[tokio::test(start_paused = true)]
async fn journal_j3_failing_resumes_a_and_never_sends_take_over() {
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J3", 99);
    let (core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "taking-over", "environment");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"send 1.0.0 resume".to_owned()), "{log:?}");
    assert!(!log.contains(&"send 2.0.0 take_over".to_owned()));
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    runs(&plan_from(&rig.disk), "1.0.0");
}

#[tokio::test(start_paused = true)]
async fn journal_j4_failing_rolls_forward_in_memory_and_a_restart_reverts_to_from() {
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J4", 99);
    let (core, replies) = go!(rig, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");
    assert_eq!(active_version(&core).as_deref(), Some("2.0.0"));
    assert!(core.rt.journal_stale, "the journal is behind and retried");
    // The disk still says taking-over: a crash now reverts to A (3.3).
    let on_disk = rig.disk.on_disk().unwrap();
    assert_eq!(
        on_disk.switch.as_ref().unwrap().phase.as_str(),
        "taking-over"
    );
    let plan = plan_from(&rig.disk);
    runs(&plan, "1.0.0");
    assert!(
        plan.upserts
            .iter()
            .any(|o| o.record.result.as_deref() == Some("failed-to-load"))
    );
}

#[tokio::test(start_paused = true)]
async fn journal_j4_retries_until_it_lands_and_nothing_installs_meanwhile() {
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J4", 1);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");
    window!(core, command_tx, 700);
    let order = rig.disk.order();
    let failed_at = order.iter().position(|n| n == "J4!").unwrap();
    assert!(
        order[failed_at..].iter().any(|n| n == "J4" || n == "J5"),
        "{order:?}"
    );
    assert!(!core.rt.journal_stale);
}

#[tokio::test(start_paused = true)]
async fn journal_j5_failing_keeps_b_running_and_installs_only_after_it_lands() {
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J5", 2);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let early = async {
        // The window ended (watch) but J5 has failed twice: not installed yet.
        tokio::time::sleep(rig.cfg.timings.watch + Duration::from_secs(1)).await;
        rig.disk.state.lock().unwrap().install_calls.len()
    };
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(3000)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (_, calls_early, ()) = tokio::join!(core.run(), early, stopper);
    assert_eq!(
        calls_early, 0,
        "the installed file is untouched until J5 lands"
    );
    let order = rig.disk.order();
    let j5 = order.iter().position(|n| n == "J5").unwrap();
    let s2 = j5 + order[j5..].iter().position(|n| n == "S2").unwrap();
    assert!(
        order.iter().filter(|n| *n == "J5!").count() == 2,
        "{order:?}"
    );
    assert!(j5 < s2, "{order:?}");
    assert_eq!(rig.disk.state.lock().unwrap().install_calls.len(), 1);
    assert!(rig.disk.record(&id).unwrap().confirmed);
    assert_eq!(rig.disk.on_disk().unwrap().confirmed.version, "2.0.0");
}

#[tokio::test(start_paused = true)]
async fn t5_a_crash_after_j5_and_before_s2_replays_the_record_from_the_journal() {
    let mut rig = Rig::new().await;
    rig.disk.fail_state("S2", 99);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    window!(core, command_tx, 700);
    // J5 landed; S2 never did. The disk journal carries the outcome.
    let on_disk = rig.disk.on_disk().unwrap();
    assert_eq!(on_disk.confirmed.version, "2.0.0");
    assert!(on_disk.switch.is_none());
    assert!(
        on_disk.outcomes.iter().any(|o| o.record.id == id),
        "{on_disk:?}"
    );
    assert!(rig.disk.record(&id).is_none() || !rig.disk.record(&id).unwrap().confirmed);
    // A restart replays it: the plan upserts the confirmed record and runs 2.0.0.
    let plan = plan_from(&rig.disk);
    runs(&plan, "2.0.0");
    assert!(
        plan.upserts
            .iter()
            .any(|o| o.record.id == id && o.record.confirmed)
    );
}

#[tokio::test(start_paused = true)]
async fn s1_failing_is_retried_and_the_record_lands_without_blocking_the_switch() {
    let mut rig = Rig::new().await;
    rig.disk.fail_state("S1", 2);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    assert_eq!(done_result(&replies)["result"], "switched");
    assert!(rig.disk.record(&id).is_none(), "S1 has not landed yet");
    window!(core, command_tx, 120);
    assert!(rig.disk.record(&id).is_some(), "retried until it landed");
    let on_disk = rig.disk.on_disk().unwrap();
    assert!(
        on_disk.outcomes.iter().all(|o| o.record.id != id),
        "J8 removed it by id"
    );
}

#[tokio::test(start_paused = true)]
async fn t4_a_crash_during_the_return_runs_the_journalled_target() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 5);
    // J-R1 landed with the target; J-R2 has not: the crash leaves this.
    let on_disk = rig.disk.on_disk().unwrap();
    let ret = on_disk.switch.as_ref().and_then(|s| s.return_.clone());
    let target = ret.expect("J-R1 is on disk").target;
    let plan = plan_from(&rig.disk);
    runs(&plan, &target.version);
    // Recovery records the return without claiming it started, and keeps the
    // switch with the return's target until a worker of it has started.
    assert_eq!(plan.journal_write_name, "J-R1");
    let written = plan.journal_write.unwrap();
    let kept = written.switch.as_ref().and_then(|s| s.return_.as_ref());
    assert_eq!(kept.map(|r| r.target.version.clone()), Some(target.version));
    assert!(
        plan.upserts.iter().all(|o| o.record.returned_to.is_none()),
        "{:?}",
        plan.upserts
    );
}

#[tokio::test(start_paused = true)]
async fn t4_j_r2_failing_keeps_the_worker_and_recovery_plans_the_same_return() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "go",
        vec![
            Step::Sleep(Duration::from_secs(60)),
            Step::Exit(false, "crashed"),
        ],
    );
    rig.disk.fail_journal("J-R2", 99);
    // Later writes fail too, so the disk stays as J-R1 left it (a crash now).
    rig.disk.fail_journal("J8", 99);
    rig.disk.fail_journal("J-retry", 99);
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 120);
    assert!(core.rt.journal_stale);
    assert!(core.rt.journal_retry.is_some());
    assert!(rig.disk.order().iter().any(|name| name == "J-retry!"));
    let log = rig.driver.log();
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 2);
    let on_disk = rig.disk.on_disk().unwrap();
    assert!(
        on_disk.switch.as_ref().expect("switch").return_.is_some(),
        "{:?}",
        rig.disk.order()
    );
    let plan = plan_from(&rig.disk);
    runs(&plan, "1.0.0");
    assert_eq!(plan.journal_write_name, "J-R1");
}

#[tokio::test(start_paused = true)]
async fn j7_a_cancel_clears_the_switch_without_a_record_and_recovery_has_nothing_to_add() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![Step::Sleep(Duration::from_secs(5)), handed_over()],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let canceller = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        shared.request_cancel()
    };
    let ((), _) = tokio::join!(core.handle_switch(update(false), tx), canceller);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    let on_disk = rig.disk.on_disk().unwrap();
    assert!(on_disk.switch.is_none());
    assert!(on_disk.outcomes.is_empty(), "a cancel adds no record");
    assert!(rig.disk.state.lock().unwrap().state.is_empty());
}

// ---- the journal's own seams (U's `journal.fail/before/after:<write>`) ----------------

#[cfg(feature = "blue-green-test")]
mod seams {
    use super::*;
    use crate::disk::{ArtifactError, Disk};
    use crate::install::InstallResult;
    use crate::statefile::StateWriteError;
    use chrono::{DateTime, Utc};
    use open_ferry_update::journal::{Artifact, Journal, JournalError, JournalLoad, JournalStore};

    /// The journal goes through U's real store (and its seams) in a temp
    /// directory; everything else is the in-memory disk.
    struct SeamDisk {
        fake: crate::testkit::FakeDisk,
        store: JournalStore,
    }

    impl Disk for SeamDisk {
        fn write_journal(&self, journal: &mut Journal, name: &str) -> Result<(), JournalError> {
            self.store.write(journal, name)
        }
        fn write_state(&self, name: &str, outcome: &Outcome) -> Result<(), StateWriteError> {
            self.fake.write_state(name, outcome)
        }
        fn artifact(&self, version: &str, sha256: &str) -> Result<Artifact, ArtifactError> {
            self.fake.artifact(version, sha256)
        }
        fn check_pin(
            &self,
            id: &str,
            version: &str,
            sha256: &str,
            now: DateTime<Utc>,
        ) -> Result<(), String> {
            self.fake.check_pin(id, version, sha256, now)
        }
        fn consume_pin(&self, id: &str) -> Result<(), String> {
            self.fake.consume_pin(id)
        }
        fn installed_sha256(&self) -> Option<String> {
            self.fake.installed_sha256()
        }
        fn installed_size(&self) -> Option<u64> {
            self.fake.installed_size()
        }
        fn installed_switched_off(&self) -> (Option<String>, u64) {
            self.fake.installed_switched_off()
        }
        fn choose_aside(&self) -> Result<String, String> {
            self.fake.choose_aside()
        }
        fn install(
            &self,
            target: &Artifact,
            expected: Option<&str>,
            from: &str,
            aside: Option<&str>,
        ) -> InstallResult {
            self.fake.install(target, expected, from, aside)
        }
        fn remove_handover(&self) {
            self.fake.remove_handover();
        }
    }

    fn seam_disk(rig: &Rig, dir: &std::path::Path) -> SeamDisk {
        let store = JournalStore::new(dir).with_retry_wait(Duration::ZERO);
        let mut first = rig.rt.as_ref().unwrap().journal.clone().unwrap();
        store.write(&mut first, "init").unwrap();
        std::fs::create_dir_all(dir.join(".blue-green-test")).unwrap();
        SeamDisk {
            fake: crate::testkit::FakeDisk::default(),
            store,
        }
        .with_fake_of(rig)
    }

    impl SeamDisk {
        fn with_fake_of(self, rig: &Rig) -> Self {
            {
                let mut mine = self.fake.state.lock().unwrap();
                let theirs = rig.disk.state.lock().unwrap();
                mine.artifacts.clone_from(&theirs.artifacts);
                mine.pins.clone_from(&theirs.pins);
                mine.installed.clone_from(&theirs.installed);
            }
            self
        }
    }

    macro_rules! core_with {
        ($rig:ident, $disk:expr) => {
            crate::core::Core::new(
                &$rig.cfg,
                &$rig.driver,
                &$rig.host,
                $disk,
                &$rig.clock,
                &mut $rig.registry,
                $rig.shared.clone(),
                &mut $rig.commands,
                $rig.rt.take().expect("the runtime"),
            )
        };
    }

    fn seam(dir: &std::path::Path, name: &str, kind: &str) {
        std::fs::write(
            dir.join(".blue-green-test").join(format!("{name}.{kind}")),
            b"",
        )
        .unwrap();
    }

    /// Holds the writer at `name`, reads the journal the instant it arrives
    /// (a crash there), then lets it go.
    fn crash_at(dir: &std::path::Path, name: &str) -> std::thread::JoinHandle<JournalLoad> {
        seam(dir, name, "hold");
        let dir = dir.to_owned();
        let name = name.to_owned();
        std::thread::spawn(move || {
            let seams = dir.join(".blue-green-test");
            while !seams.join(format!("{name}.arrived")).exists() {
                std::thread::sleep(Duration::from_millis(5));
            }
            let load = JournalStore::new(&dir).load().unwrap();
            std::fs::write(seams.join(format!("{name}.release")), b"").unwrap();
            load
        })
    }

    fn plan(
        load: &JournalLoad,
        installed: Option<&str>,
    ) -> open_ferry_update::recovery::RecoveryPlan {
        use open_ferry_update::recovery::{Facts, plan_startup};
        let versions = [
            ("1.0.0", crate::testkit::SHA_1),
            ("2.0.0", crate::testkit::SHA_2),
            ("0.9.0", crate::testkit::SHA_3),
        ]
        .into_iter()
        .map(|(v, s)| (v.to_owned(), s.to_owned()))
        .collect();
        plan_startup(
            load,
            &Facts {
                now: chrono::Utc::now(),
                self_sha256: None,
                self_version: "1.0.0".to_owned(),
                installed_sha256: installed.map(str::to_owned),
                installed_size: 1,
                installed_path: std::path::PathBuf::from("/unit-test-bin/open-ferry"),
                versions,
            },
        )
    }

    #[tokio::test(start_paused = true)]
    async fn seam_fail_j3_leaves_a_journal_that_recovers_to_a() {
        let dir = tempfile::tempdir().unwrap();
        let mut rig = Rig::new().await;
        let disk = seam_disk(&rig, dir.path());
        seam(dir.path(), "journal.fail_J3", "fail");
        // The clean-up write fails too, so the disk is as a crash would leave it.
        seam(dir.path(), "journal.fail_J7", "fail");
        seam(dir.path(), "journal.fail_J8", "fail");
        let (tx, mut rx) = unbounded_channel();
        let mut core = core_with!(rig, &disk);
        core.handle_switch(update(false), tx).await;
        failed(&done_result(&drain(&mut rx)), "taking-over", "environment");
        let load = disk.store.load().unwrap();
        let JournalLoad::Loaded(journal) = &load else {
            panic!("{load:?}")
        };
        assert_eq!(journal.switch.as_ref().unwrap().phase, Phase::HandingOver);
        runs(&plan(&load, Some(crate::testkit::SHA_1)), "1.0.0");
    }

    #[tokio::test(start_paused = true)]
    async fn seam_after_j4_a_crash_in_the_window_reverts_to_from() {
        let dir = tempfile::tempdir().unwrap();
        let mut rig = Rig::new().await;
        let disk = seam_disk(&rig, dir.path());
        let snapshot = crash_at(dir.path(), "journal.after_J4");
        let (tx, mut rx) = unbounded_channel();
        let mut core = core_with!(rig, &disk);
        core.handle_switch(update(false), tx).await;
        assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
        let load = snapshot.join().unwrap();
        let JournalLoad::Loaded(journal) = &load else {
            panic!("{load:?}")
        };
        let switch = journal.switch.as_ref().unwrap();
        assert_eq!(switch.phase, Phase::Watching);
        assert_eq!(journal.active.version, "2.0.0");
        let plan = plan(&load, Some(crate::testkit::SHA_1));
        runs(&plan, "1.0.0");
        assert!(plan.upserts.iter().any(|o| {
            o.record.result.as_deref() == Some("failed-after-switch")
                && o.record.returned_to.as_deref() == Some("1.0.0")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn seam_before_j5_a_crash_still_reverts_and_after_j5_it_keeps_the_new_version() {
        for (name, version, kept) in [
            ("journal.before_J5", "1.0.0", false),
            ("journal.after_J5", "2.0.0", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut rig = Rig::new().await;
            let disk = seam_disk(&rig, dir.path());
            let snapshot = crash_at(dir.path(), name);
            let command_tx = rig.command_tx.clone();
            let (tx, mut rx) = unbounded_channel();
            let mut core = core_with!(rig, &disk);
            core.handle_switch(update(false), tx).await;
            let id = done_result(&drain(&mut rx))["id"]
                .as_str()
                .unwrap()
                .to_owned();
            window!(core, command_tx, 700);
            let load = snapshot.join().unwrap();
            let plan = plan(&load, Some(crate::testkit::SHA_1));
            runs(&plan, version);
            let JournalLoad::Loaded(journal) = &load else {
                panic!("{load:?}")
            };
            // T5: after J5 the confirmed record rides in the journal until S2.
            assert_eq!(
                journal.outcomes.iter().any(|o| o.record.id == id),
                kept,
                "{name}"
            );
            if kept {
                assert!(
                    plan.upserts
                        .iter()
                        .any(|o| o.record.id == id && o.record.confirmed)
                );
            }
        }
    }
}

// ---- the restart loop (9.2) ---------------------------------------------------------

fn crash(core_id: u64) -> (u64, crate::driver::Event) {
    (
        core_id,
        crate::driver::Event::Exited(crate::driver::ExitInfo {
            code: Some(1),
            success: false,
            text: "crashed".to_owned(),
        }),
    )
}

/// How many times `1.0.0` was started as the active worker at each of `times`.
async fn spawns_at(driver: crate::testkit::FakeDriver, times: &[u64]) -> Vec<usize> {
    let mut seen = Vec::new();
    let mut last = 0;
    for &at in times {
        tokio::time::sleep(Duration::from_secs(at - last)).await;
        last = at;
        seen.push(
            driver
                .log()
                .iter()
                .filter(|l| *l == "spawn 1.0.0 active")
                .count(),
        );
    }
    seen
}

#[tokio::test(start_paused = true)]
async fn an_active_worker_that_exits_outside_a_switch_is_restarted_after_the_first_wait() {
    let mut rig = Rig::new().await;
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let driver = rig.driver.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    let probe = spawns_at(driver, &[3, 8]);
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(9)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, seen, ()) = tokio::join!(core.run(), probe, stopper);
    assert_eq!(exit, Exit::Stopped);
    // Counts include the one the rig started: not before 5 s, then again.
    assert_eq!(seen, [1, 2]);
}

#[tokio::test(start_paused = true)]
async fn repeated_exits_wait_longer_each_time() {
    let mut rig = Rig::new().await;
    // Every restart of 1.0.0 dies a second after it starts.
    rig.driver.on(
        "1.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(1)),
            Step::Exit(false, "crashed"),
        ],
    );
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let driver = rig.driver.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    let probe = spawns_at(driver, &[4, 8, 15, 20, 30, 46]);
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(47)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, seen, ()) = tokio::join!(core.run(), probe, stopper);
    assert_eq!(exit, Exit::Stopped);
    // Restarts due at about 5 s, then +10 s, then +20 s after each crash.
    assert_eq!(seen, [1, 2, 2, 3, 3, 4], "{seen:?}");
}

#[tokio::test(start_paused = true)]
async fn a_restart_that_cannot_start_the_run_falls_back_to_previous_after_three_tries() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 400);
    let log = rig.driver.log();
    assert!(log.contains(&"spawn 0.9.0 active".to_owned()), "{log:?}");
    assert!(core.rt.degraded.is_none(), "running again");
}

#[tokio::test(start_paused = true)]
async fn the_welcome_carries_the_stop_messages_budget_not_the_kill_deadline() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    let request = core.spawn_request(
        &crate::testkit::artifact("2.0.0"),
        std::path::PathBuf::from("/unit-test-bin/open-ferry"),
        crate::protocol::Role::Active,
        None,
    );
    let timings = &rig.cfg.timings;
    assert_eq!(
        request.budgets.stop_ms,
        u64::try_from(timings.stop_message_budget().as_millis()).unwrap()
    );
    assert!(request.budgets.stop_ms < u64::try_from(timings.stop.as_millis()).unwrap());
}

#[tokio::test(start_paused = true)]
async fn a_later_terminal_outcome_retires_the_failed_s1_retry() {
    let mut rig = Rig::new().await;
    rig.disk.fail_state("S1", 1);
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    core.window_failure("B failed").await;
    assert_eq!(
        rig.disk.record(&id).unwrap().result.as_deref(),
        Some("failed-after-switch")
    );
    assert!(
        core.rt.pending.is_empty(),
        "S3 superseded the undelivered S1"
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    core.tick_background().await;
    assert_eq!(
        rig.disk.record(&id).unwrap().result.as_deref(),
        Some("failed-after-switch")
    );
}

#[tokio::test(start_paused = true)]
async fn an_install_patch_delivers_the_full_pending_confirmation() {
    let mut rig = Rig::new().await;
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let mut record = rig.disk.record(&id).unwrap();
    record.confirm();
    let outcome = Outcome {
        record,
        mark_failed: None,
    };
    core.rt
        .journal
        .as_mut()
        .unwrap()
        .push_outcome(outcome.clone());
    rig.disk.fail_state("S2", 1);
    assert!(!core.write_state("S2", &outcome, true));
    let mut patch = open_ferry_update::state::SwitchRecord {
        id: id.clone(),
        ..Default::default()
    };
    patch.installed_binary = Some("replaced".to_owned());
    let patch = Outcome {
        record: patch,
        mark_failed: None,
    };
    core.rt
        .journal
        .as_mut()
        .unwrap()
        .push_outcome(patch.clone());
    assert!(core.write_state("S2", &patch, true));
    assert!(
        rig.disk.record(&id).unwrap().confirmed,
        "the complete confirmation must land before J8"
    );
    assert!(core.rt.pending.is_empty());
    assert!(
        !core
            .rt
            .journal
            .as_ref()
            .unwrap()
            .outcomes
            .iter()
            .any(|o| o.record.id == id)
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_j8_keeps_the_structural_journal_retry() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    let outcome = Outcome {
        record: open_ferry_update::state::SwitchRecord {
            id: "test".to_owned(),
            ..Default::default()
        },
        mark_failed: None,
    };
    core.rt
        .journal
        .as_mut()
        .unwrap()
        .push_outcome(outcome.clone());
    rig.disk.fail_journal("J-R2", 1);
    rig.disk.fail_journal("J8", 1);
    assert!(core.write_journal("J-R2").is_err());
    assert!(core.write_state("S3", &outcome, true));
    assert!(core.rt.journal_stale);
    assert!(core.rt.journal_retry.is_some());
    tokio::time::advance(rig.cfg.timings.status_every).await;
    core.tick_background().await;
    assert!(!core.rt.journal_stale);
    assert!(rig.disk.order().contains(&"J-retry".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn a_kill_deadline_never_counts_as_an_exit() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "kill", vec![]);
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    let bound = rig.cfg.timings.stop * 2 + Duration::from_secs(6);
    assert!(
        tokio::time::timeout(bound, core.kill_and_wait(id))
            .await
            .is_err(),
        "no Exited event has arrived"
    );
    assert!(core.rt.workers[&id].lost);
    assert!(core.registry.running().contains(&id));
    assert!(
        rig.disk
            .on_disk()
            .unwrap()
            .workers
            .iter()
            .any(|worker| worker.pid == core.rt.workers[&id].pid && worker.lost)
    );
    assert_eq!(rig.shared.snapshot().state, "degraded");
    assert_eq!(rig.disk.state.lock().unwrap().handover_removed, 0);
    rig.driver.emit(
        "1.0.0",
        crate::driver::Event::Exited(crate::driver::ExitInfo {
            code: Some(1),
            success: false,
            text: "finally exited".to_owned(),
        }),
    );
    assert!(core.wait_exit(id).await);
    assert!(!core.rt.workers.contains_key(&id));
    assert!(core.rt.degraded.is_none(), "the termination wait ended");
}

#[tokio::test(start_paused = true)]
async fn a_service_stop_waits_for_survivors_after_the_kill_deadline() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "stop", vec![]);
    rig.driver.on("1.0.0", "kill", vec![]);
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    assert!(
        tokio::time::timeout(rig.cfg.timings.stop + Duration::from_secs(3), core.stop())
            .await
            .is_err()
    );
    assert!(core.registry.running().contains(&id));
    assert!(core.rt.workers.contains_key(&id));
    rig.driver.emit(
        "1.0.0",
        crate::driver::Event::Exited(crate::driver::ExitInfo {
            code: Some(1),
            success: false,
            text: "finally exited".to_owned(),
        }),
    );
    assert_eq!(core.stop().await, Exit::Stopped);
    assert!(core.registry.running().is_empty());
    assert!(core.rt.workers.is_empty());
}

fn recovered_started(
    rig: &Rig,
    journal: open_ferry_update::journal::Journal,
) -> crate::startup::Started {
    crate::startup::Started {
        lock: None,
        store: open_ferry_update::journal::JournalStore::new(rig.cfg.data.root()),
        journal: Some(journal),
        journal_stale: false,
        state_pending: false,
        run: None,
        switching_disabled: None,
        install_lock: None,
        switched_off: false,
        ladder_off_reason: false,
        degraded: Some("return targets missing".to_owned()),
        keep_all_versions: false,
        endpoint: None,
        listeners: std::sync::Arc::new(vec![]),
        worker: None,
        worker_greeted: None,
        run_failed: false,
        installed_rung: None,
        installed_sha256: None,
        notes: vec![],
    }
}

#[tokio::test(start_paused = true)]
async fn degraded_reentry_rebuilds_the_open_return_instead_of_restarting_b() {
    let mut rig = Rig::new().await;
    let (core, _) = go!(rig, update(false));
    let mut journal = core.rt.journal.clone().unwrap();
    drop(core);
    let target = journal.confirmed.clone();
    journal.switch.as_mut().unwrap().return_ = Some(open_ferry_update::journal::Return {
        target: target.clone(),
        blame: open_ferry_update::journal::Blame::NewVersion,
        why: "B rejected".to_owned(),
        decided: "2026-10-09T00:00:00Z".to_owned(),
        installed: false,
    });
    let started = recovered_started(&rig, journal);
    let runtime = crate::core::Runtime::from_started(
        &started,
        &rig.registry,
        &rig.cfg,
        &rig.clock,
        tokio::time::Instant::now(),
    );
    assert!(runtime.returning.is_some(), "recovery must retain J-R1");
    rig.rt = Some(runtime);
    let old_spawns = rig.driver.log_of("spawn 2.0.0").len();
    let mut core = core_of!(rig);
    tokio::time::advance(rig.cfg.timings.restart_first).await;
    core.on_tick().await;
    assert_eq!(active_version(&core).as_deref(), Some("1.0.0"));
    assert_eq!(rig.driver.log_of("spawn 2.0.0").len(), old_spawns);
    assert!(core.rt.journal.as_ref().unwrap().switch.is_none());
}

#[tokio::test(start_paused = true)]
async fn the_live_return_uses_the_installed_rung_only_by_its_pinned_hash() {
    for matches in [false, true] {
        let mut rig = Rig::new().await;
        let (mut core, _) = go!(rig, update(false));
        let watch = core.rt.watch.take().unwrap();
        let target = watch.open.from.clone();
        let ret = open_ferry_update::journal::Return {
            target: target.clone(),
            blame: open_ferry_update::journal::Blame::NewVersion,
            why: "rejected B".to_owned(),
            decided: core.stamp(),
            installed: false,
        };
        core.rt.returning = Some(crate::core::Returning {
            open: watch.open,
            initial_target: target.clone(),
            ret: ret.clone(),
            unstarted_recorded: false,
        });
        let journal = core.rt.journal.as_mut().unwrap();
        journal.switch.as_mut().unwrap().return_ = Some(ret);
        journal.installed = Some(open_ferry_update::journal::Installed {
            version: "0.8.0".to_owned(),
            sha256: "installed-hash".to_owned(),
        });
        rig.disk.state.lock().unwrap().installed =
            Some(if matches { "installed-hash" } else { "foreign" }.to_owned());
        rig.disk.state.lock().unwrap().installed_size = Some(4321);
        // The kept copy of `from` has used its three tries.
        let spent = crate::startup::Run {
            binary: rig.cfg.data.binary(&target.version, &rig.cfg.binary_name),
            artifact: target.clone(),
        };
        core.rt.restart = Some(crate::core::Restart {
            due: tokio::time::Instant::now(),
            progress: crate::ladder::Progress {
                current: None,
                spent: vec![spent],
            },
            recorded: false,
        });
        core.on_tick().await;
        if matches {
            let run = core.rt.run.as_ref().unwrap();
            assert_eq!(run.binary, rig.cfg.installed);
            assert_eq!(run.artifact.version, "0.8.0");
            assert_eq!(run.artifact.size, 4321, "the file's size, not 0");
            assert_eq!(core.rt.journal.as_ref().unwrap().confirmed.version, "0.8.0");
        } else {
            assert!(
                !rig.driver
                    .log()
                    .iter()
                    .any(|line| line == "spawn 0.8.0 active")
            );
            assert_eq!(
                core.rt.restart.as_ref().unwrap().progress,
                crate::ladder::Progress::default(),
                "retry the permitted ladder, not active=B"
            );
            assert!(core.rt.returning.is_some());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_drain_cap_stop_gets_a_handle_kill_deadline() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "release", vec![]);
    rig.driver.on("1.0.0", "stop", vec![]);
    let (mut core, _) = go!(rig, update(false));
    let a = *core
        .rt
        .workers
        .iter()
        .find(|(_, worker)| worker.role == WorkerRole::Draining)
        .unwrap()
        .0;
    let worker = core.rt.workers.get_mut(&a).unwrap();
    let drain = worker.drain.as_mut().unwrap();
    drain.deadline = tokio::time::Instant::now();
    drain.pinged = true;
    drain.stop_at = Some(tokio::time::Instant::now());
    core.tick_background().await;
    assert_eq!(
        core.rt.workers[&a].kill_at,
        Some(tokio::time::Instant::now() + rig.cfg.timings.stop)
    );
    tokio::time::advance(rig.cfg.timings.stop).await;
    core.tick_background().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while core.rt.workers.contains_key(&a) {
            let (id, event) = core.registry.next_event().await;
            core.background_event(id, event).await;
        }
    })
    .await
    .unwrap();
    assert!(rig.driver.log().iter().any(|line| line == "kill 1.0.0"));
    assert!(core.rt.workers[&core.rt.active.unwrap()].peers_gone_sent);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_during_j3_is_too_late_not_falsely_accepted() {
    let mut rig = Rig::new().await;
    rig.disk.state.lock().unwrap().cancel_during_j3 = Some(rig.shared.clone());
    let (_, replies) = go!(rig, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");
    assert_eq!(
        rig.disk.state.lock().unwrap().j3_cancel_answer,
        Some(crate::snapshot::CancelAnswer::TooLate)
    );
}

// ---- item 10: a journal write that landed but was not synced -------------------

fn at(order: &[String], name: &str) -> usize {
    order
        .iter()
        .position(|entry| entry == name)
        .unwrap_or_else(|| panic!("{name} is not in {order:?}"))
}

#[tokio::test(start_paused = true)]
async fn a_not_durable_j1_holds_the_pin_until_a_durable_write() {
    let mut rig = Rig::new().await;
    rig.disk.not_durable_journal("J1", 1);
    let (_core, replies) = go!(rig, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");
    let order = rig.disk.order();
    assert!(at(&order, "J1~") < at(&order, "J2"), "{order:?}");
    assert!(
        at(&order, "J2") < at(&order, "pin:pin"),
        "the pin went before a durable write: {order:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_not_durable_j5_holds_the_install_until_the_retry_is_durable() {
    let mut rig = Rig::new().await;
    rig.disk.not_durable_journal("J5", 1);
    // J8 follows J5 at once; it must not make the state durable by itself.
    rig.disk.not_durable_journal("J8", 1);
    let command_tx = rig.command_tx.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(700)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, ()) = tokio::join!(core.run(), stopper);
    assert_eq!(exit, Exit::Stopped);
    let order = rig.disk.order();
    assert!(at(&order, "J5~") < at(&order, "J8~"), "{order:?}");
    assert!(
        at(&order, "J-retry") < at(&order, "install"),
        "installed before the journal was durable: {order:?}"
    );
    assert_eq!(rig.disk.state.lock().unwrap().install_calls, ["2.0.0"]);
}

#[tokio::test(start_paused = true)]
async fn a_not_durable_j2_holds_the_handover_delete_until_a_durable_write() {
    let mut rig = Rig::new().await;
    rig.disk.not_durable_journal("J2", 1);
    rig.driver.on("1.0.0", "hand_over", vec![]);
    let (_core, replies) = go!(rig, update(false));
    failed(&done_result(&replies), "handing-over", "environment");
    let order = rig.disk.order();
    assert!(
        at(&order, "J-retry") < at(&order, "handover-removed"),
        "handover.json went before a durable write: {order:?}"
    );
}

// ---- items 21 and 30: the recorded install retry survives a restart -------------

#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_install_retry_time_the_journal_recorded() {
    use open_ferry_update::journal::InstallPending;
    let rig = Rig::new().await;
    let mut journal = rig.disk.on_disk().unwrap();
    let pending = |next_try: Option<&str>| InstallPending {
        target: crate::testkit::artifact("2.0.0"),
        source: "src".to_owned(),
        aside: String::new(),
        attempts: 3,
        next_try: next_try.map(str::to_owned),
        started: "2026-10-09T11:00:00Z".to_owned(),
        record_id: None,
    };
    let now = tokio::time::Instant::now();
    // The clock reads 12:00:00; the third retry was recorded for 12:04:00.
    for (recorded, due) in [
        (Some("2026-10-09T12:04:00Z"), Some(Duration::from_secs(240))),
        (Some("2026-10-09T11:00:00Z"), Some(Duration::ZERO)),
        (Some("not a time"), Some(rig.cfg.timings.install_retry)),
        (None, Some(rig.cfg.timings.install_retry)),
    ] {
        journal.install_pending = Some(pending(recorded));
        let started = recovered_started(&rig, journal.clone());
        let runtime =
            crate::core::Runtime::from_started(&started, &rig.registry, &rig.cfg, &rig.clock, now);
        assert_eq!(
            runtime.install_due,
            due.map(|wait| now + wait),
            "next_try {recorded:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_message_after_lost_is_not_a_live_workers_answer() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    let a = core.rt.active.unwrap();
    core.worker_lost(a, "the channel closed");
    rig.driver.emit(
        "1.0.0",
        crate::driver::Event::Message(crate::protocol::Message::Resumed),
    );
    let waited = core
        .wait_for(
            a,
            tokio::time::Instant::now() + Duration::from_secs(5),
            false,
            |m| matches!(m, crate::protocol::Message::Resumed),
        )
        .await;
    assert!(
        matches!(waited, crate::switch::Waited::Timeout),
        "a lost worker answered: {waited:?}"
    );
}

// ---- item 11: A exits or is lost before the hand over ---------------------------

fn a_exit_event() -> crate::driver::Event {
    crate::driver::Event::Exited(crate::driver::ExitInfo {
        code: Some(1),
        success: false,
        text: "a crashed".to_owned(),
    })
}

#[tokio::test(start_paused = true)]
async fn row_d_a_exits_while_b_loads_blames_the_environment_and_never_promotes_b() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(20)),
            crate::testkit::loaded(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let die = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exit_event());
    };
    tokio::join!(core.handle_switch(update(false), tx), die);
    let result = done_result(&drain(&mut rx));
    failed(&result, "loading", "environment");
    assert_eq!(result["returned"], true);
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()), "{log:?}");
    assert!(!log.contains(&"send 2.0.0 take_over".to_owned()));
    assert!(!log.contains(&"send 1.0.0 hand_over".to_owned()));
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 2);
    let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
    assert!(
        outcome.mark_failed.is_none(),
        "the environment, not the release"
    );
}

#[tokio::test(start_paused = true)]
async fn row_d_a_lost_while_b_loads_waits_for_a_and_never_promotes_b() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(20)),
            crate::testkit::loaded(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let lose = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver
            .emit("1.0.0", crate::driver::Event::Lost("gone".to_owned()));
    };
    tokio::join!(core.handle_switch(update(false), tx), lose);
    let result = done_result(&drain(&mut rx));
    failed(&result, "loading", "environment");
    settle().await;
    let log = rig.driver.log();
    assert!(log.contains(&"kill 2.0.0".to_owned()), "{log:?}");
    assert!(
        log.contains(&"kill 1.0.0".to_owned()),
        "A held the lease: {log:?}"
    );
    assert!(!log.contains(&"send 2.0.0 take_over".to_owned()));
}

// ---- item 12: the switch is in the snapshot, and cancellable, from admission ----

#[tokio::test(start_paused = true)]
async fn a_switch_is_visible_and_cancellable_while_it_waits_for_sign_ins() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "ping",
        vec![Step::Msg(crate::protocol::Message::Status {
            mode: "active".to_owned(),
            live: 0,
            signins_pending: 2,
            phase: "serving".to_owned(),
        })],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let watcher = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let view = shared.snapshot().switch;
        let cancel = shared.request_cancel();
        (view, cancel)
    };
    let ((), (view, cancel)) = tokio::join!(core.handle_switch(update(true), tx), watcher);
    let view = view.expect("the switch is in the snapshot during the sign-in wait");
    assert_eq!(view.phase, "loading");
    assert!(view.cancellable);
    assert_eq!(cancel, crate::snapshot::CancelAnswer::Cancelled);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
}

// ---- item 15: peers_gone (L9.15) ------------------------------------------------

#[tokio::test(start_paused = true)]
async fn peers_gone_goes_to_a_lone_active_worker_once_and_again_after_a_resume() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            crate::testkit::handed_over(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    // A, alone from the start, is told once; a repeat check is silent.
    core.peers_gone_check();
    core.peers_gone_check();
    settle().await;
    assert_eq!(rig.driver.log_of("send 1.0.0 peers_gone").len(), 1);
    // The switch cancels at checkpoint 2: A resumes, B is killed.
    let off = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 1);
    };
    tokio::join!(core.handle_switch(update(false), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    settle().await;
    // A resumed and is alone again: it is told again, and B never was.
    assert_eq!(rig.driver.log_of("send 1.0.0 peers_gone").len(), 2);
    assert!(rig.driver.log_of("send 2.0.0 peers_gone").is_empty());
}

#[tokio::test(start_paused = true)]
async fn peers_gone_waits_while_a_standby_is_alive() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(5)),
            crate::testkit::loaded(),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let probe = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.driver.log_of("send 1.0.0 peers_gone").len()
    };
    let ((), sent_during_load) = tokio::join!(core.handle_switch(update(false), tx), probe);
    assert_eq!(sent_during_load, 0, "B was loading");
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
}

// ---- item 18: a rollback checks the binary's own --version ----------------------

#[tokio::test(start_paused = true)]
async fn a_rollback_refuses_a_binary_whose_version_is_another() {
    for runner in [
        crate::testkit::ScriptedRunner(Ok("open-ferry 9.9.9".to_owned())),
        // Longer versions that contain the target as a substring.
        crate::testkit::ScriptedRunner(Ok("open-ferry 11.0.0".to_owned())),
        crate::testkit::ScriptedRunner(Ok("open-ferry 1.0.0-rc.1".to_owned())),
        crate::testkit::ScriptedRunner(Err("it would not start".to_owned())),
    ] {
        let mut rig = Rig::new().await;
        rig.cfg.runner = std::sync::Arc::new(runner);
        let (mut core, replies) = go!(rig, update(false));
        assert_eq!(done_result(&replies)["result"], "switched");
        let (tx, mut rx) = unbounded_channel();
        core.handle_switch(rollback_to("1.0.0", crate::testkit::SHA_1), tx)
            .await;
        let replies = drain(&mut rx);
        assert!(
            matches!(replies.last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("--version")),
            "{replies:?}"
        );
        assert!(!rig.driver.log().contains(&"spawn 1.0.0 standby".to_owned()));
    }
}

// ---- item 25: the sign-in wait trusts only a status after its own ping -----------

#[tokio::test(start_paused = true)]
async fn the_sign_in_wait_does_not_trust_a_status_older_than_its_ping() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "ping",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Msg(crate::protocol::Message::Status {
                mode: "active".to_owned(),
                live: 0,
                signins_pending: 0,
                phase: "serving".to_owned(),
            }),
        ],
    );
    let mut core = core_of!(rig);
    let a = core.rt.active.unwrap();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        core.signin_wait(a).await,
        crate::switch::SignIn::Go
    ));
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "it waited for the reply, not the stale count: {:?}",
        started.elapsed()
    );
}

// ---- item 24: a lost or silent A gets the stop budget first -----------------------

#[tokio::test(start_paused = true)]
async fn a_lost_a_gets_the_stop_budget_to_exit_before_its_handle_is_used() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver.on("1.0.0", "resume", vec![Step::Lost]);
    rig.driver.on("1.0.0", "stop", vec![]);
    let stop = rig.cfg.timings.stop;
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    // Watches the log: A is lost when it is sent `resume`, and the handle is
    // used when `kill 1.0.0` is logged. Time is measured from the Lost event,
    // not from the start of the switch.
    let script = async {
        let mut lost_at = None;
        let mut killed_at = None;
        for _ in 0..40_000 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let now = tokio::time::Instant::now();
            if lost_at.is_none() && !rig.driver.log_of("send 1.0.0 resume").is_empty() {
                lost_at = Some(now);
            }
            if killed_at.is_none() && !rig.driver.log_of("kill 1.0.0").is_empty() {
                killed_at = Some(now);
                break;
            }
        }
        (lost_at, killed_at)
    };
    let ((), (lost_at, killed_at)) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(done_result(&drain(&mut rx))["result"], "failed-to-load");
    let (lost_at, killed_at) = (
        lost_at.expect("A was sent resume"),
        killed_at.expect("A was killed"),
    );
    assert!(
        killed_at - lost_at >= stop,
        "its handle was used {:?} after the Lost event, before the stop budget {stop:?}",
        killed_at - lost_at
    );
}

// ---- item 28: the lost mark reaches the journal ----------------------------------

#[tokio::test(start_paused = true)]
async fn a_lost_worker_is_marked_in_the_journal_workers() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    let a = core.rt.active.unwrap();
    core.worker_lost(a, "test");
    let on_disk = rig.disk.on_disk().unwrap();
    assert!(
        on_disk.workers.iter().any(|worker| worker.lost),
        "{on_disk:?}"
    );
}

// ---- item 26: a worker spawn's handshake is selected against Stop ----------------

#[tokio::test(start_paused = true)]
async fn a_stop_during_a_hanging_spawn_ends_the_switch_and_a_second_switch_is_busy() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Hang);
    let command_tx = rig.command_tx.clone();
    let (tx, mut rx) = unbounded_channel();
    let (second_tx, mut second_rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let commands = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = command_tx.send(Command::Switch {
            request: update(false),
            replies: second_tx,
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let started = tokio::time::Instant::now();
    tokio::join!(core.handle_switch(update(false), tx), commands);
    assert!(
        started.elapsed() < rig.cfg.timings.load,
        "the stop did not wait for the load timeout"
    );
    assert!(core.rt.stop_requested);
    assert!(
        matches!(drain(&mut rx).last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("stopping"))
    );
    assert!(
        matches!(drain(&mut second_rx).last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("busy"))
    );
}

// ---- the handshake is cancellable (5.3) -------------------------------------------

/// B stalls before greeting (its child is made, the spawn never returns).
/// `act` runs at second 2 and ends the switch in some way; the switch must end
/// cancelled long before the load deadline, with no record and no failed mark,
/// and the child reaped.
async fn a_cancel_during_the_handshake(
    auto: bool,
    act: impl FnOnce(&crate::testkit::FakeHost, &crate::snapshot::Shared),
) {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Survive);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        act(&rig.host, &shared);
    };
    let started = tokio::time::Instant::now();
    tokio::join!(core.handle_switch(update(auto), tx), script);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the cancel did not wait for the load deadline ({:?})",
        started.elapsed()
    );
    assert!(started.elapsed() < rig.cfg.timings.load);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    settle().await;
    assert_eq!(
        rig.driver.log_of("exited 2.0.0").len(),
        1,
        "the child was reaped"
    );
    assert!(core.registry.survivors().is_empty());
    assert!(
        rig.disk.state.lock().unwrap().state.is_empty(),
        "no record, no failed mark"
    );
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert!(journal.outcomes.is_empty());
    assert_eq!(rig.driver.log_of("spawn 2.0.0").len(), 1);
    assert!(
        rig.driver
            .log()
            .iter()
            .all(|l| !l.starts_with("send 1.0.0 hand_over")),
        "nothing past checkpoint 1"
    );
}

#[tokio::test(start_paused = true)]
async fn an_explicit_cancel_ends_a_stalled_handshake_at_once() {
    a_cancel_during_the_handshake(false, |_, shared| {
        assert_eq!(
            shared.request_cancel(),
            crate::snapshot::CancelAnswer::Cancelled
        );
    })
    .await;
}

// 7: a cancel accepted just as the spawn fails wins: the handshake's wait may
// see the failure first, but the cancel was answered, so nothing is recorded.
#[tokio::test(start_paused = true)]
async fn an_accepted_cancel_wins_over_a_spawn_failure_that_is_ready_with_it() {
    use crate::testkit::SpawnHook;
    let mut rig = Rig::new().await;
    let shared = rig.shared.clone();
    let answer = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = answer.clone();
    rig.driver.fail_spawn(
        "2.0.0",
        SpawnFail::HookThenStart(SpawnHook(std::sync::Arc::new(move || {
            *seen.lock().unwrap() = Some(shared.request_cancel());
        }))),
    );
    let (_core, replies) = go!(rig, update(false));
    assert_eq!(
        *answer.lock().unwrap(),
        Some(crate::snapshot::CancelAnswer::Cancelled),
        "the cancel was accepted"
    );
    assert_eq!(done_result(&replies)["result"], "cancelled");
    assert!(
        rig.disk.state.lock().unwrap().state.is_empty(),
        "no record, no failed mark"
    );
}

#[tokio::test(start_paused = true)]
async fn mode_off_ends_a_stalled_handshake_of_an_automatic_switch_at_once() {
    a_cancel_during_the_handshake(true, |host, _| host.set_mode(true, 1)).await;
}

#[tokio::test(start_paused = true)]
async fn a_manual_switch_admitted_while_off_is_not_cancelled_by_a_stalled_handshakes_tick() {
    let mut rig = Rig::new().await;
    rig.host.set_mode(true, 4);
    rig.driver.fail_spawn("2.0.0", SpawnFail::Survive);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    // Off at admission and unchanged: no tick cancelled it, so B stalled to the
    // load deadline and the switch failed on it.
    let result = done_result(&drain(&mut rx));
    assert_ne!(result["result"], "cancelled", "{result:?}");
    assert!(
        rig.driver.log_of("exited 2.0.0").len() == 1,
        "the stalled child was reaped at the deadline"
    );
}

// ---- A-N2: a panic inside the switch leaves the journalled switch --------------

#[cfg(feature = "blue-green-test")]
#[tokio::test(start_paused = true)]
async fn a_panic_inside_the_switch_leaves_the_journalled_switch_for_the_restart() {
    let mut rig = Rig::new().await;
    let data = std::env::temp_dir().join(format!("ofp-s3-panic-{}", std::process::id()));
    let fail = data
        .join(".blue-green-test")
        .join("fault.supervisor-panic.fail");
    std::fs::create_dir_all(fail.parent().unwrap()).unwrap();
    std::fs::write(&fail, []).unwrap();
    rig.cfg.data = open_ferry_update::data_dir::DataDir::at(data.clone());
    let (tx, _rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let outcome = crate::reentry::CatchUnwind::new(core.handle_switch(update(false), tx)).await;
    let gone = !fail.exists();
    let _ = std::fs::remove_dir_all(&data);
    assert!(
        outcome.is_err_and(|message| message.contains("injected fault")),
        "the fault fired inside the switch"
    );
    assert!(gone, "the fault fires once");
    assert!(
        rig.disk.on_disk().unwrap().switch.is_some(),
        "the journalled switch is left for the restart"
    );
}

// ---- S4 --------------------------------------------------------------------------

/// A loop that never yields cannot be stopped by a timer, so a regression that
/// spins would hang the run. This ends the process instead, naming the test.
struct Watchdog(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Watchdog {
    fn new(test: &'static str) -> Self {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watched = done.clone();
        std::thread::spawn(move || {
            let end = std::time::Instant::now() + Duration::from_secs(60);
            while std::time::Instant::now() < end {
                if watched.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("{test}: the core spun without yielding");
            std::process::exit(101);
        });
        Self(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn a_exits() -> crate::driver::Event {
    crate::driver::Event::Exited(crate::driver::ExitInfo {
        code: Some(1),
        success: false,
        text: "crashed".to_owned(),
    })
}

// ---- item 1: A failing during an exit cleanup ------------------------------------

#[tokio::test(start_paused = true)]
async fn an_active_worker_that_exits_during_an_exit_cleanup_does_not_spin_the_wait() {
    let _watchdog = Watchdog::new("a_exit_during_cleanup");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        })],
    );
    // B's termination is held for 3 s.
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let exit_a = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exits());
    };
    let started = tokio::time::Instant::now();
    tokio::join!(core.handle_switch(update(false), tx), exit_a);
    // B's exit was consumed (after its 3 s), then the switch took A's branch.
    assert!(started.elapsed() >= Duration::from_secs(3));
    let result = done_result(&drain(&mut rx));
    failed(&result, "loading", "environment");
    assert!(
        core.rt
            .workers
            .values()
            .all(|w| w.artifact.version != "2.0.0"),
        "B's exit was not consumed"
    );
    assert!(core.registry.survivors().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stop_still_takes_effect_in_an_exit_cleanup_after_the_active_worker_failed() {
    let _watchdog = Watchdog::new("stop_during_cleanup");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        })],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(10)),
            Step::Exit(false, "killed"),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exits());
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = command_tx.send(Command::Stop);
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    assert!(core.rt.stop_requested);
    assert!(
        matches!(drain(&mut rx).last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("stopping"))
    );
    // The stop did not abandon B: it was waited for, and the switch cleared.
    assert!(rig.disk.on_disk().unwrap().switch.is_none());
    assert!(
        core.rt
            .workers
            .values()
            .all(|w| w.artifact.version != "2.0.0")
    );
}

/// B's load fails and its kill is held for 3 s. A exits at 1 s, during the
/// clean-up; with `cancel` a cancel is accepted at 2 s, still during it.
async fn a_exits_in_bs_cleanup_with_a_cancel(cancel: bool) {
    let _watchdog = Watchdog::new("a_exit_in_cleanup_cancel");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        })],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exits());
        tokio::time::sleep(Duration::from_secs(1)).await;
        if cancel {
            assert_eq!(
                shared.request_cancel(),
                crate::snapshot::CancelAnswer::Cancelled
            );
        }
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    settle().await;
    let order = rig.disk.order();
    if cancel {
        // The cancel was answered first: nothing is recorded or returned, and
        // A's exit is the active worker's ordinary one.
        assert_eq!(result["result"], "cancelled", "{result}");
        assert!(
            rig.disk.state.lock().unwrap().state.is_empty(),
            "no record, no failed mark"
        );
        assert!(core.rt.returning.is_none(), "no return");
        assert!(
            order.iter().all(|n| n != "J-R1"),
            "no return journalled: {order:?}"
        );
        let journal = core.rt.journal.as_ref().unwrap();
        assert!(journal.switch.is_none());
        assert!(journal.outcomes.is_empty());
        let restart = core.rt.restart.as_ref().expect("A's version restarts");
        tokio::time::advance(restart.due - tokio::time::Instant::now()).await;
        core.on_tick().await;
        assert_eq!(
            rig.driver.log_of("spawn 1.0.0").len(),
            2,
            "the restart ladder started A's version again: {:?}",
            rig.driver.log()
        );
    } else {
        failed(&result, "loading", "environment");
        assert_eq!(result["returned"], true);
        assert!(order.iter().any(|n| n == "J-R1"), "{order:?}");
        let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
        assert!(outcome.mark_failed.is_none());
        assert_eq!(outcome.record.returned_to.as_deref(), Some("1.0.0"));
    }
}

#[tokio::test(start_paused = true)]
async fn a_cancel_ready_when_a_exits_in_bs_cleanup_wins_and_a_restarts_on_its_ladder() {
    a_exits_in_bs_cleanup_with_a_cancel(true).await;
}

#[tokio::test(start_paused = true)]
async fn without_a_cancel_a_exiting_in_bs_cleanup_is_still_failed_to_load_with_the_return() {
    a_exits_in_bs_cleanup_with_a_cancel(false).await;
}

/// Round 6b item 4: the lost-A branch of `cancelled_after_cleanup`. B's load
/// fails and its kill is held for 3 s. A's control channel is lost at 1 s,
/// during the clean-up (the process lives), and a cancel is accepted at 2 s.
/// The switch ends cancelled: no record, no return. A is awaited (its handle
/// is used) before A's version restarts on its ladder.
#[tokio::test(start_paused = true)]
async fn a_cancel_ready_when_a_is_lost_in_bs_cleanup_wins_awaits_a_and_restarts_it() {
    let _watchdog = Watchdog::new("a_lost_in_cleanup_cancel");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        })],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver
            .emit("1.0.0", crate::driver::Event::Lost("gone".to_owned()));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            shared.request_cancel(),
            crate::snapshot::CancelAnswer::Cancelled
        );
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    settle().await;
    assert_eq!(result["result"], "cancelled", "{result}");
    assert!(
        rig.disk.state.lock().unwrap().state.is_empty(),
        "no record, no failed mark"
    );
    assert!(core.rt.returning.is_none(), "no return");
    let order = rig.disk.order();
    assert!(
        order.iter().all(|n| n != "J-R1"),
        "no return journalled: {order:?}"
    );
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert!(journal.outcomes.is_empty());
    // A was awaited: its handle was used (the fake's kill step ends it), and
    // it is no longer tracked, before any restart.
    let log = rig.driver.log();
    let killed = log
        .iter()
        .position(|line| line == "kill 1.0.0")
        .unwrap_or_else(|| panic!("A's handle was not used: {log:?}"));
    assert!(
        core.rt
            .workers
            .values()
            .all(|w| w.artifact.version != "1.0.0"),
        "A is forgotten"
    );
    assert!(core.rt.degraded.is_some());
    let restart = core.rt.restart.as_ref().expect("A's version restarts");
    tokio::time::advance(restart.due - tokio::time::Instant::now()).await;
    core.on_tick().await;
    let log = rig.driver.log();
    let respawn = log
        .iter()
        .rposition(|line| line.starts_with("spawn 1.0.0"))
        .expect("A's version restarted");
    assert_eq!(rig.driver.log_of("spawn 1.0.0").len(), 2, "{log:?}");
    assert!(
        killed < respawn,
        "A was awaited before its restart: {log:?}"
    );
}

// ---- item 2: the survivor of a handshake -----------------------------------------

/// Runs one switch whose spawn leaves a child that ignores kills until `release`
/// seconds in; `act` runs at second `act_at`. Returns what was seen at `look`
/// seconds, while the child still lived: whether J1 was on disk, whether the
/// snapshot still showed the switch, whether the switch had answered, and how
/// often 2.0.0 was spawned.
async fn survivor_held(
    fail: SpawnFail,
    stop_at: Option<u64>,
    look: u64,
    release: u64,
) -> (bool, bool, bool, usize, Vec<String>, Value) {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", fail);
    rig.driver.hold_termination(true);
    let command_tx = rig.command_tx.clone();
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        if let Some(at) = stop_at {
            tokio::time::sleep(Duration::from_secs(at)).await;
            let _ = command_tx.send(Command::Stop);
        }
        tokio::time::sleep(Duration::from_secs(look - stop_at.unwrap_or(0))).await;
        let seen = (
            rig.disk.on_disk().unwrap().switch.is_some(),
            shared.snapshot().switch.is_some(),
            rig.driver.log_of("exited 2.0.0").is_empty(),
            rig.driver.log_of("spawn 2.0.0").len(),
        );
        tokio::time::sleep(Duration::from_secs(release - look)).await;
        rig.driver.hold_termination(false);
        seen
    };
    let ((), (j1, snapshot, still_alive, spawns)) =
        tokio::join!(core.handle_switch(update(false), tx), script);
    assert!(
        core.registry.survivors().is_empty(),
        "the exit was observed"
    );
    assert!(
        rig.disk.on_disk().unwrap().switch.is_none(),
        "J1 is cleared once the child has exited"
    );
    let replies = drain(&mut rx);
    let result = match replies.last() {
        Some(EndpointReply::Done { result, .. }) => result.clone(),
        Some(EndpointReply::Error { reason, .. }) => json!({ "error": reason }),
        other => panic!("no answer: {other:?}"),
    };
    (
        j1,
        snapshot,
        still_alive,
        spawns,
        rig.driver.log_of("exited 2.0.0"),
        result,
    )
}

use serde_json::json;

#[tokio::test(start_paused = true)]
async fn a_stop_during_a_handshake_keeps_the_switch_until_the_childs_exit() {
    let (j1, snapshot, alive, spawns, exited, result) =
        survivor_held(SpawnFail::Survive, Some(2), 10, 12).await;
    assert!(j1, "J1 stays while the child lives");
    assert!(snapshot, "the switch is still shown");
    assert!(alive);
    assert_eq!(spawns, 1);
    assert_eq!(exited.len(), 1);
    assert!(result["error"].as_str().unwrap().contains("stopping"));
}

// ---- items 5 to 8: the driver's refusals and the survivor wait before a spawn ----------

/// Leaves a child of a failed spawn that ignores kills until the termination
/// is released: a spawn of 2.0.0 that is dropped after a second.
macro_rules! with_survivor {
    ($rig:ident, $core:ident, $driver:ident) => {
        $rig.driver.fail_spawn("2.0.0", SpawnFail::Survive);
        $rig.driver.hold_termination(true);
        let $driver = $rig.driver.clone();
        let mut $core = core_of!($rig);
        {
            use crate::driver::Driver as _;
            let request = $core.spawn_request(
                &crate::testkit::artifact("2.0.0"),
                std::path::PathBuf::from("/unit-test-bin/open-ferry"),
                crate::protocol::Role::Standby,
                None,
            );
            let events = $core.registry.events();
            let _ =
                tokio::time::timeout(Duration::from_secs(1), $driver.spawn(request, events)).await;
        }
        assert_eq!($core.registry.survivors().len(), 1);
    };
}

/// A per-id survivor wait waits for the registry's `Exited` for that id, though
/// the id is in no `rt.workers` (ruling 17).
#[tokio::test(start_paused = true)]
async fn a_survivor_wait_waits_for_the_childs_exit_not_for_a_port() {
    let mut rig = Rig::new().await;
    with_survivor!(rig, core, driver);
    let id = core.registry.survivors()[0];
    let release = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(driver.log_of("exited 2.0.0").is_empty());
        driver.hold_termination(false);
    };
    let started = tokio::time::Instant::now();
    let (waited, ()) = tokio::join!(core.wait_exit(id), release);
    assert!(waited);
    assert!(
        started.elapsed() >= Duration::from_secs(10),
        "it returned early"
    );
    assert!(core.registry.survivors().is_empty());
}

/// No restart spawn starts while a survivor lives; once it has exited the
/// restart runs, and the waiting was no attempt (ruling 15, 18).
#[tokio::test(start_paused = true)]
async fn a_restart_waits_for_a_survivor_across_several_intervals_then_retries() {
    let mut rig = Rig::new().await;
    let command_tx = rig.command_tx.clone();
    with_survivor!(rig, core, driver);
    let a = core.rt.active.unwrap();
    core.registry.events().send(crash(a)).unwrap();
    let script = async {
        // Well past three restart intervals: nothing was started.
        tokio::time::sleep(Duration::from_secs(40)).await;
        let before = driver.log_of("spawn 1.0.0 active").len();
        driver.hold_termination(false);
        tokio::time::sleep(Duration::from_secs(20)).await;
        let after = driver.log_of("spawn 1.0.0 active").len();
        let _ = command_tx.send(Command::Stop);
        (before, after)
    };
    let (exit, (before, after)) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(before, 1, "the rig's own start only");
    assert_eq!(after, 2, "a real retry after the child's exit");
    assert!(core.rt.degraded.is_none(), "{:?}", core.rt.degraded);
    assert!(core.rt.restart.is_none(), "no attempt was used up");
}

/// A `SurvivorPending` refusal of a switch's spawn is waited out and retried:
/// the switch is not failed, and no version is blamed.
#[tokio::test(start_paused = true)]
async fn a_survivor_pending_refusal_of_the_switch_is_retried() {
    let mut rig = Rig::new().await;
    rig.driver
        .fail_spawn("2.0.0", SpawnFail::SurvivorPending(3));
    let (_core, replies) = go!(rig, update(false));
    assert_eq!(done_result(&replies)["result"], "switched");
    assert_eq!(rig.driver.log_of("spawn 2.0.0").len(), 4);
    assert!(
        rig.disk
            .state
            .lock()
            .unwrap()
            .state
            .iter()
            .all(|o| o.mark_failed.is_none())
    );
}

/// The same refusal of a restart's spawn: no ladder step is used up.
#[tokio::test(start_paused = true)]
async fn a_survivor_pending_refusal_of_a_restart_uses_no_attempt() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver
        .fail_spawn("1.0.0", SpawnFail::SurvivorPending(5));
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    window!(core, command_tx, 60);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 1.0.0 active").count(),
        7,
        "the rig's start, five refusals, then the start: {log:?}"
    );
    assert!(!log.contains(&"spawn 0.9.0 active".to_owned()), "{log:?}");
    assert!(core.rt.degraded.is_none());
}

/// `Untrusted` is the environment's: switching goes off, nothing is marked
/// failed (ruling 16).
#[tokio::test(start_paused = true)]
async fn an_untrusted_refusal_of_the_switch_is_the_environments() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Untrusted);
    let (core, replies) = go!(rig, update(false));
    let result = done_result(&replies);
    failed(&result, "loading", "environment");
    let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
    assert!(outcome.mark_failed.is_none());
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert!(core.rt.journal.as_ref().unwrap().switch.is_none());
}

/// The restart ladder never starts a refused version again: it falls to the
/// next rung at once, and no attempt is counted.
#[tokio::test(start_paused = true)]
async fn an_untrusted_restart_falls_back_and_is_not_retried() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    // This test runs for longer than the active load deadline (3 minutes) and
    // is about what happens at the refusal, not before `loaded`: the version
    // that starts reports `loaded`, as a healthy worker does.
    rig.driver.on("0.9.0", "spawn", vec![loaded()]);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Untrusted);
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    window!(core, command_tx, 400);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 1.0.0 active").count(),
        2,
        "the rig's start and one refusal, never again: {log:?}"
    );
    assert!(log.contains(&"spawn 0.9.0 active".to_owned()), "{log:?}");
    assert!(core.rt.disabled.is_some());
    assert!(core.rt.degraded.is_none(), "running again");
}

/// With nothing else to run, the installed file is the way out.
#[tokio::test(start_paused = true)]
async fn an_untrusted_restart_with_no_other_rung_runs_the_installed_file() {
    let mut rig = Rig::new().await;
    {
        let journal = rig.rt.as_mut().unwrap().journal.as_mut().unwrap();
        journal.installed = Some(open_ferry_update::journal::Installed::from(
            &crate::testkit::artifact("0.9.0"),
        ));
        let mut state = rig.disk.state.lock().unwrap();
        state.installed = Some(crate::testkit::SHA_3.to_owned());
        state.on_disk = Some(journal.clone());
    }
    rig.driver.fail_spawn("1.0.0", SpawnFail::Untrusted);
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    window!(core, command_tx, 60);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 1.0.0 active").count(),
        2,
        "{log:?}"
    );
    assert!(log.contains(&"spawn 0.9.0 active".to_owned()), "{log:?}");
}

/// A refusal is about a location: the installed file with the same bytes as a
/// refused kept copy stays a rung (restart ladder).
#[tokio::test(start_paused = true)]
async fn a_refused_kept_copy_does_not_shut_out_the_installed_file_of_the_same_bytes() {
    let mut rig = Rig::new().await;
    // This test runs for longer than the active load deadline (3 minutes) and
    // is about what happens at the refusal, not before `loaded`: the version
    // that starts reports `loaded`, as a healthy worker does.
    rig.driver.on("1.0.0", "spawn", vec![loaded()]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::UntrustedKept);
    let installed = rig.cfg.installed.clone();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    window!(core, command_tx, 400);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "refused kept 1.0.0").count(),
        1,
        "the refused kept path is not tried again: {log:?}"
    );
    assert!(log.contains(&"ran installed 1.0.0".to_owned()), "{log:?}");
    assert_eq!(core.rt.run.as_ref().unwrap().binary, installed);
    assert!(core.rt.disabled.is_some(), "switching stays off");
    assert!(core.rt.degraded.is_none(), "running again");
}

/// The same on the return ladder.
#[tokio::test(start_paused = true)]
async fn a_refused_kept_copy_does_not_shut_out_the_installed_file_on_the_return() {
    let mut rig = Rig::new().await;
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::UntrustedKept);
    let installed = rig.cfg.installed.clone();
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 400);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "refused kept 1.0.0").count(),
        1,
        "{log:?}"
    );
    assert!(log.contains(&"ran installed 1.0.0".to_owned()), "{log:?}");
    assert_eq!(core.rt.run.as_ref().unwrap().binary, installed);
    assert!(core.rt.disabled.is_some());
    assert!(core.rt.degraded.is_none(), "running again");
    assert!(core.rt.returning.is_none());
}

/// An exhausted kept copy does not hide the installed file of the same bytes
/// (restart ladder): kept 1.0.0 fails its three tries, `previous` is refused
/// as untrusted, and the installed 1.0.0 is the next rung.
#[tokio::test(start_paused = true)]
async fn an_exhausted_kept_copy_does_not_hide_the_installed_file_of_the_same_bytes() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    // This test runs for longer than the active load deadline (3 minutes) and
    // is about what happens at the refusal, not before `loaded`: the version
    // that starts reports `loaded`, as a healthy worker does.
    rig.driver.on("1.0.0", "spawn", vec![loaded()]);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start, SpawnFail::Start]),
    );
    rig.driver.fail_spawn("0.9.0", SpawnFail::Untrusted);
    let installed = rig.cfg.installed.clone();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    window!(core, command_tx, 600);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 1.0.0 active").count(),
        5,
        "the rig's start, three kept tries, the installed file: {log:?}"
    );
    assert_eq!(core.rt.run.as_ref().unwrap().binary, installed, "{log:?}");
    assert!(core.rt.disabled.is_some(), "switching stays off");
    assert!(core.rt.degraded.is_none(), "running again");
}

/// The same on the return ladder (location-based already: a pin).
#[tokio::test(start_paused = true)]
async fn an_exhausted_kept_copy_does_not_hide_the_installed_file_on_the_return() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start, SpawnFail::Start]),
    );
    rig.driver.fail_spawn("0.9.0", SpawnFail::Untrusted);
    let installed = rig.cfg.installed.clone();
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 600);
    let log = rig.driver.log();
    assert_eq!(core.rt.run.as_ref().unwrap().binary, installed, "{log:?}");
    assert!(core.rt.disabled.is_some());
    assert!(core.rt.degraded.is_none(), "running again");
    assert!(core.rt.returning.is_none());
}

/// A removed candidate's attempts do not pass on: the next one has its own
/// three (restart ladder). 1.0.0 fails twice then is refused; 0.9.0 fails
/// once and starts on its second try.
#[tokio::test(start_paused = true)]
async fn a_refused_candidates_attempts_do_not_pass_to_the_next_on_a_restart() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    // This test runs for longer than the active load deadline (3 minutes) and
    // is about what happens at the refusal, not before `loaded`: the version
    // that starts reports `loaded`, as a healthy worker does.
    rig.driver.on("0.9.0", "spawn", vec![loaded()]);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![
            SpawnFail::Start,
            SpawnFail::Start,
            SpawnFail::Untrusted,
        ]),
    );
    rig.driver
        .fail_spawn("0.9.0", SpawnFail::Script(vec![SpawnFail::Start]));
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    window!(core, command_tx, 600);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 0.9.0 active").count(),
        2,
        "one failure, then its second attempt: {log:?}"
    );
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 4);
    assert!(core.rt.degraded.is_none(), "running again");
    assert_eq!(core.rt.run.as_ref().unwrap().artifact.version, "0.9.0");
}

/// The same on the return ladder.
#[tokio::test(start_paused = true)]
async fn a_refused_candidates_attempts_do_not_pass_to_the_next_on_a_return() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    // This test runs for longer than the active load deadline (3 minutes) and
    // is about what happens at the refusal, not before `loaded`: the version
    // that starts reports `loaded`, as a healthy worker does.
    rig.driver.on("0.9.0", "spawn", vec![loaded()]);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![
            SpawnFail::Start,
            SpawnFail::Start,
            SpawnFail::Untrusted,
        ]),
    );
    rig.driver
        .fail_spawn("0.9.0", SpawnFail::Script(vec![SpawnFail::Start]));
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 600);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 0.9.0 active").count(),
        2,
        "{log:?}"
    );
    assert_eq!(log.iter().filter(|l| *l == "spawn 1.0.0 active").count(), 4);
    assert!(core.rt.degraded.is_none(), "running again");
    assert!(core.rt.returning.is_none());
}

/// A handshake survivor is in the status, with its pid and version, and in the
/// journal's worker list, until its `Exited` (item 4).
#[tokio::test(start_paused = true)]
async fn a_handshake_survivor_shows_in_the_status_until_its_exit() {
    let mut rig = Rig::new().await;
    rig.driver
        .fail_spawn("2.0.0", SpawnFail::SurviveThenFail(Duration::from_secs(5)));
    rig.driver.hold_termination(true);
    let shared = rig.shared.clone();
    let (tx, _rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let held = shared.snapshot().workers;
        let journal = rig.disk.on_disk().unwrap().workers;
        rig.driver.hold_termination(false);
        tokio::time::sleep(Duration::from_secs(5)).await;
        (held, journal)
    };
    let ((), (held, journal)) = tokio::join!(core.handle_switch(update(false), tx), script);
    let b = held
        .iter()
        .find(|worker| worker.version == "2.0.0")
        .expect("B is in the status while its termination is held");
    assert_eq!(b.role, "survivor");
    assert_ne!(b.pid, 0);
    assert!(
        journal
            .iter()
            .any(|worker| worker.version == "2.0.0" && worker.pid == b.pid),
        "{journal:?}"
    );
    assert!(
        shared
            .snapshot()
            .workers
            .iter()
            .all(|worker| worker.version != "2.0.0"),
        "gone after its Exited"
    );
}

/// A restart outside any switch: its handshake survivor is in the status and
/// the loaded journal while its termination is held (under the warning
/// threshold), and in neither after its `Exited` (item 4).
#[tokio::test(start_paused = true)]
async fn a_restarts_handshake_survivor_is_in_the_status_and_the_journal_until_its_exit() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::SurviveThenFail(Duration::from_secs(5))]),
    );
    rig.driver.hold_termination(true);
    let shared = rig.shared.clone();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    core.registry.events().send(crash(id)).unwrap();
    let script = async {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let held = shared.snapshot().workers;
        let journal = rig.disk.on_disk().unwrap().workers;
        rig.driver.hold_termination(false);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let after = shared.snapshot().workers;
        let after_journal = rig.disk.on_disk().unwrap().workers;
        let _ = command_tx.send(Command::Stop);
        (held, journal, after, after_journal)
    };
    let (exit, (held, journal, after, after_journal)) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    let survivor = held
        .iter()
        .find(|worker| worker.role == "survivor")
        .expect("the survivor is in the status while its termination is held");
    assert_ne!(survivor.pid, 0);
    assert!(
        journal
            .iter()
            .any(|worker| worker.pid == survivor.pid && worker.version == "1.0.0"),
        "the loaded journal lists it: {journal:?}"
    );
    assert!(after.iter().all(|worker| worker.pid != survivor.pid));
    assert!(
        after_journal
            .iter()
            .all(|worker| worker.pid != survivor.pid),
        "{after_journal:?}"
    );
}

/// A exits while a handshake survivor's termination is held (item 3): row d,
/// the environment's blame, no `mark_failed`, J-R1, then the fresh `from`.
async fn a_exits_while_the_survivor_is_reaped(fail: SpawnFail, exit_at: u64, release: u64) {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", fail);
    rig.driver.hold_termination(true);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(exit_at)).await;
        rig.driver.emit("1.0.0", a_exits());
        tokio::time::sleep(Duration::from_secs(release - exit_at)).await;
        rig.driver.hold_termination(false);
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    failed(&result, "loading", "environment");
    assert_eq!(result["returned"], true);
    settle().await;
    let outcome = stated(&rig.disk, result["id"].as_str().unwrap());
    assert!(
        outcome.mark_failed.is_none(),
        "the environment's, not 2.0.0's"
    );
    assert_eq!(outcome.record.returned_to.as_deref(), Some("1.0.0"));
    let order = rig.disk.order();
    assert!(order.iter().any(|n| n == "J-R1"), "{order:?}");
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| *l == "spawn 1.0.0 active").count(),
        2,
        "the fresh `from` ran: {log:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_exiting_while_a_failed_handshakes_child_is_reaped_is_row_d() {
    a_exits_while_the_survivor_is_reaped(
        SpawnFail::SurviveThenFail(Duration::from_secs(5)),
        10,
        15,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_exiting_while_a_timed_out_handshakes_child_is_reaped_is_row_d() {
    a_exits_while_the_survivor_is_reaped(SpawnFail::Survive, 65, 70).await;
}

/// One Stop during a handshake whose child outlives the stop budget: A is
/// stopped at once, and B's tracking and J1 stay until B's exit (item 2).
#[tokio::test(start_paused = true)]
async fn a_stop_mid_switch_stops_a_at_once_and_waits_for_bs_exit() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Survive);
    rig.driver.hold_termination(true);
    let command_tx = rig.command_tx.clone();
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = command_tx.send(Command::Stop);
        tokio::time::sleep(Duration::from_secs(3)).await;
        // A got `stop{budget}` promptly and exited within its budget.
        let early = (
            rig.driver.log_of("send 1.0.0 stop").len(),
            usize::from(
                shared
                    .snapshot()
                    .workers
                    .iter()
                    .all(|worker| worker.version != "1.0.0"),
            ),
            rig.driver.log_of("exited 2.0.0").len(),
            rig.disk.on_disk().unwrap().switch.is_some(),
        );
        // Beyond the stop budget (50 s): a deadline is not an exit.
        tokio::time::sleep(Duration::from_secs(65)).await;
        let late = (
            rig.driver.log_of("exited 2.0.0").len(),
            rig.disk.on_disk().unwrap().switch.is_some(),
        );
        rig.driver.hold_termination(false);
        (early, late)
    };
    let ((), (early, late)) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(early.0, 1, "A is told to stop at once");
    assert_eq!(early.1, 1, "and exits within its budget");
    assert_eq!(early.2, 0, "B still lives");
    assert!(early.3, "J1 stays");
    assert_eq!(late.0, 0, "no exit, so none is claimed");
    assert!(late.1, "J1 stays past the stop budget");
    assert!(core.registry.survivors().is_empty());
    assert!(rig.disk.on_disk().unwrap().switch.is_none());
    assert!(
        matches!(drain(&mut rx).last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("stopping"))
    );
    assert!(core.rt.restart.is_none(), "a stop restarts nothing");
}

#[tokio::test(start_paused = true)]
async fn a_handshake_timeout_keeps_the_switch_until_the_childs_exit() {
    // The load timeout is 60 s: the child ignores its kill past it.
    let (j1, snapshot, alive, spawns, exited, result) =
        survivor_held(SpawnFail::Survive, None, 70, 75).await;
    assert!(j1, "J1 stays while the child lives");
    assert!(snapshot);
    assert!(alive);
    assert_eq!(spawns, 1, "no other spawn starts");
    assert_eq!(exited.len(), 1);
    failed(&result, "loading", "new-version");
}

#[tokio::test(start_paused = true)]
async fn a_failed_handshake_keeps_the_switch_until_the_childs_exit() {
    let (j1, snapshot, alive, spawns, _, result) = survivor_held(
        SpawnFail::SurviveThenFail(Duration::from_secs(5)),
        None,
        20,
        25,
    )
    .await;
    assert!(j1, "J1 stays while the child lives");
    assert!(snapshot);
    assert!(alive);
    assert_eq!(spawns, 1);
    failed(&result, "loading", "new-version");
}

// ---- item 3: the deferred hand over delete --------------------------------------

#[tokio::test(start_paused = true)]
async fn a_new_switch_waits_for_the_last_ones_deferred_cleanup() {
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![Step::Sleep(Duration::from_secs(3)), handed_over()],
    );
    // The first switch's J2 and J7 land without a sync: its hand over file
    // cannot go. The retry at the next admission is not durable either, once.
    rig.disk.not_durable_journal("J2", 1);
    rig.disk.not_durable_journal("J7", 1);
    rig.disk.not_durable_journal("J-retry", 1);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let off = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.host.set_mode(true, 1);
    };
    tokio::join!(core.handle_switch(update(false), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    assert_eq!(rig.disk.state.lock().unwrap().handover_removed, 0);

    // A second switch is refused while the cleanup is pending.
    rig.disk.add_pin("pin");
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(update(false), tx).await;
    assert!(
        matches!(drain(&mut rx).last(), Some(EndpointReply::Error { reason, .. }) if reason.contains("finishing the last switch's cleanup"))
    );
    assert_eq!(rig.disk.state.lock().unwrap().handover_removed, 0);

    // The retry lands, the old file goes, and only then does the next switch
    // write anything.
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(update(false), tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    let order = rig.disk.order();
    let removed = order.iter().position(|step| step == "handover-removed");
    let second_j1 = order.iter().rposition(|step| step == "J1");
    assert_eq!(rig.disk.state.lock().unwrap().handover_removed, 1);
    assert!(
        removed < second_j1 && removed.is_some(),
        "the old file went before the new switch's J1: {order:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_deferred_hand_over_delete_leaves_the_file_of_a_newer_switch() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    core.deferred.handover = Some("sw-old".to_owned());
    let mut journal = core.rt.journal.clone().unwrap();
    journal.switch = Some(open_ferry_update::journal::Switch {
        id: "sw-new".to_owned(),
        how: open_ferry_update::journal::How::Update,
        from: crate::testkit::artifact("1.0.0"),
        to: crate::testkit::artifact("2.0.0"),
        phase: Phase::Loading,
        started: "2026-10-09T12:00:00Z".to_owned(),
        watch_until: None,
        admitted: open_ferry_update::journal::Admitted {
            while_off: false,
            mode_gen: 0,
        },
        return_: None,
    });
    core.rt.journal = Some(journal);
    core.write_journal("J1").unwrap();
    assert_eq!(rig.disk.state.lock().unwrap().handover_removed, 0);
    assert!(core.deferred.handover.is_none(), "the entry is dropped");
}

// ---- item 4: the handshake is polled like every other wait --------------------------

#[tokio::test(start_paused = true)]
async fn a_exiting_during_a_held_handshake_is_taken_at_once_not_at_the_load_deadline() {
    let mut rig = Rig::new().await;
    rig.driver
        .fail_spawn("2.0.0", SpawnFail::Delay(Duration::from_secs(50)));
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let exit_a = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.driver.emit("1.0.0", a_exit_event());
    };
    let started = tokio::time::Instant::now();
    tokio::join!(core.handle_switch(update(false), tx), exit_a);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "row d waited for the handshake: {:?}",
        started.elapsed()
    );
    failed(&done_result(&drain(&mut rx)), "loading", "environment");
}

#[tokio::test(start_paused = true)]
async fn a_mode_flip_during_a_held_handshake_cancels_at_checkpoint_one() {
    let mut rig = Rig::new().await;
    rig.host.set_mode(true, 4);
    rig.driver
        .fail_spawn("2.0.0", SpawnFail::Delay(Duration::from_secs(6)));
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    // On at 1.5 s, off again at 4 s with the generation the host reported: only
    // the once-a-second re-reading, which the handshake must keep serving, sees it.
    let flip = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        rig.host.set_mode(false, 4);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        rig.host.set_mode(true, 4);
    };
    tokio::join!(core.handle_switch(update(false), tx), flip);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
}

// ---- item 5: a status queued before the ping is not its answer ---------------------

#[tokio::test(start_paused = true)]
async fn a_status_queued_before_the_ping_does_not_end_the_sign_in_wait() {
    let mut rig = Rig::new().await;
    // The real answer to every ping arrives after 3 s and says two are pending.
    rig.driver.on(
        "1.0.0",
        "ping",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Msg(crate::protocol::Message::Status {
                mode: "active".to_owned(),
                live: 0,
                signins_pending: 2,
                phase: "serving".to_owned(),
            }),
        ],
    );
    // A periodic status that nobody had read yet, sent before any ping.
    rig.driver.emit(
        "1.0.0",
        crate::driver::Event::Message(crate::protocol::Message::Status {
            mode: "active".to_owned(),
            live: 0,
            signins_pending: 0,
            phase: "serving".to_owned(),
        }),
    );
    let signin_wait = rig.cfg.timings.signin_wait;
    let mut core = core_of!(rig);
    let a = core.rt.active.unwrap();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        core.signin_wait(a).await,
        crate::switch::SignIn::Go
    ));
    assert!(
        started.elapsed() >= signin_wait,
        "the queued status was taken for the answer: {:?}",
        started.elapsed()
    );
}

/// A confirmation whose target is already the installed file does not
/// acknowledge an install whose directory sync failed (item 3): B's install
/// lands but cannot sync; C starts and is rolled back to B; B's confirmation
/// leaves the pending install and records no success until a sync works.
#[tokio::test(start_paused = true)]
async fn a_matching_confirmation_never_acknowledges_an_unsynced_install() {
    let mut rig = Rig::new().await;
    rig.disk.state.lock().unwrap().dir_sync_fail = 1000;
    rig.disk.add_artifact("0.9.0", crate::testkit::SHA_3);
    let watch = rig.cfg.timings.watch;
    let retry_max = rig.cfg.timings.install_retry_max;
    let (mut core, _) = go!(rig, update(false));
    // B's window ends: J5, then the install lands the file and fails its sync.
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    {
        let journal = core.rt.journal.as_ref().unwrap();
        assert!(journal.install_pending.is_some(), "unsynced");
        assert_eq!(
            rig.disk.state.lock().unwrap().installed.as_deref(),
            Some(crate::testkit::SHA_2),
            "the file is in place"
        );
    }
    // C starts, and B is rolled back to before C confirms.
    rig.disk.add_pin("pin");
    let to_c = crate::core::SwitchRequest {
        to: "0.9.0".to_owned(),
        sha256: crate::testkit::SHA_3.to_owned(),
        ..update(false)
    };
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(to_c, tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(rollback_to("2.0.0", crate::testkit::SHA_2), tx)
        .await;
    let back = done_result(&drain(&mut rx))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // B's confirmation, with every sync still failing.
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    let journal = core.rt.journal.as_ref().unwrap();
    assert_eq!(journal.confirmed.version, "2.0.0");
    assert!(
        journal.install_pending.is_some(),
        "the unsynced install stays pending"
    );
    assert_ne!(
        journal.installed.as_ref().map(|i| i.version.as_str()),
        Some("2.0.0"),
        "nothing acknowledged the install"
    );
    let acknowledged = |disk: &crate::testkit::FakeDisk| {
        disk.state.lock().unwrap().state.iter().any(|outcome| {
            outcome.record.installed_binary.as_deref()
                == Some(open_ferry_update::state::installed_binary::REPLACED)
        })
    };
    assert!(!acknowledged(&rig.disk), "no success was recorded");
    // The sync works again: the retry completes the install.
    rig.disk.state.lock().unwrap().dir_sync_fail = 0;
    tokio::time::advance(retry_max + Duration::from_secs(1)).await;
    core.on_tick().await;
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.install_pending.is_none());
    assert_eq!(journal.installed.as_ref().unwrap().version, "2.0.0");
    assert_eq!(
        rig.disk.record(&back).unwrap().installed_binary.as_deref(),
        Some(open_ferry_update::state::installed_binary::REPLACED)
    );
}

/// The real install transaction (the real classification, copy, rename and
/// directory sync, on a scratch directory) under the in-memory disk for
/// everything else, so `install_pending` is kept by the failed transaction
/// itself, not built by hand.
mod real_install {
    use super::*;
    use crate::disk::{ArtifactError, Disk, RealDisk};
    use crate::install::InstallResult;
    use crate::statefile::StateWriteError;
    use chrono::{DateTime, Utc};
    use open_ferry_update::data_dir::DataDir;
    use open_ferry_update::journal::{Artifact, Journal, JournalError, JournalLoad, JournalStore};

    pub(super) struct ProxyDisk {
        pub(super) fake: crate::testkit::FakeDisk,
        pub(super) real: RealDisk,
    }

    impl Disk for ProxyDisk {
        fn write_journal(&self, journal: &mut Journal, name: &str) -> Result<(), JournalError> {
            self.fake.write_journal(journal, name)
        }
        fn write_state(&self, name: &str, outcome: &Outcome) -> Result<(), StateWriteError> {
            self.fake.write_state(name, outcome)
        }
        fn artifact(&self, version: &str, sha256: &str) -> Result<Artifact, ArtifactError> {
            self.fake.artifact(version, sha256)
        }
        fn check_pin(
            &self,
            id: &str,
            version: &str,
            sha256: &str,
            now: DateTime<Utc>,
        ) -> Result<(), String> {
            self.fake.check_pin(id, version, sha256, now)
        }
        fn consume_pin(&self, id: &str) -> Result<(), String> {
            self.fake.consume_pin(id)
        }
        fn installed_sha256(&self) -> Option<String> {
            self.real.installed_sha256()
        }
        fn installed_size(&self) -> Option<u64> {
            self.real.installed_size()
        }
        fn installed_switched_off(&self) -> (Option<String>, u64) {
            self.real.installed_switched_off()
        }
        fn choose_aside(&self) -> Result<String, String> {
            self.real.choose_aside()
        }
        fn install(
            &self,
            target: &Artifact,
            expected: Option<&str>,
            from: &str,
            aside: Option<&str>,
        ) -> InstallResult {
            self.real.install(target, expected, from, aside)
        }
        fn install_journalled(
            &self,
            journal: &mut Journal,
            target: &Artifact,
            expected: Option<&str>,
            from: &str,
            aside: Option<&str>,
        ) -> InstallResult {
            self.real
                .install_journalled(journal, target, expected, from, aside)
        }
        async fn install_transaction(&self, journal: Journal) -> (Journal, InstallResult) {
            self.real.install_transaction(journal).await
        }
        fn sync_installed_directory(&self) -> Result<(), String> {
            self.real.sync_installed_directory()
        }
        fn remove_handover(&self) {
            self.fake.remove_handover();
        }
    }

    /// A scratch directory the trust check accepts (under `$HOME` on Unix).
    pub(super) fn scratch() -> tempfile::TempDir {
        let mut builder = tempfile::Builder::new();
        builder.prefix("ofp-s6-");
        match std::env::var_os("HOME").filter(|_| cfg!(unix)) {
            Some(home) => builder.tempdir_in(home).unwrap(),
            None => builder.tempdir().unwrap(),
        }
    }

    /// The scratch data root with an old installed file and a kept 2.0.0, as
    /// a proxy disk over them. Returns the disk, the kept file's hash, the old
    /// file's hash, and the installed path.
    pub(super) fn proxy(
        rig: &Rig,
        root: &std::path::Path,
    ) -> (ProxyDisk, String, String, std::path::PathBuf) {
        let data = DataDir::at(root);
        let installed = root.join("installed").join(crate::testkit::NAME);
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, b"old binary").unwrap();
        let old_sha = open_ferry_update::staging::hash_file(&installed).unwrap();
        let kept = data.binary("2.0.0", crate::testkit::NAME);
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"new binary").unwrap();
        let new_sha = open_ferry_update::staging::hash_file(&kept).unwrap();
        let store = JournalStore::new(root).with_retry_wait(Duration::ZERO);
        let mut first = rig.rt.as_ref().unwrap().journal.clone().unwrap();
        store.write(&mut first, "init").unwrap();
        let fake = crate::testkit::FakeDisk::default();
        fake.add_artifact("1.0.0", crate::testkit::SHA_1);
        fake.add_artifact("2.0.0", &new_sha);
        fake.add_pin("pin");
        fake.state.lock().unwrap().on_disk = rig.disk.on_disk();
        let real = RealDisk::new(
            data,
            installed.clone(),
            crate::testkit::NAME.to_owned(),
            store,
        );
        (ProxyDisk { fake, real }, new_sha, old_sha, installed)
    }

    pub(super) fn load(root: &std::path::Path) -> Journal {
        match JournalStore::new(root).load().unwrap() {
            JournalLoad::Loaded(journal) => journal,
            other => panic!("{other:?}"),
        }
    }
}

/// End to end (item 5): a supervised install whose directory sync fails. The
/// failed transaction itself leaves `install_pending` in the core's journal,
/// in the journal file and in the in-memory record, with the new file in
/// place; once the sync is allowed, the retry acknowledges it.
#[tokio::test(start_paused = true)]
async fn a_supervised_install_whose_directory_sync_fails_stays_pending_until_a_retry_syncs() {
    let mut rig = Rig::new().await;
    let root = real_install::scratch();
    let (disk, new_sha, old_sha, installed) = real_install::proxy(&rig, root.path());
    {
        let journal = rig.rt.as_mut().unwrap().journal.as_mut().unwrap();
        journal.installed = Some(open_ferry_update::journal::Installed {
            version: "1.0.0".to_owned(),
            sha256: old_sha,
        });
    }
    let sync_dir = installed.parent().unwrap().to_owned();
    crate::install::sync_failures::fail(&sync_dir);
    let watch = rig.cfg.timings.watch;
    let retry_max = rig.cfg.timings.install_retry_max;
    let request = crate::core::SwitchRequest {
        sha256: new_sha.clone(),
        ..update(false)
    };
    let (tx, mut rx) = unbounded_channel();
    let mut core = crate::core::Core::new(
        &rig.cfg,
        &rig.driver,
        &rig.host,
        &disk,
        &rig.clock,
        &mut rig.registry,
        rig.shared.clone(),
        &mut rig.commands,
        rig.rt.take().expect("the runtime"),
    );
    core.handle_switch(request, tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    // The window ends: J5, then the real install lands the file and cannot
    // sync its directory.
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");
    let held = core.rt.journal.as_ref().unwrap();
    let pending = held
        .install_pending
        .as_ref()
        .expect("the failed transaction keeps the pending install");
    assert_eq!(pending.target.sha256, new_sha);
    assert_eq!(pending.attempts, 1, "one failed attempt");
    assert_eq!(held.installed.as_ref().unwrap().version, "1.0.0");
    let on_disk = disk.fake.on_disk().unwrap();
    assert!(on_disk.install_pending.is_some(), "J6 kept it");
    assert!(
        real_install::load(root.path()).install_pending.is_some(),
        "the transaction's own journal write has it"
    );
    let replaced = |disk: &real_install::ProxyDisk| {
        disk.fake.state.lock().unwrap().state.iter().any(|outcome| {
            outcome.record.installed_binary.as_deref()
                == Some(open_ferry_update::state::installed_binary::REPLACED)
        })
    };
    assert!(!replaced(&disk), "no success was recorded");
    // A retry while the sync still fails finds the file in place and stays
    // pending too.
    tokio::time::advance(retry_max + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert!(core.rt.journal.as_ref().unwrap().install_pending.is_some());
    assert!(!replaced(&disk));
    // The sync is allowed: the next retry acknowledges.
    crate::install::sync_failures::allow(&sync_dir);
    tokio::time::advance(retry_max + Duration::from_secs(1)).await;
    core.on_tick().await;
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.install_pending.is_none());
    assert_eq!(journal.installed.as_ref().unwrap().version, "2.0.0");
    assert!(replaced(&disk), "the success is recorded after the sync");
}

/// Item 9: the kept copy is checked at the install, not only at startup. A
/// source that stopped being trusted since startup installs nothing, the
/// attempt is blamed on the environment, nothing is marked failed, and the
/// retry installs once the source is trusted again.
#[tokio::test(start_paused = true)]
async fn an_install_from_a_source_that_is_not_trusted_now_installs_nothing_and_retries() {
    let mut rig = Rig::new().await;
    let watch = rig.cfg.timings.watch;
    let retry_max = rig.cfg.timings.install_retry_max;
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    // After startup, the source's directory became foreign-writable.
    rig.disk.state.lock().unwrap().untrusted_source =
        Some("the versions directory is writable by others".to_owned());
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    let held = core.rt.journal.as_ref().unwrap();
    assert!(held.install_pending.is_some(), "the install is retried");
    assert_eq!(held.installed.as_ref().unwrap().version, "1.0.0");
    {
        let state = rig.disk.state.lock().unwrap();
        assert!(state.install_calls.is_empty(), "nothing was installed");
        assert_eq!(state.installed.as_deref(), Some(crate::testkit::SHA_1));
    }
    let text = rig
        .disk
        .record(&id)
        .unwrap()
        .installed_binary
        .expect("the attempt is recorded");
    assert!(
        text.starts_with("failed: the environment is not safe to install from")
            && text.contains("writable by others"),
        "{text}"
    );
    assert!(
        rig.disk
            .state
            .lock()
            .unwrap()
            .state
            .iter()
            .all(|outcome| outcome.mark_failed.is_none()),
        "an untrusted source marks nothing failed"
    );
    // A retry while it is still untrusted installs nothing either.
    tokio::time::advance(retry_max + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert!(rig.disk.state.lock().unwrap().install_calls.is_empty());
    assert!(core.rt.journal.as_ref().unwrap().install_pending.is_some());
    // Trusted again: the retry installs.
    rig.disk.state.lock().unwrap().untrusted_source = None;
    tokio::time::advance(retry_max + Duration::from_secs(1)).await;
    core.on_tick().await;
    assert_eq!(rig.disk.state.lock().unwrap().install_calls, ["2.0.0"]);
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(journal.install_pending.is_none());
    assert_eq!(journal.installed.as_ref().unwrap().version, "2.0.0");
}

/// Collects the messages of every tracing event on this thread while the
/// returned guard lives.
pub(crate) fn capture_logs() -> (
    tracing::subscriber::DefaultGuard,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    use tracing::field::{Field, Visit};
    struct Message(String);
    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().unwrap().push(message.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(Capture(lines.clone()));
    (guard, lines)
}

/// Writes `outcome` as S3 with the state failing, then lets three retries
/// fail too. Returns the log lines about the state write.
async fn failing_state_retries(outcome: Outcome) -> Vec<String> {
    let (_guard, lines) = capture_logs();
    let mut rig = Rig::new().await;
    rig.disk.fail_state("S3", 100);
    let mut core = core_of!(rig);
    assert!(!core.write_state("S3", &outcome, false));
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(3600)).await;
        core.on_tick().await;
    }
    assert_eq!(core.rt.pending.len(), 1, "the outcome stays queued");
    let lines = lines.lock().unwrap();
    lines
        .iter()
        .filter(|line| line.starts_with("state write"))
        .cloned()
        .collect()
}

/// T3 ruling 9: a partial outcome whose original record is missing fails
/// every retry the same way; the first failure is logged, the retries are not.
#[tokio::test(start_paused = true)]
async fn the_retries_of_an_orphaned_partial_outcome_are_not_logged_again() {
    let mut record = open_ferry_update::state::SwitchRecord {
        id: "orphan".to_owned(),
        ..open_ferry_update::state::SwitchRecord::default()
    };
    record.installed_binary = Some("replaced".to_owned());
    let lines = failing_state_retries(Outcome {
        record,
        mark_failed: None,
    })
    .await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("state write S3 failed:"), "{lines:?}");
}

/// The control: a whole record's retries are each logged.
#[tokio::test(start_paused = true)]
async fn the_retries_of_a_whole_outcome_are_each_logged() {
    let record = open_ferry_update::state::SwitchRecord::started(
        "whole",
        "update",
        "1.0.0",
        "2.0.0",
        "2026-10-09T12:00:00Z",
    );
    let lines = failing_state_retries(Outcome {
        record,
        mark_failed: None,
    })
    .await;
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("state write S3 failed again"))
            .count(),
        3,
        "{lines:?}"
    );
}

/// No install retries while switching is off: the pending install stays in
/// the journal, but nothing is attempted, recorded, rescheduled or logged.
#[tokio::test(start_paused = true)]
async fn no_install_is_attempted_or_retried_while_switching_is_disabled() {
    let mut rig = Rig::new().await;
    let watch = rig.cfg.timings.watch;
    let retry_max = rig.cfg.timings.install_retry_max;
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    // After the switch, the environment was found unsafe.
    core.rt.disabled = Some("switching is off: the versions directory is unsafe".to_owned());
    rig.disk.state.lock().unwrap().untrusted_source =
        Some("the versions directory is writable by others".to_owned());
    tokio::time::advance(watch + Duration::from_secs(1)).await;
    core.on_tick().await;
    for _ in 0..3 {
        tokio::time::advance(retry_max + Duration::from_secs(1)).await;
        core.on_tick().await;
    }
    let held = core.rt.journal.as_ref().unwrap();
    let pending = held.install_pending.as_ref().expect("still pending");
    assert_eq!(pending.attempts, 0, "no attempt was made or counted");
    assert!(core.rt.install_due.is_none(), "no retry is scheduled");
    assert_eq!(held.installed.as_ref().unwrap().version, "1.0.0");
    assert!(rig.disk.state.lock().unwrap().install_calls.is_empty());
    assert_eq!(
        rig.disk.record(&id).unwrap().installed_binary.as_deref(),
        Some(open_ferry_update::state::installed_binary::PENDING),
        "no failed attempt was recorded"
    );
}

/// A restart that comes up with switching off schedules no install retry, but
/// still schedules the restart of a worker that did not start.
#[tokio::test(start_paused = true)]
async fn a_start_with_switching_off_schedules_no_install_retry_but_keeps_the_restart() {
    use open_ferry_update::journal::InstallPending;
    let rig = Rig::new().await;
    let mut journal = rig.disk.on_disk().unwrap();
    journal.install_pending = Some(InstallPending {
        target: crate::testkit::artifact("2.0.0"),
        source: "src".to_owned(),
        aside: String::new(),
        attempts: 1,
        next_try: None,
        started: "2026-10-09T11:00:00Z".to_owned(),
        record_id: None,
    });
    let now = tokio::time::Instant::now();
    let mut started = recovered_started(&rig, journal);
    started.switching_disabled = Some("switching is off".to_owned());
    started.degraded = Some("the active worker did not start".to_owned());
    let runtime =
        crate::core::Runtime::from_started(&started, &rig.registry, &rig.cfg, &rig.clock, now);
    assert!(runtime.install_due.is_none());
    assert!(runtime.restart.is_some(), "the restart is not gated");
    assert!(runtime.journal.unwrap().install_pending.is_some());
}

/// Item 10: a port that is dropped without `Registry::add` leaves a survivor
/// the registry tracks. The driver ends such a child (its poll sees only the
/// survivor entry still owns a sender, and kills it); when it exits, the
/// survivor wait wakes at once and the registry tracks nothing. (The driver's
/// own detection is `driver.rs`'s, tested with real processes there.)
#[tokio::test(start_paused = true)]
async fn the_survivor_wait_wakes_when_an_abandoned_survivor_exits() {
    use crate::driver::{Cmd, Event, ExitInfo, WorkerPort};
    use crate::process::ProcessIdentity;
    let mut rig = Rig::new().await;
    let events = rig.registry.events();
    let id = rig.registry.next_id();
    let (cmd, mut rx) = unbounded_channel::<Cmd>();
    let port = WorkerPort {
        id,
        identity: ProcessIdentity {
            pid: 900,
            started: 900,
        },
        reported_version: "2.0.0".to_owned(),
        protocol: 1,
        caps: Vec::new(),
        cmd: cmd.clone(),
    };
    events.track_survivor(port.clone());
    // Returned to the caller, which drops it without adding it.
    drop(port);
    // The child ends on a kill; the driver's poll sends one soon after.
    let child_events = events.clone();
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            if matches!(command, Cmd::Kill) {
                break;
            }
        }
        let _ = child_events.send((
            id,
            Event::Exited(ExitInfo {
                code: Some(1),
                success: false,
                text: "killed".to_owned(),
            }),
        ));
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = cmd.send(Cmd::Kill);
    });
    let mut core = core_of!(rig);
    assert_eq!(core.registry.survivors(), [id]);
    let started = tokio::time::Instant::now();
    assert!(
        core.wait_survivors_gone().await,
        "the wait woke, not stopped"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "it woke on the exit, not on a deadline: {:?}",
        started.elapsed()
    );
    assert!(core.registry.survivors().is_empty());
}

// ---- item 6: a failed directory sync is a failed install ---------------------------

#[tokio::test(start_paused = true)]
async fn a_failed_directory_sync_keeps_the_install_pending_until_a_retry_syncs() {
    let mut rig = Rig::new().await;
    // The install lands the file but cannot sync; the first retry finds the file
    // in place and cannot sync either; the second retry syncs and completes.
    rig.disk.state.lock().unwrap().dir_sync_fail = 2;
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let seen = async {
        // After the window and the first attempt.
        tokio::time::sleep(Duration::from_secs(640)).await;
        let state = rig.disk.state.lock().unwrap();
        (state.dir_sync_fail, state.install_calls.len())
    };
    let stopper = async {
        tokio::time::sleep(Duration::from_secs(1500)).await;
        let _ = command_tx.send(Command::Stop);
    };
    let (exit, (left, calls), ()) = tokio::join!(core.run(), seen, stopper);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(
        (left, calls),
        (1, 1),
        "the first attempt failed at the sync"
    );
    let journal = core.rt.journal.as_ref().unwrap();
    assert!(
        journal.install_pending.is_none(),
        "completed after the sync"
    );
    assert!(rig.disk.order().iter().filter(|s| *s == "dir-sync").count() >= 2);
    // The failure was recorded when it happened, then the success.
    let tried = rig.disk.state.lock().unwrap().state_writes.clone();
    assert!(tried.iter().any(|(_, o, _)| {
        o.record
            .installed_binary
            .as_deref()
            .is_some_and(|text| text.starts_with("failed"))
    }));
    assert_eq!(
        stated(&rig.disk, &id).record.installed_binary.as_deref(),
        Some("replaced")
    );
}

// ---- item 8: peers_gone only once the started worker is active ----------------------

#[tokio::test(start_paused = true)]
async fn a_started_worker_is_told_peers_gone_only_after_its_loaded() {
    let mut rig = Rig::new().await;
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    // A worker started at startup or by a restart: nothing is known of it yet.
    let a = core.rt.active.unwrap();
    {
        let tracked = core.rt.workers.get_mut(&a).unwrap();
        tracked.wants_peers_gone = false;
        tracked.awaiting_loaded = true;
    }
    let script = async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        // Still loading, a message a worker would ignore must not have gone.
        let early = rig.driver.log_of("send 1.0.0 peers_gone").len();
        rig.driver.emit(
            "1.0.0",
            crate::driver::Event::Message(crate::protocol::Message::Loaded { ms: 1 }),
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        let late = rig.driver.log_of("send 1.0.0 peers_gone").len();
        let _ = command_tx.send(Command::Stop);
        (early, late)
    };
    let (exit, (early, late)) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(early, 0, "sent before the worker was seen active");
    assert_eq!(late, 1, "sent once, after its loaded");
}

#[tokio::test(start_paused = true)]
async fn a_restarted_worker_is_told_peers_gone_only_after_its_loaded() {
    let mut rig = Rig::new().await;
    // The restarted worker is up at once, but says `loaded` only 4 s later.
    rig.driver.on(
        "1.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(4)),
            Step::Msg(crate::protocol::Message::Loaded { ms: 1 }),
        ],
    );
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    let script = async {
        // The restart is at about 5 s, its `loaded` at about 9 s.
        tokio::time::sleep(Duration::from_secs(7)).await;
        let before = (
            rig.driver.log_of("spawn 1.0.0").len(),
            rig.driver.log_of("send 1.0.0 peers_gone").len(),
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        let after = rig.driver.log_of("send 1.0.0 peers_gone").len();
        let _ = command_tx.send(Command::Stop);
        (before, after)
    };
    let (exit, (before, after)) = tokio::join!(core.run(), script);
    assert_eq!(exit, Exit::Stopped);
    // Restarted (second spawn), but not told yet (the first was told at start).
    assert_eq!(
        before,
        (2, 1),
        "told before the restarted worker was active"
    );
    assert_eq!(after, 2, "told once its loaded was seen");
}

// ---- the cancellable view comes before `Accepted` (A-M2) --------------------------

// A client that cancels the moment it is told `Accepted`, from another thread,
// must find the switch: the snapshot already shows this id as cancellable when
// the reply is sent.
#[tokio::test(start_paused = true)]
async fn the_switch_is_cancellable_in_the_snapshot_before_accepted_is_sent() {
    let mut rig = Rig::new().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let (shared, seen) = (rig.shared.clone(), seen.clone());
        rig.shared.on_accept(move || {
            let view = shared.snapshot().switch;
            let answer = shared.request_cancel();
            *seen.lock().unwrap() = Some((view, answer));
        });
    }
    let (_core, replies) = go!(rig, update(false));
    let (view, answer) = seen.lock().unwrap().take().expect("the accept hook ran");
    let accepted = replies
        .iter()
        .find_map(|reply| match reply {
            EndpointReply::Accepted { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("Accepted was sent");
    let view = view.expect("the snapshot shows the switch before Accepted is sent");
    assert_eq!(view.id, accepted);
    assert!(view.cancellable, "{view:?}");
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    assert_eq!(done_result(&replies)["result"], "cancelled");
}

/// Item 6: no install starts while a switch's foreground handler runs, clean-up
/// and resume included. An install that falls due during a cancelled switch's
/// resume, and is held at `install.before_replace`, must not hold A's resume up:
/// the resume is sent with no wait for the seam's arrival. Only after the
/// handler has ended does the install start (the seam arrives), and once it is
/// released the install lands. The observer is a plain thread on the real clock
/// (a paused tokio clock does not advance while a blocking task runs): at the
/// arrival it copies the driver's log, then releases the seam.
#[cfg(feature = "blue-green-test")]
#[tokio::test(start_paused = true)]
async fn a_due_install_held_at_its_seam_does_not_hold_up_a_cancelled_switchs_resume() {
    let mut rig = Rig::new().await;
    let root = real_install::scratch();
    let (disk, new_sha, old_sha, installed) = real_install::proxy(&rig, root.path());
    let seams = root.path().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("install.before_replace.hold"), []).unwrap();
    let arrived = seams.join("install.before_replace.arrived");
    let observer = {
        let arrived = arrived.clone();
        let release = seams.join("install.before_replace.release");
        let script = rig.driver.script.clone();
        std::thread::spawn(move || {
            let began = std::time::Instant::now();
            let mut seen = None;
            while began.elapsed() < std::time::Duration::from_secs(10) {
                if arrived.exists() {
                    seen = Some(script.lock().unwrap().log.clone());
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = std::fs::write(release, []);
            seen
        })
    };
    slow_cancelled_switch_script(&rig);
    let request = crate::core::SwitchRequest {
        sha256: new_sha.clone(),
        ..update(false)
    };
    let (tx, mut rx) = unbounded_channel();
    let mut core = crate::core::Core::new(
        &rig.cfg,
        &rig.driver,
        &rig.host,
        &disk,
        &rig.clock,
        &mut rig.registry,
        rig.shared.clone(),
        &mut rig.commands,
        rig.rt.take().expect("the runtime"),
    );
    pend_install(&mut core.rt, root.path(), new_sha, old_sha);
    let off = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        rig.host.set_mode(true, 1);
    };
    tokio::join!(core.handle_switch(request, tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    assert!(
        rig.driver.log().contains(&"send 1.0.0 resume".to_owned()),
        "A was resumed"
    );
    assert!(
        !arrived.exists(),
        "an install started inside the switch's handler"
    );
    assert!(core.rt.journal.as_ref().unwrap().install_pending.is_some());
    assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
    assert!(
        core.rt.install_due.is_some(),
        "the due retry was kept for after the handler"
    );
    // The handler has ended: now the install starts, reaches its seam, and
    // lands once the observer releases it.
    core.tick_background().await;
    let at_arrival = observer.join().unwrap().expect("the install never started");
    assert!(
        at_arrival.contains(&"send 1.0.0 resume".to_owned()),
        "the resume came before the install's arrival: {at_arrival:?}"
    );
    assert!(core.rt.journal.as_ref().unwrap().install_pending.is_none());
    assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");
}

/// A's hand over and resume take long enough for an install to fall due
/// inside a switch that the mode then cancels.
fn slow_cancelled_switch_script(rig: &Rig) {
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(6)),
            crate::testkit::handed_over(),
        ],
    );
    rig.driver.on(
        "1.0.0",
        "resume",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Msg(crate::protocol::Message::Resumed),
        ],
    );
}

/// Gives the core a pending install of the kept 2.0.0, not due at admission
/// and due one second in.
fn pend_install(
    rt: &mut crate::core::Runtime,
    root: &std::path::Path,
    new_sha: String,
    old_sha: String,
) {
    use open_ferry_update::journal::{Artifact, InstallPending, Installed};
    let journal = rt.journal.as_mut().unwrap();
    journal.installed = Some(Installed {
        version: "1.0.0".to_owned(),
        sha256: old_sha,
    });
    // A pending install is of the version that is confirmed.
    let target = Artifact {
        version: "2.0.0".to_owned(),
        sha256: new_sha,
        size: 10,
    };
    journal.confirmed = target.clone();
    journal.install_pending = Some(InstallPending {
        target,
        source: root
            .join("versions")
            .join("2.0.0")
            .join(crate::testkit::NAME)
            .to_string_lossy()
            .into_owned(),
        aside: String::new(),
        attempts: 0,
        next_try: None,
        started: "2026-10-09T11:00:00Z".to_owned(),
        record_id: None,
    });
    rt.install_due = Some(tokio::time::Instant::now() + Duration::from_secs(1));
}

/// An install that falls due while one switch runs is not starved by the next:
/// it runs once that handler has ended, before the next switch is admitted.
#[tokio::test(start_paused = true)]
async fn an_install_due_during_a_switch_runs_before_the_next_is_admitted() {
    let mut rig = Rig::new().await;
    let root = real_install::scratch();
    let (disk, new_sha, old_sha, installed) = real_install::proxy(&rig, root.path());
    slow_cancelled_switch_script(&rig);
    let request = crate::core::SwitchRequest {
        sha256: new_sha.clone(),
        ..update(false)
    };
    let (tx, mut rx) = unbounded_channel();
    let mut core = crate::core::Core::new(
        &rig.cfg,
        &rig.driver,
        &rig.host,
        &disk,
        &rig.clock,
        &mut rig.registry,
        rig.shared.clone(),
        &mut rig.commands,
        rig.rt.take().expect("the runtime"),
    );
    pend_install(&mut core.rt, root.path(), new_sha, old_sha);
    let off = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        rig.host.set_mode(true, 1);
    };
    tokio::join!(core.handle_switch(request.clone(), tx), off);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        b"old binary",
        "not installed inside the handler"
    );
    // The next switch, admitted while off (a manual one, so it runs): the
    // overdue install goes first.
    disk.fake.add_pin("pin");
    let (tx, mut rx) = unbounded_channel();
    core.handle_switch(request, tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");
    let order = disk.fake.order();
    let j6 = order.iter().position(|name| name == "J6");
    let second_j1 = order
        .iter()
        .enumerate()
        .filter(|(_, name)| *name == "J1")
        .nth(1)
        .map(|(at, _)| at);
    assert!(
        j6.is_some() && second_j1.is_some() && j6 < second_j1,
        "the install's J6 came before the next switch's J1: {order:?}"
    );
}

// ---- item 10: an exit before `loaded` is a failed start --------------------

/// A worker that handshakes and then exits before it reports `loaded` has not
/// started (6.1 g): its rung is charged a try. Three tries on the run, three on
/// `previous`, three on the installed file by hash (a rung of its own, with no
/// untrusted refusal needed to reach it), then the ladder is spent: the status
/// is degraded and a new cycle starts again from the first rung on the restart
/// backoff.
#[tokio::test(start_paused = true)]
async fn workers_that_exit_before_loaded_walk_the_restart_ladder() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    for version in ["1.0.0", "0.9.0"] {
        rig.driver.on(
            version,
            "spawn",
            vec![
                Step::Sleep(Duration::from_secs(1)),
                Step::Exit(false, "crashed before loaded"),
            ],
        );
    }
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 3000);
    let places = rig.driver.places();
    let rung = |text: &str, times: usize| std::iter::repeat_n(text.to_owned(), times);
    // The rig's own start, which was not a restart, then three tries on the
    // run, three on `previous`, three on the installed file: one cycle.
    let cycle: Vec<String> = rung("1.0.0 kept", 4)
        .chain(rung("0.9.0 kept", 3))
        .chain(rung("1.0.0 installed", 3))
        .collect();
    assert_eq!(places[..cycle.len()], cycle[..], "{places:?}");
    // The spent ladder starts a new cycle from its first rung, on the backoff.
    let again: Vec<String> = rung("1.0.0 kept", 3)
        .chain(rung("0.9.0 kept", 3))
        .chain(rung("1.0.0 installed", 3))
        .collect();
    assert_eq!(
        places[cycle.len()..cycle.len() + again.len()],
        again[..],
        "{places:?}"
    );
    assert!(core.rt.degraded.is_some(), "{places:?}");
}

/// The reverse: a worker that reached `loaded` and exited later starts the
/// count again, so the restart is the same first rung, however often it dies.
#[tokio::test(start_paused = true)]
async fn a_worker_that_loaded_and_exits_later_restarts_on_the_first_rung_again() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.on(
        "1.0.0",
        "spawn",
        vec![
            crate::testkit::loaded(),
            Step::Sleep(Duration::from_secs(1)),
            Step::Exit(false, "crashed after loaded"),
        ],
    );
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 600);
    let log = rig.driver.log();
    let spawns = log
        .iter()
        .filter(|line| **line == "spawn 1.0.0 active")
        .count();
    assert!(spawns > 4, "it restarted each time: {spawns} {log:?}");
    assert!(
        log.iter().all(|line| line != "spawn 0.9.0 active"),
        "{log:?}"
    );
}

/// Item 7: the install seam `install.before_replace` fails the supervised
/// install before anything is replaced. The pending install stays in the
/// journal with the attempt counted and the next try set, the installed file
/// is unchanged, and the prepared copy beside it is removed.
#[cfg(feature = "blue-green-test")]
#[tokio::test(start_paused = true)]
async fn a_failure_before_the_replace_keeps_the_install_pending_and_removes_the_prepared_copy() {
    use open_ferry_update::journal::{Artifact, InstallPending, Installed};
    let mut rig = Rig::new().await;
    let root = real_install::scratch();
    let (disk, new_sha, old_sha, installed) = real_install::proxy(&rig, root.path());
    let seams = root.path().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    std::fs::write(seams.join("install.before_replace.fail"), []).unwrap();
    let mut core = core_of_proxy(&mut rig, &disk);
    {
        let journal = core.rt.journal.as_mut().unwrap();
        journal.installed = Some(Installed {
            version: "1.0.0".to_owned(),
            sha256: old_sha,
        });
        // A pending install is for what is confirmed (item 1), so the target
        // is the confirmed version.
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: new_sha.clone(),
            size: 10,
        };
        journal.confirmed = target.clone();
        journal.install_pending = Some(InstallPending {
            target,
            source: String::new(),
            aside: String::new(),
            attempts: 0,
            next_try: None,
            started: "2026-10-09T11:00:00Z".to_owned(),
            record_id: None,
        });
    }
    core.rt.install_due = Some(tokio::time::Instant::now());
    core.on_tick().await;
    assert!(
        seams.join("install.before_replace.failed").exists(),
        "the seam did not fire"
    );
    let journal = core.rt.journal.as_ref().unwrap();
    let pending = journal.install_pending.as_ref().expect("still pending");
    assert_eq!(pending.target.sha256, new_sha);
    assert_eq!(pending.attempts, 1, "the failed attempt is counted");
    assert!(pending.next_try.is_some(), "the next try is set");
    assert!(core.rt.install_due.is_some(), "a retry is scheduled");
    assert_eq!(journal.installed.as_ref().unwrap().version, "1.0.0");
    assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
    let beside: Vec<_> = std::fs::read_dir(installed.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        beside,
        [std::ffi::OsString::from(crate::testkit::NAME)],
        "the prepared copy was left beside the installed file"
    );
}

/// A core over the proxy disk and the rig's runtime.
#[cfg(feature = "blue-green-test")]
fn core_of_proxy<'a>(
    rig: &'a mut Rig,
    disk: &'a real_install::ProxyDisk,
) -> crate::core::Core<
    'a,
    crate::testkit::FakeDriver,
    crate::testkit::FakeHost,
    real_install::ProxyDisk,
> {
    crate::core::Core::new(
        &rig.cfg,
        &rig.driver,
        &rig.host,
        disk,
        &rig.clock,
        &mut rig.registry,
        rig.shared.clone(),
        &mut rig.commands,
        rig.rt.take().expect("the runtime"),
    )
}

// ---- round 5: the ladder model -----------------------------------------------

fn runs_of(places: &[String], from: usize, text: &str, times: usize) {
    assert!(
        places[from..from + times].iter().all(|place| place == text),
        "{times} x {text} from {from}: {places:?}"
    );
}

/// (a) The return's progress is translated at J-R2: A fails its three tries,
/// P handshakes (one try on its rung), J-R2 runs and P exits before `loaded`.
/// P is tried exactly twice more, then the installed file by hash, then the
/// status is degraded.
#[tokio::test(start_paused = true)]
async fn a_the_return_ladders_progress_is_translated_at_j_r2() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![
            SpawnFail::Start,
            SpawnFail::Start,
            SpawnFail::Start,
            SpawnFail::Start,
            SpawnFail::Start,
            SpawnFail::Start,
        ]),
    );
    rig.driver.on(
        "0.9.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(1)),
            Step::Exit(false, "crashed before loaded"),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 1000);
    let places = rig.driver.places();
    let at = places
        .iter()
        .position(|place| place == "0.9.0 kept")
        .expect("P started");
    // A's three tries came first, in the return.
    runs_of(&places, at - 3, "1.0.0 kept", 3);
    // P: the return's start and two more; no more than three on its rung.
    runs_of(&places, at, "0.9.0 kept", 3);
    runs_of(&places, at + 3, "1.0.0 installed", 3);
    let order = rig.disk.order();
    assert!(order.contains(&"J-R2".to_owned()), "{order:?}");
    // The installed file's three failed, so the ladder is spent: a new cycle
    // starts again at the first rung (`confirmed` is P).
    assert_eq!(places[at + 6], "0.9.0 kept", "{places:?}");
}

/// Ticks the core past the restart backoff `times`, noting how many workers
/// each tick started.
macro_rules! ticks {
    ($core:ident, $rig:ident, $times:expr) => {{
        let mut started = Vec::new();
        for _ in 0..$times {
            tokio::time::advance(Duration::from_secs(70)).await;
            let before = $rig.driver.places().len();
            $core.on_tick().await;
            started.push($rig.driver.places().len() - before);
        }
        started
    }};
}

/// (b) A worker on `confirmed` (the run it had, `active`, would not start)
/// reports `loaded` and exits later: the restart is on `confirmed` with no
/// failed attempts, not back on `active`.
#[tokio::test(start_paused = true)]
async fn b_a_worker_that_loaded_on_confirmed_restarts_on_confirmed() {
    let mut rig = Rig::new().await;
    {
        let journal = rig.rt.as_mut().unwrap().journal.as_mut().unwrap();
        journal.active = crate::testkit::artifact("2.0.0");
        rig.disk.state.lock().unwrap().on_disk = Some(journal.clone());
    }
    rig.driver.fail_spawn("2.0.0", SpawnFail::Start);
    rig.driver.on(
        "1.0.0",
        "spawn",
        vec![
            crate::testkit::loaded(),
            Step::Sleep(Duration::from_secs(1)),
            Step::Exit(false, "crashed after loaded"),
        ],
    );
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 600);
    let places = rig.driver.places();
    // The rig's start, three tries on `active`, then `confirmed`, which keeps
    // being restarted on its own rung.
    assert_eq!(places[0], "1.0.0 kept");
    runs_of(&places, 1, "2.0.0 kept", 3);
    assert!(places.len() > 8, "{places:?}");
    assert!(
        places[4..].iter().all(|place| place == "1.0.0 kept"),
        "`active` is not tried again: {places:?}"
    );
}

/// (f) A spent ladder is degraded, spends the backoff, and starts a new
/// cycle from its first permitted rung.
#[tokio::test(start_paused = true)]
async fn f_a_spent_ladder_starts_a_new_cycle_from_the_first_rung() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 1);
    let before = rig.driver.places().len();
    let started = ticks!(core, rig, 12);
    let places = rig.driver.places()[before..].to_vec();
    // Nine tries (3 x kept, 3 x previous, 3 x installed), minus any already
    // made inside the window, then a tick that starts nothing: degraded.
    let quiet = started.iter().position(|n| *n == 0).expect("spent");
    assert!(core.rt.degraded.is_some());
    assert_eq!(started[quiet + 1], 1, "a new cycle: {started:?} {places:?}");
    assert_eq!(
        places[quiet], "1.0.0 kept",
        "from the first rung: {places:?}"
    );
}

/// (h) A refusal, then exhaustion: the refused location stays excluded in the
/// next cycle.
#[tokio::test(start_paused = true)]
async fn h_a_refused_location_stays_excluded_in_the_next_cycle() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Untrusted);
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 1);
    let _ = ticks!(core, rig, 30);
    let log = rig.driver.log();
    assert_eq!(
        log.iter().filter(|l| l.starts_with("spawn 0.9.0")).count(),
        1,
        "refused once, then excluded: {log:?}"
    );
    assert!(
        rig.driver
            .places()
            .iter()
            .filter(|place| *place == "1.0.0 kept")
            .count()
            >= 7,
        "cycles went on without it"
    );
}

/// (i) Every rung refused: degraded, and the evaluations are spaced by the
/// backoff, never a busy loop.
#[tokio::test(start_paused = true)]
async fn i_every_rung_refused_is_degraded_and_spaced_by_the_backoff() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    rig.driver.fail_spawn("1.0.0", SpawnFail::Untrusted);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Untrusted);
    let events = rig.registry.events();
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    let id = core.rt.active.unwrap();
    events.send(crash(id)).unwrap();
    window!(core, command_tx, 1);
    let _ = ticks!(core, rig, 10);
    assert!(core.rt.degraded.is_some());
    let spawns = rig.driver.log().len();
    let due = core.rt.restart.as_ref().expect("scheduled again").due;
    // No time passes: nothing is evaluated.
    core.on_tick().await;
    core.on_tick().await;
    assert_eq!(rig.driver.log().len(), spawns);
    assert_eq!(core.rt.restart.as_ref().unwrap().due, due);
    // At its time it is evaluated once and scheduled after the backoff again.
    tokio::time::advance(due - tokio::time::Instant::now()).await;
    core.on_tick().await;
    let next = core.rt.restart.as_ref().unwrap().due;
    assert!(
        next > tokio::time::Instant::now(),
        "spaced, not a busy loop"
    );
    assert_eq!(rig.driver.log().len(), spawns, "nothing permitted to start");
}

/// (g) An installed rung and a kept rung with the same bytes are two rungs:
/// the return starts from the installed file (its kept copy refused), and
/// after J-R2 the worker's progress stays on the installed location, so it
/// gets three tries in all there, not one on a kept copy mapped from it.
#[tokio::test(start_paused = true)]
async fn g_the_installed_rung_is_kept_when_the_return_becomes_the_restart() {
    let mut rig = Rig::new().await;
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::UntrustedKept);
    rig.driver.on(
        "1.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(1)),
            Step::Exit(false, "crashed before loaded"),
        ],
    );
    let command_tx = rig.command_tx.clone();
    let (mut core, _replies) = go!(rig, update(false));
    window!(core, command_tx, 120);
    let places = rig.driver.places();
    let first: Vec<_> = places
        .iter()
        .skip_while(|place| *place != "1.0.0 installed")
        .take_while(|place| *place == "1.0.0 installed")
        .collect();
    assert_eq!(
        first.len(),
        3,
        "three tries on the installed rung: {places:?}"
    );
}

/// (j) A crash after J-R2 and before `loaded`: the restart is on the right
/// rung (a failed try on the worker's own) and the forward record is
/// unchanged.
#[tokio::test(start_paused = true)]
async fn j_a_crash_after_j_r2_before_loaded_restarts_on_its_rung_and_keeps_the_record() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let (mut record, mut before) = (None, 0);
    let script = async {
        // The return is over by now: a worker of P runs and has not loaded.
        tokio::time::sleep(Duration::from_secs(100)).await;
        record = Some(stated(&rig.disk, &id));
        before = rig.driver.places().len();
        rig.driver.emit(
            "0.9.0",
            crate::driver::Event::Exited(crate::driver::ExitInfo {
                code: Some(1),
                success: false,
                text: "crashed".to_owned(),
            }),
        );
        tokio::time::sleep(Duration::from_secs(120)).await;
    };
    tokio::select! {
        exit = core.run() => panic!("the core exited: {exit:?}"),
        () = script => {}
    }
    let places = rig.driver.places();
    assert!(places.len() > before, "restarted: {places:?}");
    assert_eq!(places[before], "0.9.0 kept", "on its own rung: {places:?}");
    // The crash was before `loaded`: it is one failed try on the worker's own
    // rung, which the worker that replaced it carries.
    let restarted = core
        .rt
        .workers
        .get(&core.rt.active.expect("a worker"))
        .unwrap();
    assert_eq!(
        restarted.attempts, 1,
        "one failed try is charged to the rung"
    );
    assert_eq!(stated(&rig.disk, &id), record.unwrap(), "record unchanged");
    assert!(rig.disk.order().contains(&"J-R2".to_owned()));
}

fn pending_install_of_confirmed(
    core: &mut crate::core::Core<
        '_,
        crate::testkit::FakeDriver,
        crate::testkit::FakeHost,
        crate::testkit::FakeDisk,
    >,
    record_id: &str,
) {
    use open_ferry_update::journal::{InstallPending, Installed};
    let journal = core.rt.journal.as_mut().unwrap();
    journal.installed = Some(Installed {
        version: "0.0.1".to_owned(),
        sha256: "old-installed-hash".to_owned(),
    });
    journal.install_pending = Some(InstallPending {
        target: journal.confirmed.clone(),
        source: String::new(),
        aside: String::new(),
        attempts: 0,
        next_try: None,
        started: "2026-10-09T11:00:00Z".to_owned(),
        record_id: Some(record_id.to_owned()),
    });
    core.rt.install_due = Some(tokio::time::Instant::now());
}

/// Item 1: an install pending of A (confirmed) whose first attempt failed
/// must never land once the return has moved to P: it is superseded, its
/// record says so, and `install_pending` is cleared.
#[tokio::test(start_paused = true)]
async fn an_install_of_a_never_lands_after_the_return_moves_to_p() {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.rt.as_mut().unwrap().journal = rig.disk.on_disk();
    {
        let mut state = rig.disk.state.lock().unwrap();
        state.installed = Some("old-installed-hash".to_owned());
        state.install_fail = 1;
    }
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    pending_install_of_confirmed(&mut core, "op-install");
    core.on_tick().await;
    assert_eq!(
        rig.disk.state.lock().unwrap().install_calls,
        ["1.0.0"],
        "the first attempt was made"
    );
    let pending = core.rt.journal.as_ref().unwrap().install_pending.clone();
    assert_eq!(pending.expect("still pending").attempts, 1, "and it failed");
    // The switch to B fails; A fails three starts; the return moves to P.
    let (tx, _rx) = unbounded_channel();
    core.handle_switch(update(false), tx).await;
    window!(core, command_tx, 1200);
    let state = rig.disk.state.lock().unwrap();
    assert_eq!(
        state.install_calls,
        ["1.0.0"],
        "no second attempt: {:?}",
        state.order
    );
    assert_eq!(state.installed.as_deref(), Some("old-installed-hash"));
    drop(state);
    let journal = core.rt.journal.as_ref().unwrap();
    assert_eq!(journal.confirmed.version, "0.9.0", "J-R2 ran on P");
    assert!(journal.install_pending.is_none(), "cleared");
    assert_eq!(
        stated(&rig.disk, "op-install").record.installed_binary,
        Some("superseded".to_owned()),
    );
}

/// Item 1, the deferred case: a pending install of the return's own target
/// stays pending, keeps its retry, and lands only after J-R2.
#[tokio::test(start_paused = true)]
async fn an_install_of_the_returns_own_target_waits_for_j_r2() {
    let mut rig = Rig::new().await;
    rig.disk.state.lock().unwrap().installed = Some("old-installed-hash".to_owned());
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    // The return's own start takes two retries: the install falls due while
    // the return is still open.
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start]),
    );
    let command_tx = rig.command_tx.clone();
    let mut core = core_of!(rig);
    pending_install_of_confirmed(&mut core, "op-install");
    core.rt.install_due = Some(tokio::time::Instant::now() + Duration::from_secs(2));
    let (tx, _rx) = unbounded_channel();
    core.handle_switch(update(false), tx).await;
    window!(core, command_tx, 400);
    let order = rig.disk.order();
    let j_r2 = order.iter().position(|name| name == "J-R2").expect("J-R2");
    let install = order
        .iter()
        .position(|name| name == "install")
        .unwrap_or_else(|| panic!("the install landed after the return: {order:?}"));
    assert!(j_r2 < install, "{order:?}");
    assert_eq!(rig.disk.state.lock().unwrap().install_calls, ["1.0.0"]);
}

/// Item 1, the deferral itself (round 5b): an install of the return's own
/// target is due while the return is still open. It is watched a second at a
/// time: `install_calls` stays empty until J-R2 has run, then the install is
/// made. The test above reads the order only after the whole run, so a
/// deferral that was off would show there only as an install before J-R2; this
/// one names the step where it happens.
#[tokio::test(start_paused = true)]
async fn an_install_due_while_the_return_is_open_is_not_made_before_j_r2() {
    let mut rig = Rig::new().await;
    rig.disk.state.lock().unwrap().installed = Some("old-installed-hash".to_owned());
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    rig.driver.fail_spawn(
        "1.0.0",
        SpawnFail::Script(vec![SpawnFail::Start, SpawnFail::Start]),
    );
    let mut core = core_of!(rig);
    pending_install_of_confirmed(&mut core, "op-install");
    core.rt.install_due = Some(tokio::time::Instant::now() + Duration::from_secs(2));
    let (tx, _rx) = unbounded_channel();
    core.handle_switch(update(false), tx).await;
    let (mut open_steps, mut steps) = (0, 0);
    while core.rt.returning.is_some() {
        window_running!(core, 1);
        steps += 1;
        assert!(steps < 200, "the return never ended");
        if core.rt.returning.is_some() {
            open_steps += 1;
            let calls = rig.disk.state.lock().unwrap().install_calls.clone();
            assert!(
                calls.is_empty(),
                "an install was made with the return open (step {steps}): {calls:?} {:?}",
                rig.disk.order()
            );
        }
    }
    assert!(
        open_steps >= 3,
        "the install fell due while the return was open: {open_steps}"
    );
    assert!(rig.disk.order().contains(&"J-R2".to_owned()));
    window_running!(core, 60);
    assert_eq!(rig.disk.state.lock().unwrap().install_calls, ["1.0.0"]);
}

/// Item 3 (b): a recovered core whose replay already gave the forward switch
/// its record (the journal holds the outcome) makes no extra write of it when
/// the next spawn fails and nothing can run, even with S3 and J7 failing.
#[tokio::test(start_paused = true)]
async fn an_unstarted_return_already_recorded_is_not_written_again() {
    an_unstarted_return_recorded_by(false).await;
}

/// The same, with the evidence only in the state file's history: the replay
/// gave it the forward switch's failed record, a later switch is its
/// `last_switch`, and the journal has no outcome for the id.
#[tokio::test(start_paused = true)]
async fn an_unstarted_return_recorded_only_in_the_state_history_is_not_written_again() {
    an_unstarted_return_recorded_by(true).await;
}

async fn an_unstarted_return_recorded_by(in_state_history: bool) {
    use open_ferry_update::journal::{Blame, Return};
    let mut rig = Rig::new().await;
    let (core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    let mut journal = core.rt.journal.clone().unwrap();
    drop(core);
    journal.switch.as_mut().unwrap().return_ = Some(Return {
        target: journal.confirmed.clone(),
        blame: Blame::NewVersion,
        why: "B rejected".to_owned(),
        decided: "2026-10-09T00:00:00Z".to_owned(),
        installed: false,
    });
    let record = open_ferry_update::state::SwitchRecord {
        id,
        result: Some("failed-after-switch".to_owned()),
        ..Default::default()
    };
    let scratch = tempfile::tempdir().unwrap();
    if in_state_history {
        rig.cfg.data = open_ferry_update::data_dir::DataDir::at(scratch.path().join("data"));
        std::fs::create_dir_all(rig.cfg.data.state_file().parent().unwrap()).unwrap();
        open_ferry_update::state::State {
            last_switch: Some(open_ferry_update::state::SwitchRecord {
                id: "sw-later".to_owned(),
                result: Some("switched".to_owned()),
                ..Default::default()
            }),
            history: vec![
                open_ferry_update::state::SwitchRecord {
                    id: "sw-later".to_owned(),
                    result: Some("switched".to_owned()),
                    ..Default::default()
                },
                record,
            ],
            ..Default::default()
        }
        .save(&rig.cfg.data.state_file())
        .unwrap();
        assert!(
            journal.outcomes.is_empty(),
            "the journal has no outcome for the id"
        );
    } else {
        journal.outcomes.push(Outcome {
            record,
            mark_failed: None,
        });
    }
    let started = recovered_started(&rig, journal);
    rig.rt = Some(crate::core::Runtime::from_started(
        &started,
        &rig.registry,
        &rig.cfg,
        &rig.clock,
        tokio::time::Instant::now(),
    ));
    assert!(
        rig.rt
            .as_ref()
            .unwrap()
            .returning
            .as_ref()
            .unwrap()
            .unstarted_recorded
    );
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.disk.fail_state("S3", 9);
    rig.disk.fail_journal("J7", 9);
    let before = rig.disk.order().len();
    let writes = rig.disk.state.lock().unwrap().state_writes.len();
    let mut core = core_of!(rig);
    let _ = ticks!(core, rig, 8);
    assert!(core.rt.degraded.is_some(), "nothing could start");
    let order = rig.disk.order()[before..].to_vec();
    assert!(
        order.iter().all(|name| name != "J7" && name != "S3"),
        "no extra write was attempted: {order:?}"
    );
    assert_eq!(rig.disk.state.lock().unwrap().state_writes.len(), writes);
}

/// Item 10: B exits in its window (row f), the return is decided, and every
/// rung fails: the forward switch is recorded at once with the stored blame
/// and why, B in the failed list, no `returned_to`, the return kept, the
/// state degraded. A later cycle records nothing again.
async fn row_f_with_no_rung_that_starts(window_secs: u64) -> (Rig, String) {
    let mut rig = Rig::new().await;
    with_previous(&mut rig);
    rig.driver.on(
        "2.0.0",
        "go",
        vec![
            Step::Sleep(Duration::from_secs(60)),
            Step::Exit(false, "crashed"),
        ],
    );
    rig.driver.fail_spawn("1.0.0", SpawnFail::Start);
    rig.driver.fail_spawn("0.9.0", SpawnFail::Start);
    let command_tx = rig.command_tx.clone();
    let (mut core, replies) = go!(rig, update(false));
    let id = done_result(&replies)["id"].as_str().unwrap().to_owned();
    window!(core, command_tx, window_secs);
    assert!(core.rt.degraded.is_some());
    let journal = core.rt.journal.as_ref().unwrap();
    let ret = journal
        .switch
        .as_ref()
        .and_then(|switch| switch.return_.as_ref())
        .expect("the return is kept with the switch");
    assert_eq!(ret.blame, open_ferry_update::journal::Blame::NewVersion);
    drop(core);
    (rig, id)
}

fn forward_writes(rig: &Rig, id: &str) -> usize {
    rig.disk
        .state
        .lock()
        .unwrap()
        .state_writes
        .iter()
        .filter(|(_, outcome, _)| outcome.record.id == id && outcome.record.returned_to.is_none())
        .filter(|(_, outcome, _)| outcome.record.result.as_deref() == Some("failed-after-switch"))
        .count()
}

#[tokio::test(start_paused = true)]
async fn row_f_with_no_rung_that_starts_records_the_outcome_once() {
    let (rig, id) = row_f_with_no_rung_that_starts(100).await;
    let outcome = stated(&rig.disk, &id);
    assert_eq!(
        outcome.record.result.as_deref(),
        Some("failed-after-switch")
    );
    assert_eq!(outcome.record.phase.as_deref(), Some("watching"));
    assert_eq!(outcome.record.blame.as_deref(), Some("new-version"));
    assert_eq!(outcome.record.returned_to, None);
    assert_eq!(outcome.mark_failed.expect("B is marked").version, "2.0.0");
    assert_eq!(forward_writes(&rig, &id), 1);
    assert_eq!(
        rig.disk
            .state
            .lock()
            .unwrap()
            .state
            .iter()
            .filter(|outcome| outcome.record.id == id)
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn row_f_with_no_rung_that_starts_records_nothing_again_in_a_new_cycle() {
    // 20 minutes: many cycles of the spent ladder on the backoff.
    let (rig, id) = row_f_with_no_rung_that_starts(1300).await;
    let places = rig.driver.places();
    assert!(
        places.iter().filter(|place| *place == "1.0.0 kept").count() > 6,
        "later cycles ran: {places:?}"
    );
    assert_eq!(forward_writes(&rig, &id), 1, "recorded once");
}

fn record_at(id: &str) -> open_ferry_update::state::SwitchRecord {
    open_ferry_update::state::SwitchRecord {
        id: id.to_owned(),
        at: "2026-10-09T10:00:00Z".to_owned(),
        ..Default::default()
    }
}

fn last_switch_of(ids: &[&str]) -> (Option<String>, Vec<String>) {
    let mut state = open_ferry_update::state::State::default();
    for id in ids {
        state.upsert_record(record_at(id));
    }
    (
        state.last_switch.map(|record| record.id),
        state.history.into_iter().map(|record| record.id).collect(),
    )
}

#[tokio::test(start_paused = true)]
async fn item_8_a_cores_real_ids_order_its_operations_in_the_same_second() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    let (first, second) = (core.new_id(), core.new_id());
    assert!(first.starts_with("sw-") && first.len() == "sw-".len() + 16 + "-1".len());
    for order in [[&first, &second], [&second, &first]] {
        let (last, history) = last_switch_of(&[order[0], order[1]]);
        assert_eq!(last.as_deref(), Some(second.as_str()), "{order:?}");
        assert_eq!(history, [second.clone(), first.clone()], "{order:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn item_8_b_a_core_started_later_is_ahead_in_the_same_second() {
    let mut earlier_rig = Rig::new().await;
    let mut later_rig = Rig::new().await;
    // The second Core starts 5 ms after the first; the counters are equal, so
    // only the start time can order them (and the first's counter is larger).
    let mut earlier = core_of!(earlier_rig);
    let (_, _, older) = (earlier.new_id(), earlier.new_id(), earlier.new_id());
    later_rig.clock.advance(Duration::from_millis(5));
    let mut later = core_of!(later_rig);
    let newer = later.new_id();
    for order in [[&older, &newer], [&newer, &older]] {
        let (last, history) = last_switch_of(&[order[0], order[1]]);
        assert_eq!(last.as_deref(), Some(newer.as_str()), "{order:?}");
        assert_eq!(history, [newer.clone(), older.clone()], "{order:?}");
    }
}

// ---- round 7, items 6 and 8: a cancel and A's failure -----------------------------

/// What a cancel accepted during the clean-up that follows A's failure leaves:
/// the switch ends cancelled, nothing is recorded, nothing is returned, and A's
/// failure is the active worker's ordinary one (a restart is due).
async fn assert_cancel_won(
    disk: &crate::testkit::FakeDisk,
    rt: &crate::core::Runtime,
    result: &serde_json::Value,
) {
    settle().await;
    assert_eq!(result["result"], "cancelled", "{result}");
    assert!(
        disk.state.lock().unwrap().state.is_empty(),
        "no record, no failed mark"
    );
    assert!(rt.returning.is_none(), "no return");
    let order = disk.order();
    assert!(
        order.iter().all(|n| n != "J-R1"),
        "no return journalled: {order:?}"
    );
    let journal = rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert!(journal.outcomes.is_empty());
    assert!(rt.restart.is_some(), "A's restart is due");
}

/// Item 6, the handshake: A exits while B's spawn is pending (`Shaken::AGone`),
/// B's child is reaped with its termination held, and with `cancel` a cancel is
/// accepted during that clean-up.
async fn a_exits_in_the_handshake_with_a_cancel(cancel: bool) {
    let _watchdog = Watchdog::new("a_exit_in_handshake_cancel");
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Survive);
    rig.driver.hold_termination(true);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        rig.driver.emit("1.0.0", a_exits());
        tokio::time::sleep(Duration::from_secs(2)).await;
        if cancel {
            assert_eq!(
                shared.request_cancel(),
                crate::snapshot::CancelAnswer::Cancelled
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        rig.driver.hold_termination(false);
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    if cancel {
        assert_cancel_won(&rig.disk, &core.rt, &result).await;
    } else {
        failed(&result, "loading", "environment");
        assert_eq!(result["returned"], true);
    }
}

#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_bs_child_is_reaped_after_a_exits_in_the_handshake_wins() {
    a_exits_in_the_handshake_with_a_cancel(true).await;
}

#[tokio::test(start_paused = true)]
async fn without_a_cancel_a_exiting_in_the_handshake_is_still_failed_to_load() {
    a_exits_in_the_handshake_with_a_cancel(false).await;
}

/// Item 6, the wait for `loaded`: A exits while B loads (`Waited::AGone`), B's
/// kill is held for 3 s, and with `cancel` a cancel is accepted during it.
async fn a_exits_in_the_loaded_wait_with_a_cancel(cancel: bool) {
    let _watchdog = Watchdog::new("a_exit_in_loaded_wait_cancel");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![
            Step::Sleep(Duration::from_secs(20)),
            crate::testkit::loaded(),
        ],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exits());
        tokio::time::sleep(Duration::from_secs(1)).await;
        if cancel {
            assert_eq!(
                shared.request_cancel(),
                crate::snapshot::CancelAnswer::Cancelled
            );
        }
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    if cancel {
        assert_cancel_won(&rig.disk, &core.rt, &result).await;
    } else {
        failed(&result, "loading", "environment");
        assert_eq!(result["returned"], true);
    }
}

#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_b_is_killed_after_a_exits_in_the_loaded_wait_wins() {
    a_exits_in_the_loaded_wait_with_a_cancel(true).await;
}

#[tokio::test(start_paused = true)]
async fn without_a_cancel_a_exiting_in_the_loaded_wait_is_still_failed_to_load() {
    a_exits_in_the_loaded_wait_with_a_cancel(false).await;
}

/// Item 6, the hand over: A exits during it (`a_gone`), B's kill is held for
/// 3 s, and a cancel is accepted during it.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_b_is_killed_after_a_exits_in_the_hand_over_wins() {
    let _watchdog = Watchdog::new("a_exit_in_handover_cancel");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![
            Step::Sleep(Duration::from_secs(5)),
            Step::Exit(false, "died"),
        ],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(
            shared.request_cancel(),
            crate::snapshot::CancelAnswer::Cancelled
        );
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won(&rig.disk, &core.rt, &result).await;
}

/// Item 8: A is lost during B's clean-up and no cancel is accepted. The window
/// is closed in the core's own picture too, so a tick (here, those of the wait
/// for the lost A) does not publish it open again: a cancel is `TooLate`, and
/// the switch ends failed, never cancelled.
#[tokio::test(start_paused = true)]
async fn a_closed_cancel_window_is_not_published_open_again_by_a_tick() {
    let _watchdog = Watchdog::new("closed_window_stays_closed");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "2.0.0",
        "spawn",
        vec![Step::Msg(crate::protocol::Message::LoadFailed {
            reason: "no".to_owned(),
        })],
    );
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver
            .emit("1.0.0", crate::driver::Event::Lost("gone".to_owned()));
        // B's clean-up ends at 3 s and the window closes; the wait for the lost
        // A then ticks and publishes. The cancel comes after those ticks.
        tokio::time::sleep(Duration::from_secs(6)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::TooLate);
    let result = done_result(&drain(&mut rx));
    failed(&result, "loading", "environment");
}

// ---- round 8, items 3 and 4: one arbitration, and A's own progress ----------------

/// Puts A's entry on a rung, as a ladder start would have: `last_try` is a
/// start that has not reported `loaded` after two failed tries (so its exit
/// spends the rung); otherwise it has loaded. Returns what A's ladder stands
/// at once A is gone, the progress every restart of A must go on from.
macro_rules! a_on_a_rung {
    ($core:ident, $rig:ident, $last_try:expr) => {{
        let a = $core.rt.active.expect("A is active");
        let artifact = $core.rt.workers[&a].artifact.clone();
        let rung = crate::startup::Run {
            binary: $rig
                .cfg
                .data
                .binary(&artifact.version, &$rig.cfg.binary_name),
            artifact,
        };
        let worker = $core.rt.workers.get_mut(&a).unwrap();
        worker.rung = Some(rung);
        worker.attempts = if $last_try { 2 } else { 0 };
        worker.awaiting_loaded = $last_try;
        worker.progress_after_exit()
    }};
}

/// Item 3, `back_to_a`'s `Resumed::No` after an accepted cancel: A hands
/// nothing over and does not resume. The cancel won, so the switch ends
/// cancelled with no record, no `mark_failed` and no return, and A is restarted
/// from its own progress, not from the default.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_when_a_does_not_resume_ends_cancelled_and_restarts_a_from_its_progress()
{
    let _watchdog = Watchdog::new("cancel_a_does_not_resume");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver.on("1.0.0", "resume", vec![]);
    rig.driver.on("1.0.0", "stop", vec![]);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let expected = a_on_a_rung!(core, rig, false);
    let canceller = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), canceller);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won(&rig.disk, &core.rt, &result).await;
    assert_eq!(core.rt.restart.as_ref().unwrap().progress, expected);
    assert_ne!(expected, crate::ladder::Progress::default());
}

/// Item 3, the J2 failure's clean-up: J2 cannot be written, B's kill is held,
/// A exits during it and a cancel is accepted. The same arbitration as every
/// other clean-up: the cancel wins, A's exit is the ordinary one and restarts
/// from A's progress.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_b_is_killed_after_a_j2_failure_wins_over_a_failing() {
    let _watchdog = Watchdog::new("j2_failure_cancel");
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J2", 99);
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let expected = a_on_a_rung!(core, rig, false);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exits());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            shared.request_cancel(),
            crate::snapshot::CancelAnswer::Cancelled
        );
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won(&rig.disk, &core.rt, &result).await;
    assert_eq!(core.rt.restart.as_ref().unwrap().progress, expected);
}

/// Item 3 and 4, the J2 failure's clean-up without a cancel: A's exit takes
/// row d. A was on its last try, so the return goes on from there: its rung is
/// spent and is not spawned again.
#[tokio::test(start_paused = true)]
async fn without_a_cancel_a_exiting_after_a_j2_failure_is_row_d_and_goes_on_from_a_progress() {
    let _watchdog = Watchdog::new("j2_failure_row_d");
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J2", 99);
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    a_on_a_rung!(core, rig, true);
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit("1.0.0", a_exits());
    };
    tokio::join!(core.handle_switch(update(false), tx), script);
    let result = done_result(&drain(&mut rx));
    failed(&result, "handing-over", "environment");
    assert_eq!(result["returned"], true, "{result}");
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "2.0.0 kept", "1.0.0 installed"],
        "A's spent kept copy is not run again: the next rung is"
    );
}

/// Item 4, the hand over: A exits during it, removed by the wait with no
/// bookkeeping of its own. Its progress was read first, so the return does not
/// start again at the rung A had just used up.
#[tokio::test(start_paused = true)]
async fn a_exiting_in_the_hand_over_goes_on_from_its_own_progress() {
    let _watchdog = Watchdog::new("a_exit_handover_progress");
    let mut rig = Rig::new().await;
    rig.driver
        .on("1.0.0", "hand_over", vec![Step::Exit(false, "died")]);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    a_on_a_rung!(core, rig, true);
    core.handle_switch(update(false), tx).await;
    let result = done_result(&drain(&mut rx));
    failed(&result, "handing-over", "environment");
    assert_eq!(result["returned"], true, "{result}");
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "2.0.0 kept", "1.0.0 installed"],
        "A's spent kept copy is not run again: the next rung is"
    );
}

/// Item 4, row l with the budget: A never hands over, then exits when it is
/// told to resume, so the wait for `resumed` removes it. The return goes on
/// from A's own progress.
#[tokio::test(start_paused = true)]
async fn a_exiting_when_told_to_resume_goes_on_from_its_own_progress() {
    let _watchdog = Watchdog::new("a_exit_resume_progress");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver
        .on("1.0.0", "resume", vec![Step::Exit(false, "died")]);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    a_on_a_rung!(core, rig, true);
    core.handle_switch(update(false), tx).await;
    let result = done_result(&drain(&mut rx));
    failed(&result, "handing-over", "environment");
    assert_eq!(result["returned"], true, "{result}");
    assert_eq!(
        rig.driver.places(),
        ["1.0.0 kept", "2.0.0 kept", "1.0.0 installed"],
        "A's spent kept copy is not run again: the next rung is"
    );
}

// ---- round 8b: a failed ladder write while a switch is open ------------------------

/// A on a rung that has not reported `loaded` (two failed tries), with the
/// ladder record the Core would hold for it. Returns A's rung.
macro_rules! a_awaiting_loaded_on_a_record {
    ($core:ident, $rig:ident) => {{
        a_on_a_rung!($core, $rig, true);
        let a = $core.rt.active.expect("A is active");
        let run = $core.rt.workers[&a].rung.clone().expect("A's rung");
        let record = crate::ladder::record_start(
            &$rig.cfg,
            None,
            &crate::ladder::Progress::on(run.clone(), 2),
            &run,
        );
        $core.rt.journal.as_mut().unwrap().ladder = Some(record);
        run
    }};
}

fn status_without_sign_ins() -> Step {
    Step::Msg(crate::protocol::Message::Status {
        mode: "active".to_owned(),
        live: 0,
        signins_pending: 0,
        phase: "serving".to_owned(),
    })
}

/// Round 8b (a): A reports `loaded` during the sign-in wait, before J1. The
/// reset waits for the switch's end: no write before it (the test disk's
/// writes are empty while the switch waits). A cancel before J1 ends the
/// switch, and the reset is written then, as the last write.
#[tokio::test(start_paused = true)]
async fn a_loaded_in_the_sign_in_wait_writes_no_record_and_a_cancel_before_j1_writes_the_reset_at_the_end()
 {
    let _watchdog = Watchdog::new("r8b_a_cancel");
    let mut rig = Rig::new().await;
    rig.driver
        .on("1.0.0", "ping", vec![crate::testkit::loaded()]);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let run = a_awaiting_loaded_on_a_record!(core, rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let during = rig.disk.order();
        (during, shared.request_cancel())
    };
    let ((), (during, answer)) = tokio::join!(core.handle_switch(update(true), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    assert!(
        during.is_empty(),
        "A's loaded wrote while the switch was open: {during:?}"
    );
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    let order = rig.disk.order();
    assert_eq!(
        order.iter().filter(|name| *name == "J-L").count(),
        1,
        "{order:?}"
    );
    assert_eq!(order.last().map(String::as_str), Some("J-L"), "{order:?}");
    assert!(order.iter().all(|name| name != "J1"), "{order:?}");
    assert_eq!(
        rig.disk.on_disk().unwrap().ladder,
        Some(crate::ladder::record_loaded(&rig.cfg, &run, &[])),
        "the reset is on the disk"
    );
    assert!(core.rt.disabled.is_none());
}

/// Round 8b (a): the reset at the end of a switch that ended before J1 fails:
/// that is the no-switch case, switching goes off at once and A keeps running.
#[tokio::test(start_paused = true)]
async fn a_failed_reset_at_the_end_of_a_switch_cancelled_before_j1_turns_switching_off() {
    let _watchdog = Watchdog::new("r8b_a_cancel_fails");
    let mut rig = Rig::new().await;
    rig.driver
        .on("1.0.0", "ping", vec![crate::testkit::loaded()]);
    rig.disk.fail_journal("J-L", 1);
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let _run = a_awaiting_loaded_on_a_record!(core, rig);
    let script = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(true), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    assert!(core.rt.disabled.is_some(), "switching is off");
    assert!(core.rt.journal.is_none(), "no journal is held");
    assert!(core.rt.active.is_some(), "A keeps running");
    assert!(shared.snapshot().switching_disabled.is_some());
}

/// Round 8b (a): A reports `loaded` during the sign-in wait and the switch
/// reaches J1. No ladder record is written before J1, and J1 clears the record
/// the Core held; nothing writes it back while the switch is open.
#[tokio::test(start_paused = true)]
async fn a_loaded_in_the_sign_in_wait_writes_no_record_before_j1_and_j1_clears_it() {
    let _watchdog = Watchdog::new("r8b_a_j1");
    let mut rig = Rig::new().await;
    rig.driver.on(
        "1.0.0",
        "ping",
        vec![crate::testkit::loaded(), status_without_sign_ins()],
    );
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let _run = a_awaiting_loaded_on_a_record!(core, rig);
    core.handle_switch(update(true), tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    let order = rig.disk.order();
    assert_eq!(order.first().map(String::as_str), Some("J1"), "{order:?}");
    assert!(
        order.iter().all(|name| !name.starts_with("J-L")),
        "no ladder write: {order:?}"
    );
    assert!(
        rig.disk.on_disk().unwrap().ladder.is_none(),
        "J1 cleared the record and nothing wrote it back"
    );
}

/// Round 8b, the route table: `back_to_a`'s budget case. A does not hand over,
/// B's kill takes 3 s and a cancel is accepted during it (the window is open:
/// it closes at checkpoint 2, before J3). A then does not resume. The cancel
/// wins as it does on every other clean-up.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_b_is_killed_after_the_hand_over_budget_wins_over_a_not_resuming() {
    let _watchdog = Watchdog::new("r8b_budget_cancel");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver
        .on("1.0.0", "resume", vec![Step::Exit(false, "died")]);
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let budget = rig.cfg.timings.handover;
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let expected = a_on_a_rung!(core, rig, false);
    let script = async {
        // The wait runs out `budget` after the hand over began, which is some
        // way after the switch began: the cancel is sent well inside B's kill.
        tokio::time::sleep(budget + Duration::from_secs(1)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won(&rig.disk, &core.rt, &result).await;
    assert_eq!(core.rt.restart.as_ref().unwrap().progress, expected);
}

/// The same route with no cancel: A does not resume and the switch is failed,
/// with the return started (what the arbitration must not change).
#[tokio::test(start_paused = true)]
async fn without_a_cancel_a_not_resuming_after_the_hand_over_budget_is_still_a_failure() {
    let _watchdog = Watchdog::new("r8b_budget_no_cancel");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver
        .on("1.0.0", "resume", vec![Step::Exit(false, "died")]);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    let result = done_result(&drain(&mut rx));
    failed(&result, "handing-over", "environment");
    assert_eq!(result["returned"], true, "{result}");
}

// ---- round 9, item 3: the cancel is arbitrated whether or not A failed ----------

/// A cancel that won over a failure while A is healthy: cancelled, no record and
/// no `mark_failed`, no return, A still the active worker and left running, and
/// no restart.
async fn assert_cancel_won_a_healthy(
    disk: &crate::testkit::FakeDisk,
    rt: &crate::core::Runtime,
    result: &serde_json::Value,
    a: u64,
) {
    settle().await;
    assert_eq!(result["result"], "cancelled", "{result}");
    assert!(
        disk.state.lock().unwrap().state.is_empty(),
        "no record, no failed mark"
    );
    assert!(rt.returning.is_none(), "no return");
    let order = disk.order();
    assert!(
        order.iter().all(|n| n != "J-R1"),
        "no return journalled: {order:?}"
    );
    let journal = rt.journal.as_ref().unwrap();
    assert!(journal.switch.is_none());
    assert!(journal.outcomes.is_empty());
    assert_eq!(rt.active, Some(a), "A is still the active worker");
    assert!(rt.workers.contains_key(&a), "A is still tracked");
    assert!(rt.restart.is_none(), "no restart is scheduled");
    assert!(rt.degraded.is_none(), "nothing is degraded");
}

/// The J2 route with a healthy A: J2 cannot be written, B's kill takes 3 s and
/// a cancel is accepted during it. The cancel wins; A is never touched.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_b_is_killed_after_a_j2_failure_wins_with_a_healthy() {
    let _watchdog = Watchdog::new("r9_j2_cancel_healthy_a");
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J2", 99);
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let a = core.rt.active.expect("A is active");
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won_a_healthy(&rig.disk, &core.rt, &result, a).await;
}

/// The same route with no cancel and a healthy A: the failure stands.
#[tokio::test(start_paused = true)]
async fn without_a_cancel_a_j2_failure_with_a_healthy_a_is_still_a_failure() {
    let _watchdog = Watchdog::new("r9_j2_no_cancel_healthy_a");
    let mut rig = Rig::new().await;
    rig.disk.fail_journal("J2", 99);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.handle_switch(update(false), tx).await;
    let result = done_result(&drain(&mut rx));
    failed(&result, "handing-over", "environment");
}

/// The budget route with a healthy A: A does not hand over in time, B's kill
/// takes 3 s and a cancel is accepted during it; A then resumes. The cancel
/// wins: no record, no `mark_failed`, A left running.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_b_is_killed_after_the_hand_over_budget_wins_when_a_resumes() {
    let _watchdog = Watchdog::new("r9_budget_cancel_a_resumes");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let budget = rig.cfg.timings.handover;
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let a = core.rt.active.expect("A is active");
    let script = async {
        tokio::time::sleep(budget + Duration::from_secs(1)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won_a_healthy(&rig.disk, &core.rt, &result, a).await;
}

/// The B-failure route (`PeerGone` in the hand over): B's channel is lost, its
/// reaping takes time and a cancel is accepted during it; A resumes. The cancel
/// wins.
#[tokio::test(start_paused = true)]
async fn a_cancel_accepted_while_a_lost_b_is_reaped_in_the_hand_over_wins_when_a_resumes() {
    let _watchdog = Watchdog::new("r9_peer_gone_cancel_a_resumes");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    rig.driver.on(
        "2.0.0",
        "kill",
        vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Exit(false, "killed"),
        ],
    );
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let a = core.rt.active.expect("A is active");
    let script = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.driver.emit(
            "2.0.0",
            crate::driver::Event::Lost("standby lost".to_owned()),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        shared.request_cancel()
    };
    let ((), answer) = tokio::join!(core.handle_switch(update(false), tx), script);
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    let result = done_result(&drain(&mut rx));
    assert_cancel_won_a_healthy(&rig.disk, &core.rt, &result, a).await;
}

// ---- round 9, item 2: nothing reaches the disk after the switch-off ------------

/// A forward switch succeeds and A drains. B then fails inside its watch, the
/// return's +1 cannot be written and (no switch is open) switching goes off at
/// once. A's exit then must write no S2 and queue none, with the state write
/// succeeding or failing.
async fn a_drain_ends_after_the_switch_off(state_fails: bool) {
    let _watchdog = Watchdog::new("r9_drain_after_switch_off");
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "release", vec![]);
    rig.driver.on("1.0.0", "stop", vec![]);
    let (mut core, _) = go!(rig, update(false));
    let a = *core
        .rt
        .workers
        .iter()
        .find(|(_, worker)| worker.role == WorkerRole::Draining)
        .unwrap()
        .0;
    let b = core.rt.active.expect("B is active");
    rig.disk.fail_journal("J-L", 99);
    core.worker_exited(b, "crashed", false).await;
    assert!(core.rt.disabled.is_some(), "switching went off");
    assert!(core.rt.switched_off);
    let order = rig.disk.order();
    if state_fails {
        rig.disk
            .state
            .lock()
            .unwrap()
            .fail_state
            .insert("S2".to_owned(), 99);
    }
    core.worker_exited(a, "released", true).await;
    assert_eq!(
        rig.disk.order(),
        order,
        "A's exit wrote nothing to the disk"
    );
    assert!(core.rt.pending.is_empty(), "and queued nothing");
    tokio::time::advance(Duration::from_secs(600)).await;
    core.on_tick().await;
    assert_eq!(rig.disk.order(), order, "no retry reached the disk later");
    assert!(core.rt.pending.is_empty());
}

#[tokio::test(start_paused = true)]
async fn after_the_switch_off_a_drain_ending_writes_no_s2() {
    a_drain_ends_after_the_switch_off(false).await;
}

#[tokio::test(start_paused = true)]
async fn after_the_switch_off_a_drain_ending_queues_no_s2_retry_when_the_write_would_fail() {
    a_drain_ends_after_the_switch_off(true).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_admission_rechecks_state_and_pending_journal_exclusions() {
    use open_ferry_update::journal::MarkFailed;
    use open_ferry_update::state::{State, SwitchRecord};

    for pending in [false, true] {
        for rolled_back in [false, true] {
            for auto in [false, true] {
                let mut rig = Rig::new().await;
                let root = tempfile::tempdir().unwrap();
                rig.cfg.data = open_ferry_update::DataDir::at(root.path());
                let mut state = State::default();
                if pending {
                    let mut record =
                        SwitchRecord::started("earlier", "rollback", "2.0.0", "1.0.0", "now");
                    record.result = Some("rolled-back".into());
                    rig.rt
                        .as_mut()
                        .unwrap()
                        .journal
                        .as_mut()
                        .unwrap()
                        .push_outcome(Outcome {
                            record: if rolled_back {
                                record
                            } else {
                                SwitchRecord::default()
                            },
                            mark_failed: (!rolled_back).then(|| MarkFailed {
                                version: "2.0.0".into(),
                                why: "failed its watch".into(),
                            }),
                        });
                } else if rolled_back {
                    state.rolled_back = Some("2.0.0".into());
                } else {
                    state.mark_failed("2.0.0");
                }
                state.save(&rig.cfg.data.state_file()).unwrap();
                let (_core, replies) = go!(rig, update(auto));
                if auto {
                    assert!(
                        matches!(&replies[..], [EndpointReply::Error { reason, .. }]
                        if reason.contains("automatic switching skips it")),
                        "{replies:?}"
                    );
                    assert_eq!(rig.driver.places().len(), 1, "no standby was started");
                } else {
                    assert_eq!(done_result(&replies)["result"], "switched");
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn automatic_admission_refuses_unreadable_exclusions_but_manual_admission_still_works() {
    for auto in [false, true] {
        let mut rig = Rig::new().await;
        let root = tempfile::tempdir().unwrap();
        rig.cfg.data = open_ferry_update::DataDir::at(root.path());
        std::fs::write(rig.cfg.data.state_file(), "not state JSON").unwrap();
        let (_core, replies) = go!(rig, update(auto));
        if auto {
            assert!(
                matches!(&replies[..], [EndpointReply::Error { reason, .. }]
                if reason == "automatic switch exclusions could not be read"),
                "{replies:?}"
            );
        } else {
            assert_eq!(done_result(&replies)["result"], "switched");
        }
    }
}
