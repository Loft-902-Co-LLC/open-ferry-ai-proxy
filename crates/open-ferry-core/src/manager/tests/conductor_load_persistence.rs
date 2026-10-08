// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_load_persistence_test.go
// and sdk/cliproxy/auth/conductor_saved_fields_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A reload and a change being saved never overlap: a reload waits for a
//! blocked save to land before it lists the store, and a change waits for a
//! blocked reload to replace every credential before it is made. Meanwhile
//! other credentials stay readable and selectable, a save may read the
//! manager, and a save doesn't undo what changed while it ran.
//!
//! Deviations from upstream:
//! - Upstream runs each case in a `synctest` bubble, where "blocked" means
//!   every goroutine is durably blocked. Here the operations run on threads,
//!   and "the reload hasn't listed the store" or "the registration hasn't
//!   saved" is checked after a short real wait: a manager that doesn't wait
//!   could pass by luck, but a correct one never fails.
//! - The `Prepare` operation is dropped: `UpdatePreparedAuth` isn't ported
//!   (it saves Meta's minted key).
//! - The stores set no `FileName` and add nothing: an [`AuthStore`] gets a
//!   copy it can't change. The saved and loaded credentials are compared
//!   field by field, and "a new registration survived the reload" is checked
//!   by the store holding it.
//! - There is no scheduler index to compare with the reloaded credential;
//!   B is selected through `Selection::pick_next_mixed` for its provider,
//!   as upstream's `SelectAuth` does.
//! - `TestManagerSaveReadCallbackAndCustomFields` keeps the store reading
//!   the manager during a save; the store's edits are dropped.
//! - `TestManagerSavedFieldsSkipPersistence` drops `plugin-virtual`: plugin
//!   virtual credentials aren't ported.
//! - `TestManagerSavePreservesConcurrentRefreshState` drops the check that
//!   the store's enrichment was kept.
//! - Not ported: `TestManagerSavedFieldsPublished`,
//!   `TestManagerSavedFieldsConcurrentLifecycle` and
//!   `TestMergeAuthSaveDeltaMapChanges`, about merging what the store added
//!   back into the credential, which can't happen here;
//!   `TestManagerSavedFieldsMetaMintPublication` (Meta's key mint); and
//!   `conductor_cancellation_test.go`, whose lock waits are cancelled
//!   through contexts.

use super::support::*;

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::auth::{Auth, AuthStore, Status};
use crate::exec::ExecError;
use crate::manager::models::Resolver;
use crate::manager::select::{PickArgs, Selection};
use crate::manager::{CallResult, Manager, Settings, lock};

/// How long a step may take before the test gives up on it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long a blocked step is given to show it isn't blocked.
const SETTLE: Duration = Duration::from_millis(150);

/// The model B serves.
const MODEL: &str = "audit-model";

/// A one-way gate: shut until opened, then open for good.
#[derive(Default)]
struct Latch {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Latch {
    fn open(&self) {
        *lock(&self.open) = true;
        self.opened.notify_all();
    }

    fn is_open(&self) -> bool {
        *lock(&self.open)
    }

    /// Waits for the latch to open; false when it didn't within
    /// [`TIMEOUT`].
    fn wait(&self) -> bool {
        let guard = lock(&self.open);
        let (guard, _) = self
            .opened
            .wait_timeout_while(guard, TIMEOUT, |open| !*open)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard
    }
}

/// Upstream's `reloadAuthStore`: keeps records apart from its blocked I/O.
#[derive(Default)]
struct ReloadStore {
    records: Mutex<BTreeMap<String, Auth>>,
    block_saves: bool,
    save_entered: Latch,
    save_release: Latch,
    block_list: bool,
    list_entered: Latch,
    list_release: Latch,
}

impl ReloadStore {
    fn put(&self, auth: &Auth) {
        lock(&self.records).insert(auth.id.clone(), auth.clone());
    }

    fn record(&self, id: &str) -> Option<Auth> {
        lock(&self.records).get(id).cloned()
    }
}

impl AuthStore for ReloadStore {
    fn list(&self) -> io::Result<Vec<Auth>> {
        let items = lock(&self.records).values().cloned().collect();
        self.list_entered.open();
        if self.block_list {
            self.list_release.wait();
        }
        Ok(items)
    }

    fn save(&self, auth: &Auth) -> io::Result<String> {
        if self.block_saves {
            self.save_entered.open();
            self.save_release.wait();
        }
        self.put(auth);
        Ok(format!("{}.json", auth.id))
    }

    fn delete(&self, _id: &str) -> io::Result<()> {
        Ok(())
    }
}

/// A manager saving to `store`, with an executor for `audit-b` and `audit`.
fn manager_with(store: Arc<dyn AuthStore>) -> (Manager, Arc<FakeModels>) {
    let models = Arc::new(FakeModels::default());
    let manager = Manager::with_clock(
        Settings::default(),
        models.clone(),
        Some(store),
        TestClock::new().clock(),
    );
    manager.register_executor(FakeExecutor::new("audit-b"));
    manager.register_executor(FakeExecutor::new("audit"));
    (manager, models)
}

/// Registers B on `provider`, serving [`MODEL`], without saving it.
fn register_b(manager: &Manager, models: &FakeModels, provider: &str) -> Arc<Auth> {
    models.register("B", &[MODEL]);
    let mut b = auth("B", provider);
    b.status = Status::Active;
    manager.register_unsaved(b).expect("register B")
}

/// Picks a credential for [`MODEL`] on `provider` as upstream's
/// `SelectAuth` does.
fn select_auth(
    manager: &Manager,
    models: &FakeModels,
    provider: &str,
) -> Result<Arc<Auth>, ExecError> {
    let now = manager.now();
    let (settings, oauth) = manager.resolver_parts();
    let mut guard = manager.lock();
    let state = &mut *guard;
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models,
        resolver: Resolver {
            settings: &settings,
            oauth: &oauth,
        },
        strategy: settings.routing_strategy,
        now,
    };
    let tried = HashSet::new();
    let args = PickArgs {
        model: MODEL,
        pinned: "",
        downstream_websocket: false,
        eligibility: Default::default(),
        tried: &tried,
    };
    selection
        .pick_next_mixed(&mut state.selector, &providers(&[provider]), &args)
        .map(|picked| picked.auth)
}

/// Upstream's `assertUnrelatedAuthReadable`: B can be read and selected
/// while the store is blocked.
fn assert_unrelated_auth_readable(
    manager: &Manager,
    models: &Arc<FakeModels>,
    provider: &str,
    context: &str,
) {
    let (tx, rx) = mpsc::channel();
    let manager = manager.clone();
    let models = models.clone();
    let provider = provider.to_owned();
    thread::spawn(move || {
        let result = if manager.get("B").is_none() {
            Err("B disappeared".to_owned())
        } else {
            match select_auth(&manager, &models, &provider) {
                Ok(selected) if selected.id == "B" => Ok(()),
                Ok(selected) => Err(format!("did not select B but {}", selected.id)),
                Err(err) => Err(format!("did not select B: {err}")),
            }
        };
        let _ = tx.send(result);
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("{context}: {err}"),
        Err(_) => panic!("{context}: storage I/O blocked reading/selecting unrelated B"),
    }
}

/// Waits for a thread's result.
fn join<T>(handle: thread::JoinHandle<T>, context: &str) -> T {
    match handle.join() {
        Ok(value) => value,
        Err(_) => panic!("{context}: thread panicked"),
    }
}

/// Upstream compares the saved and loaded credentials whole; `Auth` has no
/// equality, so the fields a save carries are compared.
fn assert_same_saved_fields(saved: &Auth, got: &Auth, context: &str) {
    assert_eq!(
        (
            &saved.provider,
            &saved.prefix,
            &saved.file_name,
            &saved.index,
            &saved.label,
            &saved.status,
            &saved.status_message,
            saved.disabled,
            saved.unavailable,
            &saved.proxy_url,
        ),
        (
            &got.provider,
            &got.prefix,
            &got.file_name,
            &got.index,
            &got.label,
            &got.status,
            &got.status_message,
            got.disabled,
            got.unavailable,
            &got.proxy_url,
        ),
        "{context}: store and manager diverged"
    );
    assert_eq!(
        (
            &saved.attributes,
            &saved.metadata,
            saved.created_at,
            saved.updated_at,
            saved.last_refreshed_at,
            saved.next_refresh_after,
            saved.next_retry_after,
            saved.success,
            saved.failed,
        ),
        (
            &got.attributes,
            &got.metadata,
            got.created_at,
            got.updated_at,
            got.last_refreshed_at,
            got.next_refresh_after,
            got.next_retry_after,
            got.success,
            got.failed,
        ),
        "{context}: store and manager diverged"
    );
}

/// Ports `TestManagerBlockedSaveSerializesLoad`.
#[test]
fn manager_blocked_save_serializes_load() {
    for operation in ["Register", "ReRegister", "Update", "Refresh", "MarkResult"] {
        let store = Arc::new(ReloadStore {
            block_saves: true,
            ..ReloadStore::default()
        });
        let (manager, models) = manager_with(store.clone());
        let b = register_b(&manager, &models, "audit-b");
        store.put(&b);
        let mut base = auth_with_metadata("A", "audit-a", json!({"access_token": "old"}));
        base.status = Status::Active;
        let mut base = Arc::new(base);
        if operation != "Register" {
            base = manager
                .register_unsaved((*base).clone())
                .expect("register A");
            store.put(&base);
        }
        let mut updated = (*base).clone();
        updated
            .metadata
            .insert("access_token".into(), json!("minted"));

        let worker = {
            let manager = manager.clone();
            let base = base.clone();
            thread::spawn(move || -> Result<(), String> {
                let result = match operation {
                    "Register" | "ReRegister" => manager.register(updated).map(drop),
                    "Update" => manager.update(updated).map(drop),
                    "Refresh" => manager
                        .update_refreshed(&base, base.registration_epoch, updated)
                        .map(drop),
                    _ => {
                        manager.mark_result(&CallResult {
                            auth_id: base.id.clone(),
                            provider: base.provider.clone(),
                            success: true,
                            ..CallResult::default()
                        });
                        Ok(())
                    }
                };
                result.map_err(|err| err.to_string())
            })
        };
        assert!(store.save_entered.wait(), "{operation}: no save started");

        let loader = {
            let manager = manager.clone();
            thread::spawn(move || manager.load().map_err(|err| err.to_string()))
        };
        thread::sleep(SETTLE);
        assert!(
            !store.list_entered.is_open(),
            "{operation}: Load read stale records before Save/publication completed"
        );
        assert_unrelated_auth_readable(&manager, &models, "audit-b", operation);

        store.save_release.open();
        if let Err(err) = join(worker, operation) {
            panic!("{operation}: save/publication failed: {err}");
        }
        if let Err(err) = join(loader, operation) {
            panic!("{operation}: load failed: {err}");
        }

        let got = manager
            .get(&base.id)
            .unwrap_or_else(|| panic!("{operation}: Load lost the saved credential"));
        let saved = store
            .record(&base.id)
            .unwrap_or_else(|| panic!("{operation}: nothing saved"));
        assert!(
            got.registration_epoch > saved.registration_epoch,
            "{operation}: Load did not run after save/publication"
        );
        assert_eq!(
            saved.metadata.get("access_token"),
            Some(&json!(if operation == "MarkResult" {
                "old"
            } else {
                "minted"
            })),
            "{operation}"
        );
        assert_same_saved_fields(&saved, &got, operation);
    }
}

/// Ports `TestManagerBlockedLoadSerializesNewRegistration`.
#[test]
fn manager_blocked_load_serializes_new_registration() {
    let store = Arc::new(ReloadStore {
        block_saves: true,
        block_list: true,
        ..ReloadStore::default()
    });
    let (manager, models) = manager_with(store.clone());
    let b = register_b(&manager, &models, "audit-b");
    store.put(&b);

    let loader = {
        let manager = manager.clone();
        thread::spawn(move || manager.load().map_err(|err| err.to_string()))
    };
    assert!(store.list_entered.wait(), "Load never listed the store");

    let registrar = {
        let manager = manager.clone();
        thread::spawn(move || {
            manager
                .register(auth_with_metadata("new", "audit", json!({"type": "audit"})))
                .map(drop)
                .map_err(|err| err.to_string())
        })
    };
    thread::sleep(SETTLE);
    assert!(
        !store.save_entered.is_open(),
        "new registration overtook Load's whole-map replacement"
    );
    assert_unrelated_auth_readable(&manager, &models, "audit-b", "blocked load");

    store.list_release.open();
    if let Err(err) = join(loader, "load") {
        panic!("load failed: {err}");
    }
    assert!(store.save_entered.wait(), "the registration never saved");
    store.save_release.open();
    if let Err(err) = join(registrar, "register") {
        panic!("register failed: {err}");
    }
    assert!(
        manager.get("new").is_some(),
        "Load discarded a new registration"
    );
    assert!(
        store.record("new").is_some(),
        "the registration wasn't saved"
    );
}

/// Ports `TestManagerBlockedSaveAllowsUnrelatedCredential`.
#[test]
fn manager_blocked_save_allows_unrelated_credential() {
    for operation in ["Register", "Update"] {
        let store = Arc::new(ReloadStore {
            block_saves: true,
            ..ReloadStore::default()
        });
        let (manager, models) = manager_with(store.clone());
        register_b(&manager, &models, "audit");
        let mut input = auth_with_metadata("A", "audit-a", json!({"type": "audit-a"}));
        input.status = Status::Active;
        if operation == "Update" {
            manager.register_unsaved(input.clone()).expect("register A");
        }
        let worker = {
            let manager = manager.clone();
            thread::spawn(move || {
                if operation == "Register" {
                    let _ = manager.register(input);
                } else {
                    let _ = manager.update(input);
                }
            })
        };
        assert!(store.save_entered.wait(), "{operation}: no save started");
        assert_unrelated_auth_readable(&manager, &models, "audit", operation);
        store.save_release.open();
        join(worker, operation);
    }
}

/// A store that reads the manager while it saves.
#[derive(Default)]
struct CallbackStore {
    manager: Mutex<Option<Manager>>,
    reads: AtomicUsize,
}

impl AuthStore for CallbackStore {
    fn list(&self) -> io::Result<Vec<Auth>> {
        Ok(Vec::new())
    }

    fn save(&self, auth: &Auth) -> io::Result<String> {
        let manager = lock(&self.manager).clone();
        if let Some(manager) = manager {
            let _ = manager.get(&auth.id);
            let _ = manager.list();
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        Ok(String::new())
    }

    fn delete(&self, _id: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Ports `TestManagerSaveReadCallbackAndCustomFields`, without the store's
/// edits: an [`AuthStore`] can't change what it saves.
#[test]
fn manager_save_read_callback() {
    let store = Arc::new(CallbackStore::default());
    let (manager, _models) = manager_with(store.clone());
    *lock(&store.manager) = Some(manager.clone());
    let (tx, rx) = mpsc::channel();
    {
        let manager = manager.clone();
        thread::spawn(move || {
            for operation in ["Register", "Update"] {
                let mut input = auth_with_metadata("callback", "audit", json!({"remove": true}));
                input.status = Status::Active;
                let result = if operation == "Register" {
                    manager.register(input).map(Some)
                } else {
                    manager.update(input)
                };
                let outcome = match result {
                    Ok(Some(result)) => match manager.get("callback") {
                        Some(got) if Arc::ptr_eq(&got, &result) => Ok(()),
                        Some(_) => Err(format!("{operation}: return differs from manager")),
                        None => Err(format!("{operation}: credential missing")),
                    },
                    Ok(None) => Err(format!("{operation}: nothing to update")),
                    Err(err) => Err(format!("{operation}: {err}")),
                };
                let _ = tx.send(outcome);
            }
        });
    }
    for _ in 0..2 {
        match rx.recv_timeout(TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("{err}"),
            Err(_) => panic!("a save that reads the manager deadlocked"),
        }
    }
    assert_eq!(store.reads.load(Ordering::SeqCst), 2);
    // The store's handle would keep the manager alive.
    lock(&store.manager).take();
}

/// Ports `TestManagerSavedFieldsSkipPersistence`.
#[test]
fn manager_saved_fields_skip_persistence() {
    for mode in ["context", "nil-metadata", "runtime-only", "config-api-key"] {
        let h = Harness::with_store(Settings::default());
        let mut input = auth_with_metadata(mode, "audit", json!({"type": "audit"}));
        input.status = Status::Active;
        match mode {
            "nil-metadata" => input.metadata.clear(),
            "runtime-only" => {
                input.attributes = BTreeMap::from([("runtime_only".into(), "true".into())]);
            }
            "config-api-key" => {
                input.attributes = BTreeMap::from([
                    ("source".into(), "config".into()),
                    ("api_key".into(), "test".into()),
                ]);
            }
            _ => {}
        }
        for operation in ["Register", "Update"] {
            let unsaved = mode == "context";
            let result = match (operation, unsaved) {
                ("Register", true) => h.manager.register_unsaved(input.clone()).map(Some),
                ("Register", false) => h.manager.register(input.clone()).map(Some),
                (_, true) => h.manager.update_unsaved(input.clone()),
                (_, false) => h.manager.update(input.clone()),
            };
            let result = match result {
                Ok(Some(result)) => result,
                Ok(None) => panic!("{mode} {operation}: nothing to update"),
                Err(err) => panic!("{mode} {operation}: {err}"),
            };
            let got = h.get(mode);
            assert!(
                Arc::ptr_eq(&got, &result),
                "{mode} {operation}: skip return differs from manager"
            );
        }
        assert_eq!(h.store.save_count(), 0, "{mode}: unexpected Save");
    }
}

/// Ports `TestManagerSavePreservesConcurrentRefreshState`, less the store's
/// enrichment.
#[test]
fn manager_save_preserves_concurrent_refresh_state() {
    for operation in ["Update", "MarkResult"] {
        let store = Arc::new(ReloadStore {
            block_saves: true,
            ..ReloadStore::default()
        });
        let (manager, _models) = manager_with(store.clone());
        let mut base = auth_with_metadata("runtime", "audit", json!({"type": "audit"}));
        base.status = Status::Active;
        let base = manager.register_unsaved(base).expect("register");
        let worker = {
            let manager = manager.clone();
            let base = base.clone();
            thread::spawn(move || {
                if operation == "Update" {
                    let _ = manager.update((*base).clone());
                } else {
                    manager.mark_result(&CallResult {
                        auth_id: base.id.clone(),
                        provider: base.provider.clone(),
                        success: true,
                        ..CallResult::default()
                    });
                }
            })
        };
        assert!(store.save_entered.wait(), "{operation}: no save started");
        let (tx, rx) = mpsc::channel();
        {
            let manager = manager.clone();
            let base = base.clone();
            thread::spawn(move || {
                let job = manager.mark_refresh_pending(
                    0,
                    &base.id,
                    base.registration_epoch,
                    manager.now(),
                );
                let _ = tx.send(job.map(|job| job.pending_until));
            });
        }
        let pending_until = match rx.recv_timeout(TIMEOUT) {
            Ok(Some(pending_until)) => pending_until,
            Ok(None) => panic!("{operation}: failed to schedule concurrent refresh"),
            Err(_) => panic!("{operation}: a blocked save held the credential"),
        };
        store.save_release.open();
        join(worker, operation);
        let got = manager.get(&base.id).expect("credential");
        assert_eq!(
            got.next_refresh_after,
            Some(pending_until),
            "{operation}: save lost concurrent refresh scheduling"
        );
        if operation == "MarkResult" {
            assert_eq!(got.success, 1, "{operation}: save lost result counter");
        }
    }
}
