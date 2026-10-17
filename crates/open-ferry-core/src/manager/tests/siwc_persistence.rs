// Regression tests for SIWC persistence and credential identity (v8.0.27).
use super::support::*;
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::credential_lock::credential_lock_path;
use crate::auth::{
    Auth, AuthStore, CodexLease, CodexLeaseToken, CodexOrigin, CodexPersisted, FileStore,
    GuardedSave, OriginProof, SaveDeferral, WithheldPartitions,
};
use crate::gate::{ActiveGate, GateState};
use crate::manager::{CallResult, Manager, Settings, lock};
use futures_util::FutureExt;
use serde_json::{Map, Value, json};
use std::fs::OpenOptions;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

fn fixture() -> Auth {
    auth_with_metadata(
        "codex.json",
        "codex",
        json!({
            "type":"codex", "access_token":"outer-old", "refresh_token":"outer-refresh-old",
            "expired":"2099-01-01T00:00:00Z", "use_siwc":true, "note":"initial",
            "siwc":{"access_token":"siwc-old", "refresh_token":"siwc-refresh-old",
                "client_id":"client", "device_id":"device-old", "expired":"2099-01-01T00:00:00Z"}
        }),
    )
}

type SameRecordHook = Arc<dyn Fn(&Auth, &Auth) + Send + Sync>;
type SaveHook = Arc<dyn Fn(&Auth) + Send + Sync>;
/// Called with the map `codex_lease_read` is about to return; may edit it.
type LeaseReadHook = Arc<dyn Fn(&mut Map<String, Value>) + Send + Sync>;

pub(super) struct ObservedStore {
    pub(super) files: Arc<FileStore>,
    pub(super) error_once: Mutex<Option<String>>,
    pub(super) fail_refresh: AtomicBool,
    pub(super) skip_refresh_read: AtomicBool,
    pub(super) fail_after_save: AtomicBool,
    pub(super) writes: Mutex<Vec<u64>>,
    pub(super) saved: Notify,
    pub(super) same_record_hook: Mutex<Option<SameRecordHook>>,
    pub(super) same_record_error: AtomicBool,
    pub(super) save_hook: Mutex<Option<SaveHook>>,
    pub(super) before_save_hook: Mutex<Option<SaveHook>>,
    pub(super) read_hook: Mutex<Option<SaveHook>>,
    /// While set, `codex_origin` gives `Ok(None)`: a record with no resolved
    /// identity. The lease methods and `save_bound` still forward.
    pub(super) opaque_origin: AtomicBool,
    /// While non-zero, `codex_lease_commit` fails before it forwards;
    /// `u32::MAX` never counts down.
    pub(super) fail_lease_commit: AtomicU32,
    /// Every `codex_lease_commit` call, failed or not.
    pub(super) lease_commit_attempts: AtomicU32,
    /// Every `codex_refresh_lease` call.
    pub(super) lease_calls: AtomicU32,
    /// Every `save_bound` call.
    pub(super) save_bound_calls: AtomicU32,
    /// Fires at the start of every `codex_lease_commit`.
    pub(super) before_lease_commit_hook: Mutex<Option<SaveHook>>,
    /// Fires on the map `codex_lease_read` is about to return.
    pub(super) lease_read_hook: Mutex<Option<LeaseReadHook>>,
    /// While non-zero, `codex_lease_commit` forwards and then returns an
    /// injected error in place of the result (a write that landed unseen).
    pub(super) fail_after_lease_commit: AtomicU32,
    /// While non-zero, `codex_lease_read` fails without forwarding.
    pub(super) fail_lease_read: AtomicU32,
    /// Each `save_guarded` result with the record's generation; `None` is a
    /// write.
    pub(super) guarded: Mutex<Vec<(u64, Option<SaveDeferral>)>>,
    /// While set, `codex_refresh_lease` gives `Unsupported` without forwarding.
    pub(super) lease_unsupported: AtomicBool,
}

/// Counts a fault counter down; `u32::MAX` stays armed. True when it was armed.
fn take_fault(counter: &AtomicU32) -> bool {
    match counter.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
        0 | u32::MAX => None,
        n => Some(n - 1),
    }) {
        Ok(_) => true,
        Err(n) => n == u32::MAX,
    }
}

impl ObservedStore {
    /// What every save method does first: the hook, `error_once`, then
    /// `fail_refresh` for a `Refresh` intent.
    fn save_preamble(&self, auth: &Auth) -> io::Result<()> {
        let hook = lock(&self.before_save_hook).clone();
        if let Some(hook) = hook {
            hook(auth);
        }
        if let Some(error) = lock(&self.error_once).take() {
            return Err(io::Error::other(error));
        }
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && self.fail_refresh.swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other("injected refresh save failure"));
        }
        Ok(())
    }

    /// What every save method does after a write.
    fn after_write(&self, auth: &Auth) {
        let hook = lock(&self.save_hook).clone();
        if let Some(hook) = hook {
            hook(auth);
        }
        lock(&self.writes).push(auth.generation);
        self.saved.notify_waiters();
    }
}

impl AuthStore for ObservedStore {
    fn list(&self) -> io::Result<Vec<Auth>> {
        self.files.list()
    }

    fn save(&self, auth: &Auth) -> io::Result<String> {
        self.save_preamble(auth)?;
        let path = self.files.save(auth)?;
        if self.fail_after_save.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected error after landed write"));
        }
        self.after_write(auth);
        Ok(path)
    }

    fn codex_refresh_lease(&self, auth: &Auth) -> io::Result<CodexLease> {
        self.lease_calls.fetch_add(1, Ordering::SeqCst);
        if self.lease_unsupported.load(Ordering::SeqCst) {
            return Ok(CodexLease::Unsupported);
        }
        self.files.codex_refresh_lease(auth)
    }

    fn codex_lease_read(&self, token: &CodexLeaseToken) -> io::Result<Option<Map<String, Value>>> {
        if take_fault(&self.fail_lease_read) {
            return Err(io::Error::other("injected lease read failure"));
        }
        let mut map = self.files.codex_lease_read(token)?;
        let hook = lock(&self.lease_read_hook).clone();
        if let (Some(hook), Some(map)) = (hook, map.as_mut()) {
            hook(map);
        }
        Ok(map)
    }

    fn codex_lease_commit(
        &self,
        token: &CodexLeaseToken,
        auth: &Auth,
        patch: &crate::auth::codex_refresh::CredentialRefreshPatch,
    ) -> io::Result<CodexPersisted> {
        self.lease_commit_attempts.fetch_add(1, Ordering::SeqCst);
        let hook = lock(&self.before_lease_commit_hook).clone();
        if let Some(hook) = hook {
            hook(auth);
        }
        if take_fault(&self.fail_lease_commit) {
            return Err(io::Error::other("injected lease commit failure"));
        }
        let result = self.files.codex_lease_commit(token, auth, patch)?;
        if take_fault(&self.fail_after_lease_commit) {
            return Err(io::Error::other("injected error after landed lease commit"));
        }
        Ok(result)
    }

    fn codex_origin(&self, auth: &Auth) -> io::Result<Option<CodexOrigin>> {
        if self.opaque_origin.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.files.codex_origin(auth)
    }

    fn save_bound(
        &self,
        auth: &Auth,
        origin: &CodexOrigin,
        refused: &mut dyn FnMut(&CodexOrigin, OriginProof),
    ) -> io::Result<Option<String>> {
        self.save_bound_calls.fetch_add(1, Ordering::SeqCst);
        self.save_preamble(auth)?;
        let written = self.files.save_bound(auth, origin, refused)?;
        if written.is_some() {
            if self.fail_after_save.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected error after landed write"));
            }
            self.after_write(auth);
        }
        Ok(written)
    }

    fn save_guarded(
        &self,
        auth: &Auth,
        guard: &mut dyn FnMut(Option<&CodexOrigin>) -> Result<WithheldPartitions, SaveDeferral>,
    ) -> io::Result<GuardedSave> {
        self.save_preamble(auth)?;
        let result = self.files.save_guarded(auth, guard)?;
        match &result {
            GuardedSave::Written(_) => {
                lock(&self.guarded).push((auth.generation, None));
                if self.fail_after_save.swap(false, Ordering::SeqCst) {
                    return Err(io::Error::other("injected error after landed write"));
                }
                self.after_write(auth);
            }
            GuardedSave::Deferred(reason) => {
                lock(&self.guarded).push((auth.generation, Some(*reason)));
            }
        }
        Ok(result)
    }

    fn delete(&self, id: &str) -> io::Result<()> {
        self.files.delete(id)
    }

    fn read_back(&self, auth: &Auth) -> io::Result<Option<Auth>> {
        if self.skip_refresh_read.load(Ordering::SeqCst)
            && matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
        {
            return Ok(None);
        }
        self.files.read_back(auth)
    }

    fn read_for_adoption(&self, auth: &Auth) -> io::Result<crate::auth::AdoptionRead> {
        if let Some(hook) = lock(&self.read_hook).clone() {
            hook(auth);
        }
        self.files.read_for_adoption(auth)
    }

    fn supports_codex_refresh_patches(&self) -> bool {
        true
    }

    fn same_record(&self, a: &Auth, b: &Auth) -> io::Result<bool> {
        if self.same_record_error.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected record identity error"));
        }
        let result = self.files.same_record(a, b);
        let hook = lock(&self.same_record_hook).clone();
        if let Some(hook) = hook {
            hook(a, b);
        }
        result
    }

    fn binding_generation(&self) -> u64 {
        self.files.binding_generation()
    }
}

impl ObservedStore {
    async fn wait_saved(&self, generation: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.saved.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if lock(&self.writes).contains(&generation) {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the save completes");
    }
}

/// An [`ObservedStore`] over a given `FileStore`, with no fault armed.
/// `lease_unsupported` makes it answer `Unsupported` to every lease.
pub(super) fn observe(files: Arc<FileStore>, lease_unsupported: bool) -> Arc<ObservedStore> {
    Arc::new(ObservedStore {
        files,
        error_once: Mutex::new(None),
        fail_refresh: AtomicBool::new(false),
        skip_refresh_read: AtomicBool::new(false),
        fail_after_save: AtomicBool::new(false),
        writes: Mutex::new(Vec::new()),
        saved: Notify::new(),
        same_record_hook: Mutex::new(None),
        same_record_error: AtomicBool::new(false),
        save_hook: Mutex::new(None),
        before_save_hook: Mutex::new(None),
        read_hook: Mutex::new(None),
        opaque_origin: AtomicBool::new(false),
        fail_lease_commit: AtomicU32::new(0),
        lease_commit_attempts: AtomicU32::new(0),
        lease_calls: AtomicU32::new(0),
        save_bound_calls: AtomicU32::new(0),
        before_lease_commit_hook: Mutex::new(None),
        lease_read_hook: Mutex::new(None),
        fail_after_lease_commit: AtomicU32::new(0),
        fail_lease_read: AtomicU32::new(0),
        guarded: Mutex::new(Vec::new()),
        lease_unsupported: AtomicBool::new(lease_unsupported),
    })
}

fn disk_manager() -> (tempfile::TempDir, Arc<ObservedStore>, Manager, ActiveGate) {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let files = Arc::new(FileStore::new(dir.path()));
    let gate = ActiveGate::open();
    files.set_gate(gate.clone());
    // A login no longer writes SIWC, so seed the fixture's keys with a raw write.
    files.save_new(&fixture()).unwrap();
    let path = dir.path().join("codex.json");
    let mut seeded: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for key in ["siwc", "use_siwc"] {
        seeded.insert(key.into(), fixture().metadata[key].clone());
    }
    std::fs::write(&path, serde_json::to_vec(&seeded).unwrap()).unwrap();
    let store = observe(files, false);
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    manager.set_gate(gate.clone());
    manager.register_unsaved(fixture()).unwrap();
    (dir, store, manager, gate)
}

fn disk(dir: &tempfile::TempDir) -> Value {
    serde_json::from_slice(&std::fs::read(dir.path().join("codex.json")).unwrap()).unwrap()
}

fn set_note(manager: &Manager, note: &str) {
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata.insert("note".into(), json!(note));
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
}

fn success(manager: &Manager) -> CallResult {
    let auth = manager.get("codex.json").unwrap();
    CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        success: true,
        credential_version: auth.credential_version,
        registration_epoch: auth.registration_epoch,
        ..CallResult::default()
    }
}

/// A raw OS holder, not our process mutex. The watchdog makes a regression
/// fail promptly instead of leaving a blocked current-thread test for 30 s.
struct OsHold {
    release: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OsHold {
    fn new(dir: &tempfile::TempDir) -> Self {
        let path = credential_lock_path(&dir.path().join("codex.json")).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .unwrap();
            file.lock().unwrap();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            file.unlock().unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            release: Some(release_tx),
            thread: Some(thread),
        }
    }
}

impl Drop for OsHold {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// Stops an older save before its persist watermark check. Dropping this
/// releases the hook even when an assertion fails.
struct SaveHold {
    release: Option<mpsc::Sender<()>>,
    entered: Arc<Notify>,
}

impl SaveHold {
    fn new(manager: &Manager) -> Self {
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let entered = Arc::new(Notify::new());
        let notify = entered.clone();
        let armed = AtomicBool::new(true);
        *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth: &Auth| {
            if auth.metadata.get("note") == Some(&json!("older"))
                && armed.swap(false, Ordering::SeqCst)
            {
                notify.notify_one();
                lock(&release_rx)
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the older save is released");
            }
        }));
        Self {
            release: Some(release_tx),
            entered,
        }
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("the older save reaches the hook");
    }
}

impl Drop for SaveHold {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[tokio::test]
async fn failed_refresh_save_then_edit_persists_both_rotations_and_settings() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("outer-new"));
        after
            .metadata
            .insert("refresh_token".into(), json!("outer-refresh-new"));
        after.metadata["siwc"]["access_token"] = json!("siwc-new");
        after.metadata["siwc"]["refresh_token"] = json!("siwc-refresh-new");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            true,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert!(!store.fail_refresh.load(Ordering::SeqCst));
    let refreshed = manager.get("codex.json").unwrap();
    assert_eq!(refreshed.metadata["access_token"], "outer-new");
    assert_eq!(
        refreshed.metadata["siwc"]["refresh_token"],
        "siwc-refresh-new"
    );
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");
    assert_eq!(disk(&dir)["siwc"]["refresh_token"], "siwc-refresh-old");
    manager
        .run_persistence(|manager| {
            manager.edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("edited"));
                Ok::<_, ()>(true)
            })
        })
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = disk(&dir);
    assert_eq!(saved["access_token"], "outer-new");
    assert_eq!(saved["refresh_token"], "outer-refresh-new");
    assert_eq!(saved["siwc"]["access_token"], "siwc-new");
    assert_eq!(saved["siwc"]["refresh_token"], "siwc-refresh-new");
    assert_eq!(saved["note"], "edited");
}

#[tokio::test(flavor = "current_thread")]
async fn configuration_snapshot_carries_published_rotation_before_refresh_save_fails() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("outer-new"));
        after
            .metadata
            .insert("refresh_token".into(), json!("outer-refresh-new"));
        after.metadata["siwc"]["access_token"] = json!("siwc-new");
        after.metadata["siwc"]["refresh_token"] = json!("siwc-refresh-new");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            true,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    let (refresh_tx, refresh_rx) = mpsc::channel();
    let (config_tx, config_rx) = mpsc::channel();
    let refresh_rx = Mutex::new(refresh_rx);
    let config_rx = Mutex::new(config_rx);
    let refresh_entered = Arc::new(Notify::new());
    let config_entered = Arc::new(Notify::new());
    let refresh_notify = refresh_entered.clone();
    let config_notify = config_entered.clone();
    let refresh_armed = AtomicBool::new(true);
    let config_armed = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && refresh_armed.swap(false, Ordering::SeqCst)
        {
            refresh_notify.notify_one();
            lock(&refresh_rx)
                .recv_timeout(Duration::from_secs(5))
                .expect("the refresh save is released");
        } else if auth.metadata.get("note") == Some(&json!("concurrent-config"))
            && config_armed.swap(false, Ordering::SeqCst)
        {
            config_notify.notify_one();
            lock(&config_rx)
                .recv_timeout(Duration::from_secs(5))
                .expect("the configuration save is released");
        }
    }));
    let refresh_hold = SaveHold {
        release: Some(refresh_tx),
        entered: refresh_entered,
    };
    let config_hold = SaveHold {
        release: Some(config_tx),
        entered: config_entered,
    };
    store.fail_refresh.store(true, Ordering::SeqCst);
    let refreshing = manager.clone();
    let refresh_task = tokio::spawn(async move { refreshing.force_refresh("codex.json").await });
    refresh_hold.wait_entered().await;
    let editing = manager.clone();
    let config_task = tokio::spawn(async move {
        editing
            .run_persistence(|manager| {
                manager.edit("codex.json", |auth| {
                    auth.metadata
                        .insert("note".into(), json!("concurrent-config"));
                    Ok::<_, ()>(true)
                })
            })
            .await
    });
    config_hold.wait_entered().await;
    let config_generation = manager.get("codex.json").unwrap().generation;
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    drop(refresh_hold);
    refresh_task.await.unwrap().unwrap();
    assert!(!store.fail_refresh.load(Ordering::SeqCst));
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.generation, config_generation);
    assert!(!live.codex_pending_refresh.is_empty());
    drop(config_hold);
    config_task.await.unwrap().unwrap().unwrap().unwrap();
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-new");
    manager
        .run_persistence(|manager| {
            manager.edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("final-config"));
                Ok::<_, ()>(true)
            })
        })
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = disk(&dir);
    assert_eq!(saved["access_token"], "outer-new");
    assert_eq!(saved["refresh_token"], "outer-refresh-new");
    assert_eq!(saved["siwc"]["access_token"], "siwc-new");
    assert_eq!(saved["siwc"]["refresh_token"], "siwc-refresh-new");
    assert_eq!(saved["note"], "final-config");
}

#[tokio::test]
async fn explicit_outer_token_edit_keeps_pending_siwc_rotation() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("outer-new"));
        after
            .metadata
            .insert("refresh_token".into(), json!("outer-refresh-new"));
        after.metadata["siwc"]["access_token"] = json!("siwc-new");
        after.metadata["siwc"]["refresh_token"] = json!("siwc-refresh-new");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            true,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert!(!store.fail_refresh.load(Ordering::SeqCst));
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    let original = manager.get("codex.json").unwrap().codex_pending_refresh[0].clone();
    let mut explicit = fixture();
    for key in crate::auth::codex_refresh::OUTER_KEYS {
        explicit.metadata.remove(*key);
    }
    for (key, value) in [
        ("access_token", "outer-explicit"),
        ("refresh_token", "outer-refresh-explicit"),
        ("id_token", "outer-id-explicit"),
        ("account_id", "outer-account-explicit"),
        ("expired", "2099-04-01T00:00:00Z"),
    ] {
        explicit.metadata.insert(key.into(), json!(value));
    }
    explicit.codex_persist_intent = CodexPersistIntent::OuterLogin;
    store.files.save(&explicit).unwrap();
    let edited = manager
        .edit_unsaved("codex.json", |auth| {
            for key in crate::auth::codex_refresh::OUTER_KEYS {
                auth.metadata.remove(*key);
                if let Some(value) = explicit.metadata.get(*key) {
                    auth.metadata.insert((*key).into(), value.clone());
                }
            }
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert_eq!(edited.codex_pending_refresh.len(), 1);
    let retained = &edited.codex_pending_refresh[0];
    assert!(retained.same_operation(&original));
    assert!(!retained.includes_partition(crate::auth::CodexCredentialPartition::Outer));
    assert!(retained.includes_partition(crate::auth::CodexCredentialPartition::Siwc));
    assert_eq!(edited.metadata["access_token"], "outer-explicit");
    assert_eq!(edited.metadata["siwc"]["access_token"], "siwc-new");
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    let saved = disk(&dir);
    assert_eq!(saved["siwc"]["refresh_token"], "siwc-refresh-new");
    for key in crate::auth::codex_refresh::OUTER_KEYS {
        assert_eq!(
            saved.get(*key),
            explicit.metadata.get(*key),
            "explicit outer field {key}"
        );
    }
    assert!(original.is_written());
}

#[tokio::test]
async fn consecutive_failed_refreshes_then_edit_persist_latest_rotations() {
    // Exercise both a same-partition chain and a later SIWC-only rotation
    // that must retain the earlier outer rotation.
    for second_outer in [true, false] {
        let (dir, store, manager, _gate) = disk_manager();
        // The fault or hook is on the unleased refresh save, so the store gives no lease.
        store.lease_unsupported.store(true, Ordering::SeqCst);
        for (round, outer) in [(1, true), (2, second_outer)] {
            let executor = FakeExecutor::new("codex");
            executor.set_refresh(move |before| {
                let mut after = before.clone();
                if outer {
                    after
                        .metadata
                        .insert("access_token".into(), json!(format!("outer-{round}")));
                    after.metadata.insert(
                        "refresh_token".into(),
                        json!(format!("outer-refresh-{round}")),
                    );
                }
                after.metadata["siwc"]["access_token"] = json!(format!("siwc-{round}"));
                after.metadata["siwc"]["refresh_token"] = json!(format!("siwc-refresh-{round}"));
                after.codex_persist_intent = CodexPersistIntent::Refresh(
                    CredentialRefreshPatch::new(&before.metadata, &after.metadata, outer, true),
                );
                Ok(after)
            });
            manager.register_executor(executor);
            store.fail_refresh.store(true, Ordering::SeqCst);
            manager.force_refresh("codex.json").await.unwrap();
            assert!(!store.fail_refresh.load(Ordering::SeqCst));
            assert!(
                !manager
                    .get("codex.json")
                    .unwrap()
                    .codex_pending_refresh
                    .is_empty()
            );
        }
        assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");
        assert_eq!(disk(&dir)["siwc"]["refresh_token"], "siwc-refresh-old");
        manager
            .run_persistence(|manager| {
                manager.edit("codex.json", |auth| {
                    auth.metadata.insert("note".into(), json!("edited"));
                    Ok::<_, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let saved = disk(&dir);
        let outer_round = if second_outer { 2 } else { 1 };
        assert_eq!(saved["access_token"], format!("outer-{outer_round}"));
        assert_eq!(
            saved["refresh_token"],
            format!("outer-refresh-{outer_round}")
        );
        assert_eq!(saved["siwc"]["access_token"], "siwc-2");
        assert_eq!(saved["siwc"]["refresh_token"], "siwc-refresh-2");
        assert_eq!(saved["note"], "edited");
        assert!(
            manager
                .get("codex.json")
                .unwrap()
                .codex_pending_refresh
                .is_empty()
        );
    }
}

#[test]
fn settings_edit_during_record_comparison_does_not_discard_scoped_refresh() {
    let (dir, store, manager, _gate) = disk_manager();
    let before = manager.get("codex.json").unwrap();
    let mut refreshed = (*before).clone();
    refreshed
        .metadata
        .insert("access_token".into(), json!("outer-new"));
    refreshed
        .metadata
        .insert("refresh_token".into(), json!("outer-refresh-new"));
    refreshed.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &refreshed.metadata,
        true,
        false,
    ));
    // A refresh publishes before it persists: live holds the refreshed tokens.
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata
                .insert("access_token".into(), json!("outer-new"));
            auth.metadata
                .insert("refresh_token".into(), json!("outer-refresh-new"));
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    manager
        .edit("codex.json", |auth| {
            auth.metadata.insert("note".into(), json!("saved-newer"));
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert!(manager.get("codex.json").unwrap().generation > refreshed.generation);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let armed = AtomicBool::new(true);
    *lock(&store.same_record_hook) = Some(Arc::new(move |snapshot, _live| {
        if matches!(
            snapshot.codex_persist_intent,
            CodexPersistIntent::Refresh(_)
        ) && armed.swap(false, Ordering::SeqCst)
        {
            entered_tx.send(()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(5))
                .expect("the live-generation comparison is released");
        }
    }));
    let saving = manager.clone();
    let thread = std::thread::spawn(move || {
        saving
            .persist(
                &refreshed,
                refreshed.registration_epoch,
                refreshed.generation,
                0,
                crate::manager::lifecycle::Save::Yes,
            )
            .unwrap();
    });
    // The sender is owned by a guard, so assertion unwinding also releases it.
    let hold = SaveHold {
        release: Some(release_tx),
        entered: Arc::new(Notify::new()),
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (published_tx, published_rx) = mpsc::channel();
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.metadata.get("note") == Some(&json!("live-newer")) {
            published_tx.send(()).unwrap();
        }
    }));
    let editing = manager.clone();
    let edit_thread = std::thread::spawn(move || set_note(&editing, "live-newer"));
    published_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(hold);
    thread.join().unwrap();
    edit_thread.join().unwrap();
    let saved = disk(&dir);
    assert_eq!(saved["access_token"], "outer-new");
    assert_eq!(saved["refresh_token"], "outer-refresh-new");
    assert_eq!(saved["note"], "saved-newer");
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["note"],
        "live-newer"
    );
}

#[test]
fn result_save_failure_log_scrubs_outer_and_siwc_tokens() {
    struct Fields(String);
    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={value:?} ", field.name()));
        }
    }
    struct Capture(Arc<Mutex<Vec<String>>>);
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
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            lock(&self.0).push(fields.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let (_dir, store, manager, _gate) = disk_manager();
    let secrets = [
        "outer-old",
        "outer-refresh-old",
        "siwc-old",
        "siwc-refresh-old",
    ];
    *lock(&store.error_once) = Some(format!("injected save failure: {}", secrets.join(" ")));
    crate::test_tracing::keep_every_callsite_open();
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = Capture(events.clone());
    // This exercises the same logging helper as detached saves. A scoped
    // dispatcher is not carried into Operation::run_blocking, so capture
    // the synchronous no-runtime fallback rather than change global tracing.
    tracing::subscriber::with_default(subscriber, || manager.mark_result(&success(&manager)));
    assert!(lock(&store.error_once).is_none());
    let logged = lock(&events).join("\n");
    assert!(
        logged.contains("failed to persist credential state"),
        "{logged}"
    );
    assert!(logged.contains("injected save failure"), "{logged}");
    for secret in secrets {
        assert!(!logged.contains(secret), "log contains {secret}: {logged}");
    }
}

#[test]
fn non_codex_account_id_edit_leaves_credential_version_unchanged() {
    let h = Harness::new(Settings::default());
    let before = h
        .manager
        .register(auth_with_metadata(
            "gemini",
            "gemini",
            json!({"type":"gemini", "account_id":"before"}),
        ))
        .unwrap();
    let after = h
        .manager
        .edit("gemini", |auth| {
            auth.metadata.insert("account_id".into(), json!("after"));
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert_eq!(after.metadata["account_id"], "after");
    assert_eq!(after.credential_version, before.credential_version);
}

#[test]
fn codex_switch_compares_parsed_bool_and_device_id_is_not_token_identity() {
    let h = Harness::new(Settings::default());
    let before = h.manager.register(fixture()).unwrap();
    let after = h
        .manager
        .edit("codex.json", |auth| {
            auth.metadata.insert("use_siwc".into(), json!("true"));
            auth.metadata["siwc"]["device_id"] = json!("device-new");
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert!(after.use_siwc());
    assert_eq!(after.credential_version, before.credential_version);
    let switched = h
        .manager
        .edit("codex.json", |auth| {
            auth.metadata.insert("use_siwc".into(), json!("false"));
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert!(!switched.use_siwc());
    assert!(switched.credential_version > after.credential_version);
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_result_returns_promptly_while_os_lock_is_held_then_saves() {
    let (dir, store, manager, _gate) = disk_manager();
    set_note(&manager, "saved-after-release");
    let held = OsHold::new(&dir);
    let start = Instant::now();
    manager.mark_result(&success(&manager));
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "result recording blocked on storage"
    );
    let generation = manager.get("codex.json").unwrap().generation;
    tokio::time::timeout(Duration::from_millis(500), async {
        tokio::task::yield_now().await;
        assert_eq!(disk(&dir)["note"], "initial");
        assert!(lock(&store.writes).is_empty());
    })
    .await
    .expect("the current-thread runtime keeps making progress");
    drop(held);
    store.wait_saved(generation).await;
    assert_eq!(disk(&dir)["note"], "saved-after-release");
}

#[tokio::test(flavor = "current_thread")]
async fn quiesce_waits_for_the_detached_result_save() {
    let (dir, store, manager, gate) = disk_manager();
    set_note(&manager, "saved-while-quiescing");
    let held = OsHold::new(&dir);
    manager.mark_result(&success(&manager));
    let generation = manager.get("codex.json").unwrap().generation;
    let epoch = gate.epoch();
    gate.quiesce(epoch).unwrap();
    assert!(
        gate.settle(epoch).now_or_never().is_none(),
        "the pending save must keep admission alive"
    );
    assert_eq!(gate.state(), GateState::Quiescing);
    assert_eq!(disk(&dir)["note"], "initial");
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), gate.settle(epoch))
        .await
        .unwrap()
        .unwrap();
    store.wait_saved(generation).await;
    assert_eq!(gate.state(), GateState::Closed);
    assert_eq!(disk(&dir)["note"], "saved-while-quiescing");
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_older_detached_save_cannot_overwrite_a_newer_save() {
    let (dir, store, manager, gate) = disk_manager();
    set_note(&manager, "older");
    let held = SaveHold::new(&manager);
    manager.mark_result(&success(&manager));
    held.wait_entered().await;
    set_note(&manager, "newer");
    manager.mark_result(&success(&manager));
    let generation = manager.get("codex.json").unwrap().generation;
    store.wait_saved(generation).await;
    assert_eq!(disk(&dir)["note"], "newer");
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), gate.close(gate.epoch()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(disk(&dir)["note"], "newer");
    assert_eq!(*lock(&store.writes), vec![generation]);
}

#[test]
fn result_save_without_a_runtime_runs_inline() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let (dir, store, manager, gate) = disk_manager();
    set_note(&manager, "inline");
    manager.mark_result(&success(&manager));
    let generation = manager.get("codex.json").unwrap().generation;
    assert_eq!(disk(&dir)["note"], "inline");
    assert_eq!(*lock(&store.writes), vec![generation]);
    gate.quiesce(gate.epoch()).unwrap();
    assert!(gate.settle(gate.epoch()).now_or_never().unwrap().is_ok());
}

#[tokio::test]
async fn sparse_refresh_failure_settings_edit_and_retry_keeps_pending() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let mut before = fixture();
    before
        .metadata
        .insert("id_token".into(), json!("retained-id"));
    store.files.save_new(&before).unwrap();
    manager.update_unsaved(before).unwrap();
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("rotated"));
        after
            .metadata
            .insert("refresh_token".into(), json!("rotated-refresh"));
        after.metadata.remove("id_token");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    set_note(&manager, "settings-only");
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    manager
        .run_persistence(|manager| {
            manager.edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("retry"));
                Ok::<_, ()>(true)
            })
        })
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = disk(&dir);
    assert_eq!(saved["access_token"], "rotated");
    assert_eq!(saved["refresh_token"], "rotated-refresh");
    assert_eq!(saved["id_token"], "retained-id");
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

#[tokio::test]
async fn sparse_second_refresh_saves_pending_refresh_token_rotation() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata.insert("access_token".into(), json!("A1"));
        after.metadata.insert("refresh_token".into(), json!("R1"));
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert!(!store.fail_refresh.load(Ordering::SeqCst));
    let live = manager.get("codex.json").unwrap();
    assert!(!live.codex_pending_refresh.is_empty());
    assert_eq!(live.metadata["access_token"], "A1");
    assert_eq!(live.metadata["refresh_token"], "R1");
    assert_eq!(disk(&dir)["access_token"], "outer-old");
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");

    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata.insert("access_token".into(), json!("A2"));
        after.metadata.remove("refresh_token");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    manager.force_refresh("codex.json").await.unwrap();
    let saved = disk(&dir);
    let live = manager.get("codex.json").unwrap();
    assert_eq!(saved["access_token"], "A2");
    assert_eq!(saved["refresh_token"], "R1");
    assert_eq!(live.metadata["access_token"], "A2");
    assert_eq!(live.metadata["refresh_token"], "R1");
    assert!(live.codex_pending_refresh.is_empty());
}

#[tokio::test]
async fn expired_only_content_merge_reaches_disk_and_live_state() {
    let (dir, _store, manager, _gate) = disk_manager();
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut updated = before.clone();
        updated.codex_persist_intent = CodexPersistIntent::Configuration;
        updated
            .metadata
            .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        // Exercise merge_auth_content's synthesized patch for a content edit,
        // rather than supplying the executor's own explicit refresh patch.
        Ok(crate::manager::merge::merge_auth_content(
            before, before, &updated,
        ))
    });
    manager.register_executor(executor);
    manager.force_refresh("codex.json").await.unwrap();
    let saved = disk(&dir);
    let live = manager.get("codex.json").unwrap();
    assert_eq!(saved["expired"], "2099-02-01T00:00:00Z");
    assert_eq!(live.metadata["expired"], "2099-02-01T00:00:00Z");
    assert_eq!(saved["access_token"], "outer-old");
    assert_eq!(saved["refresh_token"], "outer-refresh-old");
    assert_eq!(live.metadata["access_token"], "outer-old");
    assert_eq!(live.metadata["refresh_token"], "outer-refresh-old");
}

#[tokio::test(flavor = "current_thread")]
async fn unsaved_adoption_returns_promptly_during_save_and_keeps_watermark() {
    let (dir, store, manager, gate) = disk_manager();
    let held = OsHold::new(&dir);
    let waiting = Arc::new(Notify::new());
    let notify = waiting.clone();
    let path = dir.path().join("codex.json");
    crate::auth::credential_lock::set_test_hook(
        &path,
        Some(Arc::new(move |event| {
            if event == "waiting" {
                notify.notify_one();
            }
        })),
    );
    manager.mark_result(&success(&manager));
    let stale = manager.get("codex.json").unwrap();
    tokio::time::timeout(Duration::from_secs(5), waiting.notified())
        .await
        .unwrap();
    // This is an inline synchronous call on a current-thread runtime. The OS
    // holder's watchdog releases a regressed blocking call, so it can fail.
    let start = Instant::now();
    set_note(&manager, "adopted");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "unsaved adoption waited for the save mutex"
    );
    let adopted = manager.get("codex.json").unwrap();
    drop(held);
    store.wait_saved(stale.generation).await;
    tokio::time::timeout(Duration::from_secs(5), gate.close(gate.epoch()))
        .await
        .unwrap()
        .unwrap();
    crate::auth::credential_lock::set_test_hook(&path, None);
    let watermark = lock(&manager.shared.persist_locks)
        .get("codex.json")
        .unwrap()
        .clone();
    assert_eq!(
        *lock(&watermark),
        (adopted.registration_epoch, adopted.generation)
    );
    let writes = lock(&store.writes).len();
    manager
        .persist(
            &stale,
            stale.registration_epoch,
            stale.generation,
            0,
            crate::manager::lifecycle::Save::Yes,
        )
        .unwrap();
    assert_eq!(
        lock(&store.writes).len(),
        writes,
        "stale save after adoption must be skipped"
    );
}

#[test]
fn reconciliation_removes_disk_omissions_but_preserves_independent_identity() {
    for partition in ["outer", "siwc-key", "siwc-object"] {
        for independent in [false, true] {
            let mut snapshot = fixture();
            snapshot
                .metadata
                .insert("id_token".into(), json!("outer-id"));
            snapshot.metadata["siwc"]["id_token"] = json!("siwc-id");
            let mut current = snapshot.clone();
            current.generation += 1;
            current
                .metadata
                .insert("note".into(), json!("concurrent-settings"));
            let mut disk = snapshot.clone();
            if partition == "outer" {
                disk.metadata
                    .insert("access_token".into(), json!("disk-new"));
                disk.metadata.remove("id_token");
                if independent {
                    current
                        .metadata
                        .insert("access_token".into(), json!("live-new"));
                }
            } else {
                if partition == "siwc-key" {
                    disk.metadata["siwc"]["access_token"] = json!("disk-new");
                    disk.metadata["siwc"]
                        .as_object_mut()
                        .unwrap()
                        .remove("id_token");
                } else {
                    disk.metadata.remove("siwc");
                }
                if independent {
                    current.metadata["siwc"]["access_token"] = json!("live-new");
                }
            }
            let merged =
                crate::manager::merge::reconcile_persisted_codex(&snapshot, &current, &disk);
            assert_eq!(merged.metadata["note"], "concurrent-settings");
            if independent {
                let live = if partition == "outer" {
                    &merged.metadata
                } else {
                    merged.metadata["siwc"].as_object().unwrap()
                };
                assert_eq!(live["access_token"], "live-new");
                assert!(live.contains_key("id_token"));
            } else if partition == "siwc-object" {
                assert!(!merged.metadata.contains_key("siwc"));
            } else {
                let live = if partition == "outer" {
                    &merged.metadata
                } else {
                    merged.metadata["siwc"].as_object().unwrap()
                };
                assert!(
                    !live.contains_key("id_token"),
                    "disk omission must be authoritative"
                );
                assert_eq!(live["access_token"], "disk-new");
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reconciliation_publishes_authoritative_disk_omissions() {
    for partition in ["outer", "siwc-key", "siwc-object"] {
        for independent in [false, true] {
            let (_dir, store, manager, _gate) = disk_manager();
            // The fault or hook is on the unleased refresh save, so the store gives no lease.
            store.lease_unsupported.store(true, Ordering::SeqCst);
            let mut before = fixture();
            before.metadata.insert("id_token".into(), json!("outer-id"));
            before.metadata["siwc"]["id_token"] = json!("siwc-id");
            store
                .files
                .write_file("codex.json", &serde_json::to_vec(&before.metadata).unwrap())
                .unwrap();
            manager.update_unsaved(before.clone()).unwrap();
            let executor = FakeExecutor::new("codex");
            executor.set_refresh(|before| {
                let mut after = before.clone();
                after
                    .metadata
                    .insert("access_token".into(), json!("refreshed-outer"));
                after.metadata["siwc"]["access_token"] = json!("refreshed-siwc");
                after.codex_persist_intent = CodexPersistIntent::Refresh(
                    CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true),
                );
                Ok(after)
            });
            manager.register_executor(executor);
            let (release_tx, release_rx) = mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            let entered = Arc::new(Notify::new());
            let notify = entered.clone();
            let armed = AtomicBool::new(true);
            *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
                if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
                    && armed.swap(false, Ordering::SeqCst)
                {
                    notify.notify_one();
                    lock(&release_rx)
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
            }));
            let hold = SaveHold {
                release: Some(release_tx),
                entered,
            };
            let worker = manager.clone();
            let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
            hold.wait_entered().await;
            let mut newer = before.metadata;
            if partition == "outer" {
                newer.insert("access_token".into(), json!("disk-new"));
                newer.remove("id_token");
            } else if partition == "siwc-key" {
                newer["siwc"]["access_token"] = json!("disk-new");
                newer["siwc"].as_object_mut().unwrap().remove("id_token");
            } else {
                newer.remove("siwc");
            }
            store
                .files
                .write_file("codex.json", &serde_json::to_vec(&newer).unwrap())
                .unwrap();
            manager
                .edit_unsaved("codex.json", |auth| {
                    auth.metadata
                        .insert("note".into(), json!("concurrent-settings"));
                    if independent {
                        if partition == "outer" {
                            auth.metadata
                                .insert("access_token".into(), json!("live-new"));
                        } else {
                            auth.metadata["siwc"]["access_token"] = json!("live-new");
                        }
                    }
                    Ok::<_, ()>(true)
                })
                .unwrap()
                .unwrap();
            drop(hold);
            refresh.await.unwrap().unwrap();
            let live = manager.get("codex.json").unwrap();
            assert_eq!(live.metadata["note"], "concurrent-settings");
            if independent {
                let live = if partition == "outer" {
                    &live.metadata
                } else {
                    live.metadata["siwc"].as_object().unwrap()
                };
                assert_eq!(live["access_token"], "live-new");
                assert!(live.contains_key("id_token"));
            } else if partition == "siwc-object" {
                assert!(!live.metadata.contains_key("siwc"));
            } else {
                let live = if partition == "outer" {
                    &live.metadata
                } else {
                    live.metadata["siwc"].as_object().unwrap()
                };
                assert_eq!(live["access_token"], "disk-new");
                assert!(
                    !live.contains_key("id_token"),
                    "published live identity must reflect disk omission"
                );
            }
        }
    }
}

async fn wait_pending_saves(manager: &Manager) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.lock().pending_saves.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all admitted saves finish");
}

fn expiry_executor() -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    executor
}

async fn refresh_with_result_during_save(store: &ObservedStore, manager: &Manager) {
    manager.register_executor(expiry_executor());
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&store.save_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && armed.swap(false, Ordering::SeqCst)
        {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(5))
                .expect("the refresh save is released");
        }
    }));
    let held = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let refreshing = manager.clone();
    let task = tokio::spawn(async move { refreshing.force_refresh("codex.json").await });
    held.wait_entered().await;
    // The store has written, but has not returned from the refresh's save.
    manager.mark_result(&success(manager));
    drop(held);
    task.await.unwrap().unwrap();
    wait_pending_saves(manager).await;
    *lock(&store.save_hook) = None;
}

#[tokio::test(flavor = "current_thread")]
async fn saved_refresh_intent_does_not_replay_after_concurrent_result() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    refresh_with_result_during_save(&store, &manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    let mut outside = disk(&dir);
    outside["expired"] = json!("2099-03-01T00:00:00Z");
    std::fs::write(
        dir.path().join("codex.json"),
        serde_json::to_vec(&outside).unwrap(),
    )
    .unwrap();
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
    assert!(matches!(
        manager.get("codex.json").unwrap().codex_persist_intent,
        CodexPersistIntent::Configuration
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn failed_refresh_intent_retries_pending_then_does_not_replay() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert!(!live.codex_pending_refresh.is_empty());
    assert!(matches!(
        live.codex_persist_intent,
        CodexPersistIntent::Configuration
    ));
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    let mut outside = disk(&dir);
    outside["expired"] = json!("2099-03-01T00:00:00Z");
    std::fs::write(
        dir.path().join("codex.json"),
        serde_json::to_vec(&outside).unwrap(),
    )
    .unwrap();
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test(flavor = "current_thread")]
async fn reconciliation_without_siwc_does_not_create_empty_partition() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let mut before = fixture();
    before.metadata.remove("siwc");
    before.metadata.remove("use_siwc");
    // An outer login preserves the existing SIWC partition, so replace the
    // synthetic disk fixture directly to start with no partition on either side.
    std::fs::write(
        dir.path().join("codex.json"),
        serde_json::to_vec(&before.metadata).unwrap(),
    )
    .unwrap();
    manager.update_unsaved(before).unwrap();
    refresh_with_result_during_save(&store, &manager).await;
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .metadata
            .contains_key("siwc")
    );
    assert!(disk(&dir).get("siwc").is_none());
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .metadata
            .contains_key("siwc")
    );
    assert!(disk(&dir).get("siwc").is_none());
}

#[tokio::test]
async fn pending_explicit_null_survives_live_removal_and_sparse_refresh() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let mut before = fixture();
    before.metadata.insert("id_token".into(), json!("I0"));
    store.files.save_new(&before).unwrap();
    manager.update_unsaved(before).unwrap();
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata.insert("access_token".into(), json!("A1"));
        after.metadata.insert("id_token".into(), Value::Null);
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata.remove("id_token");
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata.insert("access_token".into(), json!("A2"));
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    manager.force_refresh("codex.json").await.unwrap();
    assert_eq!(disk(&dir)["access_token"], "A2");
    assert_eq!(disk(&dir).get("id_token"), Some(&Value::Null));
    assert_eq!(
        manager.get("codex.json").unwrap().metadata.get("id_token"),
        Some(&Value::Null)
    );
}

fn hold_counter_save(manager: &Manager, count: i64) -> SaveHold {
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.success == count && armed.swap(false, Ordering::SeqCst) {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(5))
                .expect("the counter save is released");
        }
    }));
    SaveHold {
        release: Some(release_tx),
        entered,
    }
}

fn rotation_executor(partial_failure: bool) -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(move |before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("outer-rotated"));
        after
            .metadata
            .insert("refresh_token".into(), json!("outer-refresh-rotated"));
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        after.codex_refresh_failure = partial_failure.then(|| crate::auth::CodexRefreshFailure {
            partition: crate::auth::CodexCredentialPartition::Siwc,
            error: crate::exec::ExecError::upstream(500, "inactive partition refresh failed"),
        });
        Ok(after)
    });
    executor
}

#[tokio::test(flavor = "current_thread")]
async fn skipped_pending_save_keeps_rotation_and_unsaved_edit() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(rotation_executor(false));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let pending = manager
        .get("codex.json")
        .unwrap()
        .codex_pending_refresh
        .clone();
    assert!(!pending.is_empty());
    let held = hold_counter_save(&manager, 1);
    manager.mark_result(&success(&manager));
    held.wait_entered().await;
    set_note(&manager, "unsaved-edit");
    drop(held);
    wait_pending_saves(&manager).await;
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.metadata["access_token"], "outer-rotated");
    assert_eq!(live.codex_pending_refresh, pending);
    assert_eq!(live.metadata["note"], "unsaved-edit");
    assert_eq!(disk(&dir)["access_token"], "outer-old");
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["access_token"], "outer-rotated");
    assert_eq!(disk(&dir)["note"], "unsaved-edit");
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

async fn failed_inactive_refresh(store: &ObservedStore, manager: &Manager) {
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata.insert("use_siwc".into(), json!(false));
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    manager.register_executor(rotation_executor(true));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert!(!live.codex_pending_refresh.is_empty());
    assert_eq!(
        live.codex_refresh_failure.as_ref().unwrap().partition,
        crate::auth::CodexCredentialPartition::Siwc
    );
    assert!(live.next_refresh_after.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn pending_retry_preserves_inactive_partition_failure_and_backoff() {
    let (_dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    failed_inactive_refresh(&store, &manager).await;
    let backoff = manager.get("codex.json").unwrap().next_refresh_after;
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    let live = manager.get("codex.json").unwrap();
    assert!(live.codex_pending_refresh.is_empty());
    assert_eq!(
        live.codex_refresh_failure.as_ref().unwrap().partition,
        crate::auth::CodexCredentialPartition::Siwc
    );
    assert_eq!(live.next_refresh_after, backoff);
    assert_eq!(live.siwc_refresh_due_at(backoff.unwrap()), backoff);
}

#[tokio::test(flavor = "current_thread")]
async fn configuration_completion_preserves_newer_partition_failure() {
    let (_dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.reconcile_hook) = Some(Arc::new(move |auth| {
        if auth.metadata.get("note") == Some(&json!("path-b"))
            && armed.swap(false, Ordering::SeqCst)
        {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    }));
    let held = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let editing = manager.clone();
    let edit = tokio::spawn(async move {
        editing
            .run_persistence(|manager| {
                manager.edit("codex.json", |auth| {
                    auth.metadata.insert("note".into(), json!("path-b"));
                    Ok::<_, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    });
    held.wait_entered().await;
    failed_inactive_refresh(&store, &manager).await;
    let backoff = manager.get("codex.json").unwrap().next_refresh_after;
    drop(held);
    edit.await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(
        live.codex_refresh_failure.as_ref().unwrap().partition,
        crate::auth::CodexCredentialPartition::Siwc
    );
    assert_eq!(live.next_refresh_after, backoff);
}

#[tokio::test(flavor = "current_thread")]
async fn outer_only_refresh_preserves_unattempted_siwc_failure() {
    let (_dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    failed_inactive_refresh(&store, &manager).await;
    let backoff = manager.get("codex.json").unwrap().next_refresh_after;
    let executor = expiry_executor();
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        after.codex_refresh_failure = None;
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    manager.force_refresh("codex.json").await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(
        live.codex_refresh_failure.as_ref().unwrap().partition,
        crate::auth::CodexCredentialPartition::Siwc
    );
    assert_eq!(live.next_refresh_after, backoff);
}

fn outside_expiry(dir: &tempfile::TempDir) {
    let mut outside = disk(dir);
    outside["expired"] = json!("2099-03-01T00:00:00Z");
    std::fs::write(
        dir.path().join("codex.json"),
        serde_json::to_vec(&outside).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn queued_refresh_snapshot_does_not_replay_consumed_patch() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&store.save_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && armed.swap(false, Ordering::SeqCst)
        {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    }));
    let refresh_hold = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let refreshing = manager.clone();
    let refresh = tokio::spawn(async move { refreshing.force_refresh("codex.json").await });
    refresh_hold.wait_entered().await;
    let counter_hold = hold_counter_save(&manager, 1);
    manager.mark_result(&success(&manager));
    counter_hold.wait_entered().await;
    drop(refresh_hold);
    refresh.await.unwrap().unwrap();
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    outside_expiry(&dir);
    drop(counter_hold);
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test(flavor = "current_thread")]
async fn queued_pending_snapshot_does_not_replay_consumed_patch() {
    let (dir, store, manager, _gate) = disk_manager();
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let held = hold_counter_save(&manager, 2);
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&store.save_hook) = Some(Arc::new(move |auth| {
        if auth.success == 1 && armed.swap(false, Ordering::SeqCst) {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    }));
    let first_hold = SaveHold {
        release: Some(release_tx),
        entered,
    };
    manager.mark_result(&success(&manager));
    first_hold.wait_entered().await;
    let first_generation = manager.get("codex.json").unwrap().generation;
    manager.mark_result(&success(&manager));
    held.wait_entered().await;
    drop(first_hold);
    store.wait_saved(first_generation).await;
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    outside_expiry(&dir);
    drop(held);
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test(flavor = "current_thread")]
async fn pending_reconciliation_without_siwc_does_not_create_empty_partition() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let mut before = fixture();
    before.metadata.remove("siwc");
    before.metadata.remove("use_siwc");
    std::fs::write(
        dir.path().join("codex.json"),
        serde_json::to_vec(&before.metadata).unwrap(),
    )
    .unwrap();
    manager.update_unsaved(before).unwrap();
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert!(
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    let live = manager.get("codex.json").unwrap();
    assert!(live.codex_pending_refresh.is_empty());
    assert!(!live.metadata.contains_key("siwc"));
    assert!(disk(&dir).get("siwc").is_none());
}

// Fix 9: completion belongs to an operation, not its token values.
fn hold_matching_save(
    manager: &Manager,
    predicate: impl Fn(&Auth) -> bool + Send + Sync + 'static,
) -> SaveHold {
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if predicate(auth) && armed.swap(false, Ordering::SeqCst) {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("save released");
        }
    }));
    SaveHold {
        release: Some(release_tx),
        entered,
    }
}

fn siwc_rotation_executor() -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after.metadata["siwc"]["access_token"] = json!("siwc-new");
        after.metadata["siwc"]["refresh_token"] = json!("siwc-refresh-new");
        after.codex_refresh_failure = None;
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            false,
            true,
        ));
        Ok(after)
    });
    executor
}

#[tokio::test(flavor = "current_thread")]
async fn pending_siwc_success_does_not_clear_newer_siwc_failure() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(siwc_rotation_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    failed_inactive_refresh(&store, &manager).await;
    let backoff = manager.get("codex.json").unwrap().next_refresh_after;
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        after.codex_refresh_failure = None;
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    manager.force_refresh("codex.json").await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(
        live.codex_refresh_failure
            .as_ref()
            .map(|failure| failure.partition),
        Some(crate::auth::CodexCredentialPartition::Siwc)
    );
    assert_eq!(live.next_refresh_after, backoff);
    let now = manager.now();
    assert_eq!(live.siwc_refresh_due_at(now), Some(now));
    assert_eq!(disk(&dir)["siwc"]["refresh_token"], "siwc-refresh-new");
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
}

#[tokio::test(flavor = "multi_thread")]
async fn consumed_pending_outer_is_not_embedded_in_later_siwc_refresh() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault is on the unleased refresh save, so the first refresh gets no
    // lease; with a lease its commit would land the patch and leave no pending
    // operation. The SIWC refresh below runs leased.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert!(
        !store.fail_refresh.load(Ordering::SeqCst),
        "the refresh save fault fired"
    );
    let pending = manager
        .get("codex.json")
        .unwrap()
        .codex_pending_refresh
        .clone();
    assert_eq!(pending.len(), 1, "an outer operation is pending");
    assert!(!pending[0].is_written(), "and it is not written");
    store.lease_unsupported.store(false, Ordering::SeqCst);
    let mut counter = (*manager.get("codex.json").unwrap()).clone();
    counter.success = 1;
    // The counter save writes and consumes the outer patch. Check (vi) holds
    // the refresh mutex through that write, and the save is held after it.
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    *lock(&store.save_hook) = Some(Arc::new(move |auth| {
        if auth.success == 1 {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("counter save released");
        }
    }));
    let hold = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let saving = manager.clone();
    let counter_task = tokio::task::spawn_blocking(move || {
        saving.persist(
            &counter,
            counter.registration_epoch,
            counter.generation,
            0,
            crate::manager::lifecycle::Save::Yes,
        )
    });
    hold.wait_entered().await;
    // The SIWC refresh starts now and waits on the refresh mutex.
    manager.register_executor(siwc_rotation_executor());
    let refreshing = manager.clone();
    let refresh_task = tokio::spawn(async move { refreshing.force_refresh("codex.json").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!refresh_task.is_finished());
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    outside_expiry(&dir);
    drop(hold);
    assert!(matches!(
        counter_task.await.unwrap().unwrap(),
        super::super::Persisted::Written { .. }
    ));
    refresh_task.await.unwrap().unwrap();
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
    assert_eq!(disk(&dir)["siwc"]["refresh_token"], "siwc-refresh-new");
}

#[tokio::test(flavor = "current_thread")]
async fn stale_counter_save_parked_across_siwc_refresh_is_deferred() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let mut counter = (*manager.get("codex.json").unwrap()).clone();
    counter.success = 1;
    let counter_hold = hold_matching_save(&manager, |auth| auth.success == 1);
    let saving = manager.clone();
    let counter_task = tokio::task::spawn_blocking(move || {
        saving.persist(
            &counter,
            counter.registration_epoch,
            counter.generation,
            0,
            crate::manager::lifecycle::Save::Yes,
        )
    });
    counter_hold.wait_entered().await;
    let refresh_hold = hold_matching_save(&manager, |auth| {
        matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
    });
    manager.register_executor(siwc_rotation_executor());
    let refreshing = manager.clone();
    let refresh_task = tokio::spawn(async move { refreshing.force_refresh("codex.json").await });
    refresh_hold.wait_entered().await;
    drop(counter_hold);
    // Check (vi) refuses the counter save while the pass is in flight. The
    // pass's tail writes the pending outer patch, and step 14 replays the
    // counter once the pass ends.
    assert!(matches!(
        counter_task.await.unwrap().unwrap(),
        super::super::Persisted::Deferred(crate::auth::SaveDeferral::RefreshInFlight)
    ));
    assert_eq!(disk(&dir)["expired"], "2099-01-01T00:00:00Z");
    outside_expiry(&dir);
    drop(refresh_hold);
    refresh_task.await.unwrap().unwrap();
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    assert_eq!(disk(&dir)["siwc"]["refresh_token"], "siwc-refresh-new");
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_refresh_snapshot_remains_written_after_seventeen_refreshes() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    let captured = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    *lock(&store.save_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && lock(&capture).is_none()
        {
            *lock(&capture) = Some(auth.clone());
        }
    }));
    manager.register_executor(expiry_executor());
    manager.force_refresh("codex.json").await.unwrap();
    let mut delayed = lock(&captured).take().unwrap();
    *lock(&store.save_hook) = None;
    delayed.metadata.insert("note".into(), json!("older"));
    let held = hold_matching_save(&manager, |auth| {
        auth.metadata.get("note") == Some(&json!("older"))
    });
    let saving = manager.clone();
    let task = tokio::task::spawn_blocking(move || {
        saving.persist(
            &delayed,
            delayed.registration_epoch,
            delayed.generation,
            0,
            crate::manager::lifecycle::Save::Yes,
        )
    });
    held.wait_entered().await;
    for day in 2..=18 {
        let executor = FakeExecutor::new("codex");
        executor.set_refresh(move |before| {
            let mut after = before.clone();
            after.metadata.insert(
                "expired".into(),
                json!(format!("2099-04-{day:02}T00:00:00Z")),
            );
            after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                &before.metadata,
                &after.metadata,
                true,
                false,
            ));
            Ok(after)
        });
        manager.register_executor(executor);
        manager.force_refresh("codex.json").await.unwrap();
    }
    drop(held);
    assert_eq!(
        task.await.unwrap().unwrap(),
        super::super::Persisted::Skipped
    );
    assert_eq!(disk(&dir)["expired"], "2099-04-18T00:00:00Z");
}

#[tokio::test]
async fn independent_equal_valued_refresh_is_written() {
    let (dir, _store, manager, _gate) = disk_manager();
    manager.register_executor(expiry_executor());
    manager.force_refresh("codex.json").await.unwrap();
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    let original = fixture();
    std::fs::write(
        dir.path().join("codex.json"),
        serde_json::to_vec(&original.metadata).unwrap(),
    )
    .unwrap();
    manager.update_unsaved(original).unwrap();
    manager.force_refresh("codex.json").await.unwrap();
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
}

#[test]
fn clones_racing_to_save_write_one_refresh_and_one_configuration() {
    let (dir, store, manager, _gate) = disk_manager();
    let mut snapshot = (*manager.get("codex.json").unwrap()).clone();
    let before = snapshot.metadata.clone();
    snapshot
        .metadata
        .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
    snapshot.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before,
        &snapshot.metadata,
        true,
        false,
    ));
    let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let configurations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_refreshes = refreshes.clone();
    let observed_configurations = configurations.clone();
    *lock(&store.save_hook) = Some(Arc::new(move |auth| match auth.codex_persist_intent {
        CodexPersistIntent::Refresh(_) => {
            observed_refreshes.fetch_add(1, Ordering::SeqCst);
        }
        CodexPersistIntent::Configuration => {
            observed_configurations.fetch_add(1, Ordering::SeqCst);
        }
        _ => panic!("unexpected save intent"),
    }));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let manager = manager.clone();
            let snapshot = snapshot.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                manager
                    .persist(
                        &snapshot,
                        snapshot.registration_epoch,
                        snapshot.generation,
                        0,
                        crate::manager::lifecycle::Save::Yes,
                    )
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    for task in tasks {
        assert!(matches!(
            task.join().unwrap(),
            super::super::Persisted::Written { .. }
        ));
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(configurations.load(Ordering::SeqCst), 1);
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
}

#[tokio::test]
async fn outer_only_refresh_after_expired_siwc_backoff_keeps_later_time() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    failed_inactive_refresh(&store, &manager).await;
    let now = manager.now();
    let expired = now - chrono::TimeDelta::seconds(1);
    {
        let mut state = manager.lock();
        Arc::make_mut(&mut state.auths.get_mut("codex.json").unwrap().auth).next_refresh_after =
            Some(expired);
    }
    let later = now + chrono::TimeDelta::minutes(1);
    let before = manager.get("codex.json").unwrap();
    let mut after = (*before).clone();
    after
        .metadata
        .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
    after.next_refresh_after = Some(later);
    after.codex_refresh_failure = None;
    after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &after.metadata,
        true,
        false,
    ));
    manager
        .run_persistence(move |manager| {
            manager.update_refreshed(&before, before.registration_epoch, after)
        })
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        manager.get("codex.json").unwrap().next_refresh_after,
        Some(later)
    );
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
}

#[tokio::test(flavor = "current_thread")]
async fn edit_writes_published_rotation_when_its_own_save_is_skipped() {
    let (dir, store, manager, _gate) = disk_manager();
    manager.register_executor(rotation_executor(false));
    let held = hold_matching_save(&manager, |auth| {
        matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
    });
    let refreshing = manager.clone();
    let refresh = tokio::spawn(async move { refreshing.force_refresh("codex.json").await });
    held.wait_entered().await;
    manager
        .run_persistence(|manager| {
            manager.edit("codex.json", |auth| {
                auth.metadata
                    .insert("note".into(), json!("edit-during-refresh"));
                Ok::<_, ()>(true)
            })
        })
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    store.same_record_error.store(true, Ordering::SeqCst);
    drop(held);
    refresh.await.unwrap().unwrap();
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-rotated");
    assert_eq!(disk(&dir)["note"], "edit-during-refresh");
}

fn partition_rotation_executor(outer: bool, label: &'static str) -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(move |before| {
        let mut after = before.clone();
        let partition = if outer {
            &mut after.metadata
        } else {
            after
                .metadata
                .get_mut("siwc")
                .unwrap()
                .as_object_mut()
                .unwrap()
        };
        partition.insert("access_token".into(), json!(format!("{label}-access")));
        partition.insert("refresh_token".into(), json!(format!("{label}-refresh")));
        after.codex_refresh_failure = None;
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            outer,
            !outer,
        ));
        Ok(after)
    });
    executor
}

async fn independent_pending_partition_survives_replace(outer_first: bool) {
    let (dir, store, manager, _gate) = disk_manager();
    for (outer, label) in [
        (outer_first, "independent-p"),
        (!outer_first, "independent-q"),
    ] {
        manager.register_executor(partition_rotation_executor(outer, label));
        store.fail_refresh.store(true, Ordering::SeqCst);
        manager.force_refresh("codex.json").await.unwrap();
    }
    let mut replacement = (*manager.get("codex.json").unwrap()).clone();
    let changed = if outer_first {
        replacement
            .metadata
            .get_mut("siwc")
            .unwrap()
            .as_object_mut()
            .unwrap()
    } else {
        &mut replacement.metadata
    };
    changed.insert("access_token".into(), json!("management-explicit-access"));
    changed.insert("refresh_token".into(), json!("management-explicit-refresh"));
    manager
        .run_persistence(move |manager| manager.update(replacement))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = disk(&dir);
    let independent = if outer_first { &saved } else { &saved["siwc"] };
    assert_eq!(independent["refresh_token"], "independent-p-refresh");
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

#[tokio::test]
async fn pending_outer_survives_later_siwc_rotation_and_management_replace() {
    independent_pending_partition_survives_replace(true).await;
}

#[tokio::test]
async fn pending_siwc_survives_later_outer_rotation_and_management_replace() {
    independent_pending_partition_survives_replace(false).await;
}

fn later_expiry_executor() -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("expired".into(), json!("2099-03-01T00:00:00Z"));
        after.codex_refresh_failure = None;
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
        Ok(after)
    });
    executor
}

#[tokio::test(flavor = "current_thread")]
async fn older_pending_snapshot_does_not_reconcile_over_newer_expiry_rotation() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let held = hold_counter_save(&manager, 1);
    manager.mark_result(&success(&manager));
    held.wait_entered().await;
    manager.register_executor(later_expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    drop(held);
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.metadata["expired"], "2099-03-01T00:00:00Z");
    assert!(
        live.codex_pending_refresh
            .iter()
            .any(|operation| !operation.is_written())
    );
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test(flavor = "current_thread")]
async fn written_edit_snapshot_keeps_rotation_attached_after_its_save() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.reconcile_hook) = Some(Arc::new(move |auth| {
        if auth.metadata.get("note") == Some(&json!("held-edit"))
            && armed.swap(false, Ordering::SeqCst)
        {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("edit completion released");
        }
    }));
    let held = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let editing = manager.clone();
    let edit = tokio::spawn(async move {
        editing
            .run_persistence(|manager| {
                manager.edit("codex.json", |auth| {
                    auth.metadata.insert("note".into(), json!("held-edit"));
                    Ok::<_, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    });
    held.wait_entered().await;
    manager.register_executor(later_expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    drop(held);
    edit.await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.metadata["expired"], "2099-03-01T00:00:00Z");
    assert_eq!(live.codex_pending_refresh.len(), 1);
    assert!(!live.codex_pending_refresh[0].is_written());
    assert_eq!(disk(&dir)["expired"], "2099-02-01T00:00:00Z");
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stale_generation_refresh_folds_into_pending_for_later_save() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "skipped"));
    let held = hold_matching_save(&manager, |auth| {
        matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
    });
    let refreshing = manager.clone();
    let task = tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    held.wait_entered().await;
    // A counter-only generation advances without changing the live operation,
    // and unsaved adoption records that newer generation in the watermark.
    let adopted = {
        let mut state = manager.lock();
        let entry = state.auths.get_mut("codex.json").unwrap();
        Arc::make_mut(&mut entry.auth).generation += 1;
        entry.auth.clone()
    };
    assert_eq!(
        manager
            .persist(
                &adopted,
                adopted.registration_epoch,
                adopted.generation,
                0,
                crate::manager::lifecycle::Save::No
            )
            .unwrap(),
        super::super::Persisted::Skipped
    );
    store.same_record_error.store(true, Ordering::SeqCst);
    drop(held);
    task.await.unwrap();
    store.same_record_error.store(false, Ordering::SeqCst);
    let live = manager.get("codex.json").unwrap();
    assert!(matches!(
        live.codex_persist_intent,
        CodexPersistIntent::Configuration
    ));
    assert_eq!(live.codex_pending_refresh.len(), 1);
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["refresh_token"], "skipped-refresh");
}

#[tokio::test]
async fn load_discards_pending_work_and_requires_quiescent_initialization() {
    let (_dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "before-load"));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    assert_eq!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .len(),
        1
    );
    manager.load().unwrap();
    let loaded = manager.get("codex.json").unwrap();
    assert!(loaded.codex_pending_refresh.is_empty());
    assert!(matches!(
        loaded.codex_persist_intent,
        CodexPersistIntent::Configuration
    ));
    assert_eq!(loaded.metadata["refresh_token"], "outer-refresh-old");
}

#[tokio::test]
async fn superseding_dual_rotation_keeps_outer_chain_after_explicit_siwc_edit() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "chain-b"));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("chain-c-access"));
        after
            .metadata
            .insert("refresh_token".into(), json!("chain-c-refresh"));
        after.metadata["siwc"]["access_token"] = json!("chain-y-access");
        after.metadata["siwc"]["refresh_token"] = json!("chain-y-refresh");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            true,
        ));
        Ok(after)
    });
    manager.register_executor(executor);
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let original = manager.get("codex.json").unwrap().codex_pending_refresh[1].clone();
    let edited = manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata["siwc"]["access_token"] = json!("explicit-siwc");
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert_eq!(edited.codex_pending_refresh.len(), 2);
    let retained = &edited.codex_pending_refresh[1];
    assert!(retained.same_operation(&original));
    assert!(retained.includes_partition(crate::auth::CodexCredentialPartition::Outer));
    assert!(!retained.includes_partition(crate::auth::CodexCredentialPartition::Siwc));
    let files = store.files.clone();
    *lock(&store.save_hook) = Some(Arc::new(move |_| {
        let written = files.list().unwrap().remove(0);
        assert_eq!(
            written.metadata["refresh_token"], "chain-c-refresh",
            "never persist the consumed intermediate rotation"
        );
    }));
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["refresh_token"], "chain-c-refresh");
    assert!(
        original.is_written(),
        "the pruned operation retains shared consumption identity"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn edit_completion_does_not_reconcile_over_refresh_waiting_on_save_lock() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let (edit_tx, edit_rx) = mpsc::channel();
    let edit_rx = Mutex::new(edit_rx);
    let edit_entered = Arc::new(Notify::new());
    let notify = edit_entered.clone();
    // The edit is held where its write is done and check (vi)'s hold is
    // released, still inside the save mutex the refresh will wait on.
    *lock(&manager.shared.unleased_saved_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Configuration)
            && auth.metadata.get("note") == Some(&json!("holding-save-lock"))
        {
            notify.notify_one();
            lock(&edit_rx)
                .recv_timeout(Duration::from_secs(60))
                .unwrap();
        }
    }));
    let edit_hold = SaveHold {
        release: Some(edit_tx),
        entered: edit_entered,
    };
    let (new_tx, new_rx) = mpsc::channel();
    let new_rx = Mutex::new(new_rx);
    let new_entered = Arc::new(Notify::new());
    let notify = new_entered.clone();
    *lock(&store.before_save_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
            lock(&new_rx).recv_timeout(Duration::from_secs(60)).unwrap();
        }
    }));
    let new_hold = SaveHold {
        release: Some(new_tx),
        entered: new_entered,
    };
    let published = Arc::new(Notify::new());
    let notify = published.clone();
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
        }
    }));
    let editing = manager.clone();
    let edit = tokio::spawn(async move {
        editing
            .run_persistence(|manager| {
                manager.edit("codex.json", |auth| {
                    auth.metadata
                        .insert("note".into(), json!("holding-save-lock"));
                    Ok::<_, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    });
    edit_hold.wait_entered().await;
    manager.register_executor(later_expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    tokio::time::timeout(Duration::from_secs(60), published.notified())
        .await
        .unwrap();
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["expired"],
        "2099-03-01T00:00:00Z"
    );
    drop(edit_hold);
    new_hold.wait_entered().await;
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["expired"],
        "2099-03-01T00:00:00Z"
    );
    assert_eq!(
        disk(&dir)["expired"],
        "2099-02-01T00:00:00Z",
        "the newer refresh has not written yet"
    );
    drop(new_hold);
    refresh.await.unwrap();
    // Read-back waits for the save mutex too: release Q before joining E.
    edit.await.unwrap();
    *lock(&manager.shared.unleased_saved_hook) = None;
    *lock(&store.before_save_hook) = None;
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test]
async fn permanently_ineligible_refreshes_do_not_accumulate_pending_operations() {
    for storeless in [false, true] {
        let (_dir, _store, disk_manager, _gate) = disk_manager();
        let manager = if storeless {
            let manager = Manager::new(Settings::default(), Arc::new(FakeModels::default()), None);
            manager.register_unsaved(fixture()).unwrap();
            manager
        } else {
            disk_manager
                .edit_unsaved("codex.json", |auth| {
                    auth.attributes.insert("runtime_only".into(), "true".into());
                    Ok::<_, ()>(true)
                })
                .unwrap();
            disk_manager
        };
        manager.register_executor(expiry_executor());
        for _ in 0..24 {
            manager.force_refresh("codex.json").await.unwrap();
            let live = manager.get("codex.json").unwrap();
            assert!(live.codex_pending_refresh.is_empty());
            assert!(matches!(
                live.codex_persist_intent,
                CodexPersistIntent::Configuration
            ));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_older_save_does_not_clear_newer_refresh_intent() {
    let (dir, store, manager, _gate) = disk_manager();
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    let (old_tx, old_rx) = mpsc::channel();
    let old_rx = Mutex::new(old_rx);
    let old_entered = Arc::new(Notify::new());
    let notify = old_entered.clone();
    *lock(&store.before_save_hook) = Some(Arc::new(move |auth| {
        if auth.metadata["expired"] == "2099-02-01T00:00:00Z" {
            notify.notify_one();
            lock(&old_rx).recv_timeout(Duration::from_secs(60)).unwrap();
        }
    }));
    let old_hold = SaveHold {
        release: Some(old_tx),
        entered: old_entered,
    };
    let refreshing = manager.clone();
    let old = tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    old_hold.wait_entered().await;
    let new_hold = hold_matching_save(&manager, |auth| {
        auth.metadata["expired"] == "2099-03-01T00:00:00Z"
    });
    let base = manager.get("codex.json").unwrap();
    let mut updated = (*base).clone();
    updated
        .metadata
        .insert("expired".into(), json!("2099-03-01T00:00:00Z"));
    updated.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &base.metadata,
        &updated.metadata,
        true,
        false,
    ));
    let refreshing = manager.clone();
    let new = tokio::spawn(async move {
        refreshing
            .run_persistence(move |manager| {
                manager.update_refreshed(&base, base.registration_epoch, updated)
            })
            .await
            .unwrap()
            .unwrap()
    });
    new_hold.wait_entered().await;
    let operation = manager
        .get("codex.json")
        .unwrap()
        .codex_persist_intent
        .clone();
    drop(old_hold);
    old.await.unwrap();
    assert!(matches!(
        manager.get("codex.json").unwrap().codex_persist_intent,
        CodexPersistIntent::Refresh(_)
    ));
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_persist_intent
            .same_operation(&operation)
    );
    store.fail_refresh.store(true, Ordering::SeqCst);
    drop(new_hold);
    new.await.unwrap();
    *lock(&store.before_save_hook) = None;
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test]
async fn same_record_adoption_rebases_recorded_baselines_and_prunes_new_logins() {
    for baseline in ["disk", "before", "intermediate", "new-login"] {
        let (dir, _store, manager, _gate) = disk_manager();
        let disk_base = fixture();
        let mut before = fixture();
        before
            .metadata
            .insert("access_token".into(), json!("memory-before"));
        before
            .metadata
            .insert("refresh_token".into(), json!("memory-before-refresh"));
        let mut middle = before.clone();
        middle
            .metadata
            .insert("access_token".into(), json!("middle"));
        middle
            .metadata
            .insert("refresh_token".into(), json!("middle-refresh"));
        let older = CredentialRefreshPatch::with_disk_baseline(
            &before.metadata,
            Some(&disk_base.metadata),
            &middle.metadata,
            true,
            false,
        );
        let mut after = middle.clone();
        after
            .metadata
            .insert("access_token".into(), json!("latest"));
        after
            .metadata
            .insert("refresh_token".into(), json!("latest-refresh"));
        let operation = CredentialRefreshPatch::new(&middle.metadata, &after.metadata, true, false)
            .with_pending(Some(&older));
        let path = dir.path().join("codex.json").to_string_lossy().into_owned();
        after.attributes.insert("path".into(), path.clone());
        after.file_name = "codex.json".into();
        after.codex_pending_refresh = vec![operation.clone()];
        manager.register_unsaved(after).unwrap();
        let mut incoming = match baseline {
            "disk" => disk_base,
            "before" => before,
            "intermediate" => middle,
            _ => {
                let mut auth = fixture();
                auth.metadata
                    .insert("access_token".into(), json!("new-login"));
                auth.metadata
                    .insert("refresh_token".into(), json!("new-login-refresh"));
                auth
            }
        };
        incoming
            .metadata
            .insert("note".into(), json!("watcher-setting"));
        incoming.attributes.insert("path".into(), path);
        incoming.file_name = "different-basename.json".into();
        incoming.generation = 0;
        incoming.registration_epoch = 0;
        std::fs::write(
            dir.path().join("codex.json"),
            serde_json::to_vec(&incoming.metadata).unwrap(),
        )
        .unwrap();
        let live = manager.update_unsaved(incoming).unwrap().unwrap();
        assert_eq!(live.metadata["note"], "watcher-setting");
        if baseline == "new-login" {
            assert_eq!(live.metadata["access_token"], "new-login");
            assert!(live.codex_pending_refresh.is_empty());
            assert!(!operation.is_written());
        } else {
            assert_eq!(
                live.metadata["access_token"], "latest",
                "baseline {baseline}"
            );
            assert_eq!(live.codex_pending_refresh.len(), 1);
            assert!(live.codex_pending_refresh[0].same_operation(&operation));
            manager.mark_result(&success(&manager));
            wait_pending_saves(&manager).await;
            assert_eq!(disk(&dir)["refresh_token"], "latest-refresh");
            assert!(operation.is_written());
        }
    }
}

#[tokio::test]
async fn siwc_bind_adoption_keeps_unwritten_outer_rotation() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    // Production file-synthesized live records and bind results share origins.
    let loaded = store.files.read_back(&fixture()).unwrap().unwrap();
    manager.update_unsaved(loaded).unwrap();
    manager.register_executor(partition_rotation_executor(true, "held"));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let operation = manager.get("codex.json").unwrap().codex_pending_refresh[0].clone();
    let mut siwc = fixture().metadata["siwc"].as_object().unwrap().clone();
    siwc.insert("access_token".into(), json!("bound"));
    siwc.insert("refresh_token".into(), json!("bound-refresh"));
    let bound = store.files.bind_siwc(fixture(), siwc).await.unwrap();
    assert_eq!(bound.metadata["access_token"], "outer-old");
    let live = manager.update_unsaved(bound).unwrap().unwrap();
    assert_eq!(live.metadata["access_token"], "held-access");
    assert_eq!(live.metadata["siwc"]["access_token"], "bound");
    assert_eq!(live.codex_pending_refresh.len(), 1);
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["refresh_token"], "held-refresh");
    assert_eq!(disk(&dir)["siwc"]["refresh_token"], "bound-refresh");
    assert!(operation.is_written());
}

#[tokio::test(flavor = "current_thread")]
async fn read_back_serializes_written_coverage_for_edit_saves() {
    read_back_serializes_written_coverage(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn read_back_serializes_written_coverage_for_detached_saves() {
    read_back_serializes_written_coverage(true).await;
}

async fn read_back_serializes_written_coverage(detached: bool) {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(expiry_executor());
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    set_note(&manager, "read-held");
    let save_lock = lock(&manager.shared.persist_save_locks)
        .entry("codex.json".into())
        .or_default()
        .clone();
    let read_locked = Arc::new(AtomicBool::new(false));
    let captured = read_locked.clone();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.read_back_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Configuration)
            && armed.swap(false, Ordering::SeqCst)
        {
            captured.store(save_lock.try_lock().is_err(), Ordering::SeqCst);
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("read released");
        }
    }));
    let held = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let edit = if detached {
        manager.mark_result(&success(&manager));
        None
    } else {
        let editing = manager.clone();
        Some(tokio::spawn(async move {
            editing
                .run_persistence(|manager| {
                    manager.edit("codex.json", |auth| {
                        auth.metadata.insert("note".into(), json!("read-held"));
                        Ok::<_, ()>(true)
                    })
                })
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        }))
    };
    held.wait_entered().await;
    let published = Arc::new(Notify::new());
    let notify = published.clone();
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
        }
    }));
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    *lock(&manager.shared.reconcile_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("refresh completion released");
        }
    }));
    let completion = SaveHold {
        release: Some(release_tx),
        entered,
    };
    store.skip_refresh_read.store(true, Ordering::SeqCst);
    manager.register_executor(later_expiry_executor());
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), published.notified())
        .await
        .unwrap();
    // The control can write Q before releasing the older captured read.
    // Fixed code holds the mutex, so Q must wait until that read is judged.
    if read_locked.load(Ordering::SeqCst) {
        drop(held);
        completion.wait_entered().await;
    } else {
        completion.wait_entered().await;
        drop(held);
    }
    if let Some(edit) = edit {
        edit.await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.lock().pending_saves.get("codex.json") != Some(&1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("older reconciliation completes while Q is held");
    drop(completion);
    refresh.await.unwrap();
    wait_pending_saves(&manager).await;
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["expired"],
        "2099-03-01T00:00:00Z",
        "detached {detached}"
    );
    assert_eq!(disk(&dir)["expired"], "2099-03-01T00:00:00Z");
}

#[tokio::test(flavor = "current_thread")]
async fn permanently_ineligible_concurrent_refresh_edit_cycles_do_not_retain_work() {
    for storeless in [true, false] {
        let (_dir, _store, disk_manager, _gate) = disk_manager();
        let manager = if storeless {
            let manager = Manager::new(Settings::default(), Arc::new(FakeModels::default()), None);
            manager.register_unsaved(fixture()).unwrap();
            manager
        } else {
            disk_manager
                .edit_unsaved("codex.json", |auth| {
                    auth.attributes.insert("runtime_only".into(), "true".into());
                    Ok::<_, ()>(true)
                })
                .unwrap();
            disk_manager
        };
        manager.register_executor(expiry_executor());
        for _ in 0..24 {
            // Ineligible publication clears its intent under state before the
            // persist seam. The only operation started before this hold is the
            // refresh below; keep the same publication/completion interleaving.
            let expected_generation = manager.get("codex.json").unwrap().generation + 1;
            let held =
                hold_matching_save(&manager, move |auth| auth.generation == expected_generation);
            let refreshing = manager.clone();
            // A store-less refresh persists inline; keep its blocking test
            // hook away from this single-thread runtime.
            let refresh = tokio::task::spawn_blocking(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(refreshing.force_refresh("codex.json"))
                    .unwrap()
            });
            held.wait_entered().await;
            let edit_hold = hold_matching_save(&manager, |auth| {
                matches!(auth.codex_persist_intent, CodexPersistIntent::Configuration)
            });
            let editing = manager.clone();
            let edit = tokio::task::spawn_blocking(move || {
                editing
                    .edit_unsaved("codex.json", |auth| {
                        auth.metadata
                            .insert("note".into(), json!("concurrent-edit"));
                        Ok::<_, ()>(true)
                    })
                    .unwrap()
            });
            edit_hold.wait_entered().await;
            assert!(
                manager
                    .get("codex.json")
                    .unwrap()
                    .codex_pending_refresh
                    .is_empty()
            );
            // Inspect publication before the edit's Ineligible cleanup can
            // hide incorrect folding (the edit-only negative control).
            drop(edit_hold);
            edit.await.unwrap();
            // Ineligible completion must also clear pre-existing pending work
            // when an edit has already changed the live intent to Configuration.
            {
                let mut state = manager.lock();
                let live = Arc::make_mut(&mut state.auths.get_mut("codex.json").unwrap().auth);
                live.codex_pending_refresh.push(CredentialRefreshPatch::new(
                    &live.metadata,
                    &live.metadata,
                    true,
                    false,
                ));
            }
            drop(held);
            refresh.await.unwrap();
            let live = manager.get("codex.json").unwrap();
            assert!(live.codex_pending_refresh.is_empty());
            assert!(matches!(
                live.codex_persist_intent,
                CodexPersistIntent::Configuration
            ));
        }
    }
}

#[tokio::test]
async fn removing_runtime_only_does_not_resurrect_discarded_rotations() {
    let (dir, _store, manager, _gate) = disk_manager();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.attributes.insert("runtime_only".into(), "true".into());
            Ok::<_, ()>(true)
        })
        .unwrap();
    manager.register_executor(partition_rotation_executor(true, "discarded"));
    manager.force_refresh("codex.json").await.unwrap();
    manager
        .run_persistence(|manager| {
            manager.edit("codex.json", |auth| {
                auth.attributes.remove("runtime_only");
                Ok::<_, ()>(true)
            })
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");
}

#[tokio::test(flavor = "current_thread")]
async fn failed_eligible_save_does_not_requeue_after_runtime_only_edit() {
    let (_dir, store, manager, _gate) = disk_manager();
    manager.register_executor(expiry_executor());
    let held = hold_matching_save(&manager, |auth| {
        matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
    });
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    held.wait_entered().await;
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.attributes.insert("runtime_only".into(), "true".into());
            Ok::<_, ()>(true)
        })
        .unwrap();
    store.fail_refresh.store(true, Ordering::SeqCst);
    drop(held);
    refresh.await.unwrap();
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn same_record_adoption_rebases_live_unwritten_refresh_intent() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "published"));
    let held = hold_matching_save(&manager, |auth| {
        matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
    });
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    held.wait_entered().await;
    let operation = match &manager.get("codex.json").unwrap().codex_persist_intent {
        CodexPersistIntent::Refresh(patch) => patch.clone(),
        _ => panic!("published refresh intent"),
    };
    assert!(!operation.is_written());
    let mut incoming = fixture();
    incoming
        .metadata
        .insert("note".into(), json!("watcher-settings"));
    let live = manager.update_unsaved(incoming).unwrap().unwrap();
    assert_eq!(live.metadata["access_token"], "published-access");
    assert_eq!(live.metadata["note"], "watcher-settings");
    assert_eq!(live.codex_pending_refresh.len(), 1);
    assert!(live.codex_pending_refresh[0].same_operation(&operation));
    drop(held);
    refresh.await.unwrap();
    assert_eq!(disk(&dir)["refresh_token"], "published-refresh");
    assert!(operation.is_written());
}

#[tokio::test]
async fn pathless_bind_adoption_uses_resolved_origin() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .attribute("path")
            .is_none()
    );
    manager.register_executor(partition_rotation_executor(true, "pathless"));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let operation = manager.get("codex.json").unwrap().codex_pending_refresh[0].clone();
    let mut siwc = fixture().metadata["siwc"].as_object().unwrap().clone();
    siwc.insert("access_token".into(), json!("bound"));
    siwc.insert("refresh_token".into(), json!("bound-refresh"));
    let bound = store.files.bind_siwc(fixture(), siwc).await.unwrap();
    assert!(bound.attribute("path").is_some());
    let live = manager.update_unsaved(bound).unwrap().unwrap();
    assert_eq!(live.metadata["refresh_token"], "pathless-refresh");
    assert_eq!(live.metadata["siwc"]["refresh_token"], "bound-refresh");
    assert_eq!(live.codex_pending_refresh.len(), 1);
    assert!(live.codex_pending_refresh[0].same_operation(&operation));
    manager.mark_result(&success(&manager));
    wait_pending_saves(&manager).await;
    assert_eq!(disk(&dir)["refresh_token"], "pathless-refresh");
    assert_eq!(disk(&dir)["siwc"]["refresh_token"], "bound-refresh");
    assert!(operation.is_written());
}

#[tokio::test(flavor = "current_thread")]
async fn captured_refresh_is_revoked_after_discard_aba() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "revoked"));
    let held = hold_matching_save(&manager, |auth| {
        matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
    });
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    held.wait_entered().await;
    let snapshot = manager.get("codex.json").unwrap();
    let operation = match &snapshot.codex_persist_intent {
        CodexPersistIntent::Refresh(patch) => patch.clone(),
        _ => panic!("captured refresh intent"),
    };
    for runtime_only in [true, false] {
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes
                    .insert("runtime_only".into(), runtime_only.to_string());
                Ok::<_, ()>(true)
            })
            .unwrap();
    }
    let live = manager.get("codex.json").unwrap();
    assert!(
        operation.can_apply_to(&live.metadata),
        "watermark bypass remains baseline-applicable"
    );
    assert!(store.same_record(&snapshot, &live).unwrap());
    assert!(live.codex_pending_refresh.is_empty());
    assert_eq!(live.attribute("runtime_only"), Some("false"));
    drop(held);
    refresh.await.unwrap();
    assert!(
        lock(&store.writes).is_empty(),
        "revoked save must not reach the store"
    );
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");
    assert!(!operation.is_written());
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_refresh_failure_does_not_requeue_after_discard_aba() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "failed-admitted"));
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    *lock(&store.before_save_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("admitted save released");
        }
    }));
    let held = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    held.wait_entered().await;
    let operation = match &manager.get("codex.json").unwrap().codex_persist_intent {
        CodexPersistIntent::Refresh(patch) => patch.clone(),
        _ => panic!("admitted refresh intent"),
    };
    for runtime_only in [true, false] {
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes
                    .insert("runtime_only".into(), runtime_only.to_string());
                Ok::<_, ()>(true)
            })
            .unwrap();
    }
    assert!(operation.can_apply_to(&manager.get("codex.json").unwrap().metadata));
    *lock(&store.error_once) = Some("admitted save failure".into());
    drop(held);
    refresh.await.unwrap();
    assert!(
        lock(&store.error_once).is_none(),
        "the admitted save actually failed"
    );
    assert!(!operation.is_written());
    let live = manager.get("codex.json").unwrap();
    assert!(
        live.codex_pending_refresh.is_empty(),
        "discarded operation must not be requeued"
    );
    assert!(matches!(
        live.codex_persist_intent,
        CodexPersistIntent::Configuration
    ));
    assert_eq!(disk(&dir)["refresh_token"], "outer-refresh-old");
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_refresh_may_finish_after_discard_aba() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(true, "admitted"));
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    *lock(&manager.shared.persist_admitted_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("admitted save released");
        }
    }));
    let admitted = SaveHold {
        release: Some(release_tx),
        entered,
    };
    let refreshing = manager.clone();
    let refresh =
        tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
    admitted.wait_entered().await;
    let operation = match &manager.get("codex.json").unwrap().codex_persist_intent {
        CodexPersistIntent::Refresh(patch) => patch.clone(),
        _ => panic!("captured refresh intent"),
    };
    for runtime_only in [true, false] {
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes
                    .insert("runtime_only".into(), runtime_only.to_string());
                Ok::<_, ()>(true)
            })
            .unwrap();
    }
    assert!(operation.can_apply_to(&manager.get("codex.json").unwrap().metadata));
    drop(admitted);
    refresh.await.unwrap();
    assert!(
        disk(&dir)["refresh_token"] == "admitted-refresh",
        "admitted rotation reaches disk"
    );
    assert!(
        operation.is_written(),
        "admission is final despite later discard"
    );
    assert!(
        manager.get("codex.json").unwrap().metadata["refresh_token"] == "admitted-refresh",
        "admitted rotation remains live"
    );
    assert!(
        manager.lock().auths["codex.json"].undurable.outer,
        "admitted old epoch cannot clear the newer marker"
    );
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
    *lock(&manager.shared.persist_admitted_hook) = None;
    rotate_partition(&manager, &store, true, "repaired", false);
    assert!(
        !manager.lock().auths["codex.json"].undurable.outer,
        "next verified partition write clears the marker"
    );
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty()
    );
}

// Fresh observations serialize with manager saves; payloads only supply settings.
fn observed(manager: &Manager, files: &FileStore, hint: &Auth) -> crate::manager::AdoptionResult {
    manager
        .adopt_observed(hint, None, |live| {
            files
                .read_for_adoption(live)
                .map_err(crate::manager::ManagerError::Store)
        })
        .unwrap()
}

fn outer_rotation(before: &Auth, name: &str) -> Auth {
    let mut after = before.clone();
    after
        .metadata
        .insert("access_token".into(), json!(format!("{name}-access")));
    after
        .metadata
        .insert("refresh_token".into(), json!(format!("{name}-refresh")));
    after
        .codex_pending_refresh
        .push(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            true,
            false,
        ));
    after
}

#[test]
fn fresh_observation_beats_stale_hint_and_repairs_pending_rotation() {
    let (dir, store, manager, _gate) = disk_manager();
    let stale = fixture();
    let mut written = outer_rotation(&stale, "written");
    written.codex_pending_refresh.clear();
    store
        .files
        .write_file(
            "codex.json",
            &serde_json::to_vec(&written.metadata).unwrap(),
        )
        .unwrap();
    let rotated = outer_rotation(&written, "unwritten");
    let operation = rotated.codex_pending_refresh[0].clone();
    manager.register_unsaved(rotated).unwrap();
    let read_revision = crate::config::next_revision();
    let result = observed(&manager, &store.files, &stale);
    let crate::manager::AdoptionResult::Published { auth, revision } = result else {
        panic!("fresh read published");
    };
    assert!(revision > read_revision, "fresh read owns a newer revision");
    assert!(
        auth.metadata["access_token"] == "unwritten-access",
        "fresh read retains C"
    );
    assert!(
        auth.codex_pending_refresh
            .iter()
            .any(|pending| pending.same_operation(&operation)),
        "Q remains pending"
    );
    assert!(
        disk(&dir)["access_token"] == "written-access",
        "disk remains B before retry"
    );
    manager
        .edit("codex.json", |auth| {
            auth.metadata.insert("note".into(), json!("retry"));
            Ok::<_, ()>(true)
        })
        .unwrap();
    assert!(
        disk(&dir)["access_token"] == "unwritten-access",
        "next save repairs disk to C"
    );
}

#[test]
fn delayed_command_results_use_fresh_disk_and_unwritten_successor() {
    for bind in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        let mut completed = fixture();
        if bind {
            completed.metadata["siwc"]["access_token"] = json!("bound-access");
            completed.metadata["siwc"]["refresh_token"] = json!("bound-refresh");
        } else {
            completed.metadata["access_token"] = json!("login-access");
            completed.metadata["refresh_token"] = json!("login-refresh");
        }
        store
            .files
            .write_file(
                "codex.json",
                &serde_json::to_vec(&completed.metadata).unwrap(),
            )
            .unwrap();
        let mut later = outer_rotation(&completed, "later");
        later.codex_pending_refresh.clear();
        store
            .files
            .write_file("codex.json", &serde_json::to_vec(&later.metadata).unwrap())
            .unwrap();
        for pending in [false, true] {
            let live = if pending {
                outer_rotation(&later, "successor")
            } else {
                later.clone()
            };
            manager.register_unsaved(live.clone()).unwrap();
            let result = manager
                .adopt_observed(
                    &completed,
                    manager.capture_token_command(
                        "codex.json",
                        if bind {
                            crate::auth::CodexCredentialPartition::Siwc
                        } else {
                            crate::auth::CodexCredentialPartition::Outer
                        },
                    ),
                    |current| {
                        store
                            .files
                            .read_for_adoption(current)
                            .map_err(crate::manager::ManagerError::Store)
                    },
                )
                .unwrap();
            assert!(
                matches!(result, crate::manager::AdoptionResult::Published { .. }),
                "delayed result publishes fresh disk"
            );
            let actual = manager.get("codex.json").unwrap();
            assert!(
                actual.metadata["access_token"] == live.metadata["access_token"],
                "delayed command cannot restore captured tokens"
            );
            assert!(
                actual.metadata["siwc"] == live.metadata["siwc"],
                "delayed command keeps fresh SIWC"
            );
        }
    }
}

#[test]
fn adoption_excludes_detached_save_for_tokens_and_expiry() {
    for expiry_only in [false, true] {
        let (_dir, store, manager, _gate) = disk_manager();
        let stale = fixture();
        let mut live = if expiry_only {
            stale.clone()
        } else {
            outer_rotation(&stale, "held")
        };
        if expiry_only {
            live.metadata
                .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
            live.codex_pending_refresh.push(CredentialRefreshPatch::new(
                &stale.metadata,
                &live.metadata,
                true,
                false,
            ));
        }
        let expected = live.metadata.clone();
        manager.register_unsaved(live).unwrap();
        let mutex = lock(&manager.shared.persist_save_locks)
            .entry(stale.id.clone())
            .or_default()
            .clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let adopting = manager.clone();
        let files = store.files.clone();
        let hint = stale.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let adoption = std::thread::spawn(move || {
            let result = adopting.adopt_observed(&hint, None, |current| {
                let read = files.read_for_adoption(current).unwrap();
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("adoption released");
                Ok(read)
            });
            done_tx.send(result.is_ok()).unwrap();
        });
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("read acknowledged");
        let (attempt_tx, attempt_rx) = mpsc::channel();
        *lock(&manager.shared.persist_hook) = Some(Arc::new(move |_| {
            let _ = attempt_tx.send(());
        }));
        // mark_result's detached save owns its original mutation admission.
        let saving = manager.clone();
        let save = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    saving.mark_result(&success(&saving));
                    wait_pending_saves(&saving).await;
                });
        });
        attempt_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("detached save attempt acknowledged");
        let excluded = mutex.try_lock().is_err();
        release_tx.send(()).unwrap();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("adoption completes")
        );
        adoption.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !save.is_finished() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            save.is_finished(),
            "detached save owner finishes before join"
        );
        save.join().unwrap();
        *lock(&manager.shared.persist_hook) = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while manager.lock().pending_saves.contains_key("codex.json") && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(excluded, "adoption excludes detached save at the read seam");
        assert!(
            !manager.lock().pending_saves.contains_key("codex.json"),
            "detached save completes"
        );
        let actual = manager.get("codex.json").unwrap();
        assert!(
            actual.metadata["access_token"] == expected["access_token"],
            "no consumed intermediate access token"
        );
        assert!(
            actual.metadata["expired"] == expected["expired"],
            "expiry survives serialized adoption"
        );
    }
}

#[test]
fn adoption_load_and_save_complete_with_owned_threads() {
    let (_dir, store, manager, _gate) = disk_manager();
    let (save_entered_tx, save_entered_rx) = mpsc::channel();
    let (save_release_tx, save_release_rx) = mpsc::channel();
    let save_release_rx = Mutex::new(save_release_rx);
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |_| {
        if armed.swap(false, Ordering::SeqCst) {
            save_entered_tx.send(()).unwrap();
            lock(&save_release_rx)
                .recv_timeout(Duration::from_secs(60))
                .expect("save released");
        }
    }));
    let (done_tx, done_rx) = mpsc::channel();
    let saving = manager.clone();
    let done = done_tx.clone();
    let save = std::thread::spawn(move || {
        let ok = saving
            .edit("codex.json", |auth| {
                auth.metadata.insert("note".into(), json!("saved"));
                Ok::<_, ()>(true)
            })
            .is_ok();
        done.send(ok).unwrap();
    });
    save_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("save admitted to mutation barrier");
    let (read_tx, read_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let adopting = manager.clone();
    let files = store.files.clone();
    let done = done_tx.clone();
    let adoption = std::thread::spawn(move || {
        let result = adopting.adopt_observed(&fixture(), None, |current| {
            let read = files.read_for_adoption(current).unwrap();
            read_tx.send(()).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("read released");
            Ok(read)
        });
        done.send(result.is_ok()).unwrap();
    });
    read_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("adoption admitted before queued load");
    let loading = manager.clone();
    let load = std::thread::spawn(move || {
        done_tx.send(loading.load().is_ok()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock(&manager.shared.load_gate.state).2 == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        lock(&manager.shared.load_gate.state).2 != 0,
        "reload queued acknowledgement"
    );
    save_release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
    let complete = (0..3).all(|_| {
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(false)
    });
    // Negative nested-admission control may deadlock. Leak ONLY these owned
    // threads and their isolated manager on timeout; never block in join.
    if !complete {
        drop((save, adoption, load));
        panic!("owned save adoption and load complete within the bound");
    }
    *lock(&manager.shared.persist_hook) = None;
    for thread in [save, adoption, load] {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(thread.is_finished(), "owned thread exited before join");
        thread.join().unwrap();
    }
}

#[test]
fn adoption_rejects_replacement_registration_and_rebound_origin() {
    for rebind in [false, true] {
        let (dir, store, manager, _gate) = disk_manager();
        let before = manager.get("codex.json").unwrap();
        let replacement = Mutex::new(None);
        let result = manager
            .adopt_observed(&fixture(), None, |current| {
                let read = store.files.read_for_adoption(current).unwrap();
                if rebind {
                    let next = dir.path().join("rebound");
                    std::fs::create_dir(&next).unwrap();
                    store.files.set_base_dir(&next);
                    *lock(&replacement) = Some(manager.get("codex.json").unwrap());
                } else {
                    manager.remove("codex.json");
                    let mut auth = fixture();
                    auth.metadata
                        .insert("note".into(), json!("new-registration"));
                    *lock(&replacement) = Some(manager.register_unsaved(auth).unwrap());
                }
                Ok(read)
            })
            .unwrap();
        let expected = lock(&replacement).clone().unwrap();
        let actual = manager.get("codex.json").unwrap();
        assert!(
            Arc::ptr_eq(&actual, &expected),
            "replacement state is never published into"
        );
        assert!(
            matches!(result, crate::manager::AdoptionResult::RegistrationChanged),
            "registration or binding changed outcome"
        );
        if !rebind {
            assert!(actual.registration_epoch != before.registration_epoch);
        }
    }
}

#[test]
fn adoption_replays_only_unwritten_expiry_operations() {
    for written in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        let before = fixture();
        let mut live = before.clone();
        live.metadata
            .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        let operation = CredentialRefreshPatch::new(&before.metadata, &live.metadata, true, false);
        if written {
            operation.mark_written();
        }
        live.codex_pending_refresh.push(operation.clone());
        manager.register_unsaved(live).unwrap();
        let mut newer = before.clone();
        newer
            .metadata
            .insert("expired".into(), json!("2099-03-01T00:00:00Z"));
        store
            .files
            .write_file("codex.json", &serde_json::to_vec(&newer.metadata).unwrap())
            .unwrap();
        observed(&manager, &store.files, &before);
        let actual = manager.get("codex.json").unwrap();
        let expiry = if written {
            "2099-03-01T00:00:00Z"
        } else {
            "2099-02-01T00:00:00Z"
        };
        assert!(
            actual.metadata["expired"] == expiry,
            "written expiry must not replace E3; unwritten expiry wins"
        );
    }
}

#[test]
fn supported_read_failures_leave_live_and_pending_untouched() {
    let (dir, store, manager, _gate) = disk_manager();
    let rotated = outer_rotation(&fixture(), "pending");
    manager.register_unsaved(rotated).unwrap();
    for contents in [
        Some("null"),
        Some(""),
        Some("{}"),
        Some("{bad"),
        Some("{\"type\":\"gemini\"}"),
        None,
    ] {
        let path = dir.path().join("codex.json");
        match contents {
            Some(bytes) => std::fs::write(&path, bytes).unwrap(),
            None => std::fs::remove_file(&path).unwrap(),
        }
        let before = manager.get("codex.json").unwrap();
        let classification = store.files.read_for_adoption(&before).unwrap();
        assert!(
            matches!(
                classification,
                crate::auth::AdoptionRead::Invalid | crate::auth::AdoptionRead::Missing
            ),
            "requested Codex remains supported when contents are unexpected"
        );
        let result = observed(&manager, &store.files, &fixture());
        assert!(
            matches!(result, crate::manager::AdoptionResult::Retry),
            "supported read failure requests retry"
        );
        assert!(
            Arc::ptr_eq(&before, &manager.get("codex.json").unwrap()),
            "invalid read keeps live and pending untouched"
        );
    }
}

#[test]
fn unsupported_gemini_and_opaque_store_preserve_replacement_compatibility() {
    let (_dir, store, _manager, _gate) = disk_manager();
    let gemini = auth_with_metadata(
        "gemini.json",
        "gemini",
        json!({"type":"gemini", "token":"old"}),
    );
    assert!(
        matches!(
            store.files.read_for_adoption(&gemini).unwrap(),
            crate::auth::AdoptionRead::Unsupported
        ),
        "Gemini capability is unsupported"
    );
    let files: Arc<dyn AuthStore> = store.files.clone();
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(files),
    );
    manager.register_unsaved(gemini.clone()).unwrap();
    let mut incoming = gemini;
    incoming.metadata.insert("token".into(), json!("new"));
    let result = manager
        .adopt_observed(&incoming, None, |live| {
            store
                .files
                .read_for_adoption(live)
                .map_err(crate::manager::ManagerError::Store)
        })
        .unwrap();
    assert!(
        matches!(result, crate::manager::AdoptionResult::Published { .. }),
        "Unsupported is compatibility, not retry"
    );
    assert!(
        manager.get("gemini.json").unwrap().metadata["token"] == "new",
        "Gemini replacement unchanged"
    );
    let opaque = Arc::new(FakeStore::default());
    assert!(
        matches!(
            opaque.read_for_adoption(&fixture()).unwrap(),
            crate::auth::AdoptionRead::Unsupported
        ),
        "opaque default is Unsupported"
    );
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(opaque),
    );
    manager.register_unsaved(fixture()).unwrap();
    let incoming = outer_rotation(&fixture(), "opaque");
    manager.update_unsaved(incoming).unwrap();
    assert!(
        manager.get("codex.json").unwrap().metadata["access_token"] == "opaque-access",
        "opaque incoming tokens retain fix 12 precedence"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejection_adoption_does_not_fall_back_to_consumed_disk_tokens() {
    let (_dir, store, manager, gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    manager.register_executor(partition_rotation_executor(false, "rotated"));
    store.fail_refresh.store(true, Ordering::SeqCst);
    manager.force_refresh("codex.json").await.unwrap();
    let failed = manager.get("codex.json").unwrap();
    assert!(
        failed.codex_pending_refresh.len() == 1,
        "unwritten rotation retained before rejection"
    );
    let reached = Arc::new(AtomicBool::new(false));
    let flag = reached.clone();
    *lock(&manager.shared.adopt_hook) = Some(Arc::new(move || {
        flag.store(true, Ordering::SeqCst);
    }));
    gate.close(gate.epoch()).await.unwrap();
    let adopted = manager
        .try_refresh_after_unauthorized(
            &failed,
            &crate::exec::ExecError::upstream(401, "synthetic rejection"),
            false,
        )
        .await;
    assert!(
        reached.load(Ordering::SeqCst),
        "rejection fallback fresh read acknowledged"
    );
    assert!(adopted.is_none(), "rebased failed token is not an adoption");
    assert!(
        Arc::ptr_eq(&failed, &manager.get("codex.json").unwrap()),
        "consumed disk token never replaces live rotation"
    );
}

#[test]
fn replace_origin_failure_retries_but_ineligible_policy_discards() {
    let (_dir, store, manager, _gate) = disk_manager();
    manager
        .register_unsaved(outer_rotation(&fixture(), "pending"))
        .unwrap();
    let before = manager.get("codex.json").unwrap();
    store.same_record_error.store(true, Ordering::SeqCst);
    let result = manager.adopt_observed(&fixture(), None, |live| {
        store
            .read_for_adoption(live)
            .map_err(crate::manager::ManagerError::Store)
    });
    assert!(
        result.is_ok(),
        "origin resolution failure is not a hard error"
    );
    assert!(
        matches!(result.unwrap(), crate::manager::AdoptionResult::Retry),
        "origin resolution failure requests Retry"
    );
    assert!(
        Arc::ptr_eq(&before, &manager.get("codex.json").unwrap()),
        "origin failure keeps pending work untouched"
    );
    let mut incoming = fixture();
    incoming
        .attributes
        .insert("runtime_only".into(), "true".into());
    incoming
        .metadata
        .insert("note".into(), json!("policy-only"));
    let live = manager.update_unsaved(incoming).unwrap().unwrap();
    assert!(
        live.codex_pending_refresh.is_empty(),
        "ineligible Replace still discards despite unresolved origin"
    );
    assert!(
        live.metadata["access_token"] == before.metadata["access_token"],
        "policy discard keeps rotated live tokens"
    );
    assert!(
        live.metadata["note"] == "policy-only",
        "incoming noncredential fields remain last-writer-wins"
    );
}

// Manager-owned durability regressions. Synthetic credentials never leave the store.
fn rotate_partition(manager: &Manager, store: &ObservedStore, outer: bool, name: &str, fail: bool) {
    let before = manager.get("codex.json").unwrap();
    let mut after = (*before).clone();
    if outer {
        after
            .metadata
            .insert("access_token".into(), json!(format!("{name}-access")));
        after
            .metadata
            .insert("refresh_token".into(), json!(format!("{name}-refresh")));
        after.metadata.remove("expired");
    } else {
        after.metadata["siwc"]["access_token"] = json!(format!("{name}-access"));
        after.metadata["siwc"]["refresh_token"] = json!(format!("{name}-refresh"));
        after.metadata["siwc"]
            .as_object_mut()
            .unwrap()
            .remove("expired");
    }
    after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &after.metadata,
        outer,
        !outer,
    ));
    store.fail_refresh.store(fail, Ordering::SeqCst);
    manager
        .update_refreshed(&before, before.registration_epoch, after)
        .unwrap();
}

fn discard_work(manager: &Manager) {
    for runtime_only in [true, false] {
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes
                    .insert("runtime_only".into(), runtime_only.to_string());
                Ok::<_, ()>(true)
            })
            .unwrap();
    }
}

fn partition(outer: bool) -> crate::auth::CodexCredentialPartition {
    if outer {
        crate::auth::CodexCredentialPartition::Outer
    } else {
        crate::auth::CodexCredentialPartition::Siwc
    }
}

fn partition_token(auth: &Auth, outer: bool) -> &Value {
    if outer {
        &auth.metadata["refresh_token"]
    } else {
        &auth.metadata["siwc"]["refresh_token"]
    }
}

#[test]
fn undurable_observations_and_partition_commands() {
    for outer in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        rotate_partition(&manager, &store, outer, "B", true);
        discard_work(&manager);
        manager
            .edit_unsaved("codex.json", |auth| {
                if outer {
                    auth.metadata.remove("expired");
                } else {
                    auth.metadata["siwc"]
                        .as_object_mut()
                        .unwrap()
                        .remove("expired");
                }
                Ok::<_, ()>(true)
            })
            .unwrap();
        observed(&manager, &store.files, &fixture());
        let live = manager.get("codex.json").unwrap();
        assert!(
            partition_token(&live, outer) == "B-refresh",
            "observation retains live B"
        );
        let expiry = if outer {
            live.metadata.get("expired")
        } else {
            live.metadata["siwc"].get("expired")
        };
        assert!(expiry.is_none(), "marked overlay preserves field absence");
        assert!(
            manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "B marker remains set"
        );
        rotate_partition(&manager, &store, !outer, "independent", true);
        discard_work(&manager);
        rotate_partition(&manager, &store, outer, "C", true);
        let command = manager
            .capture_token_command("codex.json", partition(outer))
            .unwrap();
        let mut login = fixture();
        if outer {
            login.metadata["access_token"] = json!("L-access");
            login.metadata["refresh_token"] = json!("L-refresh");
        } else {
            login.metadata["siwc"]["access_token"] = json!("L-access");
            login.metadata["siwc"]["refresh_token"] = json!("L-refresh");
            login.metadata["use_siwc"] = json!(true);
        }
        store
            .files
            .write_file("codex.json", &serde_json::to_vec(&login.metadata).unwrap())
            .unwrap();
        manager
            .adopt_completed_token_command(&login, command)
            .unwrap();
        let live = manager.get("codex.json").unwrap();
        let marked = manager.lock().auths["codex.json"].undurable;
        assert!(
            !marked.contains(partition(outer)),
            "command clears its own marker"
        );
        assert!(
            partition_token(&live, outer) == "L-refresh",
            "valid command adopts fresh login L"
        );

        assert!(
            marked.contains(partition(!outer)),
            "command cannot clear the other marker"
        );
        assert!(
            partition_token(&live, !outer) == "independent-refresh",
            "command keeps independent live partition"
        );
        rotate_partition(&manager, &store, outer, "M", false);
        observed(&manager, &store.files, &login);
        assert!(
            partition_token(&manager.get("codex.json").unwrap(), outer) == "M-refresh",
            "post-login refresh M survives observation"
        );
    }
}

#[test]
fn opposite_partition_writes_preserve_markers_and_own_write_repairs() {
    for outer in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        rotate_partition(&manager, &store, outer, "protected", true);
        discard_work(&manager);
        rotate_partition(&manager, &store, !outer, "opposite", false);
        assert!(
            partition_token(&manager.get("codex.json").unwrap(), outer) == "protected-refresh",
            "Written overlay keeps opposite protected partition"
        );
        assert!(
            manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "opposite save cannot clear marker"
        );
        rotate_partition(&manager, &store, !outer, "detached", true);
        manager.mark_result(&success(&manager));
        assert!(
            partition_token(&manager.get("codex.json").unwrap(), outer) == "protected-refresh",
            "detached overlay keeps opposite protected partition"
        );
        // A stale observation alone still has no repair authority.
        let live = manager.get("codex.json").unwrap();
        observed(&manager, &store.files, &live);
        assert!(
            manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "raw observation alone cannot clear marker"
        );
        rotate_partition(&manager, &store, outer, "repaired", false);
        assert!(
            !manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "verified own write clears marker"
        );
    }
}

#[test]
fn delayed_and_replaced_command_contexts_are_observations() {
    for replaced in [false, true] {
        let (_dir, store, manager, _gate) = disk_manager();
        if replaced {
            rotate_partition(&manager, &store, true, "previous-registration", true);
            discard_work(&manager);
        }
        let command = manager
            .capture_token_command("codex.json", partition(true))
            .unwrap();
        if replaced {
            manager.register_unsaved(fixture()).unwrap();
        }
        rotate_partition(&manager, &store, true, "newer", true);
        discard_work(&manager);
        let mut login = fixture();
        login.metadata["access_token"] = json!("delayed-access");
        login.metadata["refresh_token"] = json!("delayed-refresh");
        store
            .files
            .write_file("codex.json", &serde_json::to_vec(&login.metadata).unwrap())
            .unwrap();
        manager
            .adopt_completed_token_command(&login, command)
            .unwrap();
        assert!(
            manager.lock().auths["codex.json"].undurable.outer,
            "delayed context cannot clear newer marker"
        );
        assert!(
            manager.get("codex.json").unwrap().metadata["refresh_token"] == "newer-refresh",
            "delayed login cannot restore disk tokens"
        );
    }
}

#[test]
fn no_store_and_opaque_replace_compatibility_with_markers() {
    for opaque in [false, true] {
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            opaque.then(|| Arc::new(FakeStore::default()) as Arc<dyn AuthStore>),
        );
        manager.register_unsaved(fixture()).unwrap();
        manager
            .update_unsaved(outer_rotation(&fixture(), "incoming"))
            .unwrap();
        assert!(
            manager.get("codex.json").unwrap().metadata["refresh_token"] == "incoming-refresh",
            "unmarked compatibility adopts incoming tokens"
        );
        manager
            .register_unsaved(outer_rotation(&fixture(), "incoming"))
            .unwrap();
        discard_work(&manager);
        assert!(
            manager.lock().auths["codex.json"].undurable.outer == opaque,
            "no store discards without a marker"
        );
        let stale = fixture();
        manager.update_unsaved(stale.clone()).unwrap();
        if opaque {
            assert!(
                manager.get("codex.json").unwrap().metadata["refresh_token"] == "incoming-refresh",
                "opaque marked Replace keeps live"
            );
            let command = manager
                .capture_token_command("codex.json", partition(true))
                .unwrap();
            manager
                .adopt_completed_token_command(&stale, command)
                .unwrap();
            assert!(
                !manager.lock().auths["codex.json"].undurable.outer,
                "opaque command clears marker"
            );
        }
        assert!(
            manager.get("codex.json").unwrap().metadata["refresh_token"] == "outer-refresh-old",
            "compatibility or valid command adopts incoming"
        );
    }
}

#[test]
fn runtime_only_replace_keeps_supported_live_partitions() {
    for condition in ["written", "marked", "missing", "invalid", "unreadable"] {
        let (dir, store, manager, _gate) = disk_manager();
        let mut incoming = fixture();
        rotate_partition(
            &manager,
            &store,
            true,
            "policy-outer",
            condition == "marked",
        );
        rotate_partition(
            &manager,
            &store,
            false,
            "policy-siwc",
            condition == "marked",
        );
        match condition {
            "missing" => std::fs::remove_file(dir.path().join("codex.json")).unwrap(),
            "invalid" => {
                store.files.write_file("codex.json", b"null").unwrap();
            }
            "unreadable" => {
                std::fs::remove_file(dir.path().join("codex.json")).unwrap();
                std::fs::create_dir(dir.path().join("codex.json")).unwrap();
            }
            _ => {}
        }
        incoming
            .attributes
            .insert("runtime_only".into(), "true".into());
        incoming.metadata["note"] = json!("incoming-policy");
        let live = manager.update_unsaved(incoming).unwrap().unwrap();
        assert!(
            live.metadata["refresh_token"] == "policy-outer-refresh",
            "supported policy Replace keeps written outer rotation"
        );
        assert!(
            live.metadata["siwc"]["refresh_token"] == "policy-siwc-refresh",
            "supported policy Replace keeps written SIWC rotation"
        );
        assert!(
            live.trimmed_attribute("runtime_only") == "true"
                && live.metadata["note"] == "incoming-policy",
            "policy applies even without a readable record"
        );
        if condition == "marked" {
            let state = manager.lock();
            assert!(
                state.auths["codex.json"].undurable.outer
                    && state.auths["codex.json"].undurable.siwc,
                "policy discard marks both failed partitions"
            );
        }
    }
    let (_dir, _store, manager, _gate) = disk_manager();
    let mut incoming = fixture();
    incoming.provider = "gemini".into();
    incoming.metadata["type"] = json!("gemini");
    manager.register_unsaved(incoming.clone()).unwrap();
    incoming
        .attributes
        .insert("runtime_only".into(), "true".into());
    incoming.metadata["access_token"] = json!("unsupported-incoming");
    let live = manager.update_unsaved(incoming).unwrap().unwrap();
    assert!(
        live.metadata["access_token"] == "unsupported-incoming",
        "Unsupported policy Replace adopts incoming"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn marked_rejection_refuses_different_disk_tokens() {
    for outer in [true, false] {
        let (_dir, store, manager, gate) = disk_manager();
        rotate_partition(&manager, &store, outer, "failed", true);
        discard_work(&manager);
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.metadata["use_siwc"] = json!(!outer);
                Ok::<_, ()>(true)
            })
            .unwrap();
        let failed = manager.get("codex.json").unwrap();
        let mut different = fixture();
        different.metadata["access_token"] = json!("different-outer");
        different.metadata["siwc"]["access_token"] = json!("different-siwc");
        store
            .files
            .write_file(
                "codex.json",
                &serde_json::to_vec(&different.metadata).unwrap(),
            )
            .unwrap();
        let reached = Arc::new(AtomicBool::new(false));
        let flag = reached.clone();
        *lock(&manager.shared.adopt_hook) = Some(Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
        }));
        gate.close(gate.epoch()).await.unwrap();
        let adopted = manager
            .try_refresh_after_unauthorized(
                &failed,
                &crate::exec::ExecError::upstream(401, "synthetic"),
                false,
            )
            .await;
        assert!(
            reached.load(Ordering::SeqCst),
            "marked rejection read was reached"
        );
        assert!(
            adopted.is_none(),
            "marked rejected partition cannot adopt a different disk token"
        );
        assert!(
            Arc::ptr_eq(&failed, &manager.get("codex.json").unwrap()),
            "marked rejection leaves live unchanged"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_rejection_uses_legacy_readback_but_invalid_does_not() {
    for (codex, missing) in [(false, false), (true, false), (true, true)] {
        let (dir, store, manager, gate) = disk_manager();
        let mut auth = fixture();
        if !codex {
            auth.provider = "claude".into();
            auth.metadata["type"] = json!("claude");
        }
        auth.metadata["use_siwc"] = json!(false);
        manager.register_unsaved(auth.clone()).unwrap();
        let mut written = auth.clone();
        written.metadata["access_token"] = json!("legacy-new");
        store
            .files
            .write_file(
                "codex.json",
                if codex {
                    b"null".to_vec()
                } else {
                    serde_json::to_vec(&written.metadata).unwrap()
                }
                .as_slice(),
            )
            .unwrap();
        if missing {
            std::fs::remove_file(dir.path().join("codex.json")).unwrap();
        }
        let failed = manager.get("codex.json").unwrap();
        gate.close(gate.epoch()).await.unwrap();
        let result = manager
            .try_refresh_after_unauthorized(
                &failed,
                &crate::exec::ExecError::upstream(401, "synthetic"),
                false,
            )
            .await;
        if codex {
            assert!(
                result.is_none(),
                "supported invalid record never falls back"
            );
        } else {
            assert!(
                result.is_some(),
                "Unsupported rejection adopts legacy readback"
            );
            assert!(
                manager.get("codex.json").unwrap().metadata["access_token"] == "legacy-new",
                "legacy rejection adopts new token"
            );
        }
    }
}

#[test]
fn empty_save_retains_unwritten_rotation_without_watermark() {
    let (dir, store, manager, _gate) = disk_manager();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.disabled = true;
            Ok::<_, ()>(true)
        })
        .unwrap();
    std::fs::remove_file(dir.path().join("codex.json")).unwrap();
    let watermark = lock(&manager.shared.persist_locks)
        .get("codex.json")
        .cloned()
        .unwrap();
    let before = *lock(&watermark);
    rotate_partition(&manager, &store, true, "not-created", false);
    let live = manager.get("codex.json").unwrap();
    assert!(
        live.codex_pending_refresh.len() == 1 && !live.codex_pending_refresh[0].is_written(),
        "empty save keeps rotation unwritten and pending"
    );
    assert!(
        *lock(&watermark) == before,
        "empty save does not advance watermark"
    );
    assert!(
        !dir.path().join("codex.json").exists(),
        "disabled removed record remains removed"
    );
}

#[test]
fn replace_retry_is_explicit_and_keeps_settings_pending() {
    let (_dir, store, manager, _gate) = disk_manager();
    let before = manager.get("codex.json").unwrap();
    store.files.write_file("codex.json", b"null").unwrap();
    let mut incoming = fixture();
    incoming.metadata["note"] = json!("must-retry-settings");
    let result = manager.update_unsaved(incoming);
    assert!(
        matches!(result, Err(crate::manager::ManagerError::AdoptionRetry)),
        "Replace Retry is explicit rather than apparent success"
    );
    assert!(
        Arc::ptr_eq(&before, &manager.get("codex.json").unwrap()),
        "Replace Retry publishes neither settings nor tokens"
    );
}

#[test]
fn opaque_default_classifies_requested_non_codex_as_unsupported() {
    struct ReadableOpaque;
    impl AuthStore for ReadableOpaque {
        fn list(&self) -> io::Result<Vec<Auth>> {
            Ok(vec![fixture()])
        }
        fn save(&self, auth: &Auth) -> io::Result<String> {
            Ok(auth.id.clone())
        }
        fn delete(&self, _: &str) -> io::Result<()> {
            Ok(())
        }
        fn read_back(&self, _: &Auth) -> io::Result<Option<Auth>> {
            Ok(Some(fixture()))
        }
    }
    let store = ReadableOpaque;
    let requested = auth_with_metadata("gemini.json", "gemini", json!({"type":"gemini"}));
    assert!(
        matches!(
            store.read_for_adoption(&requested).unwrap(),
            crate::auth::AdoptionRead::Unsupported
        ),
        "default capability follows requested non-Codex provider"
    );
    assert!(
        matches!(
            store.read_for_adoption(&fixture()).unwrap(),
            crate::auth::AdoptionRead::Record(_)
        ),
        "opaque Codex readback retains default record compatibility"
    );
}

#[test]
fn resolved_different_record_proof_does_not_rebase() {
    let live = outer_rotation(&fixture(), "unwritten");
    let mut incoming = fixture();
    incoming.file_name = "other.json".into();
    incoming
        .rebase_pending_refreshes_with_origin(&live, Some(crate::auth::ResolvedOriginProof(false)));
    assert!(
        incoming.metadata["refresh_token"] == "outer-refresh-old",
        "resolved different record does not rebase live work"
    );
}

#[test]
fn completed_commands_are_bound_to_capture_store() {
    for outer in [true, false] {
        for rebound in [false, true] {
            let (dir, store, manager, _gate) = disk_manager();
            rotate_partition(&manager, &store, outer, "bound-live", true);
            discard_work(&manager);
            let command = manager
                .capture_token_command("codex.json", partition(outer))
                .unwrap();
            let mut login = fixture();
            if outer {
                login.metadata["access_token"] = json!("bound-login-access");
                login.metadata["refresh_token"] = json!("bound-login-refresh");
            } else {
                login.metadata["siwc"]["access_token"] = json!("bound-login-access");
                login.metadata["siwc"]["refresh_token"] = json!("bound-login-refresh");
            }
            store
                .files
                .write_file("codex.json", &serde_json::to_vec(&login.metadata).unwrap())
                .unwrap();
            let other_dir = tempfile::tempdir_in(dir.path().parent().unwrap()).unwrap();
            if rebound {
                store.files.try_set_base_dir(other_dir.path()).unwrap();
                store
                    .files
                    .write_file(
                        "codex.json",
                        &serde_json::to_vec(&fixture().metadata).unwrap(),
                    )
                    .unwrap();
            }
            manager
                .adopt_completed_token_command(&login, command)
                .unwrap();
            let state = manager.lock();
            let entry = &state.auths["codex.json"];
            assert!(
                entry.undurable.contains(partition(outer)) == rebound,
                "rebound command cannot clear capture-store marker"
            );
            assert!(
                partition_token(&entry.auth, outer)
                    == if rebound {
                        "bound-live-refresh"
                    } else {
                        "bound-login-refresh"
                    },
                "command adopts only its captured binding"
            );
            let baseline = if outer {
                &entry.undurable_baselines.outer
            } else {
                &entry.undurable_baselines.siwc
            };
            assert!(
                !baseline.is_empty() == rebound,
                "command clears only valid capture baseline"
            );
        }
    }
}

#[test]
fn completed_commands_reject_changed_provider() {
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(Arc::new(FakeStore::default())),
    );
    manager
        .register_unsaved(outer_rotation(&fixture(), "provider-live"))
        .unwrap();
    discard_work(&manager);
    let command = manager
        .capture_token_command("codex.json", partition(true))
        .unwrap();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.provider = "gemini".into();
            Ok::<_, ()>(true)
        })
        .unwrap();
    manager
        .adopt_completed_token_command(&fixture(), command)
        .unwrap();
    assert!(
        manager.lock().auths["codex.json"].undurable.outer,
        "changed provider invalidates command authority"
    );
    assert!(
        manager.get("codex.json").unwrap().metadata["refresh_token"] == "provider-live-refresh",
        "provider mismatch keeps protected live partition"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn later_partition_rotation_repairs_discarded_chain() {
    for outer in [true, false] {
        for foreign in [false, true] {
            let (dir, store, manager, _gate) = disk_manager();
            // The fault or hook is on the unleased refresh save, so the store gives no lease.
            store.lease_unsupported.store(true, Ordering::SeqCst);
            for label in ["discard-first", "discard-second"] {
                manager.register_executor(partition_rotation_executor(outer, label));
                store.fail_refresh.store(true, Ordering::SeqCst);
                manager.force_refresh("codex.json").await.unwrap();
                discard_work(&manager);
            }
            {
                let state = manager.lock();
                let entry = &state.auths["codex.json"];
                let baseline = if outer {
                    entry.undurable_baselines.outer.first()
                } else {
                    entry.undurable_baselines.siwc.first()
                }
                .unwrap();
                let original = fixture();
                assert!(
                    baseline.get("refresh_token") == Some(partition_token(&original, outer)),
                    "repeated discard retains earliest durable baseline"
                );
            }
            if foreign {
                let mut other = fixture();
                if outer {
                    other.metadata["access_token"] = json!("foreign-access");
                    other.metadata["refresh_token"] = json!("foreign-refresh");
                } else {
                    other.metadata["siwc"]["access_token"] = json!("foreign-access");
                    other.metadata["siwc"]["refresh_token"] = json!("foreign-refresh");
                }
                store
                    .files
                    .write_file("codex.json", &serde_json::to_vec(&other.metadata).unwrap())
                    .unwrap();
            }
            manager.register_executor(partition_rotation_executor(outer, "repair-final"));
            manager.force_refresh("codex.json").await.unwrap();
            let raw = store.files.read_back(&fixture()).unwrap().unwrap();
            assert!(
                partition_token(&raw, outer)
                    == if foreign {
                        "foreign-refresh"
                    } else {
                        "repair-final-refresh"
                    },
                "later rotation repairs old disk without manual baseline write"
            );
            let state = manager.lock();
            let entry = &state.auths["codex.json"];
            assert!(
                partition_token(&entry.auth, outer) == "repair-final-refresh",
                "repair retains latest live rotation"
            );
            assert!(
                entry.undurable.contains(partition(outer)) == foreign,
                "repair clears marker only when disk matches live"
            );
            let baseline = if outer {
                &entry.undurable_baselines.outer
            } else {
                &entry.undurable_baselines.siwc
            };
            assert!(
                !baseline.is_empty() == foreign,
                "repair clears baseline with marker"
            );
            assert!(
                dir.path().join("codex.json").exists(),
                "repair uses existing credential record"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_and_invalid_rejection_never_calls_legacy_readback() {
    struct ClassifiedStore {
        invalid: bool,
        legacy_calls: std::sync::atomic::AtomicUsize,
    }
    impl AuthStore for ClassifiedStore {
        fn list(&self) -> io::Result<Vec<Auth>> {
            Ok(vec![])
        }
        fn save(&self, auth: &Auth) -> io::Result<String> {
            Ok(auth.id.clone())
        }
        fn delete(&self, _: &str) -> io::Result<()> {
            Ok(())
        }
        fn read_for_adoption(&self, _: &Auth) -> io::Result<crate::auth::AdoptionRead> {
            Ok(if self.invalid {
                crate::auth::AdoptionRead::Invalid
            } else {
                crate::auth::AdoptionRead::Missing
            })
        }
        fn read_back(&self, _: &Auth) -> io::Result<Option<Auth>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            let mut auth = fixture();
            auth.metadata["access_token"] = json!("legacy-different-outer");
            auth.metadata["siwc"]["access_token"] = json!("legacy-different-siwc");
            Ok(Some(auth))
        }
    }
    for invalid in [false, true] {
        for outer in [true, false] {
            let store = Arc::new(ClassifiedStore {
                invalid,
                legacy_calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let manager = Manager::new(
                Settings::default(),
                Arc::new(FakeModels::default()),
                Some(store.clone()),
            );
            let gate = ActiveGate::open();
            manager.set_gate(gate.clone());
            let mut auth = fixture();
            auth.metadata["use_siwc"] = json!(!outer);
            manager.register_unsaved(auth).unwrap();
            let before = manager.get("codex.json").unwrap();
            gate.close(gate.epoch()).await.unwrap();
            let adopted = manager
                .try_refresh_after_unauthorized(
                    &before,
                    &crate::exec::ExecError::upstream(401, "synthetic"),
                    false,
                )
                .await;
            assert!(
                store.legacy_calls.load(Ordering::SeqCst) == 0,
                "Missing and Invalid never call legacy read_back"
            );
            assert!(
                adopted.is_none() && Arc::ptr_eq(&before, &manager.get("codex.json").unwrap()),
                "classified rejection keeps live despite different legacy token"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn discarded_chain_keeps_landed_intermediate_baselines() {
    for outer in [true, false] {
        for foreign in [false, true] {
            for middle_count in [1, 3] {
                let (_dir, store, manager, _gate) = disk_manager();
                // The fault or hook is on the unleased refresh save, so the store gives no lease.
                store.lease_unsupported.store(true, Ordering::SeqCst);
                manager.register_executor(partition_rotation_executor(outer, "landed-b"));
                let (release_tx, release_rx) = mpsc::channel();
                let release_rx = Mutex::new(release_rx);
                let entered = Arc::new(Notify::new());
                let notify = entered.clone();
                *lock(&manager.shared.persist_admitted_hook) = Some(Arc::new(move |auth| {
                    if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
                        notify.notify_one();
                        lock(&release_rx)
                            .recv_timeout(Duration::from_secs(60))
                            .expect("admitted chain save released");
                    }
                }));
                let admitted = SaveHold {
                    release: Some(release_tx),
                    entered,
                };
                let refreshing = manager.clone();
                let refresh =
                    tokio::spawn(
                        async move { refreshing.force_refresh("codex.json").await.unwrap() },
                    );
                admitted.wait_entered().await;
                discard_work(&manager);
                drop(admitted);
                refresh.await.unwrap();
                *lock(&manager.shared.persist_admitted_hook) = None;
                let raw = store.files.read_back(&fixture()).unwrap().unwrap();
                assert!(
                    partition_token(&raw, outer) == "landed-b-refresh",
                    "admitted first rotation lands before middle failures"
                );
                assert!(
                    manager.lock().auths["codex.json"]
                        .undurable
                        .contains(partition(outer)),
                    "old admitted epoch leaves marker"
                );
                for label in ["failed-c", "failed-d", "failed-e"]
                    .into_iter()
                    .take(middle_count)
                {
                    manager.register_executor(partition_rotation_executor(outer, label));
                    store.fail_refresh.store(true, Ordering::SeqCst);
                    manager.force_refresh("codex.json").await.unwrap();
                    discard_work(&manager);
                }
                if foreign {
                    let mut other = fixture();
                    if outer {
                        other.metadata["access_token"] = json!("foreign-access");
                        other.metadata["refresh_token"] = json!("foreign-refresh");
                    } else {
                        other.metadata["siwc"]["access_token"] = json!("foreign-access");
                        other.metadata["siwc"]["refresh_token"] = json!("foreign-refresh");
                    }
                    store
                        .files
                        .write_file("codex.json", &serde_json::to_vec(&other.metadata).unwrap())
                        .unwrap();
                }
                manager.register_executor(partition_rotation_executor(outer, "chain-final"));
                manager.force_refresh("codex.json").await.unwrap();
                let raw = store.files.read_back(&fixture()).unwrap().unwrap();
                assert!(
                    partition_token(&raw, outer)
                        == if foreign {
                            "foreign-refresh"
                        } else {
                            "chain-final-refresh"
                        },
                    "landed intermediate baseline repairs disk without manual repair write"
                );
                let state = manager.lock();
                let entry = &state.auths["codex.json"];
                assert!(
                    partition_token(&entry.auth, outer) == "chain-final-refresh",
                    "latest chain rotation remains live"
                );
                assert!(
                    entry.undurable.contains(partition(outer)) == foreign,
                    "only matching chain write clears marker"
                );
                let baselines = if outer {
                    &entry.undurable_baselines.outer
                } else {
                    &entry.undurable_baselines.siwc
                };
                assert!(
                    !baselines.is_empty() == foreign,
                    "whole baseline set clears with marker"
                );
            }
        }
    }
}

#[test]
fn recorded_baselines_preserve_provider_baseline_and_agreement() {
    use crate::auth::codex_refresh::record_durable_baseline;
    for outer in [true, false] {
        let (_dir, store, _manager, _gate) = disk_manager();
        let original = fixture();
        let mut baselines = Vec::new();
        for n in 0..43 {
            let mut version = original.clone();
            if n != 0 {
                partition_map_mut(&mut version, outer)
                    .insert("access_token".into(), json!(format!("version-{n}")));
            }
            for map in
                CredentialRefreshPatch::new(&version.metadata, &version.metadata, outer, !outer)
                    .durable_baselines(partition(outer))
            {
                record_durable_baseline(&mut baselines, &map);
            }
        }
        let mut differing = baselines[0].clone();
        differing.insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        record_durable_baseline(&mut baselines, &differing);
        let mut before = original.clone();
        partition_map_mut(&mut before, outer).insert("access_token".into(), json!("version-43"));
        let mut after = before.clone();
        partition_map_mut(&mut after, outer)
            .insert("access_token".into(), json!("agreement-final"));
        let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, outer, !outer);
        let patch = if outer {
            patch.with_durable_baselines(&baselines, &[])
        } else {
            patch.with_durable_baselines(&[], &baselines)
        };
        let mut edited = original.clone();
        partition_map_mut(&mut edited, outer)
            .insert("expired".into(), json!("2099-03-01T00:00:00Z"));
        let (raw, applied) = patch.apply(&edited.metadata);
        let mut saved = original.clone();
        saved.metadata = raw;
        assert!(
            if outer { applied.outer } else { applied.siwc } == Some(true)
                && partition_map_mut(&mut saved, outer)["expired"] == "2099-01-01T00:00:00Z",
            "baseline agreement preserves changed-field application"
        );
        after.codex_persist_intent = CodexPersistIntent::Refresh(patch);
        store.files.save(&after).unwrap();
        let mut disk = store.files.read_back(&original).unwrap().unwrap();
        assert!(
            partition_map_mut(&mut disk, outer)["access_token"] == "agreement-final",
            "first identity repairs disk after many recorded versions"
        );
    }
    for outer in [true, false] {
        let before = fixture();
        let mut provider = before.clone();
        let mut recorded = before.clone();
        let mut after = before.clone();
        if outer {
            provider.metadata["refresh_token"] = json!("provider-baseline");
            recorded.metadata["refresh_token"] = json!("recorded-baseline");
            after.metadata["refresh_token"] = json!("final-refresh");
        } else {
            provider.metadata["siwc"]["refresh_token"] = json!("provider-baseline");
            recorded.metadata["siwc"]["refresh_token"] = json!("recorded-baseline");
            after.metadata["siwc"]["refresh_token"] = json!("final-refresh");
        }
        let recorded_patch =
            CredentialRefreshPatch::new(&recorded.metadata, &after.metadata, outer, !outer);
        let recorded = recorded_patch.durable_baselines(partition(outer));
        let patch = CredentialRefreshPatch::with_disk_baseline(
            &before.metadata,
            Some(&provider.metadata),
            &after.metadata,
            outer,
            !outer,
        );
        let patch = if outer {
            patch.with_durable_baselines(&recorded, &[])
        } else {
            patch.with_durable_baselines(&[], &recorded)
        };
        assert!(
            patch.can_apply_to(&provider.metadata),
            "attachment preserves provider disk baseline"
        );
        let (raw, _) = patch.apply(&provider.metadata);
        let mut saved = before.clone();
        saved.metadata = raw;
        assert!(
            partition_token(&saved, outer) == "final-refresh",
            "provider baseline writes final rotation"
        );
        assert!(
            patch.can_apply_to(&if outer {
                json!({"access_token":"outer-old", "refresh_token":"recorded-baseline"})
                    .as_object()
                    .unwrap()
                    .clone()
            } else {
                let mut raw = fixture().metadata;
                raw["siwc"]["refresh_token"] = json!("recorded-baseline");
                raw
            }),
            "attachment also accepts recorded baseline"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_only_empty_supported_replace_discards_and_preserves_record() {
    let (dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    for (outer, label) in [(true, "empty-outer"), (false, "empty-siwc")] {
        manager.register_executor(partition_rotation_executor(outer, label));
        store.fail_refresh.store(true, Ordering::SeqCst);
        manager.force_refresh("codex.json").await.unwrap();
    }
    let before = manager.get("codex.json").unwrap();
    assert!(
        !before.codex_pending_refresh.is_empty(),
        "empty Replace has work to discard"
    );
    let bytes = std::fs::read(dir.path().join("codex.json")).unwrap();
    let writes = lock(&store.writes).len();
    let mut replacement = (*before).clone();
    replacement.metadata.clear();
    replacement
        .attributes
        .insert("runtime_only".into(), "true".into());
    manager.update(replacement).unwrap();
    assert!(
        lock(&store.writes).len() == writes,
        "empty supported Replace never saves"
    );
    assert!(
        std::fs::read(dir.path().join("codex.json")).unwrap() == bytes,
        "empty supported Replace leaves disk bytes unchanged"
    );
    let live = manager.get("codex.json").unwrap();
    assert!(
        live.metadata == before.metadata,
        "empty supported Replace retains complete live record"
    );
    assert!(
        live.codex_pending_refresh.is_empty(),
        "empty supported Replace discards pending work"
    );
    assert!(
        matches!(live.codex_persist_intent, CodexPersistIntent::Configuration),
        "empty supported Replace discards refresh intent"
    );
    {
        let state = manager.lock();
        let entry = &state.auths["codex.json"];
        assert!(
            entry.undurable.outer && entry.undurable.siwc,
            "empty Replace marks both discarded partitions"
        );
    }
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.attributes.remove("runtime_only");
            Ok::<_, ()>(true)
        })
        .unwrap();
    for (outer, label) in [(true, "later-outer"), (false, "later-siwc")] {
        manager.register_executor(partition_rotation_executor(outer, label));
        manager.force_refresh("codex.json").await.unwrap();
        let raw = disk(&dir);
        assert!(
            raw["type"] == "codex" && raw["note"] == "initial",
            "later rotation preserves type and non-token record fields"
        );
    }
    manager
        .edit("codex.json", |auth| {
            auth.attributes
                .insert("policy_note".into(), "later-settings".into());
            Ok::<_, ()>(true)
        })
        .unwrap();
    let raw = disk(&dir);
    assert!(
        raw["type"] == "codex" && raw["note"] == "initial",
        "later attribute save preserves type and non-token record fields"
    );
    assert!(
        raw["refresh_token"] == "later-outer-refresh"
            && raw["siwc"]["refresh_token"] == "later-siwc-refresh",
        "later attribute save preserves both repaired token partitions"
    );
    assert!(
        store.files.read_back(&fixture()).unwrap().unwrap().provider == "codex",
        "later saves keep the record classified as Codex"
    );
}

#[test]
fn policy_shortcut_rejects_binding_changed_during_capability_read() {
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    struct RebindingStore {
        binding: AtomicU64,
        samples: AtomicUsize,
    }
    impl AuthStore for RebindingStore {
        fn list(&self) -> io::Result<Vec<Auth>> {
            Ok(vec![])
        }
        fn save(&self, auth: &Auth) -> io::Result<String> {
            Ok(auth.id.clone())
        }
        fn delete(&self, _: &str) -> io::Result<()> {
            Ok(())
        }
        fn read_for_adoption(&self, _: &Auth) -> io::Result<crate::auth::AdoptionRead> {
            self.binding.fetch_add(1, Ordering::SeqCst);
            Ok(crate::auth::AdoptionRead::Record(fixture()))
        }
        fn binding_generation(&self) -> u64 {
            self.samples.fetch_add(1, Ordering::SeqCst);
            self.binding.load(Ordering::SeqCst)
        }
    }
    let store = Arc::new(RebindingStore {
        binding: AtomicU64::new(1),
        samples: AtomicUsize::new(0),
    });
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    manager.register_unsaved(fixture()).unwrap();
    let before = manager.get("codex.json").unwrap();
    store.samples.store(0, Ordering::SeqCst);
    let mut replacement = (*before).clone();
    replacement
        .attributes
        .insert("runtime_only".into(), "true".into());
    manager.update_unsaved(replacement).unwrap();
    assert!(
        Arc::ptr_eq(&before, &manager.get("codex.json").unwrap()),
        "policy shortcut rejects binding changed during capability read"
    );
    assert!(
        store.samples.load(Ordering::SeqCst) >= 2,
        "binding sampled before read and revalidated at publication"
    );
}

fn hold_admitted_chain_save(manager: &Manager) -> SaveHold {
    let (release, receiver) = mpsc::channel();
    let receiver = Mutex::new(receiver);
    let entered = Arc::new(Notify::new());
    let notify = entered.clone();
    *lock(&manager.shared.persist_admitted_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            notify.notify_one();
            lock(&receiver)
                .recv_timeout(Duration::from_secs(60))
                .expect("admitted chain released");
        }
    }));
    SaveHold {
        release: Some(release),
        entered,
    }
}

async fn repair_long_discard_chain(failed_saves: bool, admitted_middle: bool) {
    for outer in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        // The fault or hook is on the unleased refresh save, so the store gives no lease.
        store.lease_unsupported.store(true, Ordering::SeqCst);
        if !failed_saves {
            manager
                .edit_unsaved("codex.json", |auth| {
                    auth.attributes.insert("runtime_only".into(), "true".into());
                    Ok::<_, ()>(true)
                })
                .unwrap();
        }
        for n in 0..40 {
            rotate_partition(
                &manager,
                &store,
                outer,
                &format!("long-first-{n}"),
                failed_saves,
            );
        }
        if failed_saves {
            discard_work(&manager);
        }
        if admitted_middle {
            manager
                .edit_unsaved("codex.json", |auth| {
                    auth.attributes.remove("runtime_only");
                    Ok::<_, ()>(true)
                })
                .unwrap();
            manager.register_executor(partition_rotation_executor(outer, "long-admitted"));
            let held = hold_admitted_chain_save(&manager);
            let refreshing = manager.clone();
            let refresh =
                tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
            held.wait_entered().await;
            manager
                .edit_unsaved("codex.json", |auth| {
                    auth.attributes.insert("runtime_only".into(), "true".into());
                    Ok::<_, ()>(true)
                })
                .unwrap();
            drop(held);
            refresh.await.unwrap();
            *lock(&manager.shared.persist_admitted_hook) = None;
            let raw = store.files.read_back(&fixture()).unwrap().unwrap();
            assert!(
                partition_token(&raw, outer) == "long-admitted-refresh",
                "middle admitted identity reaches disk"
            );
            {
                let state = manager.lock();
                let entry = &state.auths["codex.json"];
                let recorded = if outer {
                    &entry.undurable_baselines.outer
                } else {
                    &entry.undurable_baselines.siwc
                };
                let disk_baselines =
                    CredentialRefreshPatch::new(&raw.metadata, &raw.metadata, outer, !outer)
                        .durable_baselines(partition(outer));
                assert!(
                    disk_baselines.iter().all(|disk| recorded
                        .iter()
                        .any(|known| crate::auth::codex_refresh::same_version(known, disk))),
                    "admitted result identity is recorded before any later rotation"
                );
            }
            for n in 0..40 {
                rotate_partition(&manager, &store, outer, &format!("long-middle-{n}"), false);
            }
        }
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes.remove("runtime_only");
                Ok::<_, ()>(true)
            })
            .unwrap();
        rotate_partition(&manager, &store, outer, "long-repair", false);
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            partition_token(&raw, outer) == "long-repair-refresh",
            "long discarded chain repairs the retained disk identity"
        );
        let state = manager.lock();
        let entry = &state.auths["codex.json"];
        assert!(
            !entry.undurable.contains(partition(outer)),
            "long chain verified repair clears marker"
        );
        let baselines = if outer {
            &entry.undurable_baselines.outer
        } else {
            &entry.undurable_baselines.siwc
        };
        assert!(
            baselines.is_empty(),
            "long chain repair clears every baseline"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn forty_runtime_only_rotations_repair_first_disk_identity() {
    repair_long_discard_chain(false, false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn forty_failed_saves_repair_first_disk_identity() {
    repair_long_discard_chain(true, false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn forty_discards_repair_middle_admitted_disk_identity() {
    repair_long_discard_chain(false, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn historical_baselines_preserve_rotation_field_changes() {
    for outer in [true, false] {
        let original = fixture();
        let mut before = original.clone();
        let mut after = original.clone();
        let change = |auth: &mut Auth, key: &str, value: Value| {
            if outer {
                auth.metadata.insert(key.into(), value);
            } else {
                auth.metadata["siwc"]
                    .as_object_mut()
                    .unwrap()
                    .insert(key.into(), value);
            }
        };
        change(&mut before, "expired", json!("2099-02-01T00:00:00Z"));
        change(&mut after, "access_token", json!("expiry-final-access"));
        change(&mut after, "refresh_token", json!("expiry-final-refresh"));
        let old =
            CredentialRefreshPatch::new(&original.metadata, &original.metadata, outer, !outer)
                .durable_baselines(partition(outer));
        let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, outer, !outer);
        let patch = if outer {
            patch.with_durable_baselines(&old, &[])
        } else {
            patch.with_durable_baselines(&[], &old)
        };
        let (live, _) = patch.apply(&before.metadata);
        let expiry = if outer {
            &live["expired"]
        } else {
            &live["siwc"]["expired"]
        };
        assert!(
            expiry == "2099-01-01T00:00:00Z",
            "own before prevents historical baseline hiding a changed expiry"
        );
        // A second same-identity variant also matters when own before already
        // equals after: identity matching must not erase field differences.
        let variants =
            CredentialRefreshPatch::new(&original.metadata, &before.metadata, outer, !outer)
                .durable_baselines(partition(outer));
        let patch = CredentialRefreshPatch::new(&original.metadata, &after.metadata, outer, !outer);
        let patch = if outer {
            patch.with_durable_baselines(&variants, &[])
        } else {
            patch.with_durable_baselines(&[], &variants)
        };
        let (live, _) = patch.apply(&before.metadata);
        let expiry = if outer {
            &live["expired"]
        } else {
            &live["siwc"]["expired"]
        };
        assert!(
            expiry == "2099-01-01T00:00:00Z",
            "same-identity variants cannot hide expiry differences"
        );

        let (_dir, store, manager, _gate) = disk_manager();
        let executor = FakeExecutor::new("codex");
        executor.set_refresh(move |before| {
            let mut after = before.clone();
            if outer {
                after.metadata["expired"] = json!("2099-02-01T00:00:00Z");
            } else {
                after.metadata["siwc"]["expired"] = json!("2099-02-01T00:00:00Z");
            }
            after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                &before.metadata,
                &after.metadata,
                outer,
                !outer,
            ));
            Ok(after)
        });
        manager.register_executor(executor);
        let held = hold_admitted_chain_save(&manager);
        let refreshing = manager.clone();
        let refresh =
            tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
        held.wait_entered().await;
        discard_work(&manager);
        drop(held);
        refresh.await.unwrap();
        *lock(&manager.shared.persist_admitted_hook) = None;
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            if outer {
                &raw.metadata["expired"]
            } else {
                &raw.metadata["siwc"]["expired"]
            } == "2099-02-01T00:00:00Z",
            "admitted expiry-only change lands"
        );
        let before = manager.get("codex.json").unwrap();
        let mut rotated = (*before).clone();
        change(&mut rotated, "expired", json!("2099-01-01T00:00:00Z"));
        change(&mut rotated, "access_token", json!("expiry-final-access"));
        change(&mut rotated, "refresh_token", json!("expiry-final-refresh"));
        rotated.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &rotated.metadata,
            outer,
            !outer,
        ));
        manager
            .update_refreshed(&before, before.registration_epoch, rotated)
            .unwrap();
        for auth in [
            manager.get("codex.json").unwrap(),
            Arc::new(store.files.read_back(&fixture()).unwrap().unwrap()),
        ] {
            assert!(
                partition_token(&auth, outer) == "expiry-final-refresh",
                "expiry rotation keeps final identity in live and disk"
            );
            assert!(
                if outer {
                    &auth.metadata["expired"]
                } else {
                    &auth.metadata["siwc"]["expired"]
                } == "2099-01-01T00:00:00Z",
                "expiry rotation writes actual changed expiry in live and disk"
            );
        }
    }
}

#[test]
fn empty_supported_replace_preserves_pending_rotations_and_full_record() {
    for outer in [true, false] {
        for save in [true, false] {
            let (dir, store, manager, _gate) = disk_manager();
            rotate_partition(&manager, &store, outer, "empty-pending", true);
            let before = manager.get("codex.json").unwrap();
            let mut replacement = (*before).clone();
            replacement.metadata.clear();
            let writes = lock(&store.writes).len();
            if save {
                manager.update(replacement).unwrap();
                assert!(
                    lock(&store.writes).len() > writes,
                    "eligible empty Replace performs an ordinary full-record save"
                );
                let raw = store.files.read_back(&fixture()).unwrap().unwrap();
                assert!(
                    raw.metadata["type"] == "codex" && raw.metadata["note"] == "initial",
                    "eligible empty Replace immediately saves complete record"
                );
                assert!(
                    partition_token(&raw, outer) == "empty-pending-refresh",
                    "eligible empty Replace applies its pending rotation"
                );
            } else {
                manager.update_unsaved(replacement).unwrap();
                let live = manager.get("codex.json").unwrap();
                assert!(
                    !live.codex_pending_refresh.is_empty()
                        && live
                            .codex_pending_refresh
                            .iter()
                            .all(|patch| !patch.is_written()),
                    "empty unsaved Replace retains pending rotation"
                );
            }
            manager
                .edit("codex.json", |auth| {
                    auth.attributes
                        .insert("policy_note".into(), "after-empty".into());
                    Ok::<_, ()>(true)
                })
                .unwrap();
            let raw = disk(&dir);
            assert!(
                raw["type"] == "codex" && raw["note"] == "initial",
                "eligible empty Replace saves complete record"
            );
            manager.load().unwrap();
            assert!(
                partition_token(&manager.get("codex.json").unwrap(), outer)
                    == "empty-pending-refresh",
                "empty Replace and attribute save retain latest rotation after reload"
            );
        }
    }
}

#[test]
fn empty_supported_replace_keeps_edit_published_during_read() {
    let (_dir, store, manager, _gate) = disk_manager();
    let (published, ready) = mpsc::channel();
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.metadata.get("note") == Some(&json!("during-empty-read")) {
            let _ = published.send(());
        }
    }));
    let editing = manager.clone();
    let thread = Arc::new(Mutex::new(None));
    let handle = thread.clone();
    let ready = Mutex::new(ready);
    *lock(&store.read_hook) = Some(Arc::new(move |_| {
        let editing = editing.clone();
        *lock(&handle) = Some(std::thread::spawn(move || {
            editing
                .edit_unsaved("codex.json", |auth| {
                    auth.metadata
                        .insert("note".into(), json!("during-empty-read"));
                    Ok::<_, ()>(true)
                })
                .unwrap();
        }));
        lock(&ready)
            .recv_timeout(Duration::from_secs(5))
            .expect("edit published during capability read");
    }));
    let mut replacement = (*manager.get("codex.json").unwrap()).clone();
    replacement.metadata.clear();
    manager.update(replacement).unwrap();
    *lock(&store.read_hook) = None;
    *lock(&manager.shared.persist_hook) = None;
    lock(&thread).take().unwrap().join().unwrap();
    let live = manager.get("codex.json").unwrap();
    assert!(
        live.metadata["note"] == "during-empty-read",
        "empty Replace retains current-state metadata edit during read"
    );
    assert!(
        store.files.read_back(&fixture()).unwrap().unwrap().metadata["note"] == "during-empty-read",
        "empty Replace saves current-state non-token edit"
    );
}

#[test]
fn marked_written_result_is_recorded_when_readback_fails() {
    for outer in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        rotate_partition(&manager, &store, outer, "readback-first", true);
        discard_work(&manager);
        store.skip_refresh_read.store(true, Ordering::SeqCst);
        rotate_partition(&manager, &store, outer, "readback-landed", false);
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            partition_token(&raw, outer) == "readback-landed-refresh",
            "write lands despite declined refresh readback"
        );
        {
            let state = manager.lock();
            let entry = &state.auths["codex.json"];
            assert!(
                entry.undurable.contains(partition(outer)),
                "unverified landed write keeps marker"
            );
            let recorded = if outer {
                &entry.undurable_baselines.outer
            } else {
                &entry.undurable_baselines.siwc
            };
            let disk = CredentialRefreshPatch::new(&raw.metadata, &raw.metadata, outer, !outer)
                .durable_baselines(partition(outer));
            assert!(
                disk.iter().all(|disk| recorded
                    .iter()
                    .any(|known| crate::auth::codex_refresh::same_version(known, disk))),
                "marked written result remains recorded despite failed readback"
            );
        }
        store.skip_refresh_read.store(false, Ordering::SeqCst);
        rotate_partition(&manager, &store, outer, "readback-repair", false);
        assert!(
            !manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "later verified write clears readback marker"
        );
    }
}

fn partition_map_mut(auth: &mut Auth, outer: bool) -> &mut serde_json::Map<String, Value> {
    if outer {
        &mut auth.metadata
    } else {
        auth.metadata
            .get_mut("siwc")
            .unwrap()
            .as_object_mut()
            .unwrap()
    }
}

#[test]
fn sparse_discard_histories_have_one_entry_per_identity() {
    for outer in [true, false] {
        let (_dir, _store, manager, _gate) = disk_manager();
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes.insert("runtime_only".into(), "true".into());
                Ok::<_, ()>(true)
            })
            .unwrap();
        for phase in 0..2 {
            for n in 0..20 {
                manager
                    .edit_unsaved("codex.json", |auth| {
                        let key = if phase == 0 {
                            "expired"
                        } else {
                            "last_refresh"
                        };
                        partition_map_mut(auth, outer)
                            .insert(key.into(), json!(format!("2099-02-{:02}T00:00:00Z", n + 1)));
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
                let before = manager.get("codex.json").unwrap();
                let mut after = (*before).clone();
                let map = partition_map_mut(&mut after, outer);
                if phase == 0 {
                    map.insert("access_token".into(), json!(format!("growth-{n}")));
                    map.remove("expired");
                } else {
                    map.insert(
                        "expired".into(),
                        json!(format!("2099-03-{:02}T00:00:00Z", n + 1)),
                    );
                    map.remove("last_refresh");
                }
                after.codex_persist_intent = CodexPersistIntent::Refresh(
                    CredentialRefreshPatch::new(&before.metadata, &after.metadata, outer, !outer),
                );
                manager
                    .update_refreshed(&before, before.registration_epoch, after)
                    .unwrap();
                let state = manager.lock();
                let set = if outer {
                    &state.auths["codex.json"].undurable_baselines.outer
                } else {
                    &state.auths["codex.json"].undurable_baselines.siwc
                };
                // Initial identity + 20 access identities; phase two adds none.
                assert!(
                    set.len() <= 21,
                    "folded history has at most 21 distinct-identity representatives"
                );
            }
        }
    }
}

#[test]
fn landed_pending_patch_keeps_foreign_and_runtime_partition_edits() {
    for outer in [true, false] {
        // Equal before/after identities must still prefer the earlier baseline.
        let before = fixture();
        let mut after = before.clone();
        partition_map_mut(&mut after, outer)
            .insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, outer, !outer);
        let mut target = before.clone();
        partition_map_mut(&mut target, outer)
            .insert("expired".into(), json!("2099-03-01T00:00:00Z"));
        target.metadata = patch.apply(&target.metadata).0;
        assert!(
            partition_map_mut(&mut target, outer)["expired"] == "2099-02-01T00:00:00Z",
            "earlier same-identity baseline still writes changed expiry"
        );
        for runtime_edit in [false, true] {
            let (dir, store, manager, _gate) = disk_manager();
            let before = manager.get("codex.json").unwrap();
            let mut after = (*before).clone();
            let map = partition_map_mut(&mut after, outer);
            map.insert("access_token".into(), json!("landed-access"));
            map.insert("refresh_token".into(), json!("landed-refresh"));
            map.insert("expired".into(), json!("2099-02-01T00:00:00Z"));
            after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                &before.metadata,
                &after.metadata,
                outer,
                !outer,
            ));
            store.fail_after_save.store(true, Ordering::SeqCst);
            manager
                .update_refreshed(&before, before.registration_epoch, after)
                .unwrap();
            let pending = manager.get("codex.json").unwrap();
            assert!(
                pending
                    .codex_pending_refresh
                    .iter()
                    .any(|patch| !patch.is_written()),
                "landed write returning error retains an unwritten operation"
            );
            if runtime_edit {
                manager
                    .edit_unsaved("codex.json", |auth| {
                        auth.attributes.insert("runtime_only".into(), "true".into());
                        partition_map_mut(auth, outer)
                            .insert("expired".into(), json!("2099-04-01T00:00:00Z"));
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
                let mut incoming = (*manager.get("codex.json").unwrap()).clone();
                incoming.rebase_pending_refreshes_from(&pending);
                assert!(
                    partition_map_mut(&mut incoming, outer)["expired"] == "2099-04-01T00:00:00Z",
                    "landed fallback preserves runtime-only partition edit on rebase"
                );
            } else {
                let mut foreign = disk(&dir);
                if outer {
                    foreign["expired"] = json!("2099-03-01T00:00:00Z");
                } else {
                    foreign["siwc"]["expired"] = json!("2099-03-01T00:00:00Z");
                }
                std::fs::write(
                    dir.path().join("codex.json"),
                    serde_json::to_vec(&foreign).unwrap(),
                )
                .unwrap();
                manager
                    .edit("codex.json", |auth| {
                        auth.attributes
                            .insert("policy_note".into(), "save-pending".into());
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
                let mut raw = store.files.read_back(&fixture()).unwrap().unwrap();
                assert!(
                    partition_map_mut(&mut raw, outer)["expired"] == "2099-03-01T00:00:00Z",
                    "landed fallback preserves foreign expiry on Configuration save"
                );
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn empty_supported_replace_keeps_held_refresh_operation() {
    for outer in [true, false] {
        for failed_save in [false, true] {
            let (_dir, store, manager, _gate) = disk_manager();
            // The fault or hook is on the unleased refresh save, so the store gives no lease.
            store.lease_unsupported.store(true, Ordering::SeqCst);
            manager.register_executor(partition_rotation_executor(outer, "held-empty"));
            let held = hold_matching_save(&manager, |auth| {
                matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            });
            let refreshing = manager.clone();
            let refresh =
                tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
            held.wait_entered().await;
            let before = manager.get("codex.json").unwrap();
            assert!(
                matches!(&before.codex_persist_intent, CodexPersistIntent::Refresh(patch) if !patch.is_written()),
                "empty Replace test starts with a real unwritten Refresh intent"
            );
            let mut replacement = (*before).clone();
            replacement.metadata.clear();
            if failed_save {
                *lock(&store.error_once) = Some("injected unchanged Replace save failure".into());
                manager.update(replacement).unwrap();
            } else {
                manager.update_unsaved(replacement).unwrap();
            }
            let live = manager.get("codex.json").unwrap();
            assert!(
                matches!(&live.codex_persist_intent, CodexPersistIntent::Refresh(patch) if !patch.is_written())
                    && live
                        .codex_persist_intent
                        .same_operation(&before.codex_persist_intent),
                "empty supported Replace preserves held Refresh operation identity"
            );
            drop(held);
            refresh.await.unwrap();
            *lock(&manager.shared.persist_hook) = None;
        }
    }
}

fn sparse_identity_executor(outer: bool) -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(move |before| {
        let mut after = before.clone();
        let map = partition_map_mut(&mut after, outer);
        map.insert("access_token".into(), json!("sparse-landed-access"));
        map.remove("refresh_token");
        after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &after.metadata,
            outer,
            !outer,
        ));
        Ok(after)
    });
    executor
}

#[tokio::test(flavor = "current_thread")]
async fn sparse_marked_write_with_failed_readback_repairs_disk_identity() {
    for outer in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        rotate_partition(&manager, &store, outer, "sparse-start", true);
        discard_work(&manager);
        store.skip_refresh_read.store(true, Ordering::SeqCst);
        manager.register_executor(sparse_identity_executor(outer));
        manager.force_refresh("codex.json").await.unwrap();
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            partition_token(&raw, outer)
                != partition_token(&manager.get("codex.json").unwrap(), outer),
            "sparse landed identity differs from live without resurrecting consumed refresh"
        );
        store.skip_refresh_read.store(false, Ordering::SeqCst);
        rotate_partition(&manager, &store, outer, "sparse-repaired", false);
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            partition_token(&raw, outer) == "sparse-repaired-refresh",
            "marked sparse readback gap repairs the actual landed disk identity"
        );
        assert!(
            !manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "verified sparse readback repair clears marker"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_admitted_sparse_identity_output_repairs_disk() {
    for outer in [true, false] {
        let (_dir, store, manager, _gate) = disk_manager();
        rotate_partition(&manager, &store, outer, "late-sparse-start", true);
        discard_work(&manager);
        manager.register_executor(sparse_identity_executor(outer));
        let held = hold_admitted_chain_save(&manager);
        let refreshing = manager.clone();
        let refresh =
            tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
        held.wait_entered().await;
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes.insert("runtime_only".into(), "true".into());
                Ok::<_, ()>(true)
            })
            .unwrap();
        drop(held);
        refresh.await.unwrap();
        *lock(&manager.shared.persist_admitted_hook) = None;
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            partition_token(&raw, outer)
                != partition_token(&manager.get("codex.json").unwrap(), outer),
            "late admitted sparse write lands a different disk identity"
        );
        manager
            .edit_unsaved("codex.json", |auth| {
                auth.attributes.remove("runtime_only");
                Ok::<_, ()>(true)
            })
            .unwrap();
        rotate_partition(&manager, &store, outer, "late-sparse-repaired", false);
        let raw = store.files.read_back(&fixture()).unwrap().unwrap();
        assert!(
            partition_token(&raw, outer) == "late-sparse-repaired-refresh",
            "late sparse output repairs the actual landed disk identity"
        );
        assert!(
            !manager.lock().auths["codex.json"]
                .undurable
                .contains(partition(outer)),
            "verified late sparse repair clears marker"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn empty_supported_replace_preserves_actual_refresh_failure_backoff() {
    let (_dir, store, manager, _gate) = disk_manager();
    // The fault or hook is on the unleased refresh save, so the store gives no lease.
    store.lease_unsupported.store(true, Ordering::SeqCst);
    failed_inactive_refresh(&store, &manager).await;
    let before = manager.get("codex.json").unwrap();
    let failures = manager.lock().auths["codex.json"].refresh_failures;
    let mut replacement = (*before).clone();
    replacement.metadata.clear();
    replacement.codex_refresh_failure = None;
    replacement.next_refresh_after = None;
    manager.update_unsaved(replacement).unwrap();
    let live = manager.get("codex.json").unwrap();
    assert!(
        live.codex_refresh_failure
            .as_ref()
            .is_some_and(|failure| failure.partition == crate::auth::CodexCredentialPartition::Siwc)
            && manager.lock().auths["codex.json"].refresh_failures == failures,
        "empty supported Replace preserves actual partition failure and backoff"
    );
    assert!(
        live.next_refresh_after == before.next_refresh_after,
        "empty supported Replace preserves actual next_refresh_after"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn empty_supported_replace_preserves_nonzero_refresh_failure_count() {
    let (_dir, _store, manager, _gate) = disk_manager();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata.insert("use_siwc".into(), json!(false));
            auth.metadata
                .insert("expired".into(), json!("2000-01-01T00:00:00Z"));
            Ok::<_, ()>(true)
        })
        .unwrap();
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|_| Err(crate::exec::ExecError::upstream(400, "invalid_grant")));
    manager.register_executor(executor);
    assert!(
        manager.force_refresh("codex.json").await.is_err(),
        "invalid-grant provider attempt genuinely fails"
    );
    let before = manager.get("codex.json").unwrap();
    let failures = manager.lock().auths["codex.json"].refresh_failures;
    assert!(
        failures > 0,
        "invalid-grant attempt establishes nonzero failure count"
    );
    let mut replacement = (*before).clone();
    replacement.metadata.clear();
    manager.update_unsaved(replacement).unwrap();
    assert!(
        manager.lock().auths["codex.json"].refresh_failures == failures,
        "empty supported Replace preserves nonzero actual failure count"
    );
}

async fn live_record_save_after_empty_replace(saving_replace: bool, reset_quota: bool) {
    for outer in [true, false] {
        let (dir, store, manager, _gate) = disk_manager();
        // The fault or hook is on the unleased refresh save, so the store gives no lease.
        store.lease_unsupported.store(true, Ordering::SeqCst);
        rotate_partition(&manager, &store, outer, "self-older", true);
        let older = manager.get("codex.json").unwrap().codex_pending_refresh[0].clone();
        assert!(!older.is_written(), "older rotation starts unwritten");
        // The partition's tokens in the file before the pass.
        let original = {
            let saved = disk(&dir);
            let partition = if outer { &saved } else { &saved["siwc"] };
            (
                partition["access_token"].clone(),
                partition["refresh_token"].clone(),
            )
        };
        manager.register_executor(partition_rotation_executor(outer, "self-newer"));
        let held = hold_matching_save(&manager, |auth| {
            matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
        });
        let refreshing = manager.clone();
        let refresh =
            tokio::spawn(async move { refreshing.force_refresh("codex.json").await.unwrap() });
        held.wait_entered().await;
        let before = manager.get("codex.json").unwrap();
        let CodexPersistIntent::Refresh(newer) = &before.codex_persist_intent else {
            panic!("held save publishes a refresh operation");
        };
        let newer = newer.clone();
        let mut replacement = (*before).clone();
        replacement.metadata.clear();
        if saving_replace {
            // Check (vi) refuses the Replace's save while the pass is in flight.
            manager.update(replacement).unwrap();
        } else {
            manager.update_unsaved(replacement).unwrap();
        }
        let live = manager.get("codex.json").unwrap();
        assert!(
            matches!(&live.codex_persist_intent, CodexPersistIntent::Refresh(p) if p.same_operation(&newer))
                && live.codex_pending_refresh.len() == 2
                && live.codex_pending_refresh[0].same_operation(&older)
                && live.codex_pending_refresh[1].same_operation(&newer)
                && !older.is_written()
                && !newer.is_written(),
            "empty Replace retains older pending work and duplicates the held operation only across intent and pending"
        );
        if reset_quota {
            manager.reset_quota("codex.json").unwrap().unwrap();
        } else {
            manager.mark_result(&success(&manager));
        }
        let generation = manager.get("codex.json").unwrap().generation;
        // The save is refused while the pass is in flight: wait until it is
        // owed, then the file still holds the partition's original tokens.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !manager.lock().auths.get("codex.json").is_some_and(|entry| {
                entry
                    .owed_save
                    .is_some_and(|owed| owed.version >= generation)
            }) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the refused live-record save is owed");
        let saved = disk(&dir);
        let partition = if outer { &saved } else { &saved["siwc"] };
        assert_eq!(
            (
                partition["access_token"].clone(),
                partition["refresh_token"].clone()
            ),
            original,
            "the live-record save has not written while the pass is in flight"
        );
        drop(held);
        refresh.await.unwrap();
        *lock(&manager.shared.persist_hook) = None;
        wait_pending_saves(&manager).await;
        let live = manager.get("codex.json").unwrap();
        let saved = disk(&dir);
        let partition = if outer { &saved } else { &saved["siwc"] };
        assert!(
            older.is_written()
                && newer.is_written()
                && live.codex_pending_refresh.is_empty()
                && !matches!(live.codex_persist_intent, CodexPersistIntent::Refresh(_))
                && partition["access_token"] == json!("self-newer-access")
                && partition["refresh_token"] == json!("self-newer-refresh"),
            "eligible rotations are consumed only after the composed tokens reach disk"
        );
        // Step 14 ran inside the pass: the covering Configuration save cleared
        // the owed save, with no wait for it.
        assert!(
            manager.lock().auths["codex.json"].owed_save.is_none(),
            "the pass's owed save was saved again once the lock was free"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn live_record_save_preserves_chain_after_refused_empty_replace() {
    live_record_save_after_empty_replace(true, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn live_record_save_preserves_chain_after_empty_replace_skip() {
    live_record_save_after_empty_replace(false, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn reset_quota_preserves_chain_after_refused_empty_replace() {
    live_record_save_after_empty_replace(true, true).await;
}

#[test]
fn siwc_overlay_without_live_partition_preserves_nontoken_fields() {
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(Arc::new(FakeStore::default())),
    );
    let mut live = fixture();
    live.metadata.remove("siwc");
    manager.register_unsaved(live).unwrap();
    manager
        .lock()
        .auths
        .get_mut("codex.json")
        .unwrap()
        .undurable
        .siwc = true;
    let mut incoming = fixture();
    incoming.metadata["siwc"]["custom_setting"] = json!("preserved");
    manager.update_unsaved(incoming).unwrap();
    let actual = manager.get("codex.json").unwrap();
    assert_eq!(
        actual
            .metadata
            .get("siwc")
            .and_then(|siwc| siwc.get("custom_setting")),
        Some(&json!("preserved")),
        "SIWC overlay preserves nontoken fields without live object"
    );
    for key in crate::auth::codex_refresh::SIWC_KEYS {
        assert!(
            actual.metadata["siwc"].get(*key).is_none(),
            "SIWC overlay removes only token fields"
        );
    }
}

#[test]
fn unregister_prunes_only_idle_save_mutex_entries() {
    let (_dir, _store, manager, _gate) = disk_manager();
    let permit = manager.mutation_gate();
    manager.with_adoption("codex.json", &permit, |_| ());
    assert!(lock(&manager.shared.persist_save_locks).contains_key("codex.json"));
    manager.remove("codex.json");
    assert!(
        !lock(&manager.shared.persist_save_locks).contains_key("codex.json"),
        "unregister removes idle save mutex entry"
    );
    manager.with_adoption("never-registered", &permit, |_| ());
    assert!(
        !lock(&manager.shared.persist_save_locks).contains_key("never-registered"),
        "unregistered adoption leaves no mutex entry"
    );
    manager.register_unsaved(fixture()).unwrap();
    manager.with_adoption("codex.json", &permit, |_| {
        manager.remove("codex.json");
        assert!(
            lock(&manager.shared.persist_save_locks).contains_key("codex.json"),
            "active adoption keeps its save mutex registered"
        );
    });
    assert!(!lock(&manager.shared.persist_save_locks).contains_key("codex.json"));
}

#[test]
fn completed_commands_accept_case_only_provider_changes() {
    let (_dir, store, manager, _gate) = disk_manager();
    rotate_partition(&manager, &store, true, "case-live", true);
    discard_work(&manager);
    let command = manager
        .capture_token_command("codex.json", partition(true))
        .unwrap();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.provider = "CODEX".into();
            Ok::<_, ()>(true)
        })
        .unwrap();
    let mut login = fixture();
    login.metadata["access_token"] = json!("case-login");
    login.metadata["refresh_token"] = json!("case-login-refresh");
    store
        .files
        .write_file("codex.json", &serde_json::to_vec(&login.metadata).unwrap())
        .unwrap();
    manager
        .adopt_completed_token_command(&login, command)
        .unwrap();
    assert!(
        !manager.lock().auths["codex.json"].undurable.outer,
        "case-only provider change preserves command authority"
    );
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["access_token"],
        "case-login",
        "case-only provider read adopts fresh credentials"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn settings_save_before_discard_lands_only_without_discarded_refresh() {
    for includes_refresh in [false, true] {
        let (dir, store, manager, _gate) = disk_manager();
        if includes_refresh {
            rotate_partition(&manager, &store, true, "captured", true);
        }
        let held = hold_matching_save(&manager, |auth| {
            auth.metadata.get("note") == Some(&json!("captured-settings"))
        });
        let editing = manager.clone();
        let save = tokio::spawn(async move {
            editing
                .run_persistence(|manager| {
                    manager.edit("codex.json", |auth| {
                        auth.metadata
                            .insert("note".into(), json!("captured-settings"));
                        Ok::<_, ()>(true)
                    })
                })
                .await
        });
        held.wait_entered().await;
        // A later rotation/discard changes both generation and discard epoch.
        rotate_partition(&manager, &store, true, "newer", true);
        manager
            .edit("codex.json", |auth| {
                auth.attributes.insert("runtime_only".into(), "true".into());
                Ok::<_, ()>(true)
            })
            .unwrap()
            .unwrap();
        // Neither policy edit lands a newer disk generation: the first is
        // ineligible and the restoring edit fails. An unsaved edit would
        // advance the observation watermark and independently skip the save.
        *lock(&store.error_once) = Some("restoring policy save fails".into());
        manager
            .edit("codex.json", |auth| {
                auth.attributes
                    .insert("runtime_only".into(), "false".into());
                Ok::<_, ()>(true)
            })
            .unwrap()
            .unwrap();
        store.fail_refresh.store(false, Ordering::SeqCst);
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), save)
            .await
            .expect("captured settings save finishes within 5s")
            .unwrap()
            .unwrap()
            .unwrap();
        let saved = disk(&dir);
        assert_eq!(
            saved.get("note") == Some(&json!("captured-settings")),
            !includes_refresh,
            "only settings-only captured save lands after discard"
        );
        assert_eq!(
            saved["refresh_token"], "outer-refresh-old",
            "discarded refresh never reaches disk through settings save"
        );
    }
}

#[test]
fn completed_command_rejects_another_id_with_equal_epochs() {
    let (_dir, store, manager, _gate) = disk_manager();
    rotate_partition(&manager, &store, true, "kept", true);
    discard_work(&manager);
    let mut other = fixture();
    other.id = "other.json".into();
    manager.register_unsaved(other).unwrap();
    let command = manager
        .capture_token_command("other.json", partition(true))
        .unwrap();
    {
        let mut state = manager.lock();
        let source = &state.auths["other.json"];
        let epochs = (source.auth.registration_epoch, source.discard_epoch);
        let target = state.auths.get_mut("codex.json").unwrap();
        Arc::make_mut(&mut target.auth).registration_epoch = epochs.0;
        target.discard_epoch = epochs.1;
    }
    let mut login = fixture();
    login.metadata["access_token"] = json!("foreign-login");
    store
        .files
        .write_file("codex.json", &serde_json::to_vec(&login.metadata).unwrap())
        .unwrap();
    manager
        .adopt_completed_token_command(&login, command)
        .unwrap();
    assert!(
        manager.lock().auths["codex.json"].undurable.outer,
        "foreign id command keeps target marker"
    );
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["access_token"],
        "kept-access",
        "foreign id command keeps target rotation"
    );
}

#[test]
fn save_completion_prunes_only_unregistered_mutex() {
    for unregister in [false, true] {
        let (_dir, store, manager, _gate) = disk_manager();
        let saving = manager.clone();
        let reached = Arc::new(AtomicBool::new(false));
        let flag = reached.clone();
        *lock(&store.save_hook) = Some(Arc::new(move |_| {
            flag.store(true, Ordering::SeqCst);
            if unregister {
                saving.remove("codex.json");
            }
            assert!(
                lock(&saving.shared.persist_save_locks).contains_key("codex.json"),
                "held save keeps mutex entry"
            );
        }));
        manager
            .edit("codex.json", |auth| {
                auth.metadata["note"] = json!("saved");
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert!(
            reached.load(Ordering::SeqCst),
            "save completion prune hook reached"
        );
        assert_eq!(
            lock(&manager.shared.persist_save_locks).contains_key("codex.json"),
            !unregister,
            "save completion prunes only unregistered mutex"
        );
    }
}

#[test]
fn pending_refresh_rebase_accepts_folded_provider() {
    let live = outer_rotation(&fixture(), "folded");
    let mut incoming = fixture();
    incoming.provider = " CoDeX ".into();
    incoming.rebase_pending_refreshes_from(&live);
    assert_eq!(
        incoming.metadata["access_token"], "folded-access",
        "folded provider rebase keeps pending rotation"
    );
}

#[test]
fn folded_provider_adoption_and_removal_keep_origin() {
    let (_dir, store, manager, _gate) = disk_manager();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.provider = " CoDeX ".into();
            Ok::<_, ()>(true)
        })
        .unwrap();
    let changing = manager.clone();
    *lock(&store.read_hook) = Some(Arc::new(move |_| {
        let mut state = changing.lock();
        Arc::make_mut(&mut state.auths.get_mut("codex.json").unwrap().auth).provider =
            "CODEX".into();
    }));
    let result = manager
        .adopt_observed_validated(
            &fixture(),
            manager.observation_registration("codex.json"),
            |live| {
                store
                    .read_for_adoption(live)
                    .map_err(crate::manager::ManagerError::Store)
            },
            || true,
        )
        .unwrap();
    assert!(
        matches!(result, crate::manager::AdoptionResult::Published { .. }),
        "folded provider observation keeps origin proof"
    );
    let mut expected = (*manager.get("codex.json").unwrap()).clone();
    expected.provider = " CoDeX ".into();
    assert!(
        manager.remove_observed(&expected),
        "folded provider removal recognizes registration"
    );
}

#[test]
fn folded_provider_saves_reconcile_markers_and_pending() {
    let (_dir, store, manager, _gate) = disk_manager();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.provider = " CoDeX ".into();
            Ok::<_, ()>(true)
        })
        .unwrap();
    rotate_partition(&manager, &store, true, "discarded-case", true);
    discard_work(&manager);
    rotate_partition(&manager, &store, true, "persisted-case", false);
    assert!(
        !manager.lock().auths["codex.json"].undurable.outer,
        "folded provider save verifies marker"
    );
    rotate_partition(&manager, &store, false, "pending-case", true);
    store.fail_refresh.store(false, Ordering::SeqCst);
    manager.mark_result(&success(&manager));
    assert!(
        manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty(),
        "folded provider pending save reconciles written work"
    );
}

#[test]
fn provider_case_change_after_save_snapshot_clears_marker() {
    let (_dir, store, manager, _gate) = disk_manager();
    rotate_partition(&manager, &store, true, "discarded-case-race", true);
    discard_work(&manager);
    assert!(manager.lock().auths["codex.json"].undurable.outer);
    let changing = manager.clone();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    *lock(&store.save_hook) = Some(Arc::new(move |writing| {
        assert_eq!(
            writing.provider, "codex",
            "save captured original provider before case edit"
        );
        let mut state = changing.lock();
        Arc::make_mut(&mut state.auths.get_mut("codex.json").unwrap().auth).provider =
            " CoDeX ".into();
        observed.store(true, Ordering::SeqCst);
    }));
    let checking = manager.clone();
    let checked = Arc::new(AtomicBool::new(false));
    let reconciled = checked.clone();
    *lock(&manager.shared.reconcile_hook) = Some(Arc::new(move |_| {
        assert!(
            !checking.lock().auths["codex.json"].undurable.outer,
            "case-only edit after snapshot clears marker in this save"
        );
        reconciled.store(true, Ordering::SeqCst);
    }));
    rotate_partition(&manager, &store, true, "persisted-case-race", false);
    assert!(
        checked.load(Ordering::SeqCst),
        "marker checked before reconciliation"
    );
    *lock(&manager.shared.reconcile_hook) = None;
    assert!(
        called.load(Ordering::SeqCst),
        "case edit runs after save snapshot"
    );
    assert!(
        !manager.lock().auths["codex.json"].undurable.outer,
        "case-only edit after snapshot clears marker in this save"
    );
    *lock(&store.save_hook) = None;
}

#[tokio::test(flavor = "current_thread")]
async fn folded_provider_rejection_adopts_current_disk() {
    let (_dir, store, manager, gate) = disk_manager();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.provider = " CoDeX ".into();
            auth.metadata["use_siwc"] = json!(false);
            Ok::<_, ()>(true)
        })
        .unwrap();
    let failed = manager.get("codex.json").unwrap();
    let mut disk = fixture();
    disk.metadata["access_token"] = json!("case-rejected-disk");
    disk.metadata["use_siwc"] = json!(false);
    store
        .files
        .write_file("codex.json", &serde_json::to_vec(&disk.metadata).unwrap())
        .unwrap();
    let changing = manager.clone();
    *lock(&manager.shared.adopt_hook) = Some(Arc::new(move || {
        let mut state = changing.lock();
        Arc::make_mut(&mut state.auths.get_mut("codex.json").unwrap().auth).provider =
            "CODEX".into();
    }));
    tokio::time::timeout(Duration::from_secs(5), gate.close(gate.epoch()))
        .await
        .expect("folded rejection gate closes within 5s")
        .unwrap();
    let adopted = tokio::time::timeout(
        Duration::from_secs(5),
        manager.try_refresh_after_unauthorized(
            &failed,
            &crate::exec::ExecError::upstream(401, "synthetic"),
            false,
        ),
    )
    .await
    .expect("folded rejection adoption completes within 5s");
    assert_eq!(
        adopted
            .as_ref()
            .map(|auth| auth.metadata_str("access_token")),
        Some(Some("case-rejected-disk")),
        "folded provider rejection adopts disk across read and publish fences"
    );
}

// ---- B5: the leased transaction in `refresh_owned` ----
//
// Tests of the activation. Each runs under `bounded` (B-16). The leased path
// is the default here: `ObservedStore` forwards the lease, so a Codex refresh
// on its `FileStore` takes the credential lock. `lease_unsupported` and the
// `Recording` store are the ways back to the unleased path.

use crate::exec::{ExecError, Options, Request, Response, StreamResponse};
use crate::executor::{ProviderExecutor, RefreshOptions};
use crate::manager::ManagerError;
use futures_core::future::BoxFuture;
use std::future::Future;
use tokio::sync::Semaphore;

/// B-16: the longest any B test may run.
pub(super) const B_TEST_LIMIT: Duration = Duration::from_secs(30);

/// B-16: `future` under a limit, or `Err` when it runs past it.
pub(super) async fn bounded_for<F: Future>(
    limit: Duration,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(limit, future).await
}

/// B-16: runs a B test body under the 30 s limit, so a deadlock fails the
/// test instead of hanging it.
pub(super) async fn bounded<F: Future>(future: F) -> F::Output {
    // Boxed: a test body is a large future, and debug builds poll it on the
    // test thread's small stack.
    bounded_for(B_TEST_LIMIT, Box::pin(future))
        .await
        .expect("the test finishes within 30 s")
}

/// A seeded credential directory: `codex.json` holds the fixture's tokens (A).
struct Seeded {
    dir: tempfile::TempDir,
    files: Arc<FileStore>,
    gate: ActiveGate,
}

fn seeded() -> Seeded {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let files = Arc::new(FileStore::new(dir.path()));
    let gate = ActiveGate::open();
    files.set_gate(gate.clone());
    files.save_new(&fixture()).unwrap();
    let path = dir.path().join("codex.json");
    let mut seeded: Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for key in ["siwc", "use_siwc"] {
        seeded.insert(key.into(), fixture().metadata[key].clone());
    }
    std::fs::write(&path, serde_json::to_vec(&seeded).unwrap()).unwrap();
    Seeded { dir, files, gate }
}

/// A manager over a seeded directory, with the fixture registered unsaved.
struct Rig {
    dir: tempfile::TempDir,
    store: Arc<ObservedStore>,
    manager: Manager,
    models: Arc<FakeModels>,
}

impl Rig {
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("codex.json")
    }

    fn disk(&self) -> Value {
        disk(&self.dir)
    }

    fn live(&self) -> Arc<Auth> {
        self.manager.get("codex.json").unwrap()
    }
}

/// The manager's store is `outer` when given (a wrapper over `store`).
fn assemble(seeded: Seeded, store: Arc<ObservedStore>, outer: Option<Arc<dyn AuthStore>>) -> Rig {
    let models = Arc::new(FakeModels::default());
    models.register("codex.json", &["gpt-5"]);
    let outer = outer.unwrap_or_else(|| store.clone());
    let manager = Manager::new(Settings::default(), models.clone(), Some(outer));
    manager.set_gate(seeded.gate.clone());
    manager.register_unsaved(fixture()).unwrap();
    Rig {
        dir: seeded.dir,
        store,
        manager,
        models,
    }
}

fn rig() -> Rig {
    let seeded = seeded();
    let store = observe(seeded.files.clone(), false);
    assemble(seeded, store, None)
}

/// A store that records every call the manager makes on it, in order, and
/// forwards each to an `ObservedStore`. The two pure queries
/// (`supports_codex_refresh_patches`, `binding_generation`) are not recorded.
/// `lease_error` makes `codex_refresh_lease` fail with that kind.
struct Recording {
    inner: Arc<ObservedStore>,
    calls: Mutex<Vec<&'static str>>,
    lease_error: Mutex<Option<io::ErrorKind>>,
    /// Notified at every `codex_refresh_lease` call.
    leased: Notify,
}

impl Recording {
    fn note(&self, call: &'static str) {
        lock(&self.calls).push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        lock(&self.calls).clone()
    }

    fn count(&self, call: &str) -> usize {
        lock(&self.calls).iter().filter(|c| **c == call).count()
    }
}

impl AuthStore for Recording {
    fn list(&self) -> io::Result<Vec<Auth>> {
        self.note("list");
        self.inner.list()
    }

    fn save(&self, auth: &Auth) -> io::Result<String> {
        self.note("save");
        self.inner.save(auth)
    }

    fn delete(&self, id: &str) -> io::Result<()> {
        self.note("delete");
        self.inner.delete(id)
    }

    fn read_back(&self, auth: &Auth) -> io::Result<Option<Auth>> {
        self.note("read_back");
        self.inner.read_back(auth)
    }

    fn read_for_adoption(&self, auth: &Auth) -> io::Result<crate::auth::AdoptionRead> {
        self.note("read_for_adoption");
        self.inner.read_for_adoption(auth)
    }

    fn supports_codex_refresh_patches(&self) -> bool {
        self.inner.supports_codex_refresh_patches()
    }

    fn same_record(&self, a: &Auth, b: &Auth) -> io::Result<bool> {
        self.note("same_record");
        self.inner.same_record(a, b)
    }

    fn binding_generation(&self) -> u64 {
        self.inner.binding_generation()
    }

    fn codex_refresh_lease(&self, auth: &Auth) -> io::Result<CodexLease> {
        self.note("codex_refresh_lease");
        self.leased.notify_one();
        if let Some(kind) = *lock(&self.lease_error) {
            return Err(io::Error::new(kind, "injected lease failure"));
        }
        self.inner.codex_refresh_lease(auth)
    }

    fn codex_lease_read(&self, token: &CodexLeaseToken) -> io::Result<Option<Map<String, Value>>> {
        self.note("codex_lease_read");
        self.inner.codex_lease_read(token)
    }

    fn codex_lease_commit(
        &self,
        token: &CodexLeaseToken,
        auth: &Auth,
        patch: &CredentialRefreshPatch,
    ) -> io::Result<CodexPersisted> {
        self.note("codex_lease_commit");
        self.inner.codex_lease_commit(token, auth, patch)
    }

    fn codex_origin(&self, auth: &Auth) -> io::Result<Option<CodexOrigin>> {
        self.note("codex_origin");
        self.inner.codex_origin(auth)
    }

    fn save_bound(
        &self,
        auth: &Auth,
        origin: &CodexOrigin,
        refused: &mut dyn FnMut(&CodexOrigin, OriginProof),
    ) -> io::Result<Option<String>> {
        self.note("save_bound");
        self.inner.save_bound(auth, origin, refused)
    }

    fn save_guarded(
        &self,
        auth: &Auth,
        guard: &mut dyn FnMut(Option<&CodexOrigin>) -> Result<WithheldPartitions, SaveDeferral>,
    ) -> io::Result<GuardedSave> {
        self.note("save_guarded");
        self.inner.save_guarded(auth, guard)
    }
}

/// A rig whose manager's store is a `Recording` over the `ObservedStore`.
fn recording_rig(lease_unsupported: bool) -> (Rig, Arc<Recording>) {
    let seeded = seeded();
    let store = observe(seeded.files.clone(), lease_unsupported);
    let recording = Arc::new(Recording {
        inner: store.clone(),
        calls: Mutex::new(Vec::new()),
        lease_error: Mutex::new(None),
        leased: Notify::new(),
    });
    (assemble(seeded, store, Some(recording.clone())), recording)
}

/// A Codex executor that records what it is given, can park at the send, and
/// answers through its `FakeExecutor`.
struct Probe {
    fake: Arc<FakeExecutor>,
    id: String,
    entered: Notify,
    release: Semaphore,
    hold: AtomicBool,
    seen: Mutex<Vec<Arc<Auth>>>,
    options: Mutex<Vec<RefreshOptions>>,
    spent: Mutex<Vec<Value>>,
    reused: AtomicU32,
    at_entry: Mutex<Option<SaveHook>>,
}

impl Probe {
    fn new(id: &str, hold: bool) -> Arc<Self> {
        Arc::new(Self {
            fake: FakeExecutor::new(id),
            id: id.to_owned(),
            entered: Notify::new(),
            release: Semaphore::new(0),
            hold: AtomicBool::new(hold),
            seen: Mutex::new(Vec::new()),
            options: Mutex::new(Vec::new()),
            spent: Mutex::new(Vec::new()),
            reused: AtomicU32::new(0),
            at_entry: Mutex::new(None),
        })
    }

    /// Answers each refresh with an outer rotation named `name`.
    fn rotating(self: Arc<Self>, name: &'static str) -> Arc<Self> {
        self.fake.set_refresh(move |before| {
            let mut after = before.clone();
            after
                .metadata
                .insert("access_token".into(), json!(format!("{name}-access")));
            after
                .metadata
                .insert("refresh_token".into(), json!(format!("{name}-refresh")));
            after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                &before.metadata,
                &after.metadata,
                true,
                false,
            ));
            Ok(after)
        });
        self
    }

    /// How many sends carried a refresh token an earlier send had spent.
    fn reused(&self) -> u32 {
        self.reused.load(Ordering::SeqCst)
    }

    fn sent(&self) -> usize {
        lock(&self.seen).len()
    }

    /// The outer refresh token the nth send carried.
    fn sent_refresh(&self, n: usize) -> Value {
        lock(&self.seen)[n].metadata["refresh_token"].clone()
    }

    fn set_at_entry(&self, hook: impl Fn(&Auth) + Send + Sync + 'static) {
        *lock(&self.at_entry) = Some(Arc::new(hook));
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.notified())
            .await
            .expect("the refresh reaches the executor");
    }

    /// Lets a parked send go, and every later one.
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.release.add_permits(1);
    }
}

impl ProviderExecutor for Probe {
    fn id(&self) -> &str {
        &self.id
    }
    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.fake.execute(auth, request, options)
    }
    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        self.fake.execute_stream(auth, request, options)
    }
    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.fake.count_tokens(auth, request, options)
    }
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        self.refresh_with_options(auth, RefreshOptions::default())
    }
    fn refresh_with_options(
        &self,
        auth: Arc<Auth>,
        options: RefreshOptions,
    ) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move {
            let token = auth.metadata["refresh_token"].clone();
            if lock(&self.spent).contains(&token) {
                self.reused.fetch_add(1, Ordering::SeqCst);
            }
            lock(&self.seen).push(auth.clone());
            lock(&self.options).push(options);
            let hook = lock(&self.at_entry).clone();
            if let Some(hook) = hook {
                hook(&auth);
            }
            self.entered.notify_one();
            if self.hold.load(Ordering::SeqCst) {
                self.release.acquire().await.unwrap().forget();
            }
            let answer = self.fake.refresh(auth).await;
            if answer
                .as_ref()
                .is_ok_and(|after| after.metadata.get("refresh_token") != Some(&token))
            {
                lock(&self.spent).push(token);
            }
            answer
        })
    }
}

/// The fixture with its outer partition rotated A -> B, as a live refresh
/// intent no store has seen.
fn outer_b_intent() -> Auth {
    let before = fixture();
    let mut after = before.clone();
    after
        .metadata
        .insert("access_token".into(), json!("b-access"));
    after
        .metadata
        .insert("refresh_token".into(), json!("b-refresh"));
    after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &after.metadata,
        true,
        false,
    ));
    after
}

/// The newest generation published for `codex.json`'s projections.
fn projection_generation(models: &FakeModels) -> Option<u64> {
    models
        .published()
        .iter()
        .filter(|published| published.client_id == "codex.json")
        .map(|published| published.generation)
        .max()
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_wrapper_fails_a_test_that_runs_past_its_limit() {
    // B-16: the wrapper is what makes a deadlocked test fail, so it must
    // time out a future that never finishes and pass one that does.
    let never = bounded_for(Duration::from_millis(50), std::future::pending::<()>()).await;
    assert!(never.is_err(), "a pending future runs past the limit");
    let done = bounded_for(Duration::from_secs(5), async { 7 }).await;
    assert_eq!(done.ok(), Some(7));
    assert_eq!(B_TEST_LIMIT, Duration::from_secs(30));
}

#[tokio::test(flavor = "current_thread")]
async fn leased_commit_lands_live_intent_before_the_send_and_projections_advance() {
    // B-1 (F1) and B-16's projection check.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        let dir = rig.dir.path().to_path_buf();
        let at_entry = Arc::new(Mutex::new(None));
        let seen_disk = at_entry.clone();
        probe.set_at_entry(move |_| {
            let file: Value =
                serde_json::from_slice(&std::fs::read(dir.join("codex.json")).unwrap()).unwrap();
            *lock(&seen_disk) = Some(file);
        });
        rig.manager.register_executor(probe.clone());
        // Disk holds A; live holds B with the intent A -> B, in no pending list.
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        rig.manager.register_unsaved(outer_b_intent()).unwrap();
        assert!(rig.live().codex_pending_refresh.is_empty());
        let before = projection_generation(&rig.models);
        rig.manager.force_refresh("codex.json").await.unwrap();
        let at_send = lock(&at_entry).clone().expect("the executor was entered");
        assert_eq!(
            at_send["refresh_token"], "b-refresh",
            "disk holds B at the send"
        );
        assert_eq!(probe.sent(), 1);
        assert_eq!(probe.sent_refresh(0), "b-refresh", "the executor got B");
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        assert_eq!(
            rig.store.lease_calls.load(Ordering::SeqCst),
            1,
            "one lease for the pass"
        );
        let after = projection_generation(&rig.models);
        assert!(
            after > before,
            "the leased rotation published a newer projection generation"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn leased_commit_composes_pending_and_live_intent_before_the_send() {
    // B-1 variant: pending A -> B plus the live intent B -> C gives disk C.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("d");
        let dir = rig.dir.path().to_path_buf();
        let at_entry = Arc::new(Mutex::new(None));
        let seen_disk = at_entry.clone();
        probe.set_at_entry(move |_| {
            let file: Value =
                serde_json::from_slice(&std::fs::read(dir.join("codex.json")).unwrap()).unwrap();
            *lock(&seen_disk) = Some(file);
        });
        rig.manager.register_executor(probe.clone());
        let a_to_b = outer_b_intent();
        let CodexPersistIntent::Refresh(first) = a_to_b.codex_persist_intent.clone() else {
            panic!("the fixture carries an intent");
        };
        let mut live = a_to_b.clone();
        live.codex_persist_intent = CodexPersistIntent::default();
        live.codex_pending_refresh.push(first);
        let mut b_to_c = live.clone();
        b_to_c
            .metadata
            .insert("access_token".into(), json!("c-access"));
        b_to_c
            .metadata
            .insert("refresh_token".into(), json!("c-refresh"));
        b_to_c.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &live.metadata,
            &b_to_c.metadata,
            true,
            false,
        ));
        rig.manager.register_unsaved(b_to_c).unwrap();
        assert_eq!(rig.live().codex_pending_refresh.len(), 1);
        rig.manager.force_refresh("codex.json").await.unwrap();
        let at_send = lock(&at_entry).clone().expect("the executor was entered");
        assert_eq!(
            at_send["refresh_token"], "c-refresh",
            "disk holds C at the send"
        );
        assert_eq!(probe.sent_refresh(0), "c-refresh");
        assert_eq!(rig.disk()["refresh_token"], "d-refresh");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
    })
    .await;
}

/// Whether any save the store refused was refused for a refresh in flight.
fn refused_for_refresh(store: &ObservedStore) -> bool {
    lock(&store.guarded)
        .iter()
        .any(|(_, deferral)| matches!(deferral, Some(SaveDeferral::RefreshInFlight)))
}

/// The calls of a recording, without the lease query (step 2's, which a pass
/// makes before it learns the store is `Unsupported`).
fn without_lease_queries(calls: &[&'static str]) -> Vec<&'static str> {
    calls
        .iter()
        .copied()
        .filter(|call| *call != "codex_refresh_lease")
        .collect()
}

// The sequences below are the ones recorded at the commit before the lease
// went live, with the same wrapper. `codex_origin` is the guarded save's own
// origin check, which resolves the record's file before the save writes; it
// was made then too, and is pinned here, in place, not filtered out.
/// The store calls of an unleased pass that rotates the outer partition.
const ROTATING_UNLEASED_CALLS: &[&str] = &[
    "save_guarded",
    "codex_origin",
    "read_back",
    "same_record",
    "same_record",
];
/// The store calls of an unleased pass that rotates nothing.
const IDLE_UNLEASED_CALLS: &[&str] = &[
    "save_guarded",
    "codex_origin",
    "read_back",
    "same_record",
    "same_record",
];
/// The store calls of a refresh of a non-Codex record.
const GEMINI_CALLS: &[&str] = &["save_guarded"];

#[tokio::test(flavor = "current_thread")]
async fn unsupported_store_rotating_pass_keeps_todays_call_sequence() {
    // B-9 (decision 13): with `Unsupported`, the pass's own tail is exempt from
    // check (vi), so nothing is refused and nothing is replayed.
    bounded(async {
        let (rig, recording) = recording_rig(true);
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        lock(&recording.calls).clear();
        rig.manager.force_refresh("codex.json").await.unwrap();
        let calls = recording.calls();
        assert_eq!(recording.count("codex_refresh_lease"), 1, "{calls:?}");
        for never in ["codex_lease_read", "codex_lease_commit", "save_bound"] {
            assert_eq!(recording.count(never), 0, "{never} in {calls:?}");
        }
        assert!(!refused_for_refresh(&rig.store), "a refused tail save");
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        assert_eq!(
            without_lease_queries(&calls),
            ROTATING_UNLEASED_CALLS,
            "{calls:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_store_pass_without_rotation_keeps_todays_call_sequence() {
    // B-9: the same for a pass that rotates nothing.
    bounded(async {
        let (rig, recording) = recording_rig(true);
        let probe = Probe::new("codex", false);
        probe.fake.set_refresh(|before| Ok(before.clone()));
        rig.manager.register_executor(probe.clone());
        lock(&recording.calls).clear();
        rig.manager.force_refresh("codex.json").await.unwrap();
        let calls = recording.calls();
        assert_eq!(probe.sent(), 1);
        assert_eq!(recording.count("codex_refresh_lease"), 1, "{calls:?}");
        for never in ["codex_lease_read", "codex_lease_commit", "save_bound"] {
            assert_eq!(recording.count(never), 0, "{never} in {calls:?}");
        }
        assert!(!refused_for_refresh(&rig.store), "a refused tail save");
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        assert_eq!(
            without_lease_queries(&calls),
            IDLE_UNLEASED_CALLS,
            "{calls:?}"
        );
    })
    .await;
}

/// A gemini record on a `FileStore`, under a `Recording` store.
fn gemini_rig() -> (Rig, Arc<Recording>, Auth) {
    let seeded = seeded();
    let gemini = auth_with_metadata(
        "gemini.json",
        "gemini",
        json!({
            "type":"gemini", "access_token":"g-old", "refresh_token":"g-refresh-old",
            "expired":"2099-01-01T00:00:00Z"
        }),
    );
    seeded.files.save_new(&gemini).unwrap();
    let store = observe(seeded.files.clone(), false);
    let recording = Arc::new(Recording {
        inner: store.clone(),
        calls: Mutex::new(Vec::new()),
        lease_error: Mutex::new(None),
        leased: Notify::new(),
    });
    let rig = assemble(seeded, store, Some(recording.clone()));
    rig.manager.register_unsaved(gemini.clone()).unwrap();
    (rig, recording, gemini)
}

#[tokio::test(flavor = "current_thread")]
async fn non_codex_refresh_never_touches_the_lease() {
    // B-18: only a Codex record under a store takes the lease.
    bounded(async {
        let (rig, recording, gemini) = gemini_rig();
        // Any lease call would fail the refresh.
        rig.store.fail_lease_read.store(u32::MAX, Ordering::SeqCst);
        rig.store
            .fail_lease_commit
            .store(u32::MAX, Ordering::SeqCst);
        let executor = FakeExecutor::new("gemini");
        executor.set_refresh(|before| {
            let mut after = before.clone();
            after
                .metadata
                .insert("access_token".into(), json!("g-new-access"));
            after
                .metadata
                .insert("refresh_token".into(), json!("g-new-refresh"));
            Ok(after)
        });
        rig.manager.register_executor(executor.clone());
        lock(&recording.calls).clear();
        rig.manager.force_refresh("gemini.json").await.unwrap();
        let calls = recording.calls();
        assert_eq!(executor.refresh_count(), 1);
        for never in [
            "codex_refresh_lease",
            "codex_lease_read",
            "codex_lease_commit",
            "save_bound",
        ] {
            assert_eq!(recording.count(never), 0, "{never} in {calls:?}");
        }
        assert_eq!(rig.store.lease_calls.load(Ordering::SeqCst), 0);
        assert_eq!(rig.store.lease_commit_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(rig.store.save_bound_calls.load(Ordering::SeqCst), 0);
        let path = rig.dir.path().join(&gemini.id);
        assert!(
            !credential_lock_path(&path).unwrap().exists(),
            "a lock file was made for {}",
            gemini.id
        );
        let file: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(file["refresh_token"], "g-new-refresh");
        assert_eq!(calls, GEMINI_CALLS, "{calls:?}");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn lease_timeout_defers_without_touching_routing_state() {
    // B-10: a lease that times out is `LockBusy`: no send, a short wait, and
    // no change to status, availability or the recorded error.
    bounded(async {
        let (rig, recording) = recording_rig(false);
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        *lock(&recording.lease_error) = Some(io::ErrorKind::TimedOut);
        // Routing state that differs from the defaults, so an unchanged field
        // is not just an unchanged default. The error is not a 401, which
        // would let the refresh clear it.
        let mut seeded = fixture();
        seeded.status = crate::auth::Status::Error;
        seeded.unavailable = true;
        seeded.status_message = "seeded upstream failure".into();
        seeded.last_error = Some(crate::auth::AuthError {
            code: "upstream".into(),
            message: "seeded upstream failure".into(),
            retryable: true,
            http_status: 502,
        });
        rig.manager.register_unsaved(seeded.clone()).unwrap();
        let before = rig.live();
        assert_eq!(before.status, seeded.status, "the seed took");
        assert!(before.unavailable, "the seed took");
        assert_eq!(before.status_message, seeded.status_message);
        assert_eq!(before.last_error, seeded.last_error);
        let started = chrono::Utc::now();
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        let ManagerError::Store(error) = error else {
            panic!("a deferral is a store error");
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(probe.sent(), 0, "nothing was sent");
        let after = rig.live();
        assert_eq!(after.status, before.status);
        assert_eq!(after.unavailable, before.unavailable);
        assert_eq!(after.status_message, before.status_message);
        assert_eq!(after.last_error, before.last_error);
        let held = after.next_refresh_after.expect("a wait was set");
        assert!(held >= started + chrono::Duration::seconds(5), "{held}");
        assert!(
            held <= chrono::Utc::now() + chrono::Duration::seconds(30),
            "{held}"
        );
        // A 401 retry does not refresh through the held lease either.
        let unauthorized = ExecError::upstream(401, "unauthorized");
        assert!(
            rig.manager
                .try_refresh_after_unauthorized(&rig.live(), &unauthorized, false)
                .await
                .is_none(),
            "a deferred refresh yields no retry"
        );
        assert_eq!(probe.sent(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_refresh_job_keeps_the_lease_deferral() {
    // B-10: the job that ends in a deferral does not overwrite its wait.
    bounded(async {
        let (rig, recording) = recording_rig(false);
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        *lock(&recording.lease_error) = Some(io::ErrorKind::TimedOut);
        let mut due = fixture();
        due.metadata
            .insert("refresh_interval_seconds".into(), json!(0.001));
        rig.manager.register_unsaved(due).unwrap();
        let started = chrono::Utc::now();
        rig.manager
            .start_auto_refresh(Duration::from_millis(1))
            .unwrap();
        recording.leased.notified().await;
        while !rig.manager.lock().refresh_jobs.is_empty() {
            tokio::task::yield_now().await;
        }
        rig.manager.stop_auto_refresh();
        assert_eq!(probe.sent(), 0);
        let held = rig.live().next_refresh_after.expect("a wait was set");
        assert!(held >= started + chrono::Duration::seconds(5), "{held}");
    })
    .await;
}

/// The fixture with its outer partition rotated A -> B and the rotation held
/// as a pending one, which no file has seen.
fn pending_b() -> Auth {
    let mut live = outer_b_intent();
    let CodexPersistIntent::Refresh(patch) = std::mem::take(&mut live.codex_persist_intent) else {
        panic!("the fixture carries an intent");
    };
    live.codex_pending_refresh.push(patch);
    live
}

/// A contender for the credential lock of `path`, with a short wait.
async fn contend_for_lock(path: std::path::PathBuf) -> io::Result<()> {
    crate::auth::credential_lock::acquire_with_timeout(path, Duration::from_millis(200))
        .await
        .map(drop)
}

#[tokio::test(flavor = "current_thread")]
async fn pending_write_failure_defers_before_the_send_and_frees_the_lock() {
    // B-11 (N1): the step-5 commit fails, so nothing is sent and the lease is
    // released; the next pass commits the pending write, then sends it.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        rig.manager.register_unsaved(pending_b()).unwrap();
        rig.store.fail_lease_commit.store(1, Ordering::SeqCst);
        let started = chrono::Utc::now();
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        assert!(matches!(error, ManagerError::Store(_)), "{error}");
        assert_eq!(probe.sent(), 0, "nothing was sent");
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        let live = rig.live();
        assert_eq!(live.metadata["refresh_token"], "b-refresh");
        assert_eq!(
            live.codex_pending_refresh.len(),
            1,
            "the write is still owed"
        );
        let held = live.next_refresh_after.expect("a wait was set");
        assert!(held >= started + chrono::Duration::seconds(5), "{held}");
        contend_for_lock(rig.path())
            .await
            .expect("the lease was released");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(probe.sent(), 1);
        assert_eq!(probe.sent_refresh(0), "b-refresh", "B was sent, never A");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        assert!(rig.live().codex_pending_refresh.is_empty());
    })
    .await;
}

fn missing_epoch(rig: &Rig) -> Option<u64> {
    rig.manager
        .lock()
        .missing_file_deferrals
        .get("codex.json")
        .copied()
}

#[tokio::test(flavor = "current_thread")]
async fn missing_file_deferral_follows_the_episode_and_the_registration() {
    // B-14 (N4, Q1), steps 1 to 6. Log levels are not asserted.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        let saved = (*rig.live()).clone();
        rig.manager
            .run_persistence(move |m| m.register(saved))
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_file(rig.path()).unwrap();
        let started = chrono::Utc::now();
        let epoch = rig.live().registration_epoch;
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        let ManagerError::Store(error) = error else {
            panic!("a deferral is a store error");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(probe.sent(), 0);
        assert!(!rig.path().exists(), "nothing was written");
        let held = rig.live().next_refresh_after.expect("a wait was set");
        assert!(held >= started + chrono::Duration::seconds(60), "{held}");
        assert_eq!(missing_epoch(&rig), Some(epoch));
        // A second deferral in the episode leaves the map as it was.
        rig.manager.force_refresh("codex.json").await.unwrap_err();
        assert_eq!(missing_epoch(&rig), Some(epoch));
        assert_eq!(probe.sent(), 0);
        // A saved settings edit writes the file and ends the episode.
        rig.manager
            .run_persistence(|m| {
                m.edit("codex.json", |auth| {
                    auth.metadata.insert("note".into(), json!("edited"));
                    Ok::<bool, ()>(true)
                })
            })
            .await
            .unwrap()
            .unwrap();
        assert!(rig.path().exists(), "the update wrote the file");
        assert_eq!(missing_epoch(&rig), None, "the save ended the episode");
        rig.manager.force_refresh("codex.json").await.unwrap();
        assert_eq!(probe.sent(), 1);
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
        // A new episode in the same registration is first again.
        std::fs::remove_file(rig.path()).unwrap();
        rig.manager.force_refresh("codex.json").await.unwrap_err();
        assert_eq!(missing_epoch(&rig), Some(epoch));
        // Removal clears the entry; a new registration is a new epoch.
        rig.manager.remove("codex.json");
        assert_eq!(missing_epoch(&rig), None);
        rig.manager.register_unsaved(fixture()).unwrap();
        let renewed = rig.live().registration_epoch;
        assert_ne!(renewed, epoch);
        rig.manager.force_refresh("codex.json").await.unwrap_err();
        assert_eq!(missing_epoch(&rig), Some(renewed));
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_non_codex_file_for_a_codex_record_defers_as_not_codex() {
    // B-14 step 7.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        std::fs::write(
            rig.path(),
            serde_json::to_vec(&json!({"type":"gemini","refresh_token":"x"})).unwrap(),
        )
        .unwrap();
        let started = chrono::Utc::now();
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        let ManagerError::Store(error) = error else {
            panic!("a deferral is a store error");
        };
        assert!(error.to_string().contains("not a Codex file"), "{error}");
        assert_eq!(probe.sent(), 0);
        let held = rig.live().next_refresh_after.expect("a wait was set");
        assert!(held >= started + chrono::Duration::seconds(60), "{held}");
        assert_eq!(missing_epoch(&rig), None);
    })
    .await;
}

/// Runs `act` once, on the thread that takes the credential lock, as the lock
/// is acquired: while the pass holds the lease.
fn on_lease_held(rig: &Rig, act: impl Fn() + Send + Sync + 'static) {
    let armed = Arc::new(AtomicBool::new(true));
    crate::auth::credential_lock::set_test_hook(
        &rig.path(),
        Some(Arc::new(move |event: &str| {
            if event == "acquired" && armed.swap(false, Ordering::SeqCst) {
                act();
            }
        })),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rebinding_the_store_while_leased_refuses_before_the_send() {
    // B-19 (i): the directory moved while the lease was taken, so the lease is
    // for a file the registration no longer names.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        let other = tempfile::tempdir_in(rig.dir.path().parent().unwrap()).unwrap();
        let (files, target) = (rig.store.files.clone(), other.path().to_path_buf());
        on_lease_held(&rig, move || files.set_base_dir(&target));
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        crate::auth::credential_lock::set_test_hook(&rig.path(), None);
        assert!(
            error
                .to_string()
                .contains("registration changed before refresh"),
            "{error}"
        );
        assert_eq!(probe.sent(), 0, "nothing was sent");
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        assert!(!other.path().join("codex.json").exists());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replacing_the_registration_while_leased_refuses_before_the_send() {
    // B-19 (ii): the id was removed and registered again under the lease.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        let manager = rig.manager.clone();
        let epoch = rig.live().registration_epoch;
        on_lease_held(&rig, move || {
            manager.remove("codex.json");
            manager.register_unsaved(fixture()).unwrap();
        });
        let error = rig.manager.force_refresh("codex.json").await.unwrap_err();
        crate::auth::credential_lock::set_test_hook(&rig.path(), None);
        assert!(
            error.to_string().contains("registration changed"),
            "{error}"
        );
        assert_ne!(rig.live().registration_epoch, epoch);
        assert_eq!(probe.sent(), 0, "nothing was sent");
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rebinding_to_the_same_directory_while_leased_is_admitted() {
    // B-19 (iii): the same file under a rebind is the same origin.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        let (files, same) = (rig.store.files.clone(), rig.dir.path().to_path_buf());
        on_lease_held(&rig, move || files.set_base_dir(&same));
        rig.manager.force_refresh("codex.json").await.unwrap();
        crate::auth::credential_lock::set_test_hook(&rig.path(), None);
        assert_eq!(probe.sent(), 1);
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        assert_eq!(rig.disk()["refresh_token"], "c-refresh");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_commit_task_never_marks_a_different_file() {
    // P1: the commit's blocking task is lost, so the pass has no lease and its
    // proof of the record's file is made without the credential lock. The
    // record now names another file, which a proof under the lock would call
    // `Different`. Without the lock it is `Unproven`: the rotation is marked
    // for read-back, not as lost to a different file.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        let other = tempfile::tempdir_in(rig.dir.path().parent().unwrap()).unwrap();
        let (files, target) = (rig.store.files.clone(), other.path().to_path_buf());
        let armed = Arc::new(AtomicBool::new(true));
        *lock(&rig.store.before_lease_commit_hook) = Some(Arc::new(move |_: &Auth| {
            if armed.swap(false, Ordering::SeqCst) {
                // The record's file moves, then the task dies holding the lease.
                files.set_base_dir(&target);
                panic!("the commit's blocking task is lost");
            }
        }));
        let origin = crate::auth::CodexOrigin::of(&rig.path()).unwrap();
        rig.manager.force_refresh("codex.json").await.unwrap();
        *lock(&rig.store.before_lease_commit_hook) = None;
        assert_eq!(probe.sent(), 1);
        assert_eq!(rig.store.lease_commit_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
        // The commit never wrote: the file is as it was, and the rotation is
        // owed from memory.
        assert_eq!(rig.disk()["refresh_token"], "outer-refresh-old");
        let live = rig.live();
        assert_eq!(live.metadata["refresh_token"], "c-refresh");
        assert!(
            !live.codex_pending_refresh.is_empty(),
            "the rotation is owed"
        );
        let state = rig.manager.lock();
        let entry = &state.auths["codex.json"];
        assert!(entry.undurable.outer, "the rotation is marked");
        assert!(
            matches!(
                &entry.undurable_causes.outer,
                Some(crate::manager::MarkCause::ReadBack { origin: marked }) if *marked == origin
            ),
            "marked for read-back of the leased file, not as a different one: {:?}",
            entry.undurable_causes.outer
        );
        assert!(
            entry.unresolved_partitions().outer,
            "the origin is left unresolved"
        );
    })
    .await;
}

/// Gives `id` an unresolved origin (its file, the outer rotation A -> B), the
/// way a published pass whose proof failed leaves it, then runs the scheduler
/// until the one job it queues for that state alone has finished. The tokens
/// of every record here are not due, so that job is `resolution_only`.
async fn run_resolution_only_job(rig: &Rig, id: &str, file: &std::path::Path) {
    let CodexPersistIntent::Refresh(patch) = outer_b_intent().codex_persist_intent else {
        panic!("the fixture carries an intent");
    };
    let origin = crate::auth::CodexOrigin::of(file).unwrap();
    let before = {
        let mut state = rig.manager.lock();
        let entry = state.auths.get_mut(id).unwrap();
        entry.record_unresolved(&origin, &patch);
        entry.auth.generation
    };
    rig.manager
        .start_auto_refresh(Duration::from_millis(1))
        .unwrap();
    // Queuing the job and finishing it each advance the generation.
    loop {
        {
            let state = rig.manager.lock();
            if state.refresh_jobs.is_empty() && state.auths[id].auth.generation >= before + 2 {
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    rig.manager.stop_auto_refresh();
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_job_on_a_leased_store_resolves_before_it_exits() {
    // P2 (a): a leased pass queued only for the unresolved origin takes the
    // lease and resolves it (step 6c is its exit). Step 0c must not end it
    // first: that would leave the state in place, and the scheduler would
    // queue the same job again at once.
    bounded(async {
        let (rig, recording) = recording_rig(false);
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        run_resolution_only_job(&rig, "codex.json", &rig.path()).await;
        assert_eq!(probe.sent(), 0, "tokens that are not due are not sent");
        assert!(
            recording.count("codex_refresh_lease") >= 1,
            "the pass took the lease"
        );
        assert!(
            rig.manager.lock().auths["codex.json"]
                .unresolved_origin
                .is_none(),
            "the proof of the file cleared the state"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_job_on_an_unsupported_store_defers_as_unresolved() {
    // P2 (b): a store that cannot lease cannot resolve the origin. The pass
    // defers at step 2, which counts the deferral on the state; step 0c must
    // not return `Ok` before that.
    bounded(async {
        let (rig, recording) = recording_rig(true);
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        run_resolution_only_job(&rig, "codex.json", &rig.path()).await;
        assert_eq!(probe.sent(), 0);
        assert!(recording.count("codex_refresh_lease") >= 1);
        let state = rig.manager.lock();
        let unresolved = state.auths["codex.json"]
            .unresolved_origin
            .as_ref()
            .expect("the state stays: nothing could prove the file");
        assert!(
            unresolved.unproven >= 1,
            "the deferral was counted: {}",
            unresolved.unproven
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_job_on_a_non_codex_record_keeps_the_not_due_exit() {
    // P2 (c): off the lease path (0b), a job for the unresolved state alone
    // still ends at step 0c and sends nothing for tokens that are not due.
    bounded(async {
        let (rig, recording, gemini) = gemini_rig();
        let sent = Arc::new(AtomicU32::new(0));
        let executor = FakeExecutor::new("gemini");
        // Without a lead the scheduler never checks a non-Codex record.
        executor.set_lead(Some(Duration::from_secs(60)));
        let counter = sent.clone();
        executor.set_refresh(move |before| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(before.clone())
        });
        rig.manager.register_executor(executor.clone());
        lock(&recording.calls).clear();
        run_resolution_only_job(&rig, &gemini.id, &rig.dir.path().join("codex.json")).await;
        assert_eq!(sent.load(Ordering::SeqCst), 0, "the exit sent nothing");
        assert_eq!(recording.count("codex_refresh_lease"), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_resolution_job_on_an_ineligible_record_keeps_the_not_due_exit() {
    // P2 (c): step 1's ineligible branch also leaves the leased block with no
    // lease, and ends at step 0c the same way.
    bounded(async {
        let (rig, recording) = recording_rig(false);
        let probe = Probe::new("codex", false).rotating("c");
        rig.manager.register_executor(probe.clone());
        let mut runtime_only = fixture();
        runtime_only
            .attributes
            .insert("runtime_only".into(), "true".into());
        rig.manager.register_unsaved(runtime_only).unwrap();
        lock(&recording.calls).clear();
        run_resolution_only_job(&rig, "codex.json", &rig.path()).await;
        assert_eq!(probe.sent(), 0, "the exit sent nothing");
        assert_eq!(recording.count("codex_refresh_lease"), 0);
        // The send fence would also keep this record's unresolved partitions
        // from a send, but as a deferral that sets a wait. Step 0c returns
        // `Ok` and leaves the credential's next check clear.
        assert!(
            rig.live().next_refresh_after.is_none(),
            "the pass ended without a deferral"
        );
    })
    .await;
}

/// A second Codex credential, with its own file and so its own lock.
fn other_codex(id: &str) -> Auth {
    auth_with_metadata(
        id,
        "codex",
        json!({"type":"codex", "access_token":"o-old", "refresh_token":"o-refresh-old",
            "expired":"2099-01-01T00:00:00Z", "note":"other"}),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn work_on_other_credentials_does_not_wait_for_a_held_lease() {
    // B-7 (a): with the executor parked at the send, unrelated persistence
    // completes, and so does a change to the leased credential that saves nothing.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", true).rotating("c");
        rig.manager.register_executor(probe.clone());
        let other = other_codex("codex2.json");
        rig.store.files.save_new(&other).unwrap();
        rig.manager.register_unsaved(other.clone()).unwrap();
        let pass = {
            let manager = rig.manager.clone();
            tokio::spawn(async move { manager.force_refresh("codex.json").await })
        };
        probe.wait_entered().await;
        let within = Duration::from_secs(5);

        let mut changed = other.clone();
        changed.metadata.insert("note".into(), json!("changed"));
        let updated = bounded_for(
            within,
            rig.manager.run_persistence(move |m| m.update(changed)),
        )
        .await
        .expect("an update of another credential completes");
        assert!(updated.is_ok(), "update of codex2.json");

        let third = auth_with_metadata(
            "third.json",
            "gemini",
            json!({"type":"gemini","access_token":"t","refresh_token":"t-refresh"}),
        );
        bounded_for(
            within,
            rig.manager.run_persistence(move |m| m.register(third)),
        )
        .await
        .expect("a register of a third credential completes")
        .unwrap()
        .unwrap();

        let edited = bounded_for(
            within,
            rig.manager.run_persistence(|m| {
                m.edit_unsaved("codex.json", |auth| {
                    auth.metadata.insert("note".into(), json!("memory-only"));
                    Ok::<bool, ()>(true)
                })
            }),
        )
        .await
        .expect("an edit_unsaved of the leased credential completes");
        assert!(edited.is_ok());
        assert_eq!(probe.sent(), 1, "the pass is still parked at the send");

        probe.release();
        let done = bounded_for(within, pass)
            .await
            .expect("the refresh completes once released")
            .unwrap();
        assert!(done.is_ok(), "the refresh result");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn saved_update_of_the_leased_credential_waits_for_the_lease() {
    // B-7 (b): a saved update of the leased credential waits on the credential
    // lock, so it cannot finish while the pass is parked; it finishes after.
    bounded(async {
        let rig = rig();
        let probe = Probe::new("codex", true).rotating("c");
        rig.manager.register_executor(probe.clone());
        let pass = {
            let manager = rig.manager.clone();
            tokio::spawn(async move { manager.force_refresh("codex.json").await })
        };
        probe.wait_entered().await;
        let mut changed = (*rig.live()).clone();
        changed.metadata.insert("note".into(), json!("changed"));
        let mut update = {
            let manager = rig.manager.clone();
            tokio::spawn(async move { manager.run_persistence(move |m| m.update(changed)).await })
        };
        // The one place a wall-clock wait is the point: the update must not
        // complete. The lease is only released by the release below.
        let early = bounded_for(Duration::from_secs(1), &mut update).await;
        assert!(
            early.is_err(),
            "the update finished while the lease was held"
        );
        probe.release();
        let within = Duration::from_secs(5);
        bounded_for(within, pass)
            .await
            .expect("the refresh completes once released")
            .unwrap()
            .expect("the refresh result");
        // Whether the update then publishes or asks for a retry is not the point.
        let _ = bounded_for(within, update)
            .await
            .expect("the update completes once the lease is released")
            .unwrap()
            .expect("the update ran");
        assert_eq!(probe.reused(), 0, "a spent token was presented again");
    })
    .await;
}

// ---- B-7 (c): the lock-order audit of the source ----

/// The production part of a source file: everything before its test module.
fn production(src: &str) -> &str {
    let cut = src
        .find("#[cfg(test)]\nmod tests")
        .expect("the source ends with a test module");
    &src[..cut]
}

/// `src` without comments, string literals and character literals, ASCII
/// only, whitespace collapsed and spaces around dots removed, so a token such
/// as `self.lock()` is found however rustfmt broke the chain.
fn code_only(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 1;
            i += 2;
            while i < chars.len() && depth > 0 {
                match (chars[i], chars.get(i + 1)) {
                    ('/', Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    ('*', Some('/')) => {
                        depth -= 1;
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            out.push(' ');
        } else if c == 'r' && (i == 0 || !ident(chars[i - 1])) && {
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            chars.get(j) == Some(&'"')
        } {
            let mut hashes = 0;
            let mut j = i + 1;
            while chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            j += 1;
            while j < chars.len()
                && !(chars[j] == '"' && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')))
            {
                j += 1;
            }
            i = j + 1 + hashes;
            out.push_str("\"\"");
        } else if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            i += 1;
            out.push_str("\"\"");
        } else if c == '\'' {
            if next == Some('\\') {
                i += 3;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
                out.push_str("' '");
            } else if chars.get(i + 2) == Some(&'\'') {
                i += 3;
                out.push_str("' '");
            } else {
                out.push('\'');
                i += 1;
            }
        } else {
            out.push(if c.is_ascii() { c } else { '?' });
            i += 1;
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space && !collapsed.is_empty() {
                collapsed.push(' ');
            }
            space = false;
            collapsed.push(c);
        }
    }
    collapsed.replace(" .", ".").replace(". ", ".")
}

/// How many times `token` occurs in `code`, not as the tail of a longer
/// identifier when it starts like one.
fn token_count(code: &str, token: &str) -> usize {
    let starts_like_ident = token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    code.match_indices(token)
        .filter(|(at, _)| {
            !starts_like_ident
                || code[..*at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        })
        .count()
}

/// The forbidden tokens `text` holds, by name.
fn forbidden_in(scope: &str, text: &str, forbidden: &[&str]) -> Vec<String> {
    forbidden
        .iter()
        .filter(|token| token_count(text, token) > 0)
        .map(|token| format!("{scope}: {token}"))
        .collect()
}

/// The text between the bracket at `open` and its match, brackets excluded.
fn bracketed(code: &str, open: usize) -> &str {
    let bytes = code.as_bytes();
    let (opener, closer) = match bytes[open] {
        b'{' => (b'{', b'}'),
        b'(' => (b'(', b')'),
        other => panic!("not a bracket: {}", other as char),
    };
    let mut depth = 0;
    for (at, byte) in bytes.iter().enumerate().skip(open) {
        if *byte == opener {
            depth += 1;
        } else if *byte == closer {
            depth -= 1;
            if depth == 0 {
                return &code[open + 1..at];
            }
        }
    }
    panic!("an unbalanced bracket");
}

/// The body of every `fn <name>` for which `named` holds.
fn fn_bodies(code: &str, named: impl Fn(&str) -> bool) -> Vec<(String, &str)> {
    let mut found = Vec::new();
    for (at, _) in code.match_indices("fn ") {
        if code[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let name: String = code[at + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !named(&name) {
            continue;
        }
        let brace = at + code[at..].find('{').expect("a function body");
        found.push((name, bracketed(code, brace)));
    }
    found
}

/// The arguments of every `.<call>(` in `code`.
fn call_args<'a>(code: &'a str, call: &str) -> Vec<&'a str> {
    let needle = format!(".{call}(");
    code.match_indices(&needle)
        .map(|(at, _)| bracketed(code, at + needle.len() - 1))
        .collect()
}

/// The text of `src` from `from` to `to`, the latter included.
fn raw_between<'a>(src: &'a str, from: &str, to: &str) -> &'a str {
    let start = src
        .find(from)
        .unwrap_or_else(|| panic!("marker missing: {from}"));
    let end = start
        + src[start..]
            .find(to)
            .unwrap_or_else(|| panic!("marker missing: {to}"))
        + to.len();
    &src[start..end]
}

const LEASED_SECTION_FORBIDDEN: &[&str] = &[
    "mutation_gate",
    "reload_gate",
    "with_adoption",
    "with_persist_save_lock",
    "persist(",
    "persist_bound(",
    "persist_detached",
    "update_internal",
    "register",
    "load(",
    "self.lock()",
];

const LOCKED_SCOPE_FORBIDDEN: &[&str] = &[
    "publish_projections",
    "queue_refresh_reschedule",
    ".await",
    "run_leased",
    "prove(",
    "Handle::from_path",
    "is_same_file",
    "self.lock()",
];

const TAIL_FENCE_FORBIDDEN: &[&str] = &[
    "prove(",
    "Handle::from_path",
    "is_same_file",
    ".await",
    "run_leased",
    "read_back(",
    "std::fs::",
    "fs::",
];

/// Audit 1: nothing between the lease markers takes a manager-wide lock,
/// opens a gate, or persists.
fn audit_leased_section(refresh: &str) -> Vec<String> {
    let section = raw_between(production(refresh), "// lease: held", "// lease: released");
    forbidden_in(
        "leased section",
        &code_only(section),
        LEASED_SECTION_FORBIDDEN,
    )
}

/// Audit 2: every `*_locked` function, `.locked(` closure and
/// `.leased_origin_check(` argument list is short, synchronous and free of
/// I/O. Also returns how many of each it examined.
fn audit_locked_scopes(refresh: &str) -> (Vec<String>, [usize; 3]) {
    let code = code_only(production(refresh));
    let mut violations = Vec::new();
    let bodies = fn_bodies(&code, |name| name.ends_with("_locked"));
    for (name, body) in &bodies {
        violations.extend(forbidden_in(
            &format!("fn {name}"),
            body,
            LOCKED_SCOPE_FORBIDDEN,
        ));
    }
    let closures = call_args(&code, "locked");
    for body in &closures {
        violations.extend(forbidden_in("locked closure", body, LOCKED_SCOPE_FORBIDDEN));
    }
    let checks = call_args(&code, "leased_origin_check");
    for body in &checks {
        violations.extend(forbidden_in(
            "leased_origin_check argument",
            body,
            LOCKED_SCOPE_FORBIDDEN,
        ));
    }
    (violations, [bodies.len(), closures.len(), checks.len()])
}

/// Audit 3: the two fences in `lifecycle.rs` that decide under the state lock.
fn audit_tail_fences(lifecycle: &str) -> Vec<String> {
    let lifecycle = production_or_all(lifecycle);
    let mut violations = Vec::new();
    // Fence 2: the `refused` closure only hands over to `record_refused_tail`.
    let refused = code_only(raw_between(lifecycle, "let mut refused", ".save_bound("));
    violations.extend(forbidden_in(
        "refused closure",
        &refused,
        TAIL_FENCE_FORBIDDEN,
    ));
    if token_count(&refused, "self.lock()") != 0 {
        violations.push("refused closure: self.lock()".into());
    }
    let code = code_only(lifecycle);
    let tails = fn_bodies(&code, |name| name == "record_refused_tail");
    assert_eq!(tails.len(), 1, "record_refused_tail is defined once");
    violations.extend(forbidden_in(
        "record_refused_tail",
        tails[0].1,
        TAIL_FENCE_FORBIDDEN,
    ));
    if token_count(tails[0].1, "self.lock()") != 1 {
        violations.push("record_refused_tail: not exactly one self.lock()".into());
    }
    // Fence 3: the read-back is resolved before the state lock is taken.
    let fence = code_only(raw_between(
        lifecycle,
        "// Fence 3 resolves",
        "live_same && read_same",
    ));
    violations.extend(forbidden_in("fence 3", &fence, TAIL_FENCE_FORBIDDEN));
    if token_count(&fence, "self.lock()") != 1 {
        violations.push("fence 3: not exactly one self.lock()".into());
    }
    violations
}

fn production_or_all(src: &str) -> &str {
    src.find("#[cfg(test)]\nmod tests")
        .map_or(src, |cut| &src[..cut])
}

const REFRESH_SOURCE: &str = include_str!("../refresh.rs");
const LIFECYCLE_SOURCE: &str = include_str!("../lifecycle.rs");

#[test]
fn lock_order_audit_of_the_sources_finds_nothing() {
    // B-7 (c). The counts keep each audit from passing on an empty scan.
    assert_eq!(audit_leased_section(REFRESH_SOURCE), Vec::<String>::new());
    let (violations, [bodies, closures, checks]) = audit_locked_scopes(REFRESH_SOURCE);
    assert_eq!(violations, Vec::<String>::new());
    assert!(
        bodies >= 5,
        "only {bodies} *_locked functions were examined"
    );
    assert!(
        closures >= 5,
        "only {closures} locked closures were examined"
    );
    assert!(checks >= 3, "only {checks} origin checks were examined");
    assert_eq!(audit_tail_fences(LIFECYCLE_SOURCE), Vec::<String>::new());
}

/// `src` with the first `from` replaced by `to`; the text must change.
fn mutated(src: &str, from: &str, to: &str) -> String {
    let changed = src.replacen(from, to, 1);
    assert_ne!(changed, src, "the mutation target is missing: {from}");
    changed
}

#[test]
fn lock_order_audit_fails_on_mutated_copies() {
    // B-7 (c) negative cases: each audit reports a forbidden call that is
    // put into its scope.
    let leased = mutated(
        REFRESH_SOURCE,
        "// lease: released",
        "self.mutation_gate();\n// lease: released",
    );
    assert!(!audit_leased_section(&leased).is_empty(), "leased section");
    let leased = mutated(
        REFRESH_SOURCE,
        "// lease: released",
        "let _ = self.lock();\n// lease: released",
    );
    assert!(
        !audit_leased_section(&leased).is_empty(),
        "leased section lock"
    );

    let body = mutated(
        REFRESH_SOURCE,
        "fn publish_rotation_locked(",
        "fn probe_locked() { run_leased(); }\nfn publish_rotation_locked(",
    );
    assert!(!audit_locked_scopes(&body).0.is_empty(), "locked fn body");
    let closure = mutated(
        REFRESH_SOURCE,
        "self.locked(|s| self.prepare_leased_refresh_locked(",
        "self.locked(|s| { s.prove(); self.prepare_leased_refresh_locked(",
    );
    assert!(
        !audit_locked_scopes(&closure).0.is_empty(),
        "locked closure"
    );
    let check = mutated(
        REFRESH_SOURCE,
        ".leased_origin_check(",
        ".leased_origin_check(Handle::from_path(1), ",
    );
    assert!(!audit_locked_scopes(&check).0.is_empty(), "origin check");

    let refused = mutated(
        LIFECYCLE_SOURCE,
        "self.record_refused_tail(&auth.id",
        "pinned.prove(x); self.record_refused_tail(&auth.id",
    );
    assert!(!audit_tail_fences(&refused).is_empty(), "refused closure");
    let fence = mutated(
        LIFECYCLE_SOURCE,
        "// Fence 3 resolves the read-back outside the state lock.",
        "// Fence 3 resolves the read-back outside the state lock.\nlet _guard = self.lock();",
    );
    assert!(!audit_tail_fences(&fence).is_empty(), "fence 3");
    let tail = mutated(
        LIFECYCLE_SOURCE,
        "let mut guard = self.lock();\n        let state = &mut *guard;\n        let Some(entry) = state.auths.get_mut(id) else {\n            return;\n        };\n        if entry.auth.registration_epoch != epoch || self.permanently_ineligible",
        "let mut guard = self.lock();\n        let _again = self.lock();\n        let state = &mut *guard;\n        let Some(entry) = state.auths.get_mut(id) else {\n            return;\n        };\n        if entry.auth.registration_epoch != epoch || self.permanently_ineligible",
    );
    assert!(!audit_tail_fences(&tail).is_empty(), "record_refused_tail");
}
