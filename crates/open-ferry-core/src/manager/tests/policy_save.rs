// Round 3: explicit policy debt survives failed saves without blocking disk observations.
use super::support::*;
use crate::auth::codex_refresh::CredentialRefreshPatch;
use crate::auth::{AdoptionRead, Auth, AuthStore, CodexOrigin, FileStore, Status};
use crate::manager::lifecycle::{LeasedDisposition, LeasedTail};
use crate::manager::{AdoptionResult, CallResult, Manager, Settings, lock};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

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
                "removed-by-operator":"before", "operator-note":"before"
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
                if provider != "codex" {
                    auth.metadata["siwc"]["note"] = json!("operator");
                }
                auth.metadata["operator-note"] = json!("operator");
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
            assert_eq!(live.metadata["operator-note"], "operator");
            assert_eq!(
                live.metadata["siwc"]["note"],
                if provider == "codex" {
                    "before"
                } else {
                    "operator"
                }
            );
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
    calls: AtomicU64,
    success: tokio::sync::Notify,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        } else {
            let saved = self.inner.save(auth);
            self.success.notify_waiters();
            saved
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round4_policy_io_retry_is_bounded_and_self_rearms() {
    let store = Arc::new(CountingFailure {
        inner: FakeStore::default(),
        calls: AtomicU64::new(0),
        success: tokio::sync::Notify::new(),
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
    let (mut waiting, release) = retry_waits(&manager);
    store.calls.store(0, Ordering::SeqCst);
    store.fail.store(true, Ordering::SeqCst);
    manager.edit("retry", disable).unwrap();
    for (index, millis) in [100, 500, 2000].into_iter().enumerate() {
        expect_wait(&mut waiting, Duration::from_millis(millis)).await;
        assert_eq!(store.calls.load(Ordering::SeqCst), 1 + index as u64);
        // Sleeping retries have no admission and cannot delay handoff.
        let gate = manager.gate();
        tokio::time::timeout(Duration::from_secs(2), gate.close(gate.epoch()))
            .await
            .unwrap()
            .unwrap();
        gate.reopen();
        release.add_permits(1);
    }
    expect_wait(&mut waiting, Duration::from_secs(30)).await;
    assert_eq!(store.calls.load(Ordering::SeqCst), 4);
    assert!(
        waiting.try_recv().is_err(),
        "no busy retry while backoff is held"
    );
    assert!(
        manager.lock().auths["retry"]
            .policy_save
            .as_ref()
            .unwrap()
            .retry_at
            .unwrap()
            > manager.now()
    );
    store.fail.store(false, Ordering::SeqCst);
    release.add_permits(1); // Re-arm without any unrelated manager operation.
    expect_wait(&mut waiting, Duration::from_millis(100)).await;
    assert_eq!(store.calls.load(Ordering::SeqCst), 4);
    let completed = store.success.notified();
    tokio::pin!(completed);
    completed.as_mut().enable();
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), completed)
        .await
        .unwrap();
    // save() signals before returning; serialize with the completed transaction
    // instead of spinning until the coverage bookkeeping happens.
    wait_for_policy_save(&manager, "retry");
    assert_eq!(store.calls.load(Ordering::SeqCst), 5);
    assert!(manager.lock().auths["retry"].policy_save.is_none());
}

fn wait_for_policy_save(manager: &Manager, id: &str) {
    let save_lock = lock(&manager.shared.persist_save_locks)[id].clone();
    drop(lock(&save_lock));
}

fn retry_waits(
    manager: &Manager,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<Duration>,
    Arc<tokio::sync::Semaphore>,
) {
    let (waiting, observed) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    *lock(&manager.shared.policy_retry_wait) = Some((waiting, release.clone()));
    (observed, release)
}

async fn expect_wait(
    waiting: &mut tokio::sync::mpsc::UnboundedReceiver<Duration>,
    expected: Duration,
) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), waiting.recv())
            .await
            .unwrap(),
        Some(expected)
    );
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

// Round 4 uses a real FileStore, with a reversible file-authorization failure
// as the initial I/O failure. No watcher observation runs before retry_now().
struct FilePolicyRig {
    _dir: tempfile::TempDir,
    store: Arc<FileStore>,
    refuse: Arc<AtomicBool>,
    manager: Manager,
    initial: Arc<Auth>,
}

impl FilePolicyRig {
    fn new(provider: &str, typed: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let refuse = Arc::new(AtomicBool::new(false));
        let failure = refuse.clone();
        let store = Arc::new(FileStore::new(dir.path()).with_file_guard(Some(
            crate::auth::file_guard::FileGuard::new(move |_| {
                if failure.load(Ordering::SeqCst) {
                    Err("test save refusal".into())
                } else {
                    Ok(())
                }
            }),
        )));
        let mut auth = auth_with_metadata(
            "policy.json",
            provider,
            json!({
                "access_token":"dummy-before", "refresh_token":"dummy-refresh",
                "siwc":{"access_token":"dummy-nested", "refresh_token":"dummy-nested-refresh", "note":"before"},
                "use_siwc":true, "disabled":false, "note":"before", "keep":"before", "remove":"before"
            }),
        );
        if typed {
            auth.metadata.insert("type".into(), json!(provider));
        }
        auth.file_name = auth.id.clone();
        // A fixture with an existing SIWC partition, rather than an outer login.
        store.save(&auth).unwrap();
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store.clone()),
        );
        let initial = manager.register_unsaved(auth).unwrap();
        Self {
            _dir: dir,
            store,
            refuse,
            manager,
            initial,
        }
    }

    fn disk(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self._dir.path().join("policy.json")).unwrap())
            .unwrap()
    }

    fn outside_write(&self, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut disk = self.disk();
        edit(&mut disk);
        std::fs::write(
            self._dir.path().join("policy.json"),
            serde_json::to_vec(&disk).unwrap(),
        )
        .unwrap();
    }

    fn retry_now(&self) {
        self.refuse.store(false, Ordering::SeqCst);
        let live = self.manager.get("policy.json").unwrap();
        let discard = self.manager.lock().auths["policy.json"].discard_epoch;
        assert!(matches!(
            self.manager
                .persist(
                    &live,
                    live.registration_epoch,
                    live.generation,
                    discard,
                    crate::manager::lifecycle::Save::Yes
                )
                .unwrap(),
            crate::manager::Persisted::Written { .. }
        ));
    }

    fn observe(&self) {
        // Gemini and untyped credentials are synthesized by service watchers;
        // supply the equivalent record directly, keeping real file bytes.
        let mut observed = self.initial.as_ref().clone();
        observed.metadata = self.disk().as_object().unwrap().clone();
        observed.disabled = observed.metadata["disabled"].as_bool().unwrap();
        observed.status = if observed.disabled {
            Status::Disabled
        } else {
            Status::Active
        };
        assert!(matches!(
            self.manager
                .adopt_observed(&self.initial, None, |_| Ok(AdoptionRead::Record(observed)))
                .unwrap(),
            AdoptionResult::Published { .. }
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round4_retry_preserves_an_unobserved_outside_rotation_and_settings() {
    for (provider, typed) in [
        ("claude", true),
        ("codex", true),
        ("gemini", true),
        ("claude", false),
    ] {
        let rig = FilePolicyRig::new(provider, typed);
        rig.refuse.store(true, Ordering::SeqCst);
        rig.manager
            .edit("policy.json", |auth| {
                disable(auth)?;
                auth.metadata["note"] = json!("operator");
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert!(
            rig.manager.lock().auths["policy.json"]
                .policy_save
                .is_some()
        );
        rig.outside_write(|disk| {
            disk["access_token"] = json!("dummy-outside");
            disk["refresh_token"] = json!("dummy-outside-refresh");
            disk["siwc"]["access_token"] = json!("dummy-outside-nested");
            disk["note"] = json!("outside");
            disk["unrelated-setting"] = json!({"new":true});
            disk["proxy_url"] = json!("http://example.invalid:80");
        });
        assert_eq!(
            rig.manager.get("policy.json").unwrap().metadata["access_token"],
            "dummy-before",
            "the watcher observation is deliberately held back"
        );
        rig.retry_now();
        let disk = rig.disk();
        assert_eq!(disk["access_token"], "dummy-outside", "{provider}");
        assert_eq!(disk["refresh_token"], "dummy-outside-refresh", "{provider}");
        assert_eq!(
            disk["siwc"]["access_token"], "dummy-outside-nested",
            "{provider}"
        );
        assert_eq!(
            disk["note"], "outside",
            "a conflicting file field also wins before observation"
        );
        assert_eq!(disk["unrelated-setting"], json!({"new":true}));
        assert_eq!(disk["proxy_url"], "http://example.invalid:80");
        assert_eq!(disk["disabled"], true);
        assert!(
            rig.manager.lock().auths["policy.json"]
                .policy_save
                .is_none()
        );
        rig.observe();
        let live = rig.manager.get("policy.json").unwrap();
        assert!(live.disabled);
        assert_eq!(live.metadata["access_token"], "dummy-outside");
        assert_eq!(live.metadata["note"], "outside");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round4_coverage_uses_the_real_codex_commit_not_the_submission() {
    let rig = FilePolicyRig::new("codex", true);
    rig.refuse.store(true, Ordering::SeqCst);
    // Configuration saves protect the file's OAuth partition. Even an
    // accepted token edit must not be falsely acknowledged as durable.
    rig.manager
        .edit("policy.json", |auth| {
            disable(auth)?;
            auth.metadata["access_token"] = json!("dummy-submitted");
            Ok::<_, ()>(true)
        })
        .unwrap();
    rig.retry_now();
    assert_eq!(rig.disk()["access_token"], "dummy-before");
    assert_eq!(
        rig.disk()["disabled"],
        true,
        "the supported policy leaf was written"
    );
    assert!(
        rig.manager.lock().auths["policy.json"]
            .policy_save
            .is_some(),
        "a successful transaction does not cover a value FileStore protected instead"
    );
    assert_eq!(
        rig.manager.get("policy.json").unwrap().metadata["access_token"],
        "dummy-submitted"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn round4_unwritable_codex_siwc_edits_are_refused_before_publication() {
    for field in ["siwc-note", "siwc-delete", "use-siwc-delete"] {
        let rig = FilePolicyRig::new("codex", true);
        let before = rig.disk();
        let live = rig.manager.get("policy.json").unwrap();
        let result = rig.manager.edit("policy.json", |auth| {
            match field {
                "siwc-note" => {
                    auth.metadata["siwc"]["note"] = json!("operator");
                }
                "siwc-delete" => {
                    auth.metadata.remove("siwc");
                }
                _ => {
                    auth.metadata.remove("use_siwc");
                }
            }
            Ok::<_, ()>(true)
        });
        assert!(result.is_err(), "{field}");
        let after = rig.manager.get("policy.json").unwrap();
        assert_eq!(after.generation, live.generation);
        assert_eq!(after.metadata, live.metadata);
        assert_eq!(rig.disk(), before);
        assert!(
            rig.manager.lock().auths["policy.json"]
                .policy_save
                .is_none()
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round4_a_hand_edit_wins_only_the_conflicting_owed_leaves() {
    let rig = FilePolicyRig::new("claude", true);
    rig.refuse.store(true, Ordering::SeqCst);
    rig.manager
        .edit("policy.json", |auth| {
            disable(auth)?;
            auth.metadata["note"] = json!("operator");
            auth.metadata["keep"] = json!("operator");
            auth.metadata.remove("remove");
            Ok::<_, ()>(true)
        })
        .unwrap();
    // A second operator edit must still compare to the original file baseline.
    rig.manager
        .edit("policy.json", |auth| {
            auth.metadata["note"] = json!("second-operator");
            Ok::<_, ()>(true)
        })
        .unwrap();
    rig.outside_write(|disk| {
        disk["note"] = json!("hand-edited");
        disk["remove"] = json!("manually-restored");
        disk["access_token"] = json!("dummy-manual-rotation");
    });
    rig.observe();
    let live = rig.manager.get("policy.json").unwrap();
    assert_eq!(live.metadata["note"], "hand-edited");
    assert_eq!(live.metadata["remove"], "manually-restored");
    assert_eq!(
        live.metadata["keep"], "operator",
        "unchanged baseline still owes the edit"
    );
    assert!(live.disabled);
    assert!(
        rig.manager.lock().auths["policy.json"]
            .policy_save
            .is_some()
    );
    rig.retry_now();
    let disk = rig.disk();
    assert_eq!(disk["note"], "hand-edited");
    assert_eq!(disk["remove"], "manually-restored");
    assert_eq!(disk["keep"], "operator");
    assert_eq!(disk["access_token"], "dummy-manual-rotation");
    assert_eq!(disk["disabled"], true);
    assert!(
        rig.manager.lock().auths["policy.json"]
            .policy_save
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn round4_retry_rechecks_admission_after_waiting() {
    let rig = FilePolicyRig::new("claude", true);
    let (mut waiting, release) = retry_waits(&rig.manager);
    rig.refuse.store(true, Ordering::SeqCst);
    rig.manager.edit("policy.json", disable).unwrap();
    expect_wait(&mut waiting, Duration::from_millis(100)).await;
    let gate = rig.manager.gate();
    tokio::time::timeout(Duration::from_secs(2), gate.close(gate.epoch()))
        .await
        .unwrap()
        .unwrap();
    rig.refuse.store(false, Ordering::SeqCst);
    release.add_permits(1);
    expect_wait(&mut waiting, Duration::from_millis(500)).await;
    assert_eq!(
        rig.disk()["disabled"],
        false,
        "the closed gate prevented a write"
    );
    let saved = Arc::new(tokio::sync::Notify::new());
    let notify = saved.clone();
    *lock(&rig.manager.shared.unleased_saved_hook) =
        Some(Arc::new(move |_| notify.notify_waiters()));
    let completed = saved.notified();
    tokio::pin!(completed);
    completed.as_mut().enable();
    gate.reopen();
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), completed)
        .await
        .unwrap();
    wait_for_policy_save(&rig.manager, "policy.json");
    *lock(&rig.manager.shared.unleased_saved_hook) = None;
    assert_eq!(
        rig.disk()["disabled"],
        true,
        "a fresh admission saved after reopening"
    );
    assert!(
        rig.manager.lock().auths["policy.json"]
            .policy_save
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn round4_policy_transaction_returns_the_protected_siwc_representation() {
    for bound in [false, true] {
        let rig = FilePolicyRig::new("codex", true);
        let before = rig.disk();
        let mut submitted = rig.initial.as_ref().clone();
        submitted.metadata["siwc"]["note"] = json!("submitted");
        submitted.metadata.remove("use_siwc");
        let mut apply = |current: &mut Auth, fresh: bool| {
            assert!(fresh);
            current.metadata["siwc"]["note"] = json!("submitted");
            current.metadata.remove("use_siwc");
            current.disabled = true;
        };
        // Direct store callers can still submit protected values. Its result
        // must describe what landed, never claim these unsupported values did.
        let committed = if bound {
            let origin = CodexOrigin::of(&rig._dir.path().join("policy.json")).unwrap();
            let (path, committed) = rig
                .store
                .save_policy_bound(&submitted, &origin, &mut apply, &mut |_, _| {
                    panic!("same-file save was refused")
                })
                .unwrap();
            assert!(path.is_some_and(|path| !path.is_empty()));
            committed.unwrap()
        } else {
            let (outcome, committed) = rig
                .store
                .save_policy_guarded(&submitted, &mut apply, &mut |_| Ok(Default::default()))
                .unwrap();
            assert!(matches!(outcome, crate::auth::GuardedSave::Written(path) if !path.is_empty()));
            committed.unwrap()
        };
        assert_eq!(committed.metadata["siwc"], before["siwc"]);
        assert_eq!(committed.metadata["use_siwc"], before["use_siwc"]);
        assert_eq!(
            serde_json::Value::Object(committed.metadata.clone()),
            rig.disk()
        );
        assert!(committed.disabled);
        assert_eq!(committed.status, Status::Disabled);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round4_policy_transaction_keeps_guard_and_origin_refusals() {
    let rig = FilePolicyRig::new("codex", true);
    let before = rig.disk();
    let mut applied = false;
    let mut apply = |_: &mut Auth, _: bool| {
        applied = true;
    };
    let (outcome, committed) = rig
        .store
        .save_policy_guarded(&rig.initial, &mut apply, &mut |_| {
            Err(crate::auth::SaveDeferral::Stale)
        })
        .unwrap();
    assert!(matches!(
        outcome,
        crate::auth::GuardedSave::Deferred(crate::auth::SaveDeferral::Stale)
    ));
    assert!(committed.is_none());
    assert!(!applied, "policy application must follow admission");
    let other = tempfile::tempdir().unwrap();
    let origin = CodexOrigin::of(&other.path().join("policy.json")).unwrap();
    let mut refused = false;
    let mut applied = false;
    let (path, committed) = rig
        .store
        .save_policy_bound(
            &rig.initial,
            &origin,
            &mut |_, _| {
                applied = true;
            },
            &mut |_, _| {
                refused = true;
            },
        )
        .unwrap();
    assert!(refused);
    assert!(!applied);
    assert!(path.is_none());
    assert!(committed.is_none());
    assert_eq!(rig.disk(), before);
}
