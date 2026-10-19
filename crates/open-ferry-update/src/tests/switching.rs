//! Supervised preparation uses the lock and pin, then calls the endpoint.

use std::sync::{Arc, Mutex};
use std::sync::{
    Condvar,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use std::time::Instant;

use super::support::*;
use crate::fetch::BoxFuture;
use crate::journal::{Artifact, Journal, Outcome};
use crate::pins::{Pin, PinStore};
use crate::state::{SwitchRecord, result};
use crate::{
    CancelOutcome, DataDir, Report, SwitchError, SwitchEvent, SwitchHandle, SwitchProgress,
    Switcher, UpdateError, UpdateService,
};

struct Finished;
impl SwitchProgress for Finished {
    fn next(&mut self, _deadline: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
        Box::pin(async {
            Ok(SwitchEvent::Done {
                result: serde_json::json!({"result":"switched"}),
            })
        })
    }
}

struct Fake {
    data: DataDir,
    calls: Mutex<Vec<(String, bool)>>,
}

impl Fake {
    fn unlocked(&self) {
        assert!(
            self.data.try_lock().unwrap().is_some(),
            "endpoint called under update lock"
        );
    }
}

impl Switcher for Fake {
    fn status(&self, _deadline: Instant) -> BoxFuture<'_, Result<serde_json::Value, SwitchError>> {
        Box::pin(async move {
            self.unlocked();
            Ok(serde_json::json!({"state":"idle"}))
        })
    }
    fn update<'a>(
        &'a self,
        pin: &'a Pin,
        auto: bool,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(async move {
            self.unlocked();
            let pins = PinStore::new(&self.data.versions())
                .scan(chrono::Utc::now())
                .unwrap();
            assert!(
                pins.live.contains(pin),
                "pin is durable before endpoint call"
            );
            assert_eq!(
                crate::staging::hash_file(&self.data.binary(&pin.version, "open-ferry")).unwrap(),
                pin.sha256
            );
            self.calls.lock().unwrap().push((pin.version.clone(), auto));
            Ok(SwitchHandle::new("accepted".into(), Box::new(Finished)))
        })
    }
    fn rollback<'a>(
        &'a self,
        target: &'a Artifact,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(async move {
            self.unlocked();
            assert!(
                PinStore::new(&self.data.versions())
                    .scan(chrono::Utc::now())
                    .unwrap()
                    .live
                    .is_empty()
            );
            self.calls
                .lock()
                .unwrap()
                .push((target.version.clone(), false));
            Ok(SwitchHandle::new("rollback".into(), Box::new(Finished)))
        })
    }
    fn cancel(&self, _deadline: Instant) -> BoxFuture<'_, Result<CancelOutcome, SwitchError>> {
        Box::pin(async move {
            self.unlocked();
            Ok(CancelOutcome::Nothing)
        })
    }
}

fn service(f: &Fixture, mode: &str) -> (UpdateService, Arc<Fake>) {
    let mut config = open_ferry_core::config::Config::default();
    config.self_update.mode = mode.into();
    let fake = Arc::new(Fake {
        data: f.data.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let service = UpdateService::new(f.updater.clone(), crate::background::http_fetch(), &config)
        .with_switcher(fake.clone());
    (service, fake)
}

#[tokio::test]
async fn staged_install_pins_before_calling_without_the_lock_or_a_download() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let (service, fake) = service(&f, "auto");
    service.check_once().await.unwrap().unwrap();
    let requests = f.server.requests();
    let mut handle = service.install_now(true).await.unwrap();
    assert_eq!(handle.id, "accepted");
    assert!(matches!(
        handle.next(Instant::now()).await.unwrap(),
        Some(SwitchEvent::Done { .. })
    ));
    assert_eq!(handle.next(Instant::now()).await.unwrap(), None);
    assert_eq!(f.server.requests(), requests);
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), true)]);
    assert_eq!(service.supervisor().await.unwrap()["state"], "idle");
    assert_eq!(
        service.cancel_switch().await.unwrap(),
        CancelOutcome::Nothing
    );
}

#[tokio::test]
async fn notify_install_stages_the_latest_known_release() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let (service, fake) = service(&f, "notify");
    assert!(matches!(
        service.check_once().await.unwrap().unwrap().report,
        Report::Available { .. }
    ));
    assert!(f.updater.state().staged.is_none());
    service.install_now(false).await.unwrap();
    assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), false)]);
}

#[tokio::test]
async fn an_older_stage_does_not_hide_a_newer_notify_release() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(crate::SelfUpdateMode::Auto).await.unwrap();
    f.release("0.3.0");
    let (service, fake) = service(&f, "notify");
    service.check_once().await.unwrap().unwrap();
    service.install_now(false).await.unwrap();
    assert_eq!(*fake.calls.lock().unwrap(), [("0.3.0".into(), false)]);
}

#[tokio::test]
async fn a_missing_or_changed_staged_binary_is_refused_without_calling_the_endpoint() {
    for missing in [true, false] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        let (service, fake) = service(&f, "auto");
        service.check_once().await.unwrap().unwrap();
        let binary = f.data.binary("0.2.0", "open-ferry");
        if missing {
            std::fs::remove_file(binary).unwrap();
        } else {
            std::fs::write(binary, b"changed").unwrap();
        }
        let error = service.install_now(false).await.unwrap_err();
        assert!(
            matches!(
                error,
                SwitchError::Update(UpdateError::StagedChanged { .. })
            ),
            "{error}"
        );
        assert!(fake.calls.lock().unwrap().is_empty());
        assert!(
            PinStore::new(&f.data.versions())
                .scan(chrono::Utc::now())
                .unwrap()
                .live
                .is_empty()
        );
    }
}

#[tokio::test]
async fn the_update_lock_and_off_mode_refuse_before_the_endpoint() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let (service, fake) = service(&f, "off");
    assert!(matches!(
        service.install_now(true).await,
        Err(SwitchError::UpdatesOff)
    ));
    {
        let _lock = f.updater.lock().unwrap();
        assert!(matches!(
            service.install_now(false).await,
            Err(SwitchError::Update(UpdateError::Busy))
        ));
    }
    assert!(fake.calls.lock().unwrap().is_empty());
    service.install_now(false).await.unwrap();
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unsupervised_service_refuses_every_switch_method_clearly() {
    let f = Fixture::new().await;
    let service = UpdateService::new(
        f.updater.clone(),
        crate::background::http_fetch(),
        &open_ferry_core::config::Config::default(),
    );
    for error in [
        service.install_now(false).await.unwrap_err(),
        service.roll_back().await.unwrap_err(),
        service.supervisor().await.unwrap_err(),
        service.cancel_switch().await.unwrap_err(),
    ] {
        assert!(matches!(error, SwitchError::NotSupervised));
        assert_eq!(error.to_string(), "this install is not supervised");
    }
    assert_eq!(f.server.requests(), 0);
}

fn artifact(version: &str) -> Artifact {
    let bytes = good_binary(version);
    Artifact {
        version: version.into(),
        sha256: crate::release::sha256_hex(&bytes),
        size: bytes.len() as u64,
    }
}

fn write_journal(f: &Fixture, journal: &Journal) {
    std::fs::write(
        f.data.root().join(crate::journal::JOURNAL_FILE),
        serde_json::to_vec(journal).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn an_old_supervised_rollback_does_not_replace_a_later_plain_exclusion() {
    let f = Fixture::new().await;
    let mut state = f.updater.state();
    state.apply_outcome(
        &SwitchRecord::started(
            "supervised",
            "rollback",
            "0.3.0",
            "0.2.0",
            "2026-10-18T00:00:00Z",
        ),
        None,
    );
    assert_eq!(state.rolled_back.as_deref(), Some("0.3.0"));
    // A subsequent plain update and rollback has its own, newer exclusion.
    state.rolled_back = Some("0.4.0".into());
    state.last_switch = Some(SwitchRecord {
        from: "0.4.0".into(),
        to: "0.2.0".into(),
        how: "rollback".into(),
        at: "2026-10-18T01:00:00Z".into(),
        ..Default::default()
    });
    f.updater.save(&state).unwrap();
    let state = f.updater.state();
    write_journal(&f, &Journal::first_start(artifact("0.2.0"), None));
    let _lock = f.updater.lock().unwrap();
    let refreshed = crate::switching::read_state(&f.updater).unwrap();
    assert_eq!(refreshed.rolled_back.as_deref(), Some("0.4.0"));
    assert_eq!(f.updater.state(), state);
}

#[tokio::test]
async fn rollback_uses_the_watch_from_or_previous_and_validates_before_unlocking() {
    for watching in [false, true] {
        let f = Fixture::new().await;
        let (service, fake) = service(&f, "notify");
        let previous = artifact("0.0.9");
        let from = artifact("0.1.0");
        let active = artifact("0.2.0");
        let mut journal = Journal::first_start(active.clone(), None);
        journal.previous = Some(previous.clone());
        if watching {
            journal.switch = Some(crate::journal::Switch {
                id: "watch".into(),
                how: crate::journal::How::Update,
                from: from.clone(),
                to: active,
                phase: crate::journal::Phase::Watching,
                started: "now".into(),
                watch_until: None,
                admitted: Default::default(),
                return_: None,
            });
        }
        write_journal(&f, &journal);
        let target = if watching { from } else { previous };
        std::fs::create_dir_all(f.data.version_dir(&target.version)).unwrap();
        std::fs::write(
            f.data.binary(&target.version, "open-ferry"),
            good_binary(&target.version),
        )
        .unwrap();
        service.roll_back().await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [(target.version.clone(), false)]
        );
        fake.calls.lock().unwrap().clear();
        std::fs::write(f.data.binary(&target.version, "open-ferry"), b"changed").unwrap();
        assert!(matches!(
            service.roll_back().await,
            Err(SwitchError::Update(UpdateError::StagedChanged { .. }))
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn pending_exclusions_apply_to_checks_and_auto_installs_but_manual_installs_retry() {
    for failed in [false, true] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        let (service, fake) = service(&f, "auto");
        let mut journal = Journal::first_start(artifact("0.1.0"), None);
        let mut record = SwitchRecord::started(
            "outcome",
            "rollback",
            "0.2.0",
            "0.1.0",
            "2026-10-18T00:00:00Z",
        );
        let mark_failed = if failed {
            record.how = "update".into();
            record.from = "0.1.0".into();
            record.to = "0.2.0".into();
            record.result = Some(result::FAILED_AFTER_SWITCH.into());
            Some(crate::journal::MarkFailed {
                version: "0.2.0".into(),
                why: "crashed".into(),
            })
        } else {
            None
        };
        journal.outcomes.push(Outcome {
            record,
            mark_failed,
        });
        write_journal(&f, &journal);
        let pending_bytes =
            std::fs::read(f.data.root().join(crate::journal::JOURNAL_FILE)).unwrap();
        let report = service.check_once().await.unwrap().unwrap().report;
        if failed {
            assert!(matches!(report, Report::SkippedFailed { .. }));
        } else {
            assert!(matches!(report, Report::SkippedRolledBack { .. }));
        }
        assert!(matches!(
            service.install_now(true).await,
            Err(SwitchError::Skipped(_))
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
        assert!(!f.data.version_dir("0.2.0").exists());
        let state = f.updater.state();
        assert!(state.failed.is_empty() && state.rolled_back.is_none());
        assert!(state.last_switch.is_none() && state.history.is_empty());
        assert_eq!(
            std::fs::read(f.data.root().join(crate::journal::JOURNAL_FILE)).unwrap(),
            pending_bytes
        );
        service.install_now(false).await.unwrap();
        assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), false)]);
    }
}

#[tokio::test]
async fn a_manual_install_reuses_a_staged_excluded_version_without_downloading_it_again() {
    for failed in [false, true] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        let (service, fake) = service(&f, "auto");
        service.check_once().await.unwrap().unwrap();
        let mut state = f.updater.state();
        if failed {
            state.mark_failed("0.2.0");
        } else {
            state.rolled_back = Some("0.2.0".into());
        }
        f.updater.save(&state).unwrap();
        let requests = f.server.requests();
        assert!(matches!(
            service.install_now(true).await,
            Err(SwitchError::Skipped(_))
        ));
        service.install_now(false).await.unwrap();
        assert_eq!(f.server.requests(), requests);
        assert_eq!(*fake.calls.lock().unwrap(), [("0.2.0".into(), false)]);
    }
}

#[tokio::test]
async fn an_unacknowledged_install_failure_cannot_regress_a_successful_retry() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let (service, _) = service(&f, "notify");
    let mut record = SwitchRecord::started(
        "install",
        "update",
        "0.1.0",
        "0.2.0",
        "2026-10-18T00:00:00Z",
    );
    record.confirm();
    record.installed_binary = Some(crate::state::installed_binary::REPLACED.into());
    let mut state = f.updater.state();
    state.upsert_record(record.clone());
    state.rolled_back = Some("0.4.0".into());
    f.updater.save(&state).unwrap();
    let mut journal = Journal::first_start(artifact("0.2.0"), None);
    journal.outcomes.push(Outcome {
        record: SwitchRecord {
            id: "install".into(),
            installed_binary: Some("failed: directory sync".into()),
            ..Default::default()
        },
        mark_failed: None,
    });
    journal.outcomes.push(Outcome {
        record: SwitchRecord::started(
            "old-rollback",
            "rollback",
            "0.3.0",
            "0.2.0",
            "2026-10-17T00:00:00Z",
        ),
        mark_failed: None,
    });
    write_journal(&f, &journal);
    service.check_once().await.unwrap().unwrap();
    let checked = f.updater.state();
    assert_eq!(checked.last_switch.as_ref(), Some(&record));
    assert_eq!(checked.history, [record]);
    assert_eq!(checked.rolled_back.as_deref(), Some("0.4.0"));
}

struct HeldRunner {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}

impl crate::runner::Runner for HeldRunner {
    fn version<'a>(
        &'a self,
        binary: &'a std::path::Path,
        _: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.entered.store(true, Ordering::SeqCst);
            let (released, _) = self
                .wake
                .wait_timeout_while(
                    self.released.lock().unwrap(),
                    Duration::from_secs(5),
                    |released| !*released,
                )
                .unwrap();
            if !*released {
                return Err("test preparation was not released".into());
            }
            std::fs::read_to_string(binary)
                .map_err(|error| error.to_string())
                .map(|text| text.trim().into())
        })
    }
}

#[tokio::test]
async fn cancelled_preparation_keeps_its_lock_without_blocking_the_runtime_or_sending() {
    for rollback in [false, true] {
        let mut f = Fixture::new().await;
        let runner = Arc::new(HeldRunner {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            wake: Condvar::new(),
        });
        f.updater.runner = runner.clone();
        if rollback {
            let mut journal = Journal::first_start(artifact("0.2.0"), None);
            journal.previous = Some(artifact("0.1.0"));
            write_journal(&f, &journal);
            std::fs::create_dir_all(f.data.version_dir("0.1.0")).unwrap();
            std::fs::write(f.data.binary("0.1.0", "open-ferry"), good_binary("0.1.0")).unwrap();
        } else {
            f.release("0.2.0");
        }
        let (service, fake) = service(&f, "notify");
        let started = Instant::now();
        let task = tokio::spawn(async move {
            if rollback {
                service.roll_back().await
            } else {
                service.install_now(false).await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runner.entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the runtime remains responsive during blocking preparation");
        assert!(started.elapsed() < Duration::from_secs(2));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            matches!(f.updater.lock(), Err(UpdateError::Busy)),
            "the detached task still owns its guard"
        );
        *runner.released.lock().unwrap() = true;
        runner.wake.notify_all();
        tokio::time::timeout(Duration::from_secs(3), async {
            while f.updater.lock().is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the preparation released its lock");
        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "a cancelled caller never sends a switch"
        );
    }
}
