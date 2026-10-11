//! Not upstream's: the status snapshot (item 6). A golden JSON of a full
//! snapshot, an older reader's view of a snapshot that lacks the new fields,
//! and one test per new field showing that it moves when its source does.

use std::time::Duration;

use crate::clock::Clock;

use serde_json::json;
use tokio::sync::mpsc::unbounded_channel;

use crate::snapshot::{DrainView, InstallView, Snapshot, SwitchView, WorkerView};
use crate::testkit::{Rig, SpawnFail, Step, artifact, core_of, done_result, drain, update};

fn full() -> Snapshot {
    Snapshot {
        state: "switching".to_owned(),
        supervisor_version: "1.2.3".to_owned(),
        installed_version: Some("1.0.0".to_owned()),
        commit_pending: true,
        needs_restart: true,
        journal: "stale".to_owned(),
        state_write: "pending".to_owned(),
        drain: Some(DrainView {
            live: 3,
            since: "2026-10-09T11:55:00Z".to_owned(),
            cap_at: "2026-10-09T12:25:00Z".to_owned(),
        }),
        switch: Some(SwitchView {
            id: "7c1f".to_owned(),
            how: "update".to_owned(),
            from: "1.0.0".to_owned(),
            to: "2.0.0".to_owned(),
            phase: "watching".to_owned(),
            status: "watching".to_owned(),
            since: "2026-10-09T11:59:00Z".to_owned(),
            cancellable: false,
            watch_until: Some("2026-10-09T12:04:00Z".to_owned()),
        }),
        active_version: Some("2.0.0".to_owned()),
        workers: vec![WorkerView {
            pid: 4242,
            version: "2.0.0".to_owned(),
            role: "active".to_owned(),
            lost: false,
            live: 5,
            signins_pending: 1,
        }],
        journal_stale: true,
        state_write_pending: true,
        switching_disabled: Some("the journal is of a newer format".to_owned()),
        degraded: None,
        install_pending: Some(InstallView {
            target: "2.0.0".to_owned(),
            attempts: 2,
            next_try: Some("2026-10-09T12:01:00Z".to_owned()),
        }),
        prune_pending: true,
        keep: Some(vec!["1.0.0".to_owned(), "2.0.0".to_owned()]),
    }
}

#[test]
fn a_full_snapshot_is_this_json() {
    let golden = json!({
        "state": "switching",
        "supervisor_version": "1.2.3",
        "installed_version": "1.0.0",
        "commit_pending": true,
        "needs_restart": true,
        "journal": "stale",
        "state_write": "pending",
        "drain": {
            "live": 3,
            "since": "2026-10-09T11:55:00Z",
            "cap_at": "2026-10-09T12:25:00Z"
        },
        "switch": {
            "id": "7c1f",
            "how": "update",
            "from": "1.0.0",
            "to": "2.0.0",
            "phase": "watching",
            "status": "watching",
            "since": "2026-10-09T11:59:00Z",
            "cancellable": false,
            "watch_until": "2026-10-09T12:04:00Z"
        },
        "active_version": "2.0.0",
        "workers": [{
            "pid": 4242,
            "version": "2.0.0",
            "role": "active",
            "lost": false,
            "live": 5,
            "signins_pending": 1
        }],
        "journal_stale": true,
        "state_write_pending": true,
        "switching_disabled": "the journal is of a newer format",
        "degraded": null,
        "install_pending": {
            "target": "2.0.0",
            "attempts": 2,
            "next_try": "2026-10-09T12:01:00Z"
        },
        "prune_pending": true,
        "keep": ["1.0.0", "2.0.0"]
    });
    let snapshot = full();
    assert_eq!(serde_json::to_value(&snapshot).unwrap(), golden);
    assert_eq!(
        serde_json::from_value::<Snapshot>(golden).unwrap(),
        snapshot
    );
}

#[test]
fn a_snapshot_without_the_new_fields_still_reads() {
    // What an endpoint of the previous build sent.
    let old = json!({
        "state": "idle",
        "switch": {
            "id": "a", "how": "update", "from": "1.0.0", "to": "2.0.0",
            "phase": "loading", "cancellable": true, "watch_until": null
        },
        "active_version": "1.0.0",
        "workers": [],
        "journal_stale": false,
        "state_write_pending": false,
        "switching_disabled": null,
        "degraded": null,
        "install_pending": null,
        "keep": []
    });
    let read: Snapshot = serde_json::from_value(old).unwrap();
    assert_eq!(read.supervisor_version, "");
    assert_eq!(read.installed_version, None);
    assert!(!read.commit_pending && !read.needs_restart && !read.prune_pending);
    assert_eq!(read.journal, "");
    assert_eq!(read.drain, None);
    let switch = read.switch.unwrap();
    assert_eq!((switch.status.as_str(), switch.since.as_str()), ("", ""));
    // Every field after `state` defaults or is optional, so a status that
    // names only the state still reads.
    let bare: Snapshot = serde_json::from_value(json!({"state": "idle"})).unwrap();
    assert!(bare.workers.is_empty() && !bare.journal_stale && !bare.state_write_pending);
}

#[tokio::test(start_paused = true)]
async fn the_supervisors_version_the_journal_and_the_state_write_follow_their_sources() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    core.publish();
    let shown = rig.shared.snapshot();
    assert_eq!(shown.supervisor_version, "1.0.0");
    assert_eq!(
        (shown.journal.as_str(), shown.state_write.as_str()),
        ("ok", "ok")
    );
    assert!(!shown.journal_stale && !shown.state_write_pending);
    core.rt.journal_stale = true;
    core.publish();
    let shown = rig.shared.snapshot();
    assert_eq!(shown.journal, "stale");
    assert!(shown.journal_stale);
    core.rt.journal_stale = false;
    // An outcome the state file does not have yet.
    core.rt.pending.push(crate::core::PendingState {
        name: "S1",
        outcome: open_ferry_update::journal::Outcome {
            record: open_ferry_update::state::SwitchRecord::default(),
            mark_failed: None,
        },
        attempts: 0,
        due: tokio::time::Instant::now(),
        in_journal: false,
    });
    core.publish();
    let shown = rig.shared.snapshot();
    assert_eq!(shown.state_write, "pending");
    assert!(shown.state_write_pending);
    core.rt.pending.clear();
    core.publish();
    assert_eq!(rig.shared.snapshot().state_write, "ok");
}

#[tokio::test(start_paused = true)]
async fn the_installed_version_follows_the_journal_and_commit_pending_the_pending_install() {
    let mut rig = Rig::new().await;
    let mut core = core_of!(rig);
    core.publish();
    let before = rig.shared.snapshot();
    assert_eq!(before.installed_version.as_deref(), Some("1.0.0"));
    assert!(!before.commit_pending);
    // A confirmed switch records the new version as the installed one.
    core.rt.journal.as_mut().unwrap().installed = Some(
        open_ferry_update::journal::Installed::from(&artifact("2.0.0")),
    );
    core.publish();
    assert_eq!(
        rig.shared.snapshot().installed_version.as_deref(),
        Some("2.0.0")
    );
    // The file is not replaced yet.
    core.rt.journal.as_mut().unwrap().install_pending = Some(
        serde_json::from_value(json!({
            "target": serde_json::to_value(artifact("2.0.0")).unwrap(),
            "source": "versions/2.0.0/open-ferry",
            "aside": "",
            "attempts": 0,
            "started": "2026-10-09T12:00:00Z"
        }))
        .unwrap(),
    );
    core.publish();
    let shown = rig.shared.snapshot();
    assert!(shown.commit_pending);
    assert_eq!(shown.install_pending.unwrap().target, "2.0.0");
    core.rt.journal.as_mut().unwrap().install_pending = None;
    core.publish();
    assert!(!rig.shared.snapshot().commit_pending);
    core.rt.journal = None;
    core.publish();
    assert_eq!(rig.shared.snapshot().installed_version, None);
}

#[tokio::test(start_paused = true)]
async fn a_switch_that_needs_a_newer_supervisor_sets_needs_restart() {
    let mut rig = Rig::new().await;
    rig.driver.fail_spawn("2.0.0", SpawnFail::Incompatible);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.publish();
    assert!(!rig.shared.snapshot().needs_restart);
    core.handle_switch(update(false), tx).await;
    let result = done_result(&drain(&mut rx));
    assert_eq!(result["result"], "failed-to-load", "{result}");
    assert_eq!(result["phase"], "loading", "{result}");
    assert_eq!(result["blame"], "environment", "{result}");
    assert!(rig.shared.snapshot().needs_restart);
}

#[tokio::test(start_paused = true)]
async fn the_switch_shows_its_status_and_start_through_its_phases_and_then_as_a_watch() {
    // Each transition is held by a scripted delay of 10 s (B's load, A's hand
    // over, B's take over). The snapshot is read in the middle of every hold,
    // 5 s from each boundary on the paused clock: it says the phase being held,
    // not an earlier or a later one, and never a constant.
    let mut rig = Rig::new().await;
    let hold = Duration::from_secs(10);
    rig.driver.fail_spawn("2.0.0", SpawnFail::Delay(hold));
    rig.driver.on(
        "1.0.0",
        "hand_over",
        vec![Step::Sleep(hold), crate::testkit::handed_over()],
    );
    rig.driver.on(
        "2.0.0",
        "take_over",
        vec![
            Step::Sleep(hold),
            Step::Msg(crate::protocol::Message::Ready),
        ],
    );
    let shared = rig.shared.clone();
    let started = rig
        .clock
        .now()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let observe = async {
        let mut shown = Vec::new();
        tokio::time::sleep(hold / 2).await;
        for _ in 0..3 {
            let view = shared.snapshot().switch.expect("in the snapshot");
            assert_eq!(view.since, started);
            // A cancel is possible until the take over is sent.
            assert_eq!(view.cancellable, view.phase != "taking-over", "{view:?}");
            shown.push((view.phase, view.status));
            tokio::time::sleep(hold).await;
        }
        shown
    };
    let ((), shown) = tokio::join!(core.handle_switch(update(false), tx), observe);
    let pair = |name: &str| (name.to_owned(), name.to_owned());
    assert_eq!(
        shown,
        [pair("loading"), pair("handing-over"), pair("taking-over")]
    );
    // The messages say the same, in order.
    let phases: Vec<String> = drain(&mut rx)
        .into_iter()
        .filter_map(|reply| match reply {
            crate::protocol::EndpointReply::Progress { phase, .. } => Some(phase),
            _ => None,
        })
        .collect();
    assert_eq!(phases, ["loading", "handing-over", "taking-over"]);
    // Past the commit point the window is the switch.
    let watch = shared.snapshot().switch.expect("the watch window");
    assert_eq!(watch.status, "watching");
    assert_eq!(watch.since, started);
    assert!(watch.watch_until.is_some());
    assert!(!watch.cancellable);
}

#[tokio::test(start_paused = true)]
async fn an_automatic_switch_waiting_for_sign_ins_says_so_and_an_accepted_cancel_says_cancelled() {
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
        let waiting = shared.snapshot().switch.expect("in the snapshot");
        let answer = shared.request_cancel();
        // At once, from the snapshot, before the core has looked at it.
        let cancelled = shared.snapshot().switch.expect("still shown");
        (waiting, answer, cancelled)
    };
    let ((), (waiting, answer, cancelled)) =
        tokio::join!(core.handle_switch(update(true), tx), watcher);
    assert_eq!(waiting.status, "waiting-for-signins");
    assert_eq!(waiting.phase, "loading");
    assert_eq!(answer, crate::snapshot::CancelAnswer::Cancelled);
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
}

// Fix 4: a cancel that was accepted stays shown in every publication until the
// operation ends, not only in the picture `request_cancel` edits. B's exit is
// stalled, so the core unwinds slowly and publishes several times meanwhile.
#[tokio::test(start_paused = true)]
async fn an_accepted_cancel_stays_shown_until_the_operation_ends() {
    let mut rig = Rig::new().await;
    rig.driver.on("1.0.0", "hand_over", vec![]);
    for command in ["stop", "kill"] {
        rig.driver.on(
            "2.0.0",
            command,
            vec![
                Step::Sleep(Duration::from_secs(20)),
                Step::Exit(true, "gone"),
            ],
        );
    }
    let shared = rig.shared.clone();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    let canceller = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            shared.request_cancel(),
            crate::snapshot::CancelAnswer::Cancelled
        );
        let mut shown = Vec::new();
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            shown.push(shared.snapshot().switch.map(|switch| switch.status));
        }
        shown
    };
    let ((), shown) = tokio::join!(core.handle_switch(update(false), tx), canceller);
    assert_eq!(done_result(&drain(&mut rx))["result"], "cancelled");
    // While the operation was open, every picture said so.
    let open: Vec<_> = shown
        .iter()
        .filter_map(|status| status.as_deref())
        .collect();
    assert!(
        open.len() >= 3,
        "the operation stayed open across ticks: {shown:?}"
    );
    assert!(
        open.iter().all(|status| *status == "cancelled"),
        "{shown:?}"
    );
    // It ended, and the next picture shows no switch and no stale cancel.
    core.publish();
    assert!(rig.shared.snapshot().switch.is_none());
    assert!(!rig.shared.cancel_requested());
}

#[tokio::test(start_paused = true)]
async fn a_released_worker_shows_as_a_drain_with_its_connections_start_and_cap() {
    let mut rig = Rig::new().await;
    let now = rig.clock.now();
    let cap = chrono::Duration::from_std(rig.cfg.timings.drain).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let mut core = core_of!(rig);
    core.publish();
    assert_eq!(rig.shared.snapshot().drain, None);
    core.handle_switch(update(false), tx).await;
    assert_eq!(done_result(&drain(&mut rx))["result"], "switched");
    let draining = core
        .rt
        .workers
        .values_mut()
        .find(|tracked| tracked.drain.is_some())
        .expect("A is released and drains");
    draining.live = 7;
    core.publish();
    let view = rig.shared.snapshot().drain.expect("a drain");
    assert_eq!(view.live, 7);
    assert_eq!(
        view.since,
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    assert_eq!(
        view.cap_at,
        (now + cap).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    // Its connections end: the count follows.
    core.rt
        .workers
        .values_mut()
        .find(|tracked| tracked.drain.is_some())
        .unwrap()
        .live = 0;
    core.publish();
    assert_eq!(rig.shared.snapshot().drain.unwrap().live, 0);
}
