//! Not upstream's: what a hand-over to a new version asks of the manager.
//!
//! - A refresh is one task that owns an operation of the gate. It keeps the
//!   token the provider rotated through a switch, and a refresh that waited
//!   for the lock declines once the switch has begun.
//! - A full `load()` while a refresh is held loses the rotated token. That
//!   is why a resume with the lease still held reloads nothing.
//! - A worker that isn't the active version re-reads a credential after a
//!   401, and never refreshes it.
//! - The cooldown store freezes with a last save and thaws with or without a
//!   restore.
//!
//! The provider is a fake that rotates the refresh token on every refresh
//! and flags a refresh token that is presented twice, as a real provider
//! revokes the whole grant on reuse.

#[cfg(feature = "blue-green-test")]
mod log_barriers;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_core::future::BoxFuture;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Semaphore;

use super::cooldown_state_store::{failure, success};
use super::support::*;
use crate::auth::{Auth, AuthStore, FileStore, Status};
use crate::exec::{ErrorKind, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::gate::{ActiveGate, GateState};
use crate::manager::cooldown_store::{
    Record, StateStore, StoreError, Thaw, flush, freeze, install_store, restore_now, set_debounce,
    snapshot, standby, thaw,
};
use crate::manager::credential::{access_token, refresh_token};
use crate::manager::{EditError, Manager, ManagerError, Settings, lock};

const PROVIDER: &str = "codex";
const ID: &str = "codex-a.json";
/// The time the supervisor gives a switch to quiesce (`HANDOVER_BUDGET`).
/// The gate has no limit of its own: the caller bounds the wait.
const BUDGET: Duration = Duration::from_secs(60);

/// What the fake provider has handed out.
#[derive(Default)]
struct Rotation {
    current: String,
    consumed: HashSet<String>,
    calls: usize,
    reused: usize,
}

/// A provider that rotates the refresh token on every refresh and flags the
/// reuse of one it already rotated. It can hold a refresh after it rotated
/// and before it answers, which is the window a switch can fall in.
struct RotatingProvider {
    state: Mutex<Rotation>,
    hold: AtomicBool,
    rotated: Semaphore,
    release: Semaphore,
}

impl RotatingProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Rotation {
                current: "refresh-0".into(),
                ..Rotation::default()
            }),
            hold: AtomicBool::new(false),
            rotated: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    /// Holds the next refresh after it rotated.
    fn hold(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    /// Waits until a refresh has rotated.
    async fn wait_rotated(&self) {
        self.rotated.acquire().await.expect("open").forget();
    }

    /// Lets a held refresh answer.
    fn release(&self) {
        self.release.add_permits(1);
    }

    fn calls(&self) -> usize {
        lock(&self.state).calls
    }

    /// How many refresh tokens were presented after they were rotated.
    fn reused(&self) -> usize {
        lock(&self.state).reused
    }

    /// The refresh token the provider will accept.
    fn current(&self) -> String {
        lock(&self.state).current.clone()
    }
}

impl ProviderExecutor for RotatingProvider {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn execute(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not used")) })
    }

    fn execute_stream(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not used")) })
    }

    fn count_tokens(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "not used")) })
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move {
            let presented = refresh_token(&auth);
            let n = {
                let mut state = lock(&self.state);
                state.calls += 1;
                if !state.consumed.insert(presented.clone()) {
                    state.reused += 1;
                    return Err(ExecError::upstream(
                        400,
                        r#"{"error": "invalid_grant", "error_description": "refresh token reused"}"#,
                    ));
                }
                if presented != state.current {
                    return Err(ExecError::upstream(400, r#"{"error": "invalid_grant"}"#));
                }
                let n = state.calls;
                state.current = format!("refresh-{n}");
                n
            };
            self.rotated.add_permits(1);
            if self.hold.load(Ordering::SeqCst) {
                self.release.acquire().await.expect("open").forget();
            }
            let mut updated = auth.as_ref().clone();
            updated
                .metadata
                .insert("access_token".into(), json!(format!("access-{n}")));
            updated
                .metadata
                .insert("refresh_token".into(), json!(format!("refresh-{n}")));
            Ok(updated)
        })
    }
}

fn credential() -> Auth {
    let mut auth = auth_with_metadata(
        ID,
        PROVIDER,
        json!({"type": PROVIDER, "access_token": "access-0", "refresh_token": "refresh-0"}),
    );
    auth.file_name = ID.into();
    auth.status = Status::Active;
    auth
}

/// A manager saving to a real credential file under a gate, with one
/// credential registered.
struct Env {
    dir: TempDir,
    files: Arc<FileStore>,
    manager: Manager,
    provider: Arc<RotatingProvider>,
    gate: ActiveGate,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let files = Arc::new(FileStore::new(dir.path()));
        let gate = ActiveGate::open();
        files.set_gate(gate.clone());
        let store: Arc<dyn AuthStore> = files.clone();
        let manager = Manager::with_clock(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store),
            TestClock::new().clock(),
        );
        manager.set_gate(gate.clone());
        let provider = RotatingProvider::new();
        manager.register_executor(provider.clone());
        manager.register(credential()).expect("register");
        Self {
            dir,
            files,
            manager,
            provider,
            gate,
        }
    }

    /// The credential file as the next process would read it.
    fn on_disk(&self) -> serde_json::Value {
        let bytes = std::fs::read(self.dir.path().join(ID)).expect("credential file");
        serde_json::from_slice(&bytes).expect("credential json")
    }

    fn token(&self) -> String {
        access_token(&self.manager.get(ID).expect("credential"))
    }

    /// The (registration epoch, generation) of the credential.
    fn versions(&self) -> (u64, u64) {
        let state = self.manager.lock();
        state
            .auths
            .get(ID)
            .map(|entry| (entry.auth.registration_epoch, entry.auth.generation))
            .unwrap_or_default()
    }

    /// Starts a forced refresh on its own task.
    fn refresh(&self) -> tokio::task::JoinHandle<Result<Arc<Auth>, ManagerError>> {
        let manager = self.manager.clone();
        tokio::spawn(async move { manager.force_refresh(ID).await })
    }

    /// Starts closing the gate on its own task.
    fn close(&self) -> tokio::task::JoinHandle<Result<(), crate::gate::CloseError>> {
        let gate = self.gate.clone();
        let epoch = gate.epoch();
        tokio::spawn(async move { gate.close(epoch).await })
    }
}

// Not upstream's: T1. The provider rotated the refresh token and the answer
// is held. The switch begins, the budget runs out and the gate reopens with
// no reload. The refresh then finishes: the rotated token is saved, the
// registration epoch is the one it began with, and the provider never saw a
// token twice.
#[tokio::test(start_paused = true)]
async fn a_refresh_held_past_the_budget_saves_its_rotated_token() {
    let env = Env::new();
    assert_eq!(env.on_disk()["refresh_token"], "refresh-0");
    let (epoch, _) = env.versions();
    env.provider.hold();
    let refresh = env.refresh();
    env.provider.wait_rotated().await;

    let closing = tokio::time::timeout(BUDGET, env.gate.close(env.gate.epoch())).await;
    assert!(closing.is_err(), "the held refresh keeps the gate open");
    assert_eq!(env.gate.state(), GateState::Quiescing);
    // New work is refused while the switch waits.
    let refused = env.manager.force_refresh(ID).await;
    assert!(matches!(refused, Err(ManagerError::NotActive(_))));

    env.gate.reopen();
    assert_eq!(env.gate.state(), GateState::Open);
    assert_eq!(env.versions().0, epoch, "no reload");
    env.provider.release();

    let refreshed = refresh.await.expect("task").expect("the refresh finishes");
    assert_eq!(access_token(&refreshed), "access-1");
    assert_eq!(env.token(), "access-1");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-1");
    assert_eq!(env.on_disk()["access_token"], "access-1");
    assert_eq!(env.versions().0, epoch, "no stale registration epoch");
    assert_eq!((env.provider.calls(), env.provider.reused()), (1, 0));
}

// Not upstream's: a refresh that finishes while the gate is Quiescing saves
// its token, and only then does the gate close.
#[tokio::test(start_paused = true)]
async fn a_refresh_finishing_during_the_switch_saves_before_the_gate_closes() {
    let env = Env::new();
    env.provider.hold();
    let refresh = env.refresh();
    env.provider.wait_rotated().await;
    let closing = env.close();
    settle().await;
    assert_eq!(env.gate.state(), GateState::Quiescing);
    assert!(!closing.is_finished());

    env.provider.release();
    refresh.await.expect("task").expect("the refresh finishes");
    closing.await.expect("task").expect("closes");
    assert_eq!(env.gate.state(), GateState::Closed);
    assert_eq!(env.on_disk()["refresh_token"], "refresh-1");
    assert_eq!(env.token(), "access-1");
}

// Not upstream's: a refresh that waited for the lock while another ran gets
// it after the switch began, and declines before it calls the provider.
#[tokio::test(start_paused = true)]
async fn a_waiter_that_gets_the_lock_after_the_switch_began_never_calls_the_provider() {
    let env = Env::new();
    env.provider.hold();
    let first = env.refresh();
    env.provider.wait_rotated().await;
    let second = env.refresh();
    settle().await;
    let closing = env.close();
    settle().await;
    assert_eq!(env.gate.state(), GateState::Quiescing);

    env.provider.release();
    first.await.expect("task").expect("the first finishes");
    let declined = second.await.expect("task");
    assert!(matches!(declined, Err(ManagerError::NotActive(_))));
    closing.await.expect("task").expect("closes");
    assert_eq!(env.provider.calls(), 1, "the waiter never called it");
    assert_eq!(env.provider.reused(), 0);
    assert_eq!(env.on_disk()["refresh_token"], "refresh-1");
}

// Not upstream's: the caller giving up doesn't cancel the refresh. The task
// owns its operation, so the rotated token is saved anyway.
#[tokio::test(start_paused = true)]
async fn a_refresh_whose_caller_gave_up_still_saves() {
    let env = Env::new();
    env.provider.hold();
    let refresh = env.refresh();
    env.provider.wait_rotated().await;
    refresh.abort();
    settle().await;
    env.provider.release();
    settle().await;
    env.close().await.expect("task").expect("closes");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-1");
    assert_eq!(env.provider.reused(), 0);
}

// Not upstream's: the case the rule above is for. A full `load()` while the
// refresh is held bumps the registration epoch, so the finished refresh is
// stale and the rotated token is lost; the file keeps a refresh token the
// provider has already rotated, and the next refresh is flagged as reuse.
// This is why a resume with the lease still held does not reload.
#[tokio::test(start_paused = true)]
async fn a_full_load_during_a_held_refresh_loses_the_rotated_token() {
    let env = Env::new();
    let (epoch, _) = env.versions();
    env.provider.hold();
    let refresh = env.refresh();
    env.provider.wait_rotated().await;

    env.manager.load().expect("load");
    assert!(
        env.versions().0 > epoch,
        "load registers the credential again"
    );
    env.provider.release();

    let lost = refresh.await.expect("task");
    let message = lost.expect_err("the refresh is stale").to_string();
    assert!(message.contains("stale registration epoch"), "{message}");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-0", "not saved");
    assert_ne!(env.provider.current(), "refresh-0", "the provider moved on");

    let again = env.manager.force_refresh(ID).await;
    assert!(again.is_err());
    assert_eq!(env.provider.reused(), 1, "the old token is presented again");
}

/// What another process leaves in the credential file: the tokens a refresh
/// by the active version produced.
fn rotate_on_disk(env: &Env) {
    let mut file = env.on_disk();
    file["access_token"] = json!("access-9");
    file["refresh_token"] = json!("refresh-9");
    std::fs::write(
        env.dir.path().join(ID),
        serde_json::to_vec(&file).expect("json"),
    )
    .expect("write");
}

// Not upstream's: D3. After a 401 a worker whose gate is closed reads the
// credential from the file again, takes the new token in memory and tries
// again. It never calls the provider and never writes the file.
#[tokio::test(start_paused = true)]
async fn a_401_on_a_closed_worker_adopts_the_tokens_on_disk_without_refreshing() {
    let env = Env::new();
    env.close().await.expect("task").expect("closes");
    rotate_on_disk(&env);
    let before = std::fs::read(env.dir.path().join(ID)).expect("file");

    let failed = env.manager.get(ID).expect("credential");
    let adopted = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await
        .expect("the token on disk is taken");
    assert_eq!(access_token(&adopted), "access-9");
    assert_eq!(env.token(), "access-9");
    assert_eq!(env.provider.calls(), 0, "never refreshes");
    assert_eq!(
        std::fs::read(env.dir.path().join(ID)).expect("file"),
        before,
        "never writes"
    );

    // The file has nothing newer than what failed: the call fails.
    let again = env
        .manager
        .try_refresh_after_unauthorized(&adopted, &ExecError::upstream(401, "401"), false)
        .await;
    assert!(again.is_none());
    assert_eq!(env.provider.calls(), 0);
}

// Not upstream's: the same while the gate is only Quiescing.
#[tokio::test(start_paused = true)]
async fn a_401_on_a_quiescing_worker_does_not_refresh_either() {
    let env = Env::new();
    let held = env.gate.begin_operation().expect("operation");
    let closing = env.close();
    settle().await;
    assert_eq!(env.gate.state(), GateState::Quiescing);
    rotate_on_disk(&env);

    let failed = env.manager.get(ID).expect("credential");
    let adopted = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await
        .expect("the token on disk is taken");
    assert_eq!(access_token(&adopted), "access-9");
    assert_eq!(env.provider.calls(), 0);
    drop(held);
    closing.await.expect("task").expect("closes");
}

// Not upstream's: a gate that is open refreshes after a 401 as before.
#[tokio::test(start_paused = true)]
async fn a_401_on_an_open_worker_refreshes() {
    let env = Env::new();
    let failed = env.manager.get(ID).expect("credential");
    let refreshed = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await
        .expect("refreshed");
    assert_eq!(access_token(&refreshed), "access-1");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-1");
}

/// A cooldown store that keeps what it is given and can refuse a save.
#[derive(Default)]
struct SwitchStore {
    records: Mutex<Vec<Record>>,
    refuse: AtomicBool,
    saves: AtomicUsize,
    loads: AtomicUsize,
}

impl SwitchStore {
    fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }

    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    fn refuse(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::SeqCst);
    }

    /// Replaces the saved records, as another process writing the file.
    fn overwrite(&self, records: Vec<Record>) {
        *lock(&self.records) = records;
    }

    /// The credentials the saved records belong to (a credential has a
    /// record for itself and one for each model).
    fn saved_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.records)
            .iter()
            .map(|record| record.auth_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

impl StateStore for SwitchStore {
    fn load(&self) -> Result<Vec<Record>, StoreError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(lock(&self.records).clone())
    }

    fn save(&self, records: &[Record], _now: crate::auth::Timestamp) -> Result<(), StoreError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(StoreError("save refused".into()));
        }
        self.saves.fetch_add(1, Ordering::SeqCst);
        *lock(&self.records) = records.to_vec();
        Ok(())
    }
}

/// A manager with two credentials and `store` installed and restored from,
/// saving only when flushed.
fn cooldown_manager(store: &Arc<SwitchStore>) -> Harness {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    restore_now(&h.manager);
    for id in ["auth-1", "auth-2"] {
        let mut auth = auth(id, "xai");
        auth.status = Status::Active;
        h.manager.register_unsaved(auth).expect("register");
    }
    h
}

fn rate_limit(h: &Harness, id: &str) {
    h.manager
        .mark_result(&failure(id, "xai", "m1", 429, "rate limited"));
}

// Not upstream's: a freeze whose last save fails reports it and leaves the
// store as it was, enabled and still holding the change, so the next flush
// saves it.
#[tokio::test(start_paused = true)]
async fn a_freeze_that_cannot_save_leaves_the_store_enabled() {
    let store = Arc::new(SwitchStore::default());
    let h = cooldown_manager(&store);
    rate_limit(&h, "auth-1");

    store.refuse(true);
    let err = freeze(&h.manager).expect_err("the save is refused");
    assert!(err.to_string().contains("could not be saved"), "{err}");
    assert_eq!(store.saves(), 0);

    store.refuse(false);
    flush(&h.manager);
    assert_eq!(store.saves(), 1, "still enabled, and still dirty");
    assert_eq!(store.saved_ids(), ["auth-1"]);
}

// Not upstream's: a freeze saves, then nothing more is saved or read until
// the thaw. A thaw that keeps the lease reads nothing and saves the changes
// made meanwhile.
#[tokio::test(start_paused = true)]
async fn a_thaw_that_keeps_the_lease_does_not_read_the_file() {
    let store = Arc::new(SwitchStore::default());
    let h = cooldown_manager(&store);
    let loads = store.loads();
    rate_limit(&h, "auth-1");

    freeze(&h.manager).expect("freeze");
    assert_eq!(store.saves(), 1, "the last save");
    rate_limit(&h, "auth-2");
    flush(&h.manager);
    assert_eq!(store.saves(), 1, "nothing is saved while frozen");

    thaw(&h.manager, Thaw::Keep);
    flush(&h.manager);
    assert_eq!(store.saves(), 2, "the change made meanwhile is saved");
    assert_eq!(store.loads(), loads, "the file is not read");
    assert_eq!(store.saved_ids(), ["auth-1", "auth-2"]);
}

// Not upstream's: a thaw after the lease was given up restores from the
// file, which the other process may have written.
#[tokio::test(start_paused = true)]
async fn a_thaw_after_the_lease_was_released_restores_from_the_file() {
    let store = Arc::new(SwitchStore::default());
    let h = cooldown_manager(&store);
    let loads = store.loads();
    rate_limit(&h, "auth-1");
    freeze(&h.manager).expect("freeze");

    // The other process rests auth-2 and writes the file.
    let other_store = Arc::new(SwitchStore::default());
    let other = cooldown_manager(&other_store);
    rate_limit(&other, "auth-2");
    store.overwrite(snapshot(&other.manager, other.now()));

    thaw(&h.manager, Thaw::Restore);
    assert_eq!(store.loads(), loads + 1, "the file is read once");
    let rested: Vec<String> = snapshot(&h.manager, h.now())
        .into_iter()
        .map(|record| record.auth_id)
        .collect();
    assert!(rested.iter().any(|id| id == "auth-2"), "{rested:?}");
}

// Not upstream's: the file is the cooldown state once the lease was given
// up. A cooldown this worker had when it handed over, and the other process
// has since lifted, is gone after the restore, and is not written back.
#[tokio::test(start_paused = true)]
async fn a_restore_makes_the_saved_cooldowns_authoritative() {
    let store = Arc::new(SwitchStore::default());
    let h = cooldown_manager(&store);
    rate_limit(&h, "auth-1");
    freeze(&h.manager).expect("freeze");
    assert_eq!(store.saved_ids(), ["auth-1"]);
    assert!(
        snapshot(&h.manager, h.now())
            .iter()
            .any(|record| record.auth_id == "auth-1")
    );

    // The other process lifted every cooldown and saved the file.
    store.overwrite(Vec::new());
    thaw(&h.manager, Thaw::Restore);
    let kept = snapshot(&h.manager, h.now());
    assert!(kept.is_empty(), "the cooldown outlived the file: {kept:?}");
    flush(&h.manager);
    assert!(
        store.saved_ids().is_empty(),
        "the lifted cooldown was written back: {:?}",
        store.saved_ids()
    );
}

// Not upstream's: the saved cooldown wins over a model that a request went
// through while this worker was handed over. The healthy model's state is
// newer than the record (a result needs no write permit), and the restore
// must not take the record for one a later result cleared, or write the
// loss back to the file.
#[tokio::test(start_paused = true)]
async fn a_restore_does_not_let_a_newer_healthy_model_veto_a_saved_cooldown() {
    let store = Arc::new(SwitchStore::default());
    let h = cooldown_manager(&store);
    rate_limit(&h, "auth-1");
    freeze(&h.manager).expect("freeze");
    // The other process's cooldown, saved before the result below.
    let mut saved = snapshot(&h.manager, h.now());
    let until = h.now() + chrono::Duration::hours(1);
    for record in &mut saved {
        record.next_retry_after = Some(until);
        record.quota.next_recover_at = record.quota.exceeded.then_some(until);
    }
    assert!(saved.iter().any(|record| record.model == "m1"), "{saved:?}");

    h.clock.advance(Duration::from_secs(1));
    h.manager.mark_result(&success("auth-1", "xai", "m1"));
    let model = h.get("auth-1").model_states["m1"].clone();
    assert!(!model.unavailable && model.updated_at > saved[0].updated_at);

    store.overwrite(saved);
    thaw(&h.manager, Thaw::Restore);
    let kept = snapshot(&h.manager, h.now());
    assert!(
        kept.iter()
            .any(|r| r.auth_id == "auth-1" && r.model == "m1"),
        "the saved cooldown was vetoed: {kept:?}"
    );
    flush(&h.manager);
    assert!(
        lock(&store.records)
            .iter()
            .any(|r| r.auth_id == "auth-1" && r.model == "m1"),
        "the saved cooldown was erased from the file"
    );
}

// Not upstream's: a standby process neither restores nor saves until it
// thaws, and the thaw restores because it never did.
#[tokio::test(start_paused = true)]
async fn a_standby_cooldown_store_waits_for_its_thaw() {
    let store = Arc::new(SwitchStore::default());
    let other_store = Arc::new(SwitchStore::default());
    let other = cooldown_manager(&other_store);
    rate_limit(&other, "auth-2");
    store.overwrite(snapshot(&other.manager, other.now()));

    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    standby(&h.manager);
    restore_now(&h.manager);
    let mut registered = auth("auth-2", "xai");
    registered.status = Status::Active;
    h.manager.register_unsaved(registered).expect("register");
    assert_eq!(store.loads(), 0, "not restored");
    rate_limit(&h, "auth-2");
    flush(&h.manager);
    assert_eq!(store.saves(), 0, "not saved");

    thaw(&h.manager, Thaw::Keep);
    assert_eq!(store.loads(), 1, "restored once it thaws");
}

// Not upstream's: a worker that took over and hands over, or exits, before
// it thaws (it has not restored from the file) must not write its own
// cooldowns over the other worker's file.
#[tokio::test(start_paused = true)]
async fn a_worker_that_hands_over_before_its_thaw_leaves_the_others_file_alone() {
    let store = Arc::new(SwitchStore::default());
    let other_store = Arc::new(SwitchStore::default());
    let other = cooldown_manager(&other_store);
    rate_limit(&other, "auth-2");
    store.overwrite(snapshot(&other.manager, other.now()));
    let written = store.saved_ids();
    assert_eq!(written, ["auth-2"]);

    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    install_store(&h.manager, store.clone());
    standby(&h.manager);
    restore_now(&h.manager);
    let mut registered = auth("auth-1", "xai");
    registered.status = Status::Active;
    h.manager.register_unsaved(registered).expect("register");
    rate_limit(&h, "auth-1");

    freeze(&h.manager).expect("a freeze before the thaw saves nothing and succeeds");
    flush(&h.manager);
    assert_eq!(store.saves(), 0, "nothing was written");
    assert_eq!(store.loads(), 0, "nothing was read");
    assert_eq!(
        store.saved_ids(),
        written,
        "the other worker's file is as it was"
    );
}

// Not upstream's: a save needs a write permit of the manager's gate. While
// the gate is closed the cooldown file is left alone and the change waits.
#[tokio::test(start_paused = true)]
async fn a_closed_gate_holds_back_the_cooldown_save() {
    let store = Arc::new(SwitchStore::default());
    let h = cooldown_manager(&store);
    let (gate, _key) = ActiveGate::standby();
    h.manager.set_gate(gate.clone());
    rate_limit(&h, "auth-1");

    flush(&h.manager);
    assert_eq!(store.saves(), 0, "closed");
    gate.reopen();
    flush(&h.manager);
    assert_eq!(store.saves(), 1, "saved once the gate is open");
}

/// A credential store that holds the next save until released, so a refresh
/// can be stopped after it published its token and before it saved it.
struct HoldingStore {
    inner: Arc<FileStore>,
    hold: AtomicBool,
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl AuthStore for HoldingStore {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.inner.list()
    }

    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        if self.hold.swap(false, Ordering::SeqCst) {
            lock(&self.entered).send(()).expect("test is listening");
            lock(&self.release).recv().expect("test releases");
        }
        AuthStore::save(&*self.inner, auth)
    }

    fn delete(&self, id: &str) -> std::io::Result<()> {
        AuthStore::delete(&*self.inner, id)
    }

    fn read_back(&self, auth: &Auth) -> std::io::Result<Option<Auth>> {
        self.inner.read_back(auth)
    }
}

// Not upstream's: C-O1. A refresh published its token and is held before its
// save. The gate quiesces, and a late 401 on a request that used the older
// token reaches the D3 path. Memory has moved past the token that failed, so
// nothing is adopted from the file: the refresh's token is still in memory
// until it saves. L27 then reconciles memory with the newer tokens on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_after_a_refresh_published_does_not_replace_its_unsaved_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = Arc::new(FileStore::new(dir.path()));
    let gate = ActiveGate::open();
    files.set_gate(gate.clone());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = Arc::new(HoldingStore {
        inner: files,
        hold: AtomicBool::new(false),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    let manager = Manager::with_clock(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone() as Arc<dyn AuthStore>),
        TestClock::new().clock(),
    );
    manager.set_gate(gate.clone());
    let provider = RotatingProvider::new();
    manager.register_executor(provider.clone());
    manager.register(credential()).expect("register");
    let failed = manager.get(ID).expect("credential");

    // What the active process left in the file meanwhile.
    let path = dir.path().join(ID);
    let mut file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    file["access_token"] = json!("access-9");
    file["refresh_token"] = json!("refresh-9");
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    store.hold.store(true, Ordering::SeqCst);
    let refresh = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.force_refresh(ID).await })
    };
    tokio::task::spawn_blocking(move || entered_rx.recv().expect("the save is reached"))
        .await
        .unwrap();
    assert_eq!(access_token(&manager.get(ID).unwrap()), "access-1");

    let closing = {
        let gate = gate.clone();
        let epoch = gate.epoch();
        tokio::spawn(async move { gate.close(epoch).await })
    };
    while gate.state() == GateState::Open {
        tokio::task::yield_now().await;
    }
    let answered = manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await
        .expect("the credential as it is now");
    assert_eq!(access_token(&answered), "access-1");
    assert_eq!(access_token(&manager.get(ID).unwrap()), "access-1");

    release_tx.send(()).unwrap();
    let refreshed = refresh.await.unwrap().expect("the refresh finishes");
    closing.await.unwrap().expect("closes");
    // L27: the Codex disk baseline keeps newer tokens and reconciles memory.
    assert_eq!(access_token(&refreshed), "access-9");
    assert_eq!(access_token(&manager.get(ID).unwrap()), "access-9");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "access-9");
    assert_eq!(saved["refresh_token"], "refresh-9");
    assert_eq!(provider.reused(), 0);
}

// Not upstream's: C-O1. Memory still holds the token that failed and the file
// is newer: the file's tokens are taken in memory only, the persist watermark
// does not move, and nothing is written, so a later save of a newer
// generation still lands.
#[tokio::test(start_paused = true)]
async fn adopting_from_the_file_leaves_the_persist_watermark_alone() {
    let env = Env::new();
    let held = env.gate.begin_operation().expect("operation");
    let closing = env.close();
    settle().await;
    rotate_on_disk(&env);
    let before_file = std::fs::read(env.dir.path().join(ID)).expect("file");
    let watermark = || {
        let locks = lock(&env.manager.shared.persist_locks);
        *lock(locks.get(ID).expect("saved once"))
    };
    let before = watermark();

    let failed = env.manager.get(ID).expect("credential");
    let adopted = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await
        .expect("adopted");
    assert_eq!(access_token(&adopted), "access-9");
    assert_eq!(watermark(), before);
    assert_eq!(
        std::fs::read(env.dir.path().join(ID)).expect("file"),
        before_file
    );
    // A save of the next generation, as an admitted operation makes, lands.
    let (epoch, generation) = env.versions();
    let mut next = env.manager.get(ID).unwrap().as_ref().clone();
    next.metadata
        .insert("note".into(), json!("newer generation"));
    held.clone()
        .scope(async {
            env.manager.persist(
                &next,
                epoch,
                generation + 1,
                0,
                crate::manager::lifecycle::Save::Yes,
            )
        })
        .await
        .expect("saved");
    // L27: an admitted configuration save lands non-token fields, keeping disk tokens.
    assert_eq!(env.on_disk()["note"], "newer generation");
    assert_eq!(env.on_disk()["access_token"], "access-9");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-9");
    assert_eq!(watermark(), (epoch, generation + 1));
    drop(held);
    closing.await.expect("task").expect("closes");
}

// Not upstream's: C-O3. A worker that isn't active does not spin on a due
// credential: the loop pops nothing while the gate is closed, the provider is
// never called, and the credential refreshes once after the gate opens. Stop
// ends the parked loop.
#[tokio::test(start_paused = true)]
async fn the_refresh_loop_parks_while_the_gate_is_closed() {
    let h = Harness::new(Settings::default());
    let (gate, _key) = ActiveGate::standby();
    h.manager.set_gate(gate.clone());
    let executor = FakeExecutor::new("codex");
    h.executor(&executor);
    h.add(
        auth_with_metadata(
            "parked",
            "codex",
            json!({"refresh_token": "r", "refresh_interval_seconds": 0.001}),
        ),
        &[],
    );
    h.manager
        .start_auto_refresh(Duration::from_millis(1))
        .expect("start");
    let generation = || h.manager.get("parked").map_or(0, |auth| auth.generation);
    let start = generation();
    for _ in 0..200 {
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
    }
    assert_eq!(executor.refresh_count(), 0);
    assert!(
        generation() - start <= 3,
        "the loop spun: generation {} to {}",
        start,
        generation()
    );

    gate.reopen();
    for _ in 0..50 {
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
    }
    assert!(executor.refresh_count() >= 1, "refreshes once open");

    // Stop ends a loop parked on a closed gate; nothing refreshes after.
    gate.close(gate.epoch()).await.expect("closes");
    settle().await;
    h.manager.stop_auto_refresh();
    settle().await;
    let count = executor.refresh_count();
    gate.reopen();
    for _ in 0..50 {
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
    }
    assert_eq!(
        executor.refresh_count(),
        count,
        "stopped loops refresh nothing"
    );
}

/// Waits, in real time, until `cond` holds.
async fn until(cond: impl Fn() -> bool) {
    for _ in 0..5000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the condition never held");
}

/// A save stopped by [`hold_save`], after the snapshot is published and
/// before the save takes the persist lock.
struct SaveHold {
    entered: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    release: std::sync::mpsc::Sender<()>,
}

impl SaveHold {
    /// Waits until a save is stopped.
    async fn wait_entered(&self) {
        let entered = Arc::clone(&self.entered);
        tokio::task::spawn_blocking(move || lock(&entered).recv().expect("a save is stopped"))
            .await
            .expect("task");
    }

    /// Lets the stopped save go on.
    fn release(&self) {
        self.release.send(()).expect("the save is waiting");
    }
}

/// Stops the first save of a snapshot `when` accepts, before it takes the
/// persist lock, until released.
fn hold_save(manager: &Manager, when: impl Fn(&Auth) -> bool + Send + Sync + 'static) -> SaveHold {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth: &Auth| {
        if when(auth) && armed.swap(false, Ordering::SeqCst) {
            lock(&entered_tx).send(()).expect("the test is listening");
            lock(&release_rx).recv().expect("the test releases");
        }
    }));
    SaveHold {
        entered: Arc::new(Mutex::new(entered_rx)),
        release: release_tx,
    }
}

// Not upstream's: C2-O1. A forced refresh, or one made for a request, has no
// job. It is held after the provider rotated the token and before it
// publishes. The gate quiesces and a 401 on a request that used the old token
// reaches D3: the refresh holds the credential's refresh lock, so nothing is
// adopted. L27 keeps the newer disk tokens and reconciles memory after saving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_adopts_nothing_while_a_refresh_without_a_job_is_in_flight() {
    for request_path in [false, true] {
        let env = Env::new();
        env.provider.hold();
        let refresh = {
            let manager = env.manager.clone();
            tokio::spawn(async move {
                if request_path {
                    manager.refresh_at_epoch(ID, "access-0", 0).await
                } else {
                    manager.force_refresh(ID).await
                }
            })
        };
        env.provider.wait_rotated().await;
        let closing = env.close();
        until(|| env.gate.state() != GateState::Open).await;
        rotate_on_disk(&env);

        let failed = env.manager.get(ID).expect("credential");
        let answered = env
            .manager
            .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
            .await;
        assert!(answered.is_none(), "request path {request_path}");
        assert_eq!(env.token(), "access-0", "request path {request_path}");

        env.provider.release();
        let refreshed = refresh.await.expect("task").expect("the refresh finishes");
        closing.await.expect("task").expect("closes");
        // L27: the Codex disk baseline keeps newer tokens and reconciles memory.
        assert_eq!(access_token(&refreshed), "access-9");
        assert_eq!(env.token(), "access-9", "request path {request_path}");
        assert_eq!(env.on_disk()["refresh_token"], "refresh-9");
        assert_eq!(env.on_disk()["access_token"], "access-9");
        assert_eq!(env.provider.reused(), 0);
    }
}

// Not upstream's: C2-O2. An admitted refresh published its token and is held
// before it takes the persist lock. The gate quiesces, and an outcome is
// recorded without an operation: it publishes a newer generation, and its
// save is refused. The refused save must not make the refresh's look older:
// the file ends with the refresh's token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_save_does_not_hide_an_admitted_one() {
    let env = Env::new();
    let hold = hold_save(&env.manager, |auth| access_token(auth) == "access-1");
    let refresh = env.refresh();
    hold.wait_entered().await;
    assert_eq!(env.token(), "access-1");
    let published = env.versions().1;
    let closing = env.close();
    until(|| env.gate.state() != GateState::Open).await;

    // A request that used the refreshed token.
    let mut outcome = success(ID, PROVIDER, "m1");
    outcome.credential_version = env.manager.get(ID).expect("credential").credential_version;
    env.manager.mark_result(&outcome);
    assert!(env.versions().1 > published, "the outcome was published");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-0", "refused");

    hold.release();
    refresh.await.expect("task").expect("the refresh finishes");
    closing.await.expect("task").expect("closes");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-1");
    assert_eq!(env.on_disk()["access_token"], "access-1");
}

// Not upstream's: C2-O2. A save of a newer generation that fails does not
// stop a later save of an older one from landing.
#[tokio::test(start_paused = true)]
async fn a_failed_save_does_not_make_an_older_one_stale() {
    let env = Env::new();
    let held = env.gate.begin_operation().expect("operation");
    let closing = env.close();
    settle().await;
    assert_eq!(env.gate.state(), GateState::Quiescing);
    let (epoch, generation) = env.versions();
    let with_note = |note: &str| {
        let mut auth = env.manager.get(ID).expect("credential").as_ref().clone();
        auth.metadata.insert("note".into(), json!(note));
        auth
    };

    // Outside an operation the save is refused.
    let refused = env.manager.persist(
        &with_note("newer"),
        epoch,
        generation + 5,
        0,
        crate::manager::lifecycle::Save::Yes,
    );
    assert!(refused.is_err());
    assert!(env.on_disk().get("note").is_none());

    held.clone()
        .scope(async {
            env.manager.persist(
                &with_note("older"),
                epoch,
                generation + 2,
                0,
                crate::manager::lifecycle::Save::Yes,
            )
        })
        .await
        .expect("saved");
    assert_eq!(env.on_disk()["note"], "older");
    drop(held);
    closing.await.expect("task").expect("closes");
}

/// A store that fails its saves with an ordinary I/O error while `fail` is
/// set, and saves through `inner` otherwise.
struct FlakyStore {
    inner: Arc<FileStore>,
    fail: AtomicBool,
}

impl AuthStore for FlakyStore {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.inner.list()
    }

    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("disk full"));
        }
        self.inner.save(auth)
    }

    fn delete(&self, id: &str) -> std::io::Result<()> {
        self.inner.delete(id)
    }
}

// Not upstream's: C3-5. A save of a newer generation that fails with an
// ordinary store error, not a refusal, does not stop a later save of an older
// one from landing once writes work again.
#[tokio::test(start_paused = true)]
async fn a_save_that_fails_with_a_store_error_does_not_make_an_older_one_stale() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = Arc::new(FileStore::new(dir.path()));
    let flaky = Arc::new(FlakyStore {
        inner: Arc::clone(&files),
        fail: AtomicBool::new(false),
    });
    let manager = Manager::with_clock(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(flaky.clone() as Arc<dyn AuthStore>),
        TestClock::new().clock(),
    );
    manager.register(credential()).expect("register");
    let (epoch, generation) = {
        let state = manager.lock();
        let auth = &state.auths.get(ID).expect("credential").auth;
        (auth.registration_epoch, auth.generation)
    };
    let with_note = |note: &str| {
        let mut auth = manager.get(ID).expect("credential").as_ref().clone();
        auth.metadata.insert("note".into(), json!(note));
        auth
    };
    let on_disk = || -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.path().join(ID)).expect("file")).expect("json")
    };
    let save = |note: &str, generation: u64| {
        manager.persist(
            &with_note(note),
            epoch,
            generation,
            0,
            crate::manager::lifecycle::Save::Yes,
        )
    };

    flaky.fail.store(true, Ordering::SeqCst);
    let failed = save("newer", generation + 5);
    assert!(matches!(failed, Err(ManagerError::Store(_))), "{failed:?}");
    assert!(on_disk().get("note").is_none());

    flaky.fail.store(false, Ordering::SeqCst);
    save("older", generation + 2).expect("the older save lands");
    assert_eq!(on_disk()["note"], "older");
    save("newer", generation + 5).expect("the newer one lands after it");
    assert_eq!(on_disk()["note"], "newer");
}

// Not upstream's: C2-R1. An admitted update published its snapshot, holding
// the old token, and is held before its save. The gate quiesces and D3 would
// adopt the token another process left in the file: it is refused while the
// save is pending. L27 keeps the disk tokens during the save; a later D3 401
// adopts them once the save is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_adopts_nothing_while_an_admitted_save_is_pending() {
    let env = Env::new();
    let hold = hold_save(&env.manager, |auth| auth.metadata.contains_key("note"));
    let operation = env.gate.begin_operation().expect("operation");
    let mut next = env.manager.get(ID).expect("credential").as_ref().clone();
    next.metadata.insert("note".into(), json!("kept"));
    let update = {
        let manager = env.manager.clone();
        tokio::spawn(operation.clone().scope(async move { manager.update(next) }))
    };
    hold.wait_entered().await;
    let closing = env.close();
    until(|| env.gate.state() != GateState::Open).await;
    rotate_on_disk(&env);

    let failed = env.manager.get(ID).expect("credential");
    let answered = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await;
    assert!(answered.is_none(), "nothing is adopted");
    assert_eq!(env.token(), "access-0");

    hold.release();
    update.await.expect("task").expect("the update finishes");
    drop(operation);
    closing.await.expect("task").expect("closes");
    assert_eq!(env.on_disk()["note"], "kept");
    // L27: an admitted Codex configuration save keeps the newer disk tokens.
    assert_eq!(env.on_disk()["access_token"], "access-9");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-9");
    assert_eq!(env.token(), "access-0");
    assert!(lock(&env.manager.shared.state).pending_saves.is_empty());
    assert_next_401_adopts_disk_tokens(&env).await;
}

/// Once an admitted save is done, D3 can adopt the tokens it kept on disk.
async fn assert_next_401_adopts_disk_tokens(env: &Env) {
    let before = std::fs::read(env.dir.path().join(ID)).expect("file");
    let failed = env.manager.get(ID).expect("credential");
    let adopted = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await
        .expect("the next 401 adopts the disk tokens");
    assert_eq!(access_token(&adopted), "access-9");
    assert_eq!(refresh_token(&adopted), "refresh-9");
    assert_eq!(env.token(), "access-9");
    assert_eq!(adopted.metadata["note"], "kept");
    assert_eq!(env.provider.calls(), 0, "never refreshes");
    assert_eq!(
        std::fs::read(env.dir.path().join(ID)).expect("file"),
        before,
        "never writes"
    );
}

// L27 is Codex-only: an admitted non-Codex save still replaces disk tokens.
#[tokio::test(start_paused = true)]
async fn an_admitted_non_codex_save_overwrites_the_tokens_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = Arc::new(FileStore::new(dir.path()));
    let gate = ActiveGate::open();
    files.set_gate(gate.clone());
    let manager = Manager::with_clock(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(files as Arc<dyn AuthStore>),
        TestClock::new().clock(),
    );
    manager.set_gate(gate.clone());
    let mut auth = credential();
    auth.id = "gemini-a.json".into();
    auth.file_name = auth.id.clone();
    auth.provider = "gemini".into();
    auth.metadata.insert("type".into(), json!("gemini"));
    let registered = manager.register(auth).expect("register");
    let path = dir.path().join(&registered.file_name);
    let mut file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("file")).expect("json");
    file["access_token"] = json!("access-9");
    file["refresh_token"] = json!("refresh-9");
    std::fs::write(&path, serde_json::to_vec(&file).expect("json")).expect("write");

    let operation = gate.begin_operation().expect("operation");
    gate.quiesce(gate.epoch()).expect("quiesces");
    let mut next = registered.as_ref().clone();
    next.metadata.insert("note".into(), json!("kept"));
    operation
        .clone()
        .scope(async { manager.update(next) })
        .await
        .expect("the admitted update saves")
        .expect("credential");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("file")).expect("json");
    assert_eq!(saved["access_token"], "access-0");
    assert_eq!(saved["refresh_token"], "refresh-0");
    assert_eq!(saved["note"], "kept");
    assert_eq!(
        access_token(&manager.get(&registered.id).unwrap()),
        "access-0"
    );
    drop(operation);
    gate.settle(gate.epoch()).await.expect("closes");
}

// Not upstream's: an edit is made on the credential as it is, so a refresh
// that finished since the caller last looked is not undone, in memory or on
// disk.
#[tokio::test(start_paused = true)]
async fn an_edit_keeps_the_tokens_a_refresh_saved_since_the_caller_looked() {
    let env = Env::new();
    let stale = env.manager.get(ID).expect("credential");
    env.manager.force_refresh(ID).await.expect("refresh");
    let refreshed = env.token();
    assert_ne!(refreshed, access_token(&stale));
    // The version the refresh left, before the edit.
    let version = env.manager.get(ID).unwrap().credential_version;

    let edited = env
        .manager
        .edit(ID, |auth| {
            auth.metadata.insert("note".into(), json!("kept"));
            Ok::<_, String>(true)
        })
        .expect("edit")
        .expect("credential");
    assert_eq!(access_token(&edited), refreshed);
    assert_eq!(env.token(), refreshed);
    assert_eq!(env.on_disk()["access_token"], refreshed);
    assert_eq!(env.on_disk()["note"], "kept");
    assert_eq!(edited.credential_version, version);
    assert_eq!(env.manager.get(ID).unwrap().credential_version, version);
}

// Not upstream's: C4-3. An edit can't move the credential to another ID:
// that would publish this credential under the other one and leave its own
// registered. Both a registered ID and an empty one are refused, and
// nothing is published or saved.
#[tokio::test(start_paused = true)]
async fn an_edit_that_changes_the_id_is_refused() {
    let env = Env::new();
    let mut other = credential();
    other.id = "codex-b.json".into();
    other.file_name = "codex-b.json".into();
    other
        .metadata
        .insert("access_token".into(), json!("access-b"));
    env.manager.register(other).expect("register");
    let a = env.manager.get(ID).expect("credential");
    let b = env.manager.get("codex-b.json").expect("credential");
    for new_id in ["codex-b.json", ""] {
        let refused = env.manager.edit(ID, |auth| {
            auth.id = new_id.into();
            auth.metadata.insert("note".into(), json!("lost"));
            Ok::<_, String>(true)
        });
        assert!(
            matches!(refused, Err(EditError::Manager(_))),
            "{new_id:?}: {refused:?}"
        );
        assert!(Arc::ptr_eq(&a, &env.manager.get(ID).unwrap()));
        assert!(Arc::ptr_eq(&b, &env.manager.get("codex-b.json").unwrap()));
    }
    assert!(env.on_disk().get("note").is_none());
}

#[tokio::test(start_paused = true)]
async fn an_edit_that_changes_nothing_or_is_refused_publishes_nothing() {
    let env = Env::new();
    let before = env.manager.get(ID).expect("credential");
    let unchanged = env
        .manager
        .edit(ID, |_| Ok::<_, String>(false))
        .expect("edit")
        .expect("credential");
    assert!(Arc::ptr_eq(&before, &unchanged));
    let refused = env.manager.edit(ID, |auth| {
        auth.metadata.insert("note".into(), json!("lost"));
        Err::<bool, _>("no")
    });
    assert!(matches!(refused, Err(EditError::Rejected("no"))));
    assert!(Arc::ptr_eq(
        &before,
        &env.manager.get(ID).expect("credential")
    ));
    assert!(env.on_disk().get("note").is_none());
    let missing = env.manager.edit("nope", |_| Ok::<_, String>(true));
    assert!(matches!(missing, Ok(None)));
}

/// The level and message of each event `f` logs on this thread.
fn logged_events<T>(f: impl FnOnce() -> T) -> (T, Vec<(tracing::Level, String)>) {
    use tracing::field::{Field, Visit};

    struct Message(String);
    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);
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
            lock(&self.0).push((*event.metadata().level(), message.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    crate::test_tracing::keep_every_callsite_open();
    let events = Arc::new(Mutex::new(Vec::new()));
    let value = tracing::subscriber::with_default(Capture(Arc::clone(&events)), f);
    let events = lock(&events).clone();
    (value, events)
}

// Not upstream's: C2-R4 (C-L4). A save the gate refuses, on the request path
// of a switch under way, is logged at debug: every request's outcome would
// otherwise log a warning. The credential is still published in memory.
#[tokio::test(start_paused = true)]
async fn a_save_the_gate_refused_logs_at_debug_and_not_as_a_warning() {
    let env = Env::new();
    let held = env.gate.begin_operation().expect("operation");
    let closing = env.close();
    settle().await;
    assert_eq!(env.gate.state(), GateState::Quiescing);

    let mut next = env.manager.get(ID).expect("credential").as_ref().clone();
    next.metadata.insert("note".into(), json!("kept"));
    let (updated, events) = logged_events(|| env.manager.update(next));
    assert!(updated.expect("published").is_some());
    let refused: Vec<_> = events
        .iter()
        .filter(|(_, message)| message.contains("refused: the gate is not open"))
        .collect();
    assert_eq!(refused.len(), 1, "{events:?}");
    assert_eq!(refused[0].0, tracing::Level::DEBUG, "{events:?}");
    assert!(
        events
            .iter()
            .all(|(level, _)| *level != tracing::Level::WARN && *level != tracing::Level::ERROR),
        "{events:?}"
    );
    assert!(env.on_disk().get("note").is_none(), "nothing saved");

    drop(held);
    closing.await.expect("task").expect("closes");
}

/// Stops the first adoption from the store after it has read the record,
/// before it takes the state lock, until released.
fn hold_adoption(manager: &Manager) -> SaveHold {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    let armed = AtomicBool::new(true);
    *lock(&manager.shared.adopt_hook) = Some(Arc::new(move || {
        if armed.swap(false, Ordering::SeqCst) {
            lock(&entered_tx).send(()).expect("the test is listening");
            lock(&release_rx).recv().expect("the test releases");
        }
    }));
    SaveHold {
        entered: Arc::new(Mutex::new(entered_rx)),
        release: release_tx,
    }
}

// Not upstream's: C3-1 (a). A save of the credential is pending when D3
// starts: it refuses before it reads the record (the read would have called
// the hook), not only at the swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_does_not_read_the_store_while_a_save_is_pending() {
    let env = Env::new();
    let reads = Arc::new(AtomicUsize::new(0));
    {
        let reads = Arc::clone(&reads);
        *lock(&env.manager.shared.adopt_hook) = Some(Arc::new(move || {
            reads.fetch_add(1, Ordering::SeqCst);
        }));
    }
    let hold = hold_save(&env.manager, |auth| auth.metadata.contains_key("note"));
    let operation = env.gate.begin_operation().expect("operation");
    let mut next = env.manager.get(ID).expect("credential").as_ref().clone();
    next.metadata.insert("note".into(), json!("kept"));
    let update = {
        let manager = env.manager.clone();
        tokio::spawn(operation.clone().scope(async move { manager.update(next) }))
    };
    hold.wait_entered().await;
    let closing = env.close();
    until(|| env.gate.state() != GateState::Open).await;
    rotate_on_disk(&env);

    let failed = env.manager.get(ID).expect("credential");
    let answered = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await;
    assert!(answered.is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0, "the record was read");

    hold.release();
    update.await.expect("task").expect("the update finishes");
    drop(operation);
    closing.await.expect("task").expect("closes");
}

// Not upstream's: C3-1 (b). D3 passes its first look and reads the record,
// which holds a newer token. A live settings edit then publishes a save.
// Adoption excludes the write until its read is judged, and sees the pending
// publication and adopts nothing. L27 keeps disk tokens; the next D3 401
// adopts them after the save.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_adopts_nothing_when_a_save_is_published_after_its_read() {
    let env = Env::new();
    let hold = hold_adoption(&env.manager);
    let operation = env.gate.begin_operation().expect("operation");
    let closing = env.close();
    until(|| env.gate.state() != GateState::Open).await;
    rotate_on_disk(&env);
    let d3 = {
        let manager = env.manager.clone();
        let failed = env.manager.get(ID).expect("credential");
        tokio::spawn(async move {
            manager
                .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
                .await
        })
    };
    hold.wait_entered().await;

    let save_hold = hold_save(&env.manager, |auth| auth.metadata.contains_key("note"));
    let manager = env.manager.clone();
    let updating = operation.clone();
    let update = tokio::spawn(updating.scope(async move {
        manager
            .run_persistence(|manager| {
                manager.edit(ID, |auth| {
                    auth.metadata.insert("note".into(), json!("kept"));
                    Ok::<_, String>(true)
                })
            })
            .await
            .expect("blocking edit")
            .expect("the update saves")
    }));
    save_hold.wait_entered().await;
    assert!(!lock(&env.manager.shared.state).pending_saves.is_empty());
    save_hold.release();
    hold.release();
    assert!(d3.await.expect("task").is_none(), "nothing is adopted");
    update.await.expect("update task");
    drop(operation);
    assert!(lock(&env.manager.shared.state).pending_saves.is_empty());
    assert_eq!(env.token(), "access-0");
    // L27: an admitted Codex configuration save keeps the newer disk tokens.
    assert_eq!(env.on_disk()["access_token"], "access-9");
    assert_eq!(env.on_disk()["refresh_token"], "refresh-9");
    assert_eq!(env.on_disk()["note"], "kept");
    closing.await.expect("task").expect("closes");
    assert_next_401_adopts_disk_tokens(&env).await;
}

// Not upstream's: C4-1. A request holds a snapshot of the credential. The
// credential is then removed and registered again at another path (an
// `auth-dir` change does this), still with the old token, and the file at
// the old path holds a newer one. D3 must not take the old path's token
// into the new registration, whose own file holds the old token.
#[tokio::test(start_paused = true)]
async fn a_401_after_a_re_registration_at_another_path_adopts_nothing() {
    let env = Env::new();
    let failed = env.manager.get(ID).expect("credential");
    let epoch = failed.registration_epoch;
    env.manager.remove(ID);
    let mut moved = credential();
    moved.file_name = "moved.json".into();
    env.manager.register(moved).expect("register again");
    assert_ne!(env.manager.get(ID).unwrap().registration_epoch, epoch);
    // The old path now holds another process's newer tokens.
    rotate_on_disk(&env);
    let closing = env.close();
    until(|| env.gate.state() != GateState::Open).await;

    let answered = env
        .manager
        .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
        .await;
    assert!(answered.is_none(), "nothing is adopted");
    assert_eq!(env.token(), "access-0");
    closing.await.expect("task").expect("closes");
}

// Not upstream's: C5-1. D3 has read the record from the file the credential
// pointed to. Before it swaps, the credential is moved to another file with
// no save and no new epoch, and that file holds the old token. D3 must
// adopt nothing, whether `file_name` or the `path` attribute moved it.
async fn moved_after_the_read_adopts_nothing(by_path: bool) {
    let env = Env::new();
    // The file the credential will move to holds the token it has.
    let moved_path = env.dir.path().join("moved.json");
    std::fs::write(
        &moved_path,
        serde_json::to_vec(&env.on_disk()).expect("json"),
    )
    .expect("write");
    let hold = hold_adoption(&env.manager);
    let closing = env.close();
    until(|| env.gate.state() != GateState::Open).await;
    rotate_on_disk(&env);
    let d3 = {
        let manager = env.manager.clone();
        let failed = env.manager.get(ID).expect("credential");
        tokio::spawn(async move {
            manager
                .try_refresh_after_unauthorized(&failed, &ExecError::upstream(401, "401"), false)
                .await
        })
    };
    hold.wait_entered().await;

    let epoch = env.versions().0;
    let to = moved_path.clone();
    env.manager
        .edit_unsaved(ID, move |auth| {
            if by_path {
                auth.attributes
                    .insert("path".into(), to.display().to_string());
            } else {
                auth.file_name = "moved.json".into();
            }
            Ok::<_, String>(true)
        })
        .expect("edit")
        .expect("credential");
    assert_eq!(env.versions().0, epoch, "same registration");

    hold.release();
    assert!(d3.await.expect("task").is_none(), "adopted");
    assert_eq!(env.token(), "access-0");
    closing.await.expect("task").expect("closes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_adopts_nothing_when_file_name_changed_after_the_read() {
    moved_after_the_read_adopts_nothing(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_adopts_nothing_when_the_path_attribute_changed_after_the_read() {
    moved_after_the_read_adopts_nothing(true).await;
}

// Not upstream's: C6-1. The credential has a relative file name and no path
// attribute, so the store's directory decides which file it is. D3 has read
// it from directory A; the store is then rebound to directory B, which
// holds the old token, before the swap. The credential, its epoch and its
// save sequence are untouched, so only the store's binding generation shows
// it. Nothing is adopted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_adopts_nothing_when_the_store_was_rebound_after_the_read() {
    let env = Env::new();
    env.manager
        .edit_unsaved(ID, |auth| {
            auth.attributes.remove("path");
            Ok::<_, String>(true)
        })
        .expect("edit")
        .expect("credential");
    let live = env.manager.get(ID).expect("credential");
    assert!(!live.attributes.contains_key("path"));
    assert_eq!(live.file_name, ID);
    // Directory B holds the file with the token the credential has.
    let other = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        other.path().join(ID),
        serde_json::to_vec(&env.on_disk()).expect("json"),
    )
    .expect("write");
    // The store and the manager now sit behind a closed standby gate.
    let (closed, key) = ActiveGate::standby();
    env.files.set_gate(closed.clone());
    env.manager.set_gate(closed.clone());
    rotate_on_disk(&env);
    let hold = hold_adoption(&env.manager);
    let d3 = {
        let manager = env.manager.clone();
        tokio::spawn(async move {
            manager
                .try_refresh_after_unauthorized(&live, &ExecError::upstream(401, "401"), false)
                .await
        })
    };
    hold.wait_entered().await;

    let activation = key.activate(&closed).expect("activation");
    env.files
        .set_base_dir_under(&activation, other.path())
        .expect("rebind");

    hold.release();
    assert!(d3.await.expect("task").is_none(), "adopted");
    assert_eq!(env.token(), "access-0");
}
