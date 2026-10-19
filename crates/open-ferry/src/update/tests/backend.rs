//! Exercise backend selection and the real signed staging/pinning path.
use super::*;
use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::journal::{Artifact, Journal, Outcome as Pending};
use open_ferry_update::pins::{Pin, PinStore};
use open_ferry_update::test_support::{Fixture, good_binary};
use open_ferry_update::{State, Switcher};
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Endpoint {
    data: DataDir,
    status_error: Option<&'static str>,
    statuses: AtomicUsize,
    operation: Mutex<Option<String>>,
    sends: Mutex<Vec<String>>,
}
impl Endpoint {
    fn new(f: &Fixture, error: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            data: f.data.clone(),
            status_error: error,
            statuses: AtomicUsize::new(0),
            operation: Mutex::new(None),
            sends: Mutex::new(Vec::new()),
        })
    }
}
impl Switcher for Endpoint {
    fn status(&self, _: Instant) -> BoxFuture<'_, Result<Value, SwitchError>> {
        Box::pin(async move {
            self.statuses.fetch_add(1, Ordering::SeqCst);
            match self.status_error {
                Some("untrusted") => Err(SwitchError::Untrusted("wrong owner".into())),
                Some(_) => Err(SwitchError::Unreachable("endpoint closed".into())),
                None => Ok(
                    json!({"active_version":"0.1.0", "state":"idle", "switch":self.operation.lock().unwrap().as_ref().map(|id| json!({"id":id,"phase":"loading"}))}),
                ),
            }
        })
    }
    fn update<'a>(
        &'a self,
        pin: &'a Pin,
        auto: bool,
        _: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(async move {
            assert!(!auto);
            assert!(
                self.data.try_lock().unwrap().is_some(),
                "send must not block supervisor state writes"
            );
            assert!(
                PinStore::new(&self.data.versions())
                    .scan(chrono::Utc::now())
                    .unwrap()
                    .live
                    .contains(pin)
            );
            assert_eq!(
                open_ferry_update::staging::hash_file(
                    &self.data.binary(&pin.version, "open-ferry")
                )
                .unwrap(),
                pin.sha256
            );
            self.sends.lock().unwrap().push(pin.version.clone());
            Ok(SwitchHandle::new(
                "operation".into(),
                Box::new(super::supervised::Events(std::collections::VecDeque::new())),
            ))
        })
    }
    fn rollback<'a>(
        &'a self,
        target: &'a Artifact,
        _: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>> {
        Box::pin(async move {
            self.sends.lock().unwrap().push(target.version.clone());
            Err(SwitchError::Refused("test end".into()))
        })
    }
    fn cancel(&self, _: Instant) -> BoxFuture<'_, Result<CancelOutcome, SwitchError>> {
        Box::pin(async move {
            self.sends.lock().unwrap().push("cancel".into());
            Ok(CancelOutcome::Cancelled)
        })
    }
}
async fn backend(f: &Fixture, endpoint: Arc<Endpoint>, request: Request) -> Supervised {
    Real(f.updater.clone(), ServiceKind::User)
        .supervised_with(request, &Config::default(), endpoint, |_, _| async {
            Ok(Detection::Supervised(client::Running::default()))
        })
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn selection_refuses_busy_refused_untrusted_and_unreachable_without_plain_fallback() {
    let f = Fixture::new().await;
    let before = snapshot(f.temp.path());
    for detection in [
        Detection::Busy("busy owner".into()),
        Detection::Refuse("refused owner".into()),
    ] {
        let endpoint = Endpoint::new(&f, None);
        let result = Real(f.updater.clone(), ServiceKind::User)
            .supervised_with(
                Request::Update,
                &Config::default(),
                endpoint.clone(),
                |_, _| async { Ok(detection) },
            )
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("selection should refuse"),
        };
        assert!(matches!(error, UpdateError::Supervised(_)));
        for request in [Request::Update, Request::Rollback, Request::Cancel] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut console = Console::new(true, &mut out, &mut err);
            let outcome = selection_failed(
                request,
                &settings("auto", None),
                &mut console,
                UpdateError::Supervised(error.to_string()),
            );
            assert_eq!((outcome.code, outcome.result), (1, "supervised"));
            assert!(outcome.error.unwrap().contains("request was not sent"));
        }
        assert_eq!(endpoint.statuses.load(Ordering::SeqCst), 0);
        assert!(endpoint.sends.lock().unwrap().is_empty());
    }
    for reason in ["untrusted", "unreachable"] {
        let endpoint = Endpoint::new(&f, Some(reason));
        let result = Real(f.updater.clone(), ServiceKind::User)
            .supervised_with(
                Request::Update,
                &Config::default(),
                endpoint.clone(),
                |_, _| async { Ok(Detection::Supervised(client::Running::default())) },
            )
            .await;
        assert!(matches!(result, Err(UpdateError::Supervised(_))));
        assert!(endpoint.sends.lock().unwrap().is_empty());
    }
    assert_eq!(snapshot(f.temp.path()), before);
}

#[tokio::test]
async fn clear_selection_keeps_plain_checks_lenient_and_does_not_take_the_update_lock() {
    let f = Fixture::new().await;
    std::fs::write(f.data.state_file(), "{truncated").unwrap();
    let _lock = f.updater.lock().unwrap();
    let endpoint = Endpoint::new(&f, None);
    for request in [Request::Check, Request::Update, Request::Cancel] {
        let result = Real(f.updater.clone(), ServiceKind::User)
            .supervised_with(
                request,
                &Config::default(),
                endpoint.clone(),
                |_, binary| async move {
                    Ok(Detection::Clear(InstallLock::try_acquire(&binary).unwrap()))
                },
            )
            .await
            .unwrap();
        assert!(result.is_none());
    }
    assert_eq!(endpoint.statuses.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(f.data.state_file()).unwrap(),
        "{truncated"
    );
    assert!(
        Real(f.updater.clone(), ServiceKind::User)
            .status(&settings("auto", None))
            .last_check
            .is_none()
    );
}

#[tokio::test]
async fn cancel_works_under_the_update_lock_without_reading_live_status_or_history() {
    let f = Fixture::new().await;
    std::fs::write(f.data.state_file(), "{truncated").unwrap();
    let _lock = f.updater.lock().unwrap();
    let endpoint = Endpoint::new(&f, Some("unreachable"));
    let real = backend(&f, endpoint.clone(), Request::Cancel).await;
    assert_eq!(real.cancel().await.unwrap(), CancelOutcome::Cancelled);
    assert_eq!(endpoint.statuses.load(Ordering::SeqCst), 0);
    assert_eq!(*endpoint.sends.lock().unwrap(), ["cancel"]);
}

#[tokio::test]
async fn signed_fetched_release_is_staged_pinned_and_sent_after_unlocking() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let endpoint = Endpoint::new(&f, None);
    let real = backend(&f, endpoint.clone(), Request::Update).await;
    let offer = real.latest().await.unwrap();
    assert!(offer.found.found.is_some());
    let Changed::Supervised { record, .. } = real.update(offer.found).await.unwrap() else {
        panic!("expected switch");
    };
    assert_eq!(record.to, "0.2.0");
    assert_eq!(*endpoint.sends.lock().unwrap(), ["0.2.0"]);
    assert_eq!(
        std::fs::read_to_string(f.data.binary("0.2.0", "open-ferry")).unwrap(),
        "open-ferry 0.2.0"
    );
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
}

#[tokio::test]
async fn a_stage_changed_after_confirmation_names_the_version_and_never_sends() {
    for pointer in [false, true] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        let mut state = f.updater.state();
        let found = f.updater.latest(&state).await.unwrap();
        f.updater.stage(&found, &mut state).await.unwrap();
        let endpoint = Endpoint::new(&f, None);
        let real = backend(&f, endpoint.clone(), Request::Update).await;
        let offer = real.latest().await.unwrap();
        assert!(offer.found.found.is_none());
        if pointer {
            state.staged = None;
            state.staged_sha256 = None;
            f.updater.save(&state).unwrap();
        } else {
            std::fs::write(f.data.binary("0.2.0", "open-ferry"), "changed").unwrap();
        }
        let error = match real.update(offer.found).await {
            Err(error) => error,
            Ok(_) => panic!("changed stage accepted"),
        };
        assert!(
            matches!(error, SwitchError::Update(UpdateError::StagedChanged { ref version }) if version == "0.2.0")
        );
        assert!(endpoint.sends.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn the_real_backend_fetches_a_new_release_when_the_old_stage_is_running() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let mut state = f.updater.state();
    let found = f.updater.latest(&state).await.unwrap();
    f.updater.stage(&found, &mut state).await.unwrap();
    f.release("0.3.0");
    let endpoint = Endpoint::new(&f, None);
    let mut real = backend(&f, endpoint, Request::Update).await;
    real.view.supervisor.as_mut().unwrap()["active_version"] = json!("0.2.0");
    let offer = real.latest().await.unwrap();
    assert_eq!(offer.latest, "0.3.0");
    assert!(offer.newer && offer.found.found.is_some());
}

#[tokio::test]
async fn rollback_checks_the_confirmed_target_again_and_record_merges_pending_patches() {
    let f = Fixture::new().await;
    let endpoint = Endpoint::new(&f, None);
    let real = backend(&f, endpoint.clone(), Request::Rollback).await;
    let artifact = |version: &str| Artifact {
        version: version.into(),
        sha256: "a".repeat(64),
        size: good_binary(version).len() as u64,
    };
    let mut journal = Journal::first_start(artifact("0.1.0"), None);
    journal.previous = Some(artifact("0.2.0"));
    let mut state = State::default();
    state.upsert_record(SwitchRecord::started(
        "operation",
        "update",
        "0.1.0",
        "0.2.0",
        "2026-10-19T01:00:00Z",
    ));
    f.updater.save(&state).unwrap();
    journal.outcomes.push(Pending {
        record: SwitchRecord {
            id: "operation".into(),
            result: Some("switched".into()),
            confirmed: true,
            ..Default::default()
        },
        mark_failed: None,
    });
    std::fs::write(
        f.data.root().join(open_ferry_update::journal::JOURNAL_FILE),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let before = snapshot(f.temp.path());
    let error = match real.rollback("0.1.0").await {
        Err(error) => error,
        Ok(_) => panic!("stale target accepted"),
    };
    assert!(
        matches!(error, SwitchError::Update(UpdateError::RollbackChanged { expected, actual })
            if expected == "0.1.0" && actual.as_deref() == Some("0.2.0"))
    );
    let record = real.record("operation").unwrap();
    assert_eq!(record.to, "0.2.0");
    assert_eq!(record.result.as_deref(), Some("switched"));
    assert!(record.confirmed);
    assert_eq!(snapshot(f.temp.path()), before);
    assert!(endpoint.sends.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_real_detection_entry_point_keeps_a_starting_supervisor_supervised() {
    let f = Fixture::new().await;
    let _held = open_ferry_supervisor::process::SupervisorLock::try_acquire(f.data.root()).unwrap();
    let _updating = f.updater.lock().unwrap();
    let selected = Real(f.updater.clone(), ServiceKind::User)
        .supervised(Request::Cancel, &Config::default())
        .await
        .unwrap();
    assert!(
        selected.is_some(),
        "a held lifetime lock must never fall back to plain"
    );
}

#[tokio::test(start_paused = true)]
async fn the_real_backend_uses_live_status_to_follow_a_quiet_phase() {
    let f = Fixture::new().await;
    let endpoint = Endpoint::new(&f, None);
    let real = backend(&f, endpoint.clone(), Request::Update).await;
    *endpoint.operation.lock().unwrap() = Some("operation".into());
    let handle = SwitchHandle::new(
        "operation".into(),
        Box::new(super::supervised::QuietPhase(
            tokio::time::Instant::now() + Duration::from_secs(40),
        )),
    );
    let record = SwitchRecord::started(
        "operation",
        "update",
        "0.1.0",
        "0.2.0",
        "2026-10-19T01:00:00Z",
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut console = Console::new(true, &mut out, &mut err);
    let outcome = follow_until(
        &real,
        handle,
        record,
        Outcome::new("update", &settings("", None)),
        &mut console,
        std::future::pending(),
    )
    .await;
    assert_eq!((outcome.code, outcome.result), (0, "updated"));
    assert!(endpoint.statuses.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn check_reuses_its_view_and_a_missing_record_says_no_supervisor_is_running() {
    let f = Fixture::new().await;
    let journal = Journal::first_start(
        Artifact {
            version: "0.1.0".into(),
            sha256: "a".repeat(64),
            size: 1,
        },
        None,
    );
    std::fs::write(
        f.data.root().join(open_ferry_update::journal::JOURNAL_FILE),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let endpoint = Endpoint::new(&f, Some("untrusted"));
    let real = backend(&f, endpoint.clone(), Request::Check).await;
    assert_eq!(
        real.view.unreachable.as_deref(),
        Some(if cfg!(unix) {
            "no supervisor is running"
        } else {
            "the supervisor endpoint was not trusted"
        })
    );
    let _ = go(
        Request::Check,
        &real,
        &settings("auto", None),
        false,
        true,
        None,
    )
    .await;
    assert_eq!(
        endpoint.statuses.load(Ordering::SeqCst),
        1,
        "check must reuse its initial view"
    );
}

#[tokio::test]
async fn plain_checks_ignore_held_and_unopenable_install_locks_without_creating_files() {
    for unopenable in [false, true] {
        let f = Fixture::new().await;
        f.release("0.1.0");
        let lock_path = InstallLock::path_for(&f.installed).unwrap();
        let _held = if unopenable {
            std::fs::create_dir(&lock_path).unwrap();
            None
        } else {
            Some(InstallLock::try_acquire(&f.installed).unwrap())
        };
        assert!(InstallLock::try_acquire(&f.installed).is_err());
        let before = snapshot(f.temp.path());
        let real = Real(f.updater.clone(), ServiceKind::User);
        let endpoint = Endpoint::new(&f, None);
        let selected = real
            .supervised_with(
                Request::Check,
                &Config::default(),
                endpoint.clone(),
                |_, _| async { panic!("a plain check must not try detection or the install lock") },
            )
            .await
            .unwrap();
        assert!(selected.is_none());
        let ran = go(
            Request::Check,
            &real,
            &settings("auto", None),
            false,
            true,
            None,
        )
        .await;
        assert_eq!((ran.code, ran.result), (0, "up-to-date"));
        assert_eq!(endpoint.statuses.load(Ordering::SeqCst), 0);
        assert_eq!(snapshot(f.temp.path()), before);
    }
}

#[tokio::test]
async fn plain_rollback_refuses_a_changed_or_removed_confirmed_target() {
    for actual in [Some("0.0.8"), None] {
        let f = Fixture::new().await;
        for version in ["0.0.9", "0.0.8"] {
            std::fs::create_dir_all(f.data.version_dir(version)).unwrap();
            std::fs::write(f.data.binary(version, "open-ferry"), good_binary(version)).unwrap();
        }
        let mut state = f.updater.state();
        state.previous = Some("0.0.9".into());
        f.updater.save(&state).unwrap();
        let real = Real(f.updater.clone(), ServiceKind::User);
        let plan = real.rollback_plan().unwrap();
        state.previous = actual.map(str::to_owned);
        f.updater.save(&state).unwrap();
        let before = snapshot(f.temp.path());
        let error = match real.rollback(&plan.previous).await {
            Err(error) => error,
            Ok(_) => panic!("unconfirmed target restored"),
        };
        assert!(
            matches!(&error, SwitchError::Update(UpdateError::RollbackChanged { expected, actual: current }) if expected == "0.0.9" && current.as_deref() == actual)
        );
        assert!(error.to_string().contains("expected 0.0.9"));
        assert!(error.to_string().contains(actual.unwrap_or("none")));
        assert_eq!(snapshot(f.temp.path()), before);
    }
}
