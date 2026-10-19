//! The shared switch view reads through a held lock and never repairs disk.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::support::Fixture;
use crate::fetch::BoxFuture;
use crate::journal::{Artifact, How, Journal, Outcome, Phase, Switch};
use crate::pins::Pin;
use crate::state::SwitchRecord;
use crate::{CancelOutcome, SwitchError, SwitchHandle, Switcher, UpdateService};

struct Fake(Option<Value>);

impl Switcher for Fake {
    fn status(&self, _: Instant) -> BoxFuture<'_, Result<Value, SwitchError>> {
        Box::pin(async {
            self.0.clone().ok_or_else(|| {
                SwitchError::Unreachable("private endpoint path and transport details".into())
            })
        })
    }

    fn update<'a>(
        &'a self,
        _: &'a Pin,
        _: bool,
        _: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        panic!("the view must not update")
    }

    fn rollback<'a>(
        &'a self,
        _: &'a Artifact,
        _: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        panic!("the view must not roll back")
    }

    fn cancel(&self, _: Instant) -> BoxFuture<'_, Result<CancelOutcome, SwitchError>> {
        panic!("the view must not cancel")
    }
}

fn service(f: &Fixture, snapshot: Option<Value>) -> UpdateService {
    UpdateService::new(
        f.updater.clone(),
        crate::background::http_fetch(),
        &Default::default(),
    )
    .with_switcher(Arc::new(Fake(snapshot)))
}

fn artifact(version: &str) -> Artifact {
    Artifact {
        version: version.into(),
        sha256: "a".repeat(64),
        size: 1,
    }
}

fn write(f: &Fixture, journal: &Journal) -> Vec<u8> {
    let bytes = serde_json::to_vec(journal).unwrap();
    std::fs::write(f.data.root().join(crate::journal::JOURNAL_FILE), &bytes).unwrap();
    bytes
}

#[tokio::test]
async fn idle_and_watching_use_the_same_rollback_target_as_roll_back() {
    for watching in [false, true] {
        let f = Fixture::new().await;
        let mut journal = Journal::first_start(artifact("0.3.0"), None);
        journal.previous = Some(artifact("0.1.0"));
        if watching {
            journal.switch = Some(Switch {
                id: "watch".into(),
                how: How::Update,
                from: artifact("0.2.0"),
                to: artifact("0.3.0"),
                phase: Phase::Watching,
                started: "2026-10-18T01:00:00Z".into(),
                watch_until: None,
                admitted: Default::default(),
                return_: None,
            });
        }
        let bytes = write(&f, &journal);
        let service = service(
            &f,
            Some(json!({"state": if watching {"watching"} else {"idle"}})),
        );
        let _lock = f.updater.lock().unwrap();
        let view = tokio::time::timeout(Duration::from_secs(2), service.switch_view())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            view.rollback_target.as_deref(),
            Some(if watching { "0.2.0" } else { "0.1.0" })
        );
        assert!(view.last.is_none() && view.unreachable.is_none());
        assert!(view.supervisor.is_some());
        assert_eq!(
            std::fs::read(f.data.root().join(crate::journal::JOURNAL_FILE)).unwrap(),
            bytes
        );
        assert!(!f.data.state_file().exists());
        assert_eq!(f.server.requests(), 0);
    }
}

#[tokio::test]
async fn a_pending_rollback_beats_history_and_late_older_outcomes_do_not_pass_it() {
    let f = Fixture::new().await;
    let old = SwitchRecord::started("old", "update", "0.1.0", "0.2.0", "2026-10-18T00:00:00Z");
    let mut state = f.updater.state();
    state.upsert_record(old.clone());
    f.updater.save(&state).unwrap();
    let state_bytes = std::fs::read(f.data.state_file()).unwrap();
    let mut rollback = SwitchRecord::started(
        "rollback",
        "rollback",
        "0.2.0",
        "0.1.0",
        "2026-10-18T01:00:00Z",
    );
    rollback.confirmed = true;
    let mut journal = Journal::first_start(artifact("0.1.0"), None);
    journal.previous = Some(artifact("0.2.0"));
    journal.outcomes = vec![
        Outcome {
            record: rollback,
            mark_failed: None,
        },
        Outcome {
            record: old,
            mark_failed: None,
        },
    ];
    let journal_bytes = write(&f, &journal);
    let view = service(&f, Some(json!({"state":"idle"})))
        .switch_view()
        .await
        .unwrap();
    assert_eq!(view.rollback_target.as_deref(), Some("0.2.0"));
    let last = view.last.unwrap();
    assert_eq!(last.how, "rollback");
    assert_eq!(last.result, "rolled-back");
    assert_eq!(last.from.as_deref(), Some("0.2.0"));
    assert_eq!(last.to, "0.1.0");
    assert_eq!(last.at.as_deref(), Some("2026-10-18T01:00:00Z"));
    assert_eq!(last.auto, None);
    assert_eq!(std::fs::read(f.data.state_file()).unwrap(), state_bytes);
    assert_eq!(
        std::fs::read(f.data.root().join(crate::journal::JOURNAL_FILE)).unwrap(),
        journal_bytes
    );
}

#[tokio::test]
async fn terminal_patches_merge_and_live_auto_belongs_only_to_the_matching_id() {
    let f = Fixture::new().await;
    let mut state = f.updater.state();
    state.upsert_record(SwitchRecord::started(
        "operation",
        "update",
        "0.1.0",
        "0.2.0",
        "2026-10-18T03:00:00+02:00",
    ));
    f.updater.save(&state).unwrap();
    let mut journal = Journal::first_start(artifact("0.1.0"), None);
    journal.outcomes.push(Outcome {
        record: SwitchRecord {
            id: "operation".into(),
            result: Some("failed-after-switch".into()),
            why: Some("the new worker exited".into()),
            ..Default::default()
        },
        mark_failed: None,
    });
    write(&f, &journal);
    for id in ["operation", "another"] {
        let view = service(&f, Some(json!({"switch":{"id":id,"auto":true}})))
            .switch_view()
            .await
            .unwrap();
        let last = view.last.unwrap();
        assert_eq!(last.to, "0.2.0");
        assert_eq!(last.result, "failed-after-switch");
        assert_eq!(last.reason.as_deref(), Some("the new worker exited"));
        assert_eq!(last.at.as_deref(), Some("2026-10-18T01:00:00Z"));
        assert_eq!(last.auto, (id == "operation").then_some(true));
    }
}

#[tokio::test]
async fn unreachable_keeps_durable_data_without_exposing_transport_details() {
    let f = Fixture::new().await;
    let mut journal = Journal::first_start(artifact("0.2.0"), None);
    journal.previous = Some(artifact("0.1.0"));
    write(&f, &journal);
    let view = service(&f, None).switch_view().await.unwrap();
    assert!(view.supervisor.is_none());
    assert_eq!(
        view.unreachable.as_deref(),
        Some("the supervisor cannot be reached")
    );
    assert_eq!(view.rollback_target.as_deref(), Some("0.1.0"));
}

#[tokio::test]
async fn plain_never_reads_the_endpoint_or_creates_records() {
    let f = Fixture::new().await;
    let plain = UpdateService::new(
        f.updater.clone(),
        crate::background::http_fetch(),
        &Default::default(),
    );
    assert!(matches!(
        plain.switch_view().await,
        Err(SwitchError::NotSupervised)
    ));
    assert!(!f.data.state_file().exists());
    assert!(!f.data.root().join(crate::journal::JOURNAL_FILE).exists());
    assert_eq!(f.server.requests(), 0);
}

#[tokio::test]
async fn an_unreadable_or_future_journal_keeps_live_status_and_state_history_available() {
    for bytes in ["{partly written", r#"{"format":999}"#] {
        let f = Fixture::new().await;
        let mut state = f.updater.state();
        state.upsert_record(SwitchRecord::started(
            "done",
            "rollback",
            "0.2.0",
            "0.1.0",
            "2026-10-18T01:00:00Z",
        ));
        f.updater.save(&state).unwrap();
        let state_bytes = std::fs::read(f.data.state_file()).unwrap();
        let journal = f.data.root().join(crate::journal::JOURNAL_FILE);
        std::fs::write(&journal, bytes).unwrap();
        let view = service(&f, Some(json!({"state":"idle"})))
            .switch_view()
            .await
            .unwrap();
        assert_eq!(view.supervisor.unwrap()["state"], "idle");
        assert_eq!(view.last.unwrap().result, "rolled-back");
        assert!(view.rollback_target.is_none());
        assert_eq!(std::fs::read(f.data.state_file()).unwrap(), state_bytes);
        assert_eq!(std::fs::read_to_string(journal).unwrap(), bytes);
    }
}

#[tokio::test]
async fn corrupt_state_keeps_live_status_and_the_journal_without_repairing_state() {
    let f = Fixture::new().await;
    let mut journal = Journal::first_start(artifact("0.2.0"), None);
    journal.previous = Some(artifact("0.1.0"));
    write(&f, &journal);
    std::fs::write(f.data.state_file(), "{truncated").unwrap();
    let view = service(&f, Some(json!({"state":"idle"})))
        .switch_view()
        .await
        .unwrap();
    assert_eq!(view.supervisor.unwrap()["state"], "idle");
    assert_eq!(view.rollback_target.as_deref(), Some("0.1.0"));
    assert!(view.last.is_none());
    assert_eq!(
        view.notes,
        ["the update state could not be read; its history is unavailable"]
    );
    assert_eq!(
        std::fs::read_to_string(f.data.state_file()).unwrap(),
        "{truncated"
    );
}

#[tokio::test]
async fn records_with_no_result_stay_unknown_for_updates_and_rollbacks() {
    for how in ["update", "rollback"] {
        let f = Fixture::new().await;
        let mut state = f.updater.state();
        let mut record =
            SwitchRecord::started("legacy", how, "0.1.0", "0.2.0", "2026-10-19T01:00:00Z");
        record.result = None;
        state.upsert_record(record);
        f.updater.save(&state).unwrap();
        let view = service(&f, Some(json!({"state":"idle"})))
            .switch_view()
            .await
            .unwrap();
        assert_eq!(view.last.unwrap().result, "unknown");
    }
}
