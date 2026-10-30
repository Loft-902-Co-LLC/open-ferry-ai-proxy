// Round 3: explicit policy debt survives failed saves without blocking disk observations.
use super::support::*;
use crate::auth::codex_refresh::CredentialRefreshPatch;
use crate::auth::{AdoptionRead, Auth, AuthStore, CodexOrigin, FileStore, Status};
use crate::manager::lifecycle::{LeasedDisposition, LeasedTail};
use crate::manager::{AdoptionResult, CallResult, Manager, Settings, lock};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

fn disable(auth: &mut Auth) -> Result<bool, ()> {
    auth.disabled = true;
    auth.status = Status::Disabled;
    auth.metadata.insert("disabled".into(), json!(true));
    Ok(true)
}

#[test]
fn round3_failed_policy_save_keeps_only_owed_fields_for_every_provider() {
    for (provider, typed) in [
        ("claude", true),
        ("codex", true),
        ("gemini", true),
        ("claude", false),
    ] {
        let h = Harness::with_store(Settings::default());
        let mut candidate = auth_with_metadata(
            "policy",
            provider,
            json!({
                "access_token":"dummy-before", "disabled":false,
                "siwc":{"access_token":"dummy-sibling", "note":"before"},
                "removed-by-operator":"before"
            }),
        );
        if typed {
            candidate.metadata.insert("type".into(), json!(provider));
        }
        let initial = h.manager.register(candidate).unwrap();
        h.store.set_fail_saves(true);
        let edited = h
            .manager
            .edit_at_epoch("policy", initial.registration_epoch, |auth| {
                disable(auth)?;
                auth.metadata["siwc"]["note"] = json!("operator");
                auth.metadata.remove("removed-by-operator");
                Ok::<_, ()>(true)
            })
            .unwrap()
            .unwrap();
        assert!(
            edited.disabled,
            "failed ordinary saves still report the published edit"
        );
        assert!(h.manager.lock().auths["policy"].policy_save.is_some());
        for _ in 0..3 {
            let mut disk = initial.as_ref().clone();
            disk.metadata["access_token"] = json!("dummy-external");
            disk.metadata["siwc"]["access_token"] = json!("dummy-sibling-external");
            disk.metadata.insert("unrelated".into(), json!("outside"));
            assert!(matches!(
                h.manager
                    .adopt_observed(&initial, None, |_| { Ok(AdoptionRead::Record(disk)) })
                    .unwrap(),
                AdoptionResult::Published { .. }
            ));
            let live = h.manager.get("policy").unwrap();
            assert!(live.disabled);
            assert_eq!(live.metadata["access_token"], "dummy-external");
            assert_eq!(
                live.metadata["siwc"]["access_token"],
                "dummy-sibling-external"
            );
            assert_eq!(live.metadata["siwc"]["note"], "operator");
            assert_eq!(live.metadata["unrelated"], "outside");
            assert!(!live.metadata.contains_key("removed-by-operator"));
        }
        h.store.set_fail_saves(false);
        let before_save = h.manager.get("policy").unwrap();
        h.manager.mark_result(&CallResult {
            credential_version: before_save.credential_version,
            registration_epoch: before_save.registration_epoch,
            auth_id: "policy".into(),
            provider: provider.into(),
            success: true,
            ..Default::default()
        });
        assert!(
            h.manager.lock().auths["policy"].policy_save.is_none(),
            "a covering actual save clears debt"
        );
        assert!(h.store.stored("policy").unwrap().disabled);
        let mut later = initial.as_ref().clone();
        later.metadata["access_token"] = json!("dummy-later");
        h.manager
            .adopt_observed(&initial, None, |_| Ok(AdoptionRead::Record(later)))
            .unwrap();
        assert!(
            !h.manager.get("policy").unwrap().disabled,
            "after durability, a real external enable is adopted"
        );
    }
}

#[test]
fn round3_unsaved_publications_do_not_cover_a_waiting_policy_save() {
    for action in ["config-edit", "replace", "refused-tail", "unresolved-tail"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::new(dir.path()));
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store.clone()),
        );
        let mut auth = auth_with_metadata(
            "waiting.json",
            "claude",
            json!({"type":"claude", "access_token":"dummy", "disabled":false}),
        );
        auth.file_name = auth.id.clone();
        let initial = manager.register(auth).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let once = AtomicBool::new(true);
        *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
            if auth.disabled && once.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                lock(&release_rx)
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
        }));
        let editor = manager.clone();
        let worker = std::thread::spawn(move || {
            editor
                .edit_at_epoch("waiting.json", initial.registration_epoch, disable)
                .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let live = manager.get("waiting.json").unwrap();
        let mark = lock(&manager.shared.persist_locks)["waiting.json"].clone();
        let saved_before = *lock(&mark);
        match action {
            "config-edit" => {
                manager
                    .edit_unsaved("waiting.json", |auth| {
                        auth.label = "config reload".into();
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
            }
            "replace" => {
                manager.update_unsaved(live.as_ref().clone()).unwrap();
            }
            _ => {
                let tail = LeasedTail {
                    origin: CodexOrigin::of(&dir.path().join("waiting.json")).unwrap(),
                    rotation: CredentialRefreshPatch::new(
                        &live.metadata,
                        &live.metadata,
                        false,
                        false,
                    ),
                    disposition: if action == "refused-tail" {
                        LeasedDisposition::PolicyRefused
                    } else {
                        LeasedDisposition::OriginUnresolved
                    },
                };
                manager
                    .update_refreshed_leased(
                        &live,
                        live.registration_epoch,
                        live.as_ref().clone(),
                        tail,
                    )
                    .unwrap();
            }
        }
        assert_eq!(*lock(&mark), saved_before, "{action} is not a disk save");
        let disk_after_observation = store.read_back(&live).unwrap().unwrap();
        let mut observed = disk_after_observation;
        observed.metadata["access_token"] = json!("dummy-external-while-save-waits");
        std::fs::write(
            dir.path().join("waiting.json"),
            serde_json::to_vec(&observed.metadata).unwrap(),
        )
        .unwrap();
        manager
            .adopt_observed(&live, None, |_| Ok(AdoptionRead::Record(observed)))
            .unwrap();
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        *lock(&manager.shared.persist_hook) = None;
        assert!(
            store.read_back(&live).unwrap().unwrap().disabled,
            "{action} must not skip the pending disable save"
        );
        assert!(manager.get("waiting.json").unwrap().disabled);
        assert_eq!(
            store.read_back(&live).unwrap().unwrap().access_token(),
            "dummy-external-while-save-waits",
            "{action} must save the adopted token"
        );
    }
}

#[test]
fn round3_load_initializes_policy_generation() {
    let h = Harness::with_store(Settings::default());
    let mut stored = auth_with_metadata(
        "loaded",
        "claude",
        json!({"type":"claude", "access_token":"dummy"}),
    );
    stored.policy_generation = 0;
    h.store.put(stored);
    h.manager.load().unwrap();
    assert_eq!(h.manager.get("loaded").unwrap().policy_generation, 1);
}

struct CountingFailure {
    inner: FakeStore,
    events: Mutex<Option<mpsc::Sender<()>>>,
    fail: AtomicBool,
}
impl AuthStore for CountingFailure {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.inner.list()
    }
    fn delete(&self, id: &str) -> std::io::Result<()> {
        self.inner.delete(id)
    }
    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        if let Some(events) = &*lock(&self.events) {
            events.send(()).unwrap();
        }
        if self.fail.load(Ordering::SeqCst) {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        } else {
            self.inner.save(auth)
        }
    }
}

#[test]
fn round3_policy_io_retry_is_bounded_and_backed_off() {
    let (events, observed) = mpsc::channel();
    let store = Arc::new(CountingFailure {
        inner: FakeStore::default(),
        events: Mutex::new(None),
        fail: AtomicBool::new(false),
    });
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    manager
        .register(auth_with_metadata(
            "retry",
            "claude",
            json!({"type":"claude", "access_token":"dummy"}),
        ))
        .unwrap();
    *lock(&store.events) = Some(events);
    store.fail.store(true, Ordering::SeqCst);
    manager.edit("retry", disable).unwrap();
    // Initial failure plus three bounded attempts; channels order the test.
    for _ in 0..4 {
        observed.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = manager.lock();
        let policy = state.auths["retry"].policy_save.as_ref().unwrap();
        if !policy.retry_scheduled {
            assert!(policy.retry_at.unwrap() > manager.now());
            break;
        }
        drop(state);
        assert!(Instant::now() < deadline, "retry worker finished");
        std::thread::yield_now();
    }
    assert_eq!(observed.try_recv(), Err(mpsc::TryRecvError::Empty));
    store.fail.store(false, Ordering::SeqCst);
    let before_save = manager.get("retry").unwrap();
    manager.mark_result(&CallResult {
        credential_version: before_save.credential_version,
        registration_epoch: before_save.registration_epoch,
        auth_id: "retry".into(),
        provider: "claude".into(),
        success: true,
        ..Default::default()
    });
    assert!(manager.lock().auths["retry"].policy_save.is_none());
}

#[test]
fn round3_detached_save_reconcile_advances_policy_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path()));
    let mut auth = auth_with_metadata(
        "reconcile.json",
        "codex",
        json!({"type":"codex", "access_token":"dummy", "refresh_token":"dummy-refresh"}),
    );
    auth.file_name = auth.id.clone();
    store.save_new(&auth).unwrap();
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let initial = manager.register_unsaved(auth).unwrap();
    {
        let mut state = manager.lock();
        let live = Arc::make_mut(&mut state.auths.get_mut(&initial.id).unwrap().auth);
        live.codex_pending_refresh.push(CredentialRefreshPatch::new(
            &live.metadata,
            &live.metadata,
            true,
            false,
        ));
    }
    let snapshot = manager.get(&initial.id).unwrap();
    std::fs::write(
        dir.path().join(&initial.id),
        br#"{"type":"codex","access_token":"dummy-disk-new","refresh_token":"dummy-refresh"}"#,
    )
    .unwrap();
    manager.reconcile_pending_save(&snapshot, 0, Default::default());
    let live = manager.get(&initial.id).unwrap();
    assert_eq!(live.outer_access_token(), "dummy-disk-new");
    assert!(live.policy_generation > snapshot.policy_generation);
}

struct DiskWinsSave {
    inner: Arc<FileStore>,
    armed: AtomicBool,
}
impl AuthStore for DiskWinsSave {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.inner.list()
    }
    fn delete(&self, id: &str) -> std::io::Result<()> {
        self.inner.delete(id)
    }
    fn same_record(&self, a: &Auth, b: &Auth) -> std::io::Result<bool> {
        self.inner.same_record(a, b)
    }
    fn supports_codex_refresh_patches(&self) -> bool {
        true
    }
    fn read_back(&self, auth: &Auth) -> std::io::Result<Option<Auth>> {
        self.inner.read_back(auth)
    }
    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        let path = self.inner.save(auth)?;
        if self.armed.load(Ordering::SeqCst) {
            let mut metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            metadata["access_token"] = json!("dummy-save-winner");
            std::fs::write(&path, serde_json::to_vec(&metadata)?)?;
        }
        Ok(path)
    }
}

#[test]
fn round3_update_save_reconcile_advances_policy_generation() {
    let dir = tempfile::tempdir().unwrap();
    let files = Arc::new(FileStore::new(dir.path()));
    let mut auth = auth_with_metadata(
        "saved.json",
        "codex",
        json!({"type":"codex", "access_token":"dummy", "refresh_token":"dummy-refresh"}),
    );
    auth.file_name = auth.id.clone();
    files.save_new(&auth).unwrap();
    let store = Arc::new(DiskWinsSave {
        inner: files,
        armed: AtomicBool::new(false),
    });
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let initial = manager.register_unsaved(auth).unwrap();
    let captured = Arc::new(AtomicU64::new(0));
    let seen = captured.clone();
    let observing = manager.clone();
    *lock(&manager.shared.read_back_hook) = Some(Arc::new(move |auth| {
        seen.store(
            observing.get(&auth.id).unwrap().policy_generation,
            Ordering::SeqCst,
        );
    }));
    store.armed.store(true, Ordering::SeqCst);
    let mut updated = initial.as_ref().clone();
    updated.metadata["access_token"] = json!("dummy-refresh-result");
    manager
        .update_refreshed(&initial, initial.registration_epoch, updated)
        .unwrap();
    *lock(&manager.shared.read_back_hook) = None;
    let live = manager.get(&initial.id).unwrap();
    assert_eq!(live.outer_access_token(), "dummy-save-winner");
    assert!(captured.load(Ordering::SeqCst) > 0);
    assert!(live.policy_generation > captured.load(Ordering::SeqCst));
}

#[test]
fn round3_token_command_reads_through_pending_policy_debt() {
    use crate::auth::CodexCredentialPartition;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path()));
    let mut auth = auth_with_metadata(
        "command.json",
        "codex",
        json!({
            "type":"codex", "access_token":"dummy-before", "refresh_token":"dummy-refresh", "disabled":false
        }),
    );
    auth.file_name = auth.id.clone();
    store.save_new(&auth).unwrap();
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let initial = manager.register_unsaved(auth).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let once = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.disabled && once.swap(false, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }));
    let editor = manager.clone();
    let epoch = initial.registration_epoch;
    let worker = std::thread::spawn(move || {
        editor
            .edit_at_epoch("command.json", epoch, disable)
            .unwrap()
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let command = manager
        .capture_token_command("command.json", CodexCredentialPartition::Outer)
        .unwrap();
    std::fs::write(dir.path().join("command.json"), br#"{"type":"codex","access_token":"dummy-login","refresh_token":"dummy-login-refresh","disabled":false}"#).unwrap();
    let adopted = manager
        .adopt_completed_token_command(&initial, command)
        .unwrap();
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    *lock(&manager.shared.persist_hook) = None;
    assert!(matches!(adopted, AdoptionResult::Published { .. }));
    let live = manager.get("command.json").unwrap();
    assert!(live.disabled);
    assert_eq!(live.outer_access_token(), "dummy-login");
    let disk = store.read_back(&live).unwrap().unwrap();
    assert!(disk.disabled);
    assert_eq!(disk.outer_access_token(), "dummy-login");
}
