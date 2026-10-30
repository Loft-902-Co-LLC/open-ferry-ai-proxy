// Failed saves follow upstream; independent publication fences still apply.
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
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        *lock(&manager.shared.persist_hook) = None;
        assert!(
            store.read_back(&live).unwrap().unwrap().disabled,
            "{action} must not skip the pending disable save"
        );
        assert!(manager.get("waiting.json").unwrap().disabled);
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

struct FileSaveRig {
    dir: tempfile::TempDir,
    fail: Arc<AtomicBool>,
    manager: Manager,
    initial: Arc<Auth>,
}

impl FileSaveRig {
    fn new(provider: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fail = Arc::new(AtomicBool::new(false));
        let failing = fail.clone();
        let files = Arc::new(FileStore::new(dir.path()).with_file_guard(Some(
            crate::auth::file_guard::FileGuard::new(move |_| {
                if failing.load(Ordering::SeqCst) {
                    Err("test save refusal".into())
                } else {
                    Ok(())
                }
            }),
        )));
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(files),
        );
        let mut auth = auth_with_metadata(
            "ordinary.json",
            provider,
            json!({
                "type":provider, "access_token":"dummy-r1", "disabled":false, "note":"before"
            }),
        );
        auth.file_name = auth.id.clone();
        let initial = manager.register(auth).unwrap();
        Self {
            dir,
            fail,
            manager,
            initial,
        }
    }

    fn disk(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.dir.path().join(&self.initial.id)).unwrap())
            .unwrap()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round5_failed_edit_is_replaced_by_the_next_file_observation() {
    let h = Harness::with_store(Settings::default());
    let initial = h
        .manager
        .register(auth_with_metadata(
            "failed",
            "claude",
            json!({"type":"claude", "access_token":"dummy", "disabled":false, "note":"before"}),
        ))
        .unwrap();
    h.store.set_fail_saves(true);
    let edited = h
        .manager
        .edit("failed", |auth| {
            disable(auth)?;
            auth.status_message = "disabled via management API".into();
            auth.metadata["note"] = json!("operator");
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert!(edited.disabled, "an ordinary save error is nonfatal");
    assert_eq!(edited.metadata["note"], "operator");
    let file = h.store.stored("failed").unwrap();
    assert!(!file.disabled);
    assert_eq!(file.metadata["note"], "before");
    assert!(matches!(
        h.manager
            .adopt_observed(&initial, None, |_| Ok(AdoptionRead::Record(file)))
            .unwrap(),
        AdoptionResult::Published { .. }
    ));
    let observed = h.manager.get("failed").unwrap();
    assert!(!observed.disabled, "the next genuine file read wins");
    assert_eq!(observed.metadata["note"], "before");
}

#[tokio::test(flavor = "current_thread")]
async fn round5_ordinary_refresh_after_failed_save_writes_memory_fields() {
    for provider in ["claude", "gemini"] {
        let rig = FileSaveRig::new(provider);
        rig.fail.store(true, Ordering::SeqCst);
        let edited = rig.manager.edit(&rig.initial.id, disable).unwrap().unwrap();
        assert!(edited.disabled);
        assert_eq!(rig.disk()["access_token"], "dummy-r1");
        rig.fail.store(false, Ordering::SeqCst);
        let mut refreshed = edited.as_ref().clone();
        refreshed.metadata["access_token"] = json!("dummy-r2");
        refreshed
            .metadata
            .insert("refresh_token".into(), json!("dummy-r2-refresh"));
        let (saved, _) = rig
            .manager
            .update_refreshed(&edited, edited.registration_epoch, refreshed)
            .unwrap()
            .unwrap();
        assert_eq!(saved.metadata["access_token"], "dummy-r2", "{provider}");
        assert_eq!(rig.disk()["access_token"], "dummy-r2", "{provider}");
        assert_eq!(
            rig.disk()["refresh_token"],
            "dummy-r2-refresh",
            "{provider}"
        );
        rig.manager.mark_result(&CallResult {
            auth_id: saved.id.clone(),
            provider: provider.into(),
            success: true,
            registration_epoch: saved.registration_epoch,
            credential_version: saved.credential_version,
            ..Default::default()
        });
        assert_eq!(
            rig.disk()["access_token"],
            "dummy-r2",
            "an ordinary result save writes memory"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn round5_first_edit_writes_memory_over_an_unobserved_hand_edit() {
    for provider in ["claude", "gemini", "codex"] {
        let rig = FileSaveRig::new(provider);
        let mut disk = rig.disk();
        disk["note"] = json!("unobserved-hand-edit");
        std::fs::write(
            rig.dir.path().join(&rig.initial.id),
            serde_json::to_vec(&disk).unwrap(),
        )
        .unwrap();
        assert_eq!(
            rig.manager.get(&rig.initial.id).unwrap().metadata["note"],
            "before"
        );
        let edited = rig
            .manager
            .edit(&rig.initial.id, |auth| {
                auth.metadata["note"] = json!("api-edit");
                Ok::<_, ()>(true)
            })
            .unwrap()
            .unwrap();
        assert_eq!(edited.metadata["note"], "api-edit");
        assert_eq!(
            rig.disk()["note"],
            "api-edit",
            "the first explicit edit is saved for {provider}"
        );
    }
}

struct CountSaves {
    files: Arc<FileStore>,
    fail: AtomicBool,
    calls: AtomicU64,
}

impl AuthStore for CountSaves {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.files.list()
    }
    fn delete(&self, id: &str) -> std::io::Result<()> {
        self.files.delete(id)
    }
    fn read_back(&self, auth: &Auth) -> std::io::Result<Option<Auth>> {
        self.files.read_back(auth)
    }
    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(std::io::ErrorKind::PermissionDenied.into())
        } else {
            self.files.save(auth)
        }
    }
}

#[tokio::test(start_paused = true)]
async fn round5_an_unsavable_status_message_starts_no_retry_task() {
    for failed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(CountSaves {
            files: Arc::new(FileStore::new(dir.path())),
            fail: AtomicBool::new(false),
            calls: AtomicU64::new(0),
        });
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store.clone()),
        );
        let mut auth = auth_with_metadata(
            "no-timer.json",
            "claude",
            json!({"type":"claude", "access_token":"dummy", "disabled":false}),
        );
        auth.file_name = auth.id.clone();
        manager.register(auth).unwrap();
        store.calls.store(0, Ordering::SeqCst);
        store.fail.store(failed, Ordering::SeqCst);
        let metrics = tokio::runtime::Handle::current().metrics();
        let tasks = metrics.num_alive_tasks();
        let edited = manager
            .edit("no-timer.json", |auth| {
                disable(auth)?;
                auth.status_message = "disabled via management API".into();
                Ok::<_, ()>(true)
            })
            .unwrap()
            .unwrap();
        assert_eq!(edited.status_message, "disabled via management API");
        assert_eq!(store.calls.load(Ordering::SeqCst), 1);
        assert_eq!(metrics.num_alive_tasks(), tasks, "no retry task exists");
        tokio::time::advance(Duration::from_secs(86_400)).await;
        tokio::task::yield_now().await;
        assert_eq!(metrics.num_alive_tasks(), tasks);
        assert_eq!(
            store.calls.load(Ordering::SeqCst),
            1,
            "elapsed time does not retry a save"
        );
    }
}
