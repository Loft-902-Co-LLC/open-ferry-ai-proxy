// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_lifecycle.go
// (RegisterExecutor, UnregisterExecutor, Register, Update,
// UpdateRefreshedAuth, Remove, invalidateSessionAffinity, Load and persist),
// MarkResult, updateSessionAffinity, recordAvailabilityNeutralResult,
// ResetQuota and clearDisabledCooldownStates in
// sdk/cliproxy/auth/conductor_cooldown.go,
// ReconcileRegistryModelStates in sdk/cliproxy/auth/conductor_selection.go,
// and lockAuthMutation in sdk/cliproxy/auth/conductor_persistence.go
// (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Adding, changing and removing credentials and executors, recording call
//! outcomes, and saving credentials to the store.
//!
//! Every change makes a new [`Auth`] snapshot and bumps the credential's
//! generation; registering an ID again bumps its epoch, and replacing its
//! tokens or API key its credential version. A call's outcome from an
//! earlier credential version or registration than the live one is
//! dropped, so it can't cool down tokens it never ran with. A save is
//! skipped when a newer (epoch, generation) of the same credential was
//! already saved, so saves never go backwards. [`Manager::register_unsaved`] and
//! [`Manager::update_unsaved`] change a credential without saving it, for
//! one just read from its file or the config (upstream's `WithSkipPersist`).
//!
//! A reload ([`Manager::load`]) and the changes that save never overlap:
//! registering, updating, removing, recording a call's outcome, resetting a
//! quota and reconciling model states take the reload barrier shared, from
//! before the change is made until it is saved, and a reload takes it
//! alone, from before it lists the store until it has replaced every
//! credential. So a reload never reads a save that hasn't landed, nor drops
//! a credential registered while it read. Store I/O happens with the state
//! lock released, so credentials stay readable and selectable throughout.
//!
//! Deviations from upstream:
//! - A registration or update takes the live registration epoch and
//!   generation, whatever the credential carries: upstream refuses an
//!   update carrying an older epoch, and moves on from a newer generation
//!   than the live one.
//! - A replaced credential starts with no `invalid_grant` failures counted;
//!   upstream keeps whatever count the caller's copy held.
//! - A registration or update is published, then saved. Upstream saves a
//!   copy with its lock released, merges back what the store changed in
//!   it (`mergeAuthSaveDelta`, which keeps edits made meanwhile), checks the
//!   registration didn't move, then publishes. Here the store gets the
//!   published snapshot and can't change it ([`crate::auth::AuthStore`]
//!   takes `&Auth`), so there is nothing to merge back, and a failed or
//!   slow save never holds the credential back from calls.
//! - There are no per-credential mutation locks: each change is made whole
//!   under the state lock, and saves of one credential are ordered by
//!   epoch and generation, a stale one skipped. Reload exclusion uses counted,
//!   owned permits so a detached save keeps it across threads. Waiting on it
//!   can't be given up as upstream's contexts allow, and a refresh's result is
//!   committed with no point where it could be dropped (upstream's
//!   `context.WithoutCancel`).
//! - `load` doesn't reschedule refreshes, as upstream; start the refresh
//!   loop after loading. It restarts the rotation, as upstream's scheduler
//!   rebuild does.
//! - A new ID is a random version 4 UUID made with `rand`.
//! - An update with an empty index keeps the old one. Upstream doesn't for
//!   a copy whose index was set and then cleared (its private
//!   `indexAssigned` flag).
//! - The cooldown state store is told after every change that may move a
//!   cooldown, where upstream saves only when the records changed; the
//!   store compares (see [`super::cooldown_store`]).
//! - A recorded outcome also moves open-ferry's capped quota rests, and
//!   `reset_quota` forgets them (see `super::quota_check`).
//! - Not ported: hooks, the scheduler index, API-key model alias rebuilds,
//!   plugin virtual credentials, the Meta key mint save inside the lock
//!   and result policies.

use std::sync::{Arc, Mutex, MutexGuard};

use super::Shared;
use super::affinity::Session;
use super::classify::{ErrView, has_unauthorized_auth_failure, is_unauthorized_error};
use super::cooldown::{
    CallResult, apply_result, clear_cooldown_state_for_auth, cooldown_disabled_for_auth,
    has_model_error, is_credential_quota_active, is_stale_result, normalize_model_states,
    projections_for, reconcile_model_states, reset_quota, update_aggregated_availability,
};
use super::cooldown_store;
use super::credential::{
    KIND_API_KEY, SOURCE_CONFIG, auth_kind, auth_source_kind, credentials_changed, validate_weight,
};
use super::merge::{ResponseCredentials, merge_refreshed_auth_with, normalize_credential_metadata};
use super::models::Resolver;
use super::quota_signals::merge_quota_observation;
use super::refresh::clear_unauthorized_model_states;
use super::select::{executor_key_from_auth, lookup_executor};
use super::text::{canonical_model_key, equal_fold, go_lower};
use super::{EditError, Entry, Manager, ManagerError, ReloadGuard, State, lock};
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::{
    AdoptionRead, Auth, AuthStore, CodexOrigin, GuardedSave, OriginProof, SaveDeferral, Status,
    Timestamp, WithheldPartitions,
};
use crate::executor::ProviderExecutor;

/// How many times reconciliation re-reads the registry when its epoch moves
/// underneath it.
const RECONCILE_ATTEMPTS: usize = 10;

/// A random version 4 UUID (upstream's `uuid.NewString`).
fn new_uuid() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_disabled(auth: &Auth) -> bool {
    auth.disabled || auth.status == Status::Disabled
}

/// Counts a call's outcome on its credential: in its recent-request window,
/// and in the success or failure total.
fn count_result(auth: &mut Auth, result: &CallResult, now: Timestamp) {
    auth.record_recent_request(now, result.success);
    if result.success {
        auth.success = auth.success.saturating_add(1);
    } else {
        auth.failed = auth.failed.saturating_add(1);
    }
}

/// Clears the cooldowns of every credential that no longer cools down:
/// cooling is off for it under the current settings, or it is disabled
/// (upstream's `clearDisabledCooldownStates`). Returns the IDs of the
/// credentials it changed.
pub(crate) fn clear_disabled_cooldown_states(state: &mut State, now: Timestamp) -> Vec<String> {
    let settings = state.settings.clone();
    let mut cleared = Vec::new();
    let mut marked = Vec::new();
    for (id, entry) in &mut state.auths {
        if !cooldown_disabled_for_auth(&settings, &entry.auth) && !is_disabled(&entry.auth) {
            continue;
        }
        let auth = Arc::make_mut(&mut entry.auth);
        if clear_cooldown_state_for_auth(auth, now) {
            auth.generation += 1;
            cleared.push(id.clone());
        } else if state.quota_recovery.keeps(id) {
            // Already clean, but recovery still keeps a claim, a hold or a
            // mark for it (a hold that aggregation hid exists nowhere else):
            // ending them is not left to a cooldown that happened to be there.
            marked.push(id.clone());
        }
    }
    // The cooldowns are gone with no failure to show for it: what the quota
    // recovery noted of them goes too, and a read in flight is fenced.
    for id in cleared.iter().chain(&marked) {
        state.quota_recovery.reset(id);
    }
    cleared
}

/// What [`Manager::reset_quota`] cleared.
#[derive(Clone, Debug)]
pub struct QuotaReset {
    /// The credential after the reset.
    pub auth: Arc<Auth>,
    /// The models whose state was cleared, or the credential's registered
    /// models when it had none.
    pub models: Vec<String>,
}

/// Whether a change is saved to the store (upstream's `WithSkipPersist`
/// when not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Save {
    Yes,
    No,
    #[cfg(feature = "blue-green-test")]
    Failed,
}

/// Operator changes that have published but have not reached the file.
/// Leaves are retained independently, so a disk token rotation is still adopted.
#[derive(Clone, Default)]
pub(crate) struct PolicySave {
    changes: std::collections::BTreeMap<Vec<String>, Option<serde_json::Value>>,
    generation: u64,
    pub(super) retry_at: Option<Timestamp>,
    pub(super) retry_scheduled: bool,
    retry_ticket: u64,
}

fn policy_value(auth: &Auth) -> serde_json::Value {
    serde_json::json!({
        "disabled": auth.disabled, "status_disabled": auth.status == Status::Disabled,
        "status_message": auth.status_message, "metadata": auth.metadata,
        "attributes": auth.attributes, "prefix": auth.prefix,
        "label": auth.label, "proxy_url": auth.proxy_url
    })
}

fn policy_delta(
    before: Option<&serde_json::Value>,
    after: Option<&serde_json::Value>,
    path: &mut Vec<String>,
    changes: &mut std::collections::BTreeMap<Vec<String>, Option<serde_json::Value>>,
) {
    if before == after {
        return;
    }
    if let (Some(serde_json::Value::Object(before)), Some(serde_json::Value::Object(after))) =
        (before, after)
    {
        let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
        for key in keys {
            path.push(key.clone());
            policy_delta(before.get(key), after.get(key), path, changes);
            path.pop();
        }
    } else {
        changes.insert(path.clone(), after.cloned());
    }
}

fn set_policy_value(
    target: &mut serde_json::Value,
    path: &[String],
    value: &Option<serde_json::Value>,
) {
    if path.is_empty() {
        *target = value.clone().unwrap_or(serde_json::Value::Null);
        return;
    }
    if !target.is_object() {
        *target = serde_json::json!({});
    }
    let map = target.as_object_mut().expect("policy object");
    if path.len() == 1 {
        if let Some(value) = value {
            map.insert(path[0].clone(), value.clone());
        } else {
            map.remove(&path[0]);
        }
    } else {
        set_policy_value(
            map.entry(path[0].clone())
                .or_insert_with(|| serde_json::json!({})),
            &path[1..],
            value,
        );
    }
}

impl PolicySave {
    fn extend(&mut self, before: &Auth, after: &Auth, generation: u64) {
        let mut changes = std::collections::BTreeMap::new();
        policy_delta(
            Some(&policy_value(before)),
            Some(&policy_value(after)),
            &mut Vec::new(),
            &mut changes,
        );
        for (path, value) in changes {
            let ancestor = self
                .changes
                .keys()
                .find(|old| path.starts_with(old) && old.len() < path.len())
                .cloned();
            if let Some(ancestor) = ancestor {
                let mut whole = self.changes[&ancestor]
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({}));
                set_policy_value(&mut whole, &path[ancestor.len()..], &value);
                self.changes.insert(ancestor, Some(whole));
            } else {
                self.changes.retain(|old, _| !old.starts_with(&path));
                self.changes.insert(path, value);
            }
        }
        self.generation = generation;
    }

    pub(super) fn overlay(&self, auth: &mut Auth) {
        let mut value = policy_value(auth);
        for (path, desired) in &self.changes {
            set_policy_value(&mut value, path, desired);
        }
        auth.metadata = value["metadata"]
            .as_object()
            .expect("policy metadata")
            .clone();
        auth.attributes = value["attributes"]
            .as_object()
            .expect("policy attributes")
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value.as_str().expect("policy attribute").to_owned(),
                )
            })
            .collect();
        auth.disabled = value["disabled"].as_bool().expect("policy disabled");
        if self.changes.keys().any(|path| {
            path.first()
                .is_some_and(|key| key == "disabled" || key == "status_disabled")
        }) {
            if auth.disabled || value["status_disabled"].as_bool() == Some(true) {
                auth.status = Status::Disabled;
            } else if auth.status == Status::Disabled {
                auth.status = Status::Active;
            }
        }
        auth.status_message = value["status_message"]
            .as_str()
            .expect("policy message")
            .to_owned();
        auth.prefix = value["prefix"].as_str().expect("policy prefix").to_owned();
        auth.label = value["label"].as_str().expect("policy label").to_owned();
        auth.proxy_url = value["proxy_url"]
            .as_str()
            .expect("policy proxy")
            .to_owned();
    }

    fn covered_by(&self, auth: &Auth) -> bool {
        let mut expected = auth.clone();
        self.overlay(&mut expected);
        policy_value(&expected) == policy_value(auth)
    }
}

/// A published snapshot whose save hasn't returned: while it lives, the
/// credential's entry in [`State::pending_saves`] is raised.
pub(crate) struct PendingSave {
    shared: Arc<Shared>,
    id: String,
}

/// One result mutation's admission. Only the async fast path may opt out of
/// Codex tail detachment, after checking eligibility under this state lock.
struct ResultAdmission<'a> {
    gate: super::ReloadGuard,
    operation: Option<crate::gate::Operation>,
    state: MutexGuard<'a, State>,
    now: Timestamp,
    inline_ineligible: bool,
}

impl Drop for PendingSave {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        if let Some(count) = state.pending_saves.get_mut(&self.id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.pending_saves.remove(&self.id);
            }
        }
    }
}

impl Manager {
    /// Counts the snapshot of `id` the caller just published, to be saved
    /// with `save`, as pending until the returned guard drops. Called under
    /// the state lock, in the critical section that publishes, and dropped
    /// with the lock released. Every publication that is saved is counted,
    /// whether or not the save could be admitted later.
    pub(crate) fn pending_save(
        &self,
        state: &mut State,
        id: &str,
        save: Save,
    ) -> Option<PendingSave> {
        if save == Save::No {
            return None;
        }
        *state.pending_saves.entry(id.to_owned()).or_insert(0) += 1;
        state.save_publications += 1;
        let sequence = state.save_publications;
        state.save_sequence.insert(id.to_owned(), sequence);
        Some(PendingSave {
            shared: Arc::clone(&self.shared),
            id: id.to_owned(),
        })
    }
}

/// Whether an update is a plain replacement or a refresh result to merge.
enum UpdateMode<'a> {
    Replace,
    /// An edit of the live credential, made under the state lock. The
    /// closure fails with `Err(())` after it kept its own error, and says
    /// with `Ok(false)` that nothing changed.
    Edit {
        edit: &'a mut dyn FnMut(&mut Auth) -> Result<bool, ()>,
        expected_epoch: u64,
    },
    /// The credential the refresh started from, and its epoch then.
    Refresh {
        base: &'a Auth,
        base_epoch: u64,
        /// Set only by a leased pass; every caller passes `None` until then.
        lease: Option<&'a LeasedTail>,
    },
}

/// What the ordinary save after a leased refresh must respect.
#[derive(Clone)]
pub(crate) struct LeasedTail {
    /// The lease's origin identity. Every fence compares against it.
    pub(crate) origin: CodexOrigin,
    /// The pass's final rotation; shares its `written` flag with every clone.
    pub(crate) rotation: CredentialRefreshPatch,
    pub(crate) disposition: LeasedDisposition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeasedDisposition {
    /// The ordinary save may persist the rotation (idempotent once landed;
    /// retried if not).
    Persist,
    /// Policy forbade persisting the received rotation: no Refresh patch is
    /// synthesized from this response, and nothing persists it.
    PolicyRefused,
    /// Publication could not prove the record's origin: the rotation is live
    /// and the origin is unresolved, and the ordinary save writes nothing and
    /// synthesizes nothing, as for `PolicyRefused`, but merges with
    /// `ResponseCredentials::Merge`.
    OriginUnresolved,
}

/// Attempts per origin check before the result counts as `Unproven`.
pub(crate) const ORIGIN_CHECK_ATTEMPTS: u32 = 3;

/// The live record's origin moved between the proof and the decision.
pub(crate) struct OriginMoved;

/// One proof of origin identity: made outside the state lock and under the
/// lease, checked under the state lock. The tail builds none; it calls only the
/// I/O-free [`OriginCheck::resolve`].
pub(crate) struct OriginCheck {
    /// The record's origin when the proof was made.
    current: Option<CodexOrigin>,
    /// `pinned.prove(current)`, or `Unproven` for a failed task or no `current`.
    proof: OriginProof,
    /// The final permitted attempt.
    last: bool,
}

impl OriginCheck {
    /// I/O-free: the record's origin under the store's current binding. No
    /// store, no record, an error, or an opaque store's `Ok(None)` give `None`.
    pub(crate) fn resolve(
        store: Option<&dyn AuthStore>,
        record: Option<&Auth>,
    ) -> Option<CodexOrigin> {
        match (store, record) {
            (Some(store), Some(record)) => store.codex_origin(record).ok().flatten(),
            _ => None,
        }
    }

    /// For the caller that ran the proof.
    pub(crate) fn proved(current: Option<CodexOrigin>, proof: OriginProof, last: bool) -> Self {
        Self {
            current,
            proof,
            last,
        }
    }

    /// I/O-free; under the state lock. Does `live`, resolved now, name the
    /// pinned origin's file? `Err(OriginMoved)` when live resolves to a path
    /// this check did not prove and attempts remain.
    pub(crate) fn verdict(
        &self,
        store: Option<&dyn AuthStore>,
        origin: &CodexOrigin,
        live: &Auth,
    ) -> Result<OriginProof, OriginMoved> {
        let Some(now) = Self::resolve(store, Some(live)) else {
            return Ok(OriginProof::Unproven);
        };
        if now == *origin {
            return Ok(OriginProof::Same);
        }
        if self.current.as_ref() == Some(&now) {
            return Ok(self.proof);
        }
        if self.last {
            return Ok(OriginProof::Unproven);
        }
        Err(OriginMoved)
    }
}

/// Fence 3's I/O-free verdict on one resolution. `written` is the path fence 2
/// proved `Same` under that path's credential lock and then wrote. Never
/// `Different`: fence 3 holds no credential lock.
pub(crate) fn tail_verdict(
    x: Option<&CodexOrigin>,
    origin: &CodexOrigin,
    written: Option<&CodexOrigin>,
) -> OriginProof {
    match x {
        Some(x) if x == origin || Some(x) == written => OriginProof::Same,
        _ => OriginProof::Unproven,
    }
}

/// Single-use, token-free authority captured before a login or bind writes.
/// A failed command must drop this value without adopting it.
#[doc(hidden)]
pub struct CompletedTokenCommand {
    id: String,
    partition: crate::auth::CodexCredentialPartition,
    registration_epoch: u64,
    discard_epoch: u64,
    binding_generation: u64,
    provider: String,
}

/// Result of a save-serialized fresh observation.
#[doc(hidden)]
pub enum AdoptionResult {
    Published { auth: Arc<Auth>, revision: u64 },
    Unchanged,
    Retry,
    RegistrationChanged,
}

struct PublishedUpdate {
    snapshot: Arc<Auth>,
    epoch: u64,
    generation: u64,
    discard_epoch: u64,
    pending: Option<PendingSave>,
    revision: Option<u64>,
    changed: bool,
    ineligible: bool,
    metadata_unchanged: bool,
    /// The leased pass's tail; `None` for every other update.
    lease: Option<Box<LeasedTail>>,
}

#[derive(Default)]
struct PublishContext {
    file_observation: bool,
    origin: Option<(Arc<Auth>, u64)>,
    same_origin: Option<bool>,
    revision: Option<u64>,
    command: Option<CompletedTokenCommand>,
    policy_only_ineligible: bool,
    metadata_unchanged: bool,
}

/// Cannot escape the closure borrowing it, or be retained through persistence.
pub(crate) struct AdoptionGuard<'a> {
    _saved: MutexGuard<'a, ()>,
    _permit: &'a ReloadGuard,
}

enum ObservedUpdate {
    Published(PublishedUpdate),
    Unchanged,
    Retry,
    RegistrationChanged,
}

impl Manager {
    /// Registers `executor` for the provider it names, replacing (and
    /// closing the sessions of) any executor registered for it before
    /// (upstream's `RegisterExecutor`).
    pub fn register_executor(&self, executor: Arc<dyn ProviderExecutor>) {
        let provider = executor.id().trim().to_owned();
        if provider.is_empty() {
            return;
        }
        let (replaced, to_reschedule) = {
            let mut state = self.lock();
            let replaced = state.executors.insert(provider.clone(), executor.clone());
            let ids: Vec<String> = state
                .auths
                .iter()
                .filter(|(_, entry)| equal_fold(&executor_key_from_auth(&entry.auth), &provider))
                .map(|(id, _)| id.clone())
                .collect();
            (replaced, ids)
        };
        for id in &to_reschedule {
            self.queue_refresh_reschedule(id);
        }
        if let Some(replaced) = replaced
            && !Arc::ptr_eq(&replaced, &executor)
            && !replaced.shares_execution_sessions_with(&*executor)
        {
            Self::close_all_sessions(&*replaced);
        }
    }

    /// Removes the executor registered for `provider`, matched lower case
    /// (upstream's `UnregisterExecutor`).
    pub fn unregister_executor(&self, provider: &str) {
        let provider = go_lower(provider.trim());
        if provider.is_empty() {
            return;
        }
        self.lock().executors.remove(&provider);
    }

    /// Adds a credential, or registers its ID again (upstream's `Register`).
    /// A credential without an ID gets a random one, and one without an
    /// index gets one derived (see [`Auth::ensure_index`]). The credential is
    /// saved to the store; a failed save is logged, not returned.
    pub fn register(&self, auth: Auth) -> Result<Arc<Auth>, ManagerError> {
        self.register_with(auth, Save::Yes)
    }

    /// [`register`](Self::register) without saving.
    pub fn register_unsaved(&self, auth: Auth) -> Result<Arc<Auth>, ManagerError> {
        self.register_with(auth, Save::No)
    }

    fn register_with(&self, mut auth: Auth, save: Save) -> Result<Arc<Auth>, ManagerError> {
        normalize_credential_metadata(&mut auth.metadata);
        validate_weight(&auth)
            .map_err(|err| ManagerError::InvalidWeight(format!("register auth: {err}")))?;
        if auth.id.is_empty() {
            auth.id = new_uuid();
        }
        let gate = self.mutation_gate();
        let now = self.now();
        if auth.created_at.is_none() {
            auth.created_at = Some(now);
        }
        auth.updated_at = Some(now);
        normalize_model_states(&mut auth.model_states);
        let settings = self.settings();
        if cooldown_disabled_for_auth(&settings, &auth) || is_disabled(&auth) {
            clear_cooldown_state_for_auth(&mut auth, now);
        }
        auth.ensure_index();
        let snapshot = {
            let mut guard = self.lock();
            let state = &mut *guard;
            self.register_locked(state, auth, save, now)
        };
        let (snapshot, pending) = snapshot;
        self.queue_refresh_reschedule(&snapshot.id);
        cooldown_store::changed(self);
        if let Err(err) = self.persist(&snapshot, snapshot.registration_epoch, 1, 0, save) {
            log_persist_failure(&snapshot, "failed to persist registered auth", &err);
        }
        drop(pending);
        drop(gate);
        Ok(snapshot)
    }

    fn register_locked(
        &self,
        state: &mut State,
        mut auth: Auth,
        save: Save,
        now: chrono::DateTime<chrono::Utc>,
    ) -> (Arc<Auth>, Option<PendingSave>) {
        let existing = state
            .auths
            .get(&auth.id)
            .map(|entry| (entry.auth.registration_epoch, entry.auth.credential_version));
        let existing_epoch = existing.map_or(0, |(epoch, _)| epoch);
        let slot = state.epochs.entry(auth.id.clone()).or_insert(0);
        *slot = (*slot).max(existing_epoch).saturating_add(1);
        auth.registration_epoch = *slot;
        match existing {
            Some((_, version)) => {
                auth.credential_version = auth.credential_version.max(version).saturating_add(1);
            }
            None if auth.credential_version == 0 => auth.credential_version = 1,
            None => {}
        }
        auth.generation = 1;
        auth.policy_generation = 1;
        // open-ferry's capped quota rests go with the state replaced.
        state.quota_checks.forget(&auth.id);
        state.quota_recovery.forget(&auth.id);
        // The incoming credential's own cooldowns have no provenance, as
        // when saved ones are restored: holds of unknown end.
        let carried = super::quota_recovery::cooldown_scopes(&auth);
        state.quota_recovery.restored(&auth.id, &carried);
        let snapshot = Arc::new(auth);
        state.auths.insert(
            snapshot.id.clone(),
            Entry {
                auth: snapshot.clone(),
                refresh_failures: 0,
                discard_epoch: 0,
                undurable: Default::default(),
                undurable_baselines: Default::default(),
                unresolved_origin: None,
                undurable_causes: Default::default(),
                landing_unconfirmed: Default::default(),
                owed_save: None,
                policy_save: None,
                written_configuration: 0,
                read_back_warned: Default::default(),
            },
        );
        state.sync_scheduler(self.models(), &snapshot.id, now);
        let pending = self.pending_save(state, &snapshot.id, save);
        (snapshot, pending)
    }

    /// Replaces a registered credential (upstream's `Update`). Returns
    /// `None` when no credential has its ID.
    ///
    /// The new credential keeps the old one's call counts, and its index
    /// when it has none. While neither the old nor the new credential is
    /// disabled, the new one keeps the old model states when it has none, an
    /// active credential-wide quota cooldown carries over, and new tokens
    /// clear a recorded 401.
    ///
    /// This synchronous API may wait for a save and read the store. Async
    /// callers must offload it through `run_persistence` or `spawn_blocking`.
    /// For supported Codex records, an empty-metadata replacement retains the
    /// whole current metadata record and retained refresh work. An eligible
    /// publication saves the full record with pending rotations; ineligible
    /// policies still discard without saving. Unlike upstream, metadata cannot
    /// be blanked in memory:
    /// protected live tokens and non-token fields must survive later saves.
    pub fn update(&self, auth: Auth) -> Result<Option<Arc<Auth>>, ManagerError> {
        Ok(self
            .update_internal(UpdateMode::Replace, auth, Save::Yes)?
            .map(|(snapshot, _)| snapshot))
    }

    /// Changes the live credential `id` with `edit` and saves it (what
    /// [`update`](Self::update) does, but from the credential as it is when
    /// the change is made rather than from a copy the caller took earlier).
    /// `edit` runs under the manager's state lock, on a copy of the live
    /// entry: it returns `Ok(true)` to publish the copy, `Ok(false)` when
    /// nothing changed (nothing is published or saved), or `Err` to refuse
    /// (nothing is published). The result is `Ok(None)` when no credential
    /// has the ID.
    ///
    /// The copy is published as `update` publishes a replacement, so it
    /// keeps the call counts, and a refresh or adoption that landed first is
    /// never undone: an edit that leaves the tokens and API key alone can
    /// never change them, and doesn't bump the credential version. Keep
    /// `edit` short and free of calls into the manager, as it holds the
    /// lock.
    ///
    /// `edit` must not change the credential's ID: an edit whose result has
    /// another ID (an empty one included) is refused with
    /// `EditError::Manager`, and nothing is published. The registration
    /// epoch, generation, call counts and update time are the manager's own
    /// and are overwritten whatever `edit` sets.
    pub fn edit<E>(
        &self,
        id: &str,
        edit: impl FnOnce(&mut Auth) -> Result<bool, E>,
    ) -> Result<Option<Arc<Auth>>, EditError<E>> {
        self.edit_with(id, 0, edit, Save::Yes)
    }

    /// [`edit`](Self::edit) without saving, for a change that came from the
    /// store (or whatever else already holds it).
    pub fn edit_unsaved<E>(
        &self,
        id: &str,
        edit: impl FnOnce(&mut Auth) -> Result<bool, E>,
    ) -> Result<Option<Arc<Auth>>, EditError<E>> {
        self.edit_with(id, 0, edit, Save::No)
    }

    /// Edits the current record only if it still belongs to the registration
    /// selected by the caller (upstream's `Modify`). A zero epoch is unfenced;
    /// an older epoch fails before the closure runs or anything is saved.
    /// Same-registration refreshes and concurrent field edits are preserved.
    pub fn edit_at_epoch<E>(
        &self,
        id: &str,
        expected_epoch: u64,
        edit: impl FnOnce(&mut Auth) -> Result<bool, E>,
    ) -> Result<Option<Arc<Auth>>, EditError<E>> {
        self.edit_with(id, expected_epoch, edit, Save::Yes)
    }

    fn edit_with<E>(
        &self,
        id: &str,
        expected_epoch: u64,
        edit: impl FnOnce(&mut Auth) -> Result<bool, E>,
        save: Save,
    ) -> Result<Option<Arc<Auth>>, EditError<E>> {
        let mut edit = Some(edit);
        let mut rejected = None;
        let mut apply = |auth: &mut Auth| -> Result<bool, ()> {
            let edit = edit.take().ok_or(())?;
            edit(auth).map_err(|err| rejected = Some(err))
        };
        let placeholder = Auth {
            id: id.to_owned(),
            ..Auth::default()
        };
        let result = self.update_internal(
            UpdateMode::Edit {
                edit: &mut apply,
                expected_epoch,
            },
            placeholder,
            save,
        );
        if let Some(err) = rejected {
            return Err(EditError::Rejected(err));
        }
        result
            .map(|published| published.map(|(snapshot, _)| snapshot))
            .map_err(EditError::Manager)
    }

    /// [`update`](Self::update) without saving.
    pub fn update_unsaved(&self, auth: Auth) -> Result<Option<Arc<Auth>>, ManagerError> {
        Ok(self
            .update_internal(UpdateMode::Replace, auth, Save::No)?
            .map(|(snapshot, _)| snapshot))
    }

    /// Folds a refresh result into the live credential, keeping changes made
    /// since `base` was read (upstream's `UpdateRefreshedAuth`). Fails when
    /// the credential was registered again since `base_epoch`. Returns the
    /// credential and its new generation.
    ///
    /// Only a refresh pass calls this, for its unleased tail, while it holds
    /// the refresh lock; the save is therefore exempt from check (vi).
    pub(crate) fn update_refreshed(
        &self,
        base: &Auth,
        base_epoch: u64,
        updated: Auth,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        self.update_internal(
            UpdateMode::Refresh {
                base,
                base_epoch,
                lease: None,
            },
            updated,
            Save::Yes,
        )
    }

    /// [`update_refreshed`](Self::update_refreshed) for a leased pass: the
    /// ordinary save respects the tail's fences. `PolicyRefused` and
    /// `OriginUnresolved` write nothing.
    pub(crate) fn update_refreshed_leased(
        &self,
        base: &Auth,
        base_epoch: u64,
        updated: Auth,
        tail: LeasedTail,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        let save = match tail.disposition {
            LeasedDisposition::Persist => Save::Yes,
            LeasedDisposition::PolicyRefused | LeasedDisposition::OriginUnresolved => Save::No,
        };
        self.update_internal(
            UpdateMode::Refresh {
                base,
                base_epoch,
                lease: Some(&tail),
            },
            updated,
            save,
        )
    }

    // Not upstream's: a test save failure still publishes the rotated tokens
    // and reschedules, like update_refreshed when the real store fails.
    #[cfg(feature = "blue-green-test")]
    pub(crate) fn update_refreshed_after_save_failure(
        &self,
        base: &Auth,
        base_epoch: u64,
        updated: Auth,
        lease: Option<LeasedTail>,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        // The doubt must not be written under either save mode.
        let save = if lease
            .as_ref()
            .is_some_and(|tail| tail.disposition != LeasedDisposition::Persist)
        {
            Save::No
        } else {
            Save::Failed
        };
        self.update_internal(
            UpdateMode::Refresh {
                base,
                base_epoch,
                lease: lease.as_ref(),
            },
            updated,
            save,
        )
    }

    /// I3: optional command-slot mutex -> refresh mutex -> ReloadGuard ->
    /// per-id save mutex -> short state/store I/O. The slot is blocking-worker
    /// only; the registry mutex is a leaf and no synchronous guard crosses await.
    pub(crate) fn with_adoption<R>(
        &self,
        id: &str,
        permit: &ReloadGuard,
        work: impl FnOnce(&AdoptionGuard<'_>) -> R,
    ) -> R {
        self.with_persist_save_lock(id, |save_lock| {
            let guard = AdoptionGuard {
                _saved: lock(save_lock),
                _permit: permit,
            };
            work(&guard)
        })
    }

    fn with_persist_save_lock<R>(&self, id: &str, work: impl FnOnce(&Mutex<()>) -> R) -> R {
        let save_lock = lock(&self.shared.persist_save_locks)
            .entry(id.to_owned())
            .or_default()
            .clone();
        let result = work(&save_lock);
        drop(save_lock);
        if self.get(id).is_none() {
            self.prune_idle_save_lock(id);
        }
        result
    }

    fn prune_idle_save_lock(&self, id: &str) {
        let mut locks = lock(&self.shared.persist_save_locks);
        if locks
            .get(id)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            locks.remove(id);
        }
    }

    /// Adopts CURRENT bytes read by `read`, never the hint's captured tokens
    /// when read-back is supported. Missing/invalid reads leave live work alone.
    /// Store permits and credential locks must be released before entering.
    /// This is synchronous store I/O; async callers must offload the whole call.
    #[doc(hidden)]
    pub fn adopt_observed(
        &self,
        hint: &Auth,
        command: Option<CompletedTokenCommand>,
        read: impl FnOnce(&Auth) -> Result<AdoptionRead, ManagerError>,
    ) -> Result<AdoptionResult, ManagerError> {
        if hint.id.is_empty() {
            return Ok(AdoptionResult::Unchanged);
        }
        let permit = self.mutation_gate();
        let observed = self.with_adoption(&hint.id, &permit, |guard| {
            self.observe_update(
                guard,
                hint.clone(),
                command,
                Save::No,
                (false, || true),
                read,
            )
        })?;
        match observed {
            ObservedUpdate::Published(published) => {
                let revision = published.revision.expect("observation revision");
                let auth = self.finish_update(published, false, Save::No, permit)?;
                Ok(match auth {
                    Some((auth, _)) => AdoptionResult::Published { auth, revision },
                    None => AdoptionResult::Unchanged,
                })
            }
            ObservedUpdate::Unchanged => Ok(AdoptionResult::Unchanged),
            ObservedUpdate::Retry => Ok(AdoptionResult::Retry),
            ObservedUpdate::RegistrationChanged => Ok(AdoptionResult::RegistrationChanged),
        }
    }

    /// Token-free registration/tombstone context for a queued observation.
    #[doc(hidden)]
    pub fn observation_registration(&self, id: &str) -> (Option<u64>, u64) {
        let state = self.lock();
        (
            state.auths.get(id).map(|e| e.auth.registration_epoch),
            state.epochs.get(id).copied().unwrap_or(0),
        )
    }

    /// Service observation with a config-agnostic publication fence. `validate`
    /// runs under state, must not block or call the manager, and rejects with
    /// Retry without publishing. Unlike `adopt_observed`, this entry also permits
    /// initial registration if the same absence/tombstone survives the read.
    #[doc(hidden)]
    pub fn adopt_observed_validated(
        &self,
        hint: &Auth,
        registration: (Option<u64>, u64),
        read: impl FnOnce(&Auth) -> Result<AdoptionRead, ManagerError>,
        validate: impl FnOnce() -> bool,
    ) -> Result<AdoptionResult, ManagerError> {
        self.adopt_service_validated(hint, registration, &mut None, read, validate)
    }

    /// A completed service command retains its token-free authority when reading
    /// fails, and transfers it by value only to the publishing transaction.
    /// Call only after the command wrote successfully and released store locks.
    #[doc(hidden)]
    pub fn adopt_completed_token_command_validated(
        &self,
        hint: &Auth,
        registration: (Option<u64>, u64),
        command: &mut Option<CompletedTokenCommand>,
        read: impl FnOnce(&Auth) -> Result<AdoptionRead, ManagerError>,
        validate: impl FnOnce() -> bool,
    ) -> Result<AdoptionResult, ManagerError> {
        if command.is_none() {
            return Ok(AdoptionResult::Unchanged);
        }
        self.adopt_service_validated(hint, registration, command, read, validate)
    }

    fn adopt_service_validated(
        &self,
        hint: &Auth,
        registration: (Option<u64>, u64),
        command: &mut Option<CompletedTokenCommand>,
        read: impl FnOnce(&Auth) -> Result<AdoptionRead, ManagerError>,
        validate: impl FnOnce() -> bool,
    ) -> Result<AdoptionResult, ManagerError> {
        if hint.id.is_empty() {
            return Ok(AdoptionResult::Unchanged);
        }
        let permit = self.mutation_gate();
        let result = self.with_adoption(&hint.id, &permit, |guard| {
            let absence = {
                let state = self.lock();
                if (
                    state.auths.get(&hint.id).map(|e| e.auth.registration_epoch),
                    state.epochs.get(&hint.id).copied().unwrap_or(0),
                ) != registration
                {
                    return Ok(ObservedUpdate::RegistrationChanged);
                }
                if state.auths.contains_key(&hint.id) {
                    None
                } else {
                    Some(state.epochs.get(&hint.id).copied().unwrap_or(0))
                }
            };
            let Some(epoch) = absence else {
                return self.observe_update_retaining_command(
                    guard,
                    hint.clone(),
                    command,
                    Save::No,
                    (false, validate),
                    read,
                );
            };
            let binding = self
                .shared
                .store
                .as_ref()
                .map_or(0, |s| s.binding_generation());
            let revision = crate::config::next_revision();
            let mut auth = match read(hint)? {
                AdoptionRead::Record(auth) => auth,
                AdoptionRead::Unsupported => hint.clone(),
                AdoptionRead::Missing | AdoptionRead::Invalid => return Ok(ObservedUpdate::Retry),
            };
            if auth.id != hint.id {
                return Ok(ObservedUpdate::RegistrationChanged);
            }
            normalize_credential_metadata(&mut auth.metadata);
            validate_weight(&auth)
                .map_err(|e| ManagerError::InvalidWeight(format!("register auth: {e}")))?;
            let now = self.now();
            auth.created_at.get_or_insert(now);
            auth.updated_at = Some(now);
            normalize_model_states(&mut auth.model_states);
            auth.ensure_index();
            let mut state = self.lock();
            if state.auths.contains_key(&hint.id)
                || state.epochs.get(&hint.id).copied().unwrap_or(0) != epoch
                || self
                    .shared
                    .store
                    .as_ref()
                    .is_some_and(|s| s.binding_generation() != binding)
            {
                return Ok(ObservedUpdate::RegistrationChanged);
            }
            if !validate() {
                return Ok(ObservedUpdate::Retry);
            }
            if cooldown_disabled_for_auth(&state.settings, &auth) || is_disabled(&auth) {
                clear_cooldown_state_for_auth(&mut auth, now);
            }
            // A captured registration that disappeared cannot authorize this
            // new registration; successful observation still consumes the slot.
            drop(command.take());
            let (snapshot, pending) = self.register_locked(&mut state, auth, Save::No, now);
            Ok(ObservedUpdate::Published(PublishedUpdate {
                epoch: snapshot.registration_epoch,
                generation: snapshot.generation,
                snapshot,
                discard_epoch: 0,
                pending,
                revision: Some(revision),
                changed: true,
                ineligible: false,
                metadata_unchanged: false,
                lease: None,
            }))
        })?;
        match result {
            ObservedUpdate::Published(published) => {
                let revision = published.revision.expect("observation revision");
                self.queue_refresh_reschedule(&hint.id);
                let auth = self.finish_update(published, false, Save::No, permit)?;
                Ok(auth.map_or(AdoptionResult::Unchanged, |(auth, _)| {
                    AdoptionResult::Published { auth, revision }
                }))
            }
            ObservedUpdate::Unchanged => Ok(AdoptionResult::Unchanged),
            ObservedUpdate::Retry => Ok(AdoptionResult::Retry),
            ObservedUpdate::RegistrationChanged => Ok(AdoptionResult::RegistrationChanged),
        }
    }

    /// Captures token-free authority BEFORE a login or bind writes its record.
    #[doc(hidden)]
    pub fn capture_token_command(
        &self,
        id: &str,
        partition: crate::auth::CodexCredentialPartition,
    ) -> Option<CompletedTokenCommand> {
        let state = self.lock();
        let entry = state.auths.get(id)?;
        Some(CompletedTokenCommand {
            id: entry.auth.id.clone(),
            partition,
            registration_epoch: entry.auth.registration_epoch,
            discard_epoch: entry.discard_epoch,
            binding_generation: self
                .shared
                .store
                .as_ref()
                .map_or(0, |store| store.binding_generation()),
            provider: entry.auth.provider.clone(),
        })
    }

    /// Adopt only after the login or bind successfully writes its record and
    /// releases its store locks. A failed command drops its authority unused.
    /// Every outcome consumes the authority, including Retry. Retry fails closed:
    /// the marker remains until a verified write or a new valid command; callers
    /// must not expect to retry with the same authority. Returning authority with
    /// Retry is left to service plumbing, not this signature.
    #[doc(hidden)]
    pub fn adopt_completed_token_command(
        &self,
        hint: &Auth,
        command: CompletedTokenCommand,
    ) -> Result<AdoptionResult, ManagerError> {
        self.adopt_observed(hint, Some(command), |live| match &self.shared.store {
            Some(store) => store.read_for_adoption(live).map_err(ManagerError::Store),
            None => Ok(AdoptionRead::Unsupported),
        })
    }

    fn observe_update(
        &self,
        _guard: &AdoptionGuard<'_>,
        incoming: Auth,
        mut command: Option<CompletedTokenCommand>,
        save: Save,
        publication: (bool, impl FnOnce() -> bool),
        read: impl FnOnce(&Auth) -> Result<AdoptionRead, ManagerError>,
    ) -> Result<ObservedUpdate, ManagerError> {
        self.observe_update_retaining_command(
            _guard,
            incoming,
            &mut command,
            save,
            publication,
            read,
        )
    }

    fn observe_update_retaining_command(
        &self,
        _guard: &AdoptionGuard<'_>,
        mut incoming: Auth,
        command: &mut Option<CompletedTokenCommand>,
        save: Save,
        publication: (bool, impl FnOnce() -> bool),
        read: impl FnOnce(&Auth) -> Result<AdoptionRead, ManagerError>,
    ) -> Result<ObservedUpdate, ManagerError> {
        let (replace, validate) = publication;
        let captured = {
            let state = self.lock();
            let Some(entry) = state.auths.get(&incoming.id) else {
                return Ok(ObservedUpdate::RegistrationChanged);
            };
            entry.auth.clone()
        };
        let binding = self
            .shared
            .store
            .as_ref()
            .map_or(0, |store| store.binding_generation());
        let revision = crate::config::next_revision();
        let observation = read(&captured)?;
        if self
            .shared
            .store
            .as_ref()
            .is_some_and(|store| store.binding_generation() != binding)
        {
            return Ok(ObservedUpdate::RegistrationChanged);
        }
        let same_origin = match observation {
            AdoptionRead::Missing | AdoptionRead::Invalid => return Ok(ObservedUpdate::Retry),
            AdoptionRead::Unsupported => None,
            AdoptionRead::Record(record) => {
                let same = match self.shared.store.as_ref() {
                    Some(store) => match store.same_record(&captured, &record) {
                        Ok(same) => same,
                        Err(_) => {
                            tracing::debug!(auth_id = %captured.id, "adoption origin proof missed");
                            return Ok(ObservedUpdate::Retry);
                        }
                    },
                    None => true,
                };
                if !same
                    || record.id != captured.id
                    || !equal_fold(record.provider.trim(), captured.provider.trim())
                {
                    tracing::debug!(auth_id = %captured.id, "adoption origin proof missed");
                    return Ok(ObservedUpdate::Retry);
                }
                if replace {
                    copy_observed_credentials(&mut incoming, &record);
                } else {
                    incoming = record;
                }
                Some(true)
            }
        };
        normalize_credential_metadata(&mut incoming.metadata);
        validate_weight(&incoming)
            .map_err(|err| ManagerError::InvalidWeight(format!("update auth: {err}")))?;
        let context = PublishContext {
            file_observation: !replace,
            origin: Some((captured, binding)),
            same_origin,
            revision: Some(revision),
            command: None,
            policy_only_ineligible: false,
            metadata_unchanged: false,
        };
        let mut state = self.lock();
        if !validate() {
            return Ok(ObservedUpdate::Retry);
        }
        if let Some((captured, _)) = &context.origin
            && let Some(entry) = state.auths.get(&incoming.id)
            && same_record_identity(captured, &entry.auth)
            && equal_fold(entry.auth.provider.trim(), captured.provider.trim())
            && (entry.auth.policy_generation != captured.policy_generation
                || entry.auth.credential_version != captured.credential_version
                || !same_observation_policy(captured, &entry.auth))
        {
            return Ok(ObservedUpdate::Retry);
        }
        let context = PublishContext {
            command: command.take(),
            ..context
        };
        match self.publish_update_locked(
            &mut state,
            UpdateMode::Replace,
            incoming,
            context,
            save,
        )? {
            Some(published) => Ok(ObservedUpdate::Published(published)),
            None => Ok(ObservedUpdate::RegistrationChanged),
        }
    }

    /// Upstream's `updateInternal`, for the replace and refresh modes.
    fn update_internal(
        &self,
        mode: UpdateMode<'_>,
        mut auth: Auth,
        save: Save,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        if auth.id.is_empty() {
            return Ok(None);
        }
        normalize_credential_metadata(&mut auth.metadata);
        validate_weight(&auth)
            .map_err(|err| ManagerError::InvalidWeight(format!("update auth: {err}")))?;
        let gate = self.mutation_gate();
        let is_refresh = matches!(mode, UpdateMode::Refresh { .. });
        let published = if matches!(mode, UpdateMode::Replace) {
            let id = auth.id.clone();
            self.with_adoption(&id, &gate, |guard| {
                if let Some(store) = &self.shared.store
                    && self.permanently_ineligible(&auth)
                {
                    let Some(captured) = self.get(&id) else {
                        return Ok(ObservedUpdate::RegistrationChanged);
                    };
                    let binding = store.binding_generation();
                    // Policy-only discard must work even when a supported
                    // record is missing, invalid or unreadable. Never adopt disk
                    // credentials here; only Unsupported accepts incoming ones.
                    let supported = !matches!(
                        store.read_for_adoption(&captured),
                        Ok(AdoptionRead::Unsupported)
                    );
                    // Unlike upstream's blank metadata, an empty supported
                    // replacement means metadata unchanged. Read the current
                    // record under state, retaining pending work when eligible;
                    // runtime-only and other ineligible policies still discard.
                    let metadata_unchanged = supported && auth.metadata.is_empty();
                    if supported && !metadata_unchanged {
                        copy_observed_credentials(&mut auth, &captured);
                    }
                    let context = PublishContext {
                        file_observation: false,
                        origin: Some((captured, binding)),
                        same_origin: Some(true),
                        policy_only_ineligible: !metadata_unchanged,
                        metadata_unchanged,
                        ..PublishContext::default()
                    };
                    let mut state = self.lock();
                    return self
                        .publish_update_locked(&mut state, UpdateMode::Replace, auth, context, save)
                        .map(|published| {
                            published.map_or(
                                ObservedUpdate::RegistrationChanged,
                                ObservedUpdate::Published,
                            )
                        });
                }
                self.observe_update(guard, auth, None, save, (true, || true), |live| match self
                    .shared
                    .store
                    .as_ref()
                {
                    Some(store) => store.read_for_adoption(live).map_err(ManagerError::Store),
                    None => Ok(AdoptionRead::Unsupported),
                })
            })?
        } else {
            let mut state = self.lock();
            match self.publish_update_locked(
                &mut state,
                mode,
                auth,
                PublishContext::default(),
                save,
            )? {
                Some(published) => ObservedUpdate::Published(published),
                None => ObservedUpdate::Unchanged,
            }
        };
        match published {
            ObservedUpdate::Published(published) => {
                self.finish_update(published, is_refresh, save, gate)
            }
            ObservedUpdate::Unchanged | ObservedUpdate::RegistrationChanged => Ok(None),
            ObservedUpdate::Retry => Err(ManagerError::AdoptionRetry),
        }
    }

    /// State-only publication: callers already own admission and, for
    /// observations, the save mutex. Never performs store I/O or takes permits.
    fn publish_update_locked(
        &self,
        state: &mut State,
        mode: UpdateMode<'_>,
        mut auth: Auth,
        context: PublishContext,
        save: Save,
    ) -> Result<Option<PublishedUpdate>, ManagerError> {
        let now = self.now();
        let is_refresh = matches!(mode, UpdateMode::Refresh { .. });
        let is_file_observation = context.file_observation;
        let policy_edit = save == Save::Yes
            && matches!(&mode, UpdateMode::Edit { .. } | UpdateMode::Replace)
            && !is_file_observation;
        let lease_tail = match &mode {
            UpdateMode::Refresh { lease, .. } => lease.cloned().map(Box::new),
            _ => None,
        };
        if let Some((captured, binding)) = &context.origin
            && (state.auths.get(&auth.id).is_none_or(|entry| {
                !equal_fold(entry.auth.provider.trim(), captured.provider.trim())
                    || !same_record_identity(captured, &entry.auth)
            }) || self
                .shared
                .store
                .as_ref()
                .is_some_and(|store| store.binding_generation() != *binding))
        {
            return Ok(None);
        }
        let Some(existing) = state.auths.get(&auth.id) else {
            return Ok(None);
        };
        let existing_auth = existing.auth.clone();
        let existing_epoch = existing_auth.registration_epoch;
        let existing_generation = existing_auth.generation;
        let existing_failures = existing.refresh_failures;
        let existing_discard_epoch = existing.discard_epoch;
        let mut undurable = existing.undurable;
        let mut undurable_baselines = existing.undurable_baselines.clone();
        let mut undurable_causes = existing.undurable_causes.clone();
        let mut landing_unconfirmed = existing.landing_unconfirmed;
        let owed_save = existing.owed_save;
        let mut policy_save = existing.policy_save.clone();
        let written_configuration = existing.written_configuration;
        let mut read_back_warned = existing.read_back_warned;
        let mut unresolved_origin = existing.unresolved_origin.clone();
        if let Some(command) = context.command
            && command.id == existing_auth.id
            && command.registration_epoch == existing_epoch
            && command.discard_epoch == existing_discard_epoch
            && command.binding_generation
                == self
                    .shared
                    .store
                    .as_ref()
                    .map_or(0, |store| store.binding_generation())
            && equal_fold(command.provider.trim(), existing_auth.provider.trim())
        {
            match command.partition {
                crate::auth::CodexCredentialPartition::Outer => {
                    undurable.outer = false;
                    undurable_baselines.outer.clear();
                    undurable_causes.outer = None;
                    landing_unconfirmed.outer = false;
                    read_back_warned.outer = false;
                    if let Some(u) = unresolved_origin.as_mut() {
                        u.partitions.outer = false;
                    }
                }
                crate::auth::CodexCredentialPartition::Siwc => {
                    undurable.siwc = false;
                    undurable_baselines.siwc.clear();
                    undurable_causes.siwc = None;
                    landing_unconfirmed.siwc = false;
                    read_back_warned.siwc = false;
                    if let Some(u) = unresolved_origin.as_mut() {
                        u.partitions.siwc = false;
                    }
                }
            }
            // The state ends with its last partition.
            if unresolved_origin
                .as_ref()
                .is_some_and(|u| !u.partitions.outer && !u.partitions.siwc)
            {
                unresolved_origin = None;
            }
        }
        // Computed after the command's clear, so a token the command just
        // installed is not replaced by an older rotation.
        let protected = undurable.union(
            unresolved_origin
                .as_ref()
                .map(|u| u.partitions)
                .unwrap_or_default(),
        );
        if !policy_edit
            && matches!(&mode, UpdateMode::Replace)
            && let Some(policy) = &policy_save
        {
            policy.overlay(&mut auth);
        }
        let slot = state.epochs.entry(auth.id.clone()).or_insert(0);
        *slot = (*slot).max(existing_epoch);
        let live_epoch = *slot;
        let refresh_failures = match mode {
            UpdateMode::Replace => {
                if context.metadata_unchanged {
                    auth.metadata = existing_auth.metadata.clone();
                }
                if crate::auth::codex_refresh::is_codex_auth(&existing_auth) {
                    overlay_undurable(&mut auth, &existing_auth, protected);
                }
                auth.rebase_pending_refreshes_with_origin(
                    &existing_auth,
                    context.same_origin.map(crate::auth::ResolvedOriginProof),
                );
                auth.codex_pending_refresh = pending_for_edit(
                    &existing_auth,
                    &auth.metadata,
                    self.permanently_ineligible(&auth),
                );
                auth.codex_persist_intent = if context.metadata_unchanged {
                    existing_auth.codex_persist_intent.clone()
                } else {
                    Default::default()
                };
                if context.metadata_unchanged
                    || (is_file_observation && !is_disabled(&existing_auth) && !is_disabled(&auth))
                {
                    auth.codex_refresh_failure = existing_auth.codex_refresh_failure.clone();
                    auth.next_refresh_after = existing_auth.next_refresh_after;
                    existing_failures
                } else {
                    auth.codex_refresh_failure = None;
                    0
                }
            }
            UpdateMode::Edit {
                edit,
                expected_epoch,
            } => {
                if expected_epoch != 0 && expected_epoch < live_epoch {
                    return Err(ManagerError::Other(format!(
                        "update auth {}: stale registration epoch {expected_epoch} < {live_epoch}",
                        auth.id
                    )));
                }
                let mut edited = Auth::clone(&existing_auth);
                match edit(&mut edited) {
                    Err(()) => return Err(ManagerError::Other("edit refused".to_owned())),
                    Ok(false) => {
                        return Ok(Some(PublishedUpdate {
                            snapshot: existing_auth,
                            epoch: existing_epoch,
                            generation: existing_generation,
                            discard_epoch: existing_discard_epoch,
                            pending: None,
                            revision: context.revision,
                            changed: false,
                            ineligible: false,
                            metadata_unchanged: false,
                            lease: None,
                        }));
                    }
                    Ok(true) => {}
                }
                // The ID is the registration's key: an edit that moved
                // it would publish this credential under another ID.
                if edited.id != existing_auth.id {
                    return Err(ManagerError::Other(format!(
                        "edit auth {}: the edit changed the ID to {:?}",
                        existing_auth.id, edited.id
                    )));
                }
                normalize_credential_metadata(&mut edited.metadata);
                validate_weight(&edited)
                    .map_err(|err| ManagerError::InvalidWeight(format!("update auth: {err}")))?;
                auth = edited;
                auth.codex_pending_refresh = pending_for_edit(
                    &existing_auth,
                    &auth.metadata,
                    self.permanently_ineligible(&auth),
                );
                // Settings edits must not inherit an in-flight rotation's
                // partition-only persistence intent from the live snapshot.
                auth.codex_persist_intent = Default::default();
                auth.codex_refresh_failure = None;
                0
            }
            UpdateMode::Refresh {
                base,
                base_epoch,
                lease,
            } => {
                // Under `PolicyRefused` no Refresh patch is synthesized from
                // the response, and its credentials are never merged.
                let refused =
                    lease.is_some_and(|tail| tail.disposition == LeasedDisposition::PolicyRefused);
                if existing_epoch != base_epoch {
                    return Err(ManagerError::Other(format!(
                        "update auth {}: stale registration epoch {base_epoch} != {existing_epoch}",
                        auth.id
                    )));
                }
                if !refused
                    && crate::auth::codex_refresh::is_codex_auth(&auth)
                    && !matches!(
                        auth.codex_persist_intent,
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(_)
                    )
                {
                    auth.codex_persist_intent =
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(
                            crate::auth::codex_refresh::CredentialRefreshPatch::new(
                                &base.metadata,
                                &auth.metadata,
                                crate::auth::codex_refresh::OUTER_KEYS
                                    .iter()
                                    .any(|key| base.metadata.get(*key) != auth.metadata.get(*key)),
                                base.metadata.get("siwc") != auth.metadata.get("siwc")
                                    && auth.metadata.contains_key("siwc"),
                            ),
                        );
                }
                if let crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) =
                    &auth.codex_persist_intent
                {
                    auth.codex_persist_intent =
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(
                            patch.with_durable_baselines(
                                &undurable_baselines.outer,
                                &undurable_baselines.siwc,
                            ),
                        );
                }
                // A refresh that started before the tokens or API key
                // were replaced mustn't put the old ones back: the
                // credential stays as it is.
                if (existing_auth.credential_version != base.credential_version
                    || credentials_changed(base, &existing_auth))
                    && !matches!(&auth.codex_persist_intent,
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch)
                            if patch.can_apply_to(&existing_auth.metadata))
                {
                    return Ok(Some(PublishedUpdate {
                        snapshot: existing_auth,
                        epoch: existing_epoch,
                        generation: existing_generation,
                        discard_epoch: existing_discard_epoch,
                        pending: None,
                        revision: context.revision,
                        changed: false,
                        ineligible: false,
                        metadata_unchanged: false,
                        lease: lease_tail.clone(),
                    }));
                }
                auth = merge_refreshed_auth_with(
                    base,
                    &existing_auth,
                    &auth,
                    now,
                    if refused {
                        ResponseCredentials::Refused
                    } else {
                        ResponseCredentials::Merge
                    },
                );
                normalize_credential_metadata(&mut auth.metadata);
                // A marked partition's admitted write can land even when its
                // read-back fails. Retain its possible results before admission,
                // rather than waiting for a later discard of an unwritten clone.
                if let crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) =
                    &auth.codex_persist_intent
                {
                    for (partition, marked, recorded) in [
                        (
                            crate::auth::CodexCredentialPartition::Outer,
                            undurable.outer,
                            &mut undurable_baselines.outer,
                        ),
                        (
                            crate::auth::CodexCredentialPartition::Siwc,
                            undurable.siwc,
                            &mut undurable_baselines.siwc,
                        ),
                    ] {
                        if marked && patch.includes_partition(partition) {
                            for baseline in patch.durable_baselines(partition) {
                                crate::auth::codex_refresh::record_durable_baseline(
                                    recorded, &baseline,
                                );
                            }
                        }
                    }
                }
                // The merge starts from the live credential, so it
                // carries the live epoch and failure count.
                if existing_epoch < live_epoch {
                    return Err(ManagerError::Other(format!(
                        "update auth {}: stale registration epoch {existing_epoch} < {live_epoch}",
                        auth.id
                    )));
                }
                existing_failures
            }
        };
        if !policy_edit && let Some(policy) = &policy_save {
            policy.overlay(&mut auth);
        }
        let ineligible = context.policy_only_ineligible || self.permanently_ineligible(&auth);
        let mut discard_epoch = existing_discard_epoch;
        if ineligible {
            discard_refresh_work(
                &mut discard_epoch,
                &mut undurable,
                &mut undurable_baselines,
                &mut undurable_causes,
                self.shared.store.is_some(),
                &[&existing_auth, &auth],
            );
        }
        if ineligible {
            auth.codex_pending_refresh.clear();
            auth.codex_persist_intent = Default::default();
        }
        if auth.index.is_empty() {
            auth.index = existing_auth.index.clone();
        }
        if is_file_observation && auth.created_at.is_none() {
            auth.created_at = existing_auth.created_at;
        }
        auth.success = existing_auth.success;
        auth.failed = existing_auth.failed;
        auth.recent_requests = existing_auth.recent_requests.clone();
        let mut generation = existing_generation.saturating_add(1);
        let existing_version = existing_auth.credential_version.max(1);
        let changed = credentials_changed(&existing_auth, &auth);
        auth.credential_version = if changed {
            auth.credential_version
                .max(existing_version)
                .saturating_add(1)
        } else {
            existing_version
        };
        auth.preserve_rejected_tokens_from(&existing_auth, is_refresh);
        if !is_disabled(&existing_auth) && !is_disabled(&auth) {
            if is_file_observation {
                auth.model_states = existing_auth.model_states.clone();
                auth.last_refreshed_at = existing_auth.last_refreshed_at;
                auth.next_refresh_after = existing_auth.next_refresh_after;
                if matches!(auth.status, Status::Active | Status::Unknown)
                    && existing_auth.status != Status::Unknown
                {
                    auth.status = existing_auth.status;
                }
                if auth.status_message.is_empty() {
                    auth.status_message = existing_auth.status_message.clone();
                }
                auth.unavailable = existing_auth.unavailable;
                auth.next_retry_after = existing_auth.next_retry_after;
                if auth.last_error.is_none() {
                    auth.last_error = existing_auth.last_error.clone();
                }
                let observation =
                    merge_quota_observation(std::mem::take(&mut auth.quota), &existing_auth.quota);
                auth.quota = existing_auth.quota.clone();
                auth.quota.observed_at = observation.observed_at;
                auth.quota.signals = observation.signals;
                auth.cooldown_recorded_seq = existing_auth.cooldown_recorded_seq;
            } else if auth.model_states.is_empty() && !existing_auth.model_states.is_empty() {
                auth.model_states = existing_auth.model_states.clone();
            }
            if (changed || is_refresh) && !auth.is_active_access_token_rejected() {
                let new_unauthorized = auth
                    .last_error
                    .as_ref()
                    .is_some_and(|err| is_unauthorized_error(ErrView::Auth(err)));
                if has_unauthorized_auth_failure(&existing_auth) || new_unauthorized {
                    auth.unavailable = false;
                    auth.next_retry_after = None;
                    auth.cooldown_recorded_seq = 0;
                    auth.last_error = None;
                    auth.status_message.clear();
                    auth.status = Status::Active;
                }
                clear_unauthorized_model_states(&mut auth, now);
            }
            if is_credential_quota_active(&existing_auth.quota, now) {
                auth.cooldown_recorded_seq = existing_auth.cooldown_recorded_seq;
                auth.unavailable = existing_auth.unavailable;
                auth.next_retry_after = existing_auth.next_retry_after;
                // The cooldown stays; the newest snapshot of the quota,
                // from either, is kept.
                let observation =
                    merge_quota_observation(std::mem::take(&mut auth.quota), &existing_auth.quota);
                auth.quota = existing_auth.quota.clone();
                auth.quota.observed_at = observation.observed_at;
                auth.quota.signals = observation.signals;
                if auth.status == Status::Active {
                    auth.status = existing_auth.status;
                }
            }
        }
        auth.updated_at = Some(now);
        normalize_model_states(&mut auth.model_states);
        if cooldown_disabled_for_auth(&state.settings, &auth) || is_disabled(&auth) {
            let cleared = clear_cooldown_state_for_auth(&mut auth, now);
            if cleared {
                generation = generation.saturating_add(1);
            }
            // As in `clear_disabled_cooldown_states`: whatever recovery keeps
            // for a clean credential is ended all the same.
            if cleared || state.quota_recovery.keeps(&auth.id) {
                state.quota_recovery.reset(&auth.id);
            }
        }
        // An update that stores cooldowns other than the live ones (model
        // states carried in, keys merged by normalizing) has put in state no
        // claim describes, and a read in flight spoke for the old one: as for
        // a registration, fence it and hold what the credential now carries.
        if super::quota_recovery::cooldowns_differ(&existing_auth, &auth) {
            state.quota_recovery.forget(&auth.id);
            let carried = super::quota_recovery::cooldown_scopes(&auth);
            state.quota_recovery.restored(&auth.id, &carried);
        }
        if policy_edit && !ineligible && !same_observation_policy(&existing_auth, &auth) {
            policy_save.get_or_insert_with(PolicySave::default).extend(
                &existing_auth,
                &auth,
                generation,
            );
        }
        auth.policy_generation = if same_observation_policy(&existing_auth, &auth) {
            existing_auth.policy_generation
        } else {
            generation
        };
        auth.ensure_index();
        auth.registration_epoch = live_epoch;
        auth.generation = generation;
        let snapshot = Arc::new(auth);
        let mut entry = Entry {
            auth: snapshot.clone(),
            refresh_failures,
            discard_epoch,
            undurable,
            undurable_baselines,
            unresolved_origin,
            undurable_causes,
            landing_unconfirmed,
            owed_save,
            policy_save,
            written_configuration,
            read_back_warned,
        };
        // An ineligible record is never leased, so its state could never resolve.
        if ineligible {
            entry.clear_unresolved();
        }
        state.auths.insert(snapshot.id.clone(), entry);
        state.sync_scheduler(self.models(), &snapshot.id, now);
        let pending = self.pending_save(state, &snapshot.id, save);
        Ok(Some(PublishedUpdate {
            snapshot,
            epoch: live_epoch,
            generation,
            discard_epoch,
            pending,
            revision: context.revision,
            changed: true,
            ineligible,
            metadata_unchanged: context.metadata_unchanged,
            lease: lease_tail,
        }))
    }

    fn finish_update(
        &self,
        published: PublishedUpdate,
        is_refresh: bool,
        save: Save,
        gate: ReloadGuard,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        let PublishedUpdate {
            mut snapshot,
            epoch,
            mut generation,
            discard_epoch,
            pending,
            changed,
            ineligible,
            metadata_unchanged,
            lease,
            revision,
            ..
        } = published;
        if !changed {
            return Ok(Some((snapshot, generation)));
        }
        let now = self.now();
        self.queue_refresh_reschedule(&snapshot.id);
        cooldown_store::changed(self);
        // An unchanged-metadata replacement saves the whole record and folds
        // retained operations, without changing the live operation's identity.
        let configuration = metadata_unchanged.then(|| {
            let mut writing = (*snapshot).clone();
            writing.codex_persist_intent = Default::default();
            writing
        });
        let (persisted, written) = if ineligible {
            // Keep the pre-admission seam even when publication already fixed
            // this outcome; it still precedes completion reconciliation.
            #[cfg(test)]
            {
                let hook = lock(&self.shared.persist_hook).clone();
                if let Some(hook) = hook {
                    hook(&snapshot);
                }
            }
            (Ok(super::Persisted::Ineligible), None)
        } else {
            match self.persist_bound(
                configuration.as_ref().unwrap_or(&snapshot),
                epoch,
                generation,
                discard_epoch,
                save,
                lease.as_deref(),
                // Only a refresh pass reaches here with `is_refresh`, and
                // without a lease it is saving its own tail under lock 1.
                is_refresh && lease.is_none(),
            ) {
                Ok((persisted, written)) => (Ok(persisted), written),
                Err(error) => (Err(error), None),
            }
        };
        match persisted {
            Err(err) => {
                log_persist_failure(&snapshot, "failed to persist updated auth", &err);
                if !metadata_unchanged
                    && let crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) =
                        &snapshot.codex_persist_intent
                {
                    let mut state = self.lock();
                    if let Some(entry) = state.auths.get_mut(&snapshot.id)
                        && entry.auth.registration_epoch == epoch
                        && entry.discard_epoch == discard_epoch
                        && patch.can_apply_to(&entry.auth.metadata)
                    {
                        let live = Arc::make_mut(&mut entry.auth);
                        if !self.permanently_ineligible(live)
                            && !patch.is_written()
                            && !live
                                .codex_pending_refresh
                                .iter()
                                .any(|pending| pending.same_operation(patch))
                        {
                            live.codex_pending_refresh.push(patch.clone());
                        }
                        if live
                            .codex_persist_intent
                            .same_operation(&snapshot.codex_persist_intent)
                        {
                            live.codex_persist_intent = Default::default();
                        }
                        snapshot = entry.auth.clone();
                    }
                }
            }
            Ok(super::Persisted::Written { requested })
                if (is_refresh || !snapshot.codex_pending_refresh.is_empty())
                    && save == Save::Yes
                    && crate::auth::codex_refresh::is_codex_auth(&snapshot) =>
            {
                // The FileStore baseline check may have kept newer disk tokens
                // and settings. Publish the record actually persisted, but only
                // while this registration is still live, preserving concurrent edits.
                #[cfg(test)]
                {
                    let hook = lock(&self.shared.reconcile_hook).clone();
                    if let Some(hook) = hook {
                        hook(&snapshot);
                    }
                }
                let save_id = snapshot.id.clone();
                self.with_persist_save_lock(&save_id, |save_lock| {
                    // Shared written flags must stay stable from disk read through
                    // coverage checks and publication, not merely during saves.
                    let _saved = lock(save_lock);
                    let binding = self
                        .shared
                        .store
                        .as_ref()
                        .map_or(0, |store| store.binding_generation());
                    let disk = self
                        .shared
                        .store
                        .as_ref()
                        .and_then(|store| store.read_back(&snapshot).ok().flatten());
                    #[cfg(test)]
                    {
                        let hook = lock(&self.shared.read_back_hook).clone();
                        if let Some(hook) = hook {
                            hook(&snapshot);
                        }
                    }
                    let compared = self.get(&snapshot.id);
                    let same_record = compared.as_ref().is_some_and(|live| {
                        self.shared.store.as_ref().is_some_and(|store| {
                            store.supports_codex_refresh_patches()
                                && store.same_record(&snapshot, live).unwrap_or(false)
                        })
                    });
                    let raw_origin = disk.as_ref().is_some_and(|disk| {
                        disk.id == snapshot.id
                            && equal_fold(disk.provider.trim(), snapshot.provider.trim())
                            && self.shared.store.as_ref().is_some_and(|store| {
                                store.same_record(&snapshot, disk).unwrap_or(false)
                            })
                    });
                    // Fence 3 resolves the read-back outside the state lock.
                    let store_ref = self.shared.store.as_deref();
                    let read = lease
                        .as_ref()
                        .and(disk.as_ref())
                        .and_then(|disk| OriginCheck::resolve(store_ref, Some(disk)));
                    let mut state = self.lock();
                    // A leased tail keeps the read only if live and the
                    // read-back both still name the origin or the file the
                    // tail wrote. Without a lease the binding decides. The
                    // recording ignores `discard_epoch`: a discard during the
                    // save fails the gate below, but the token's home is
                    // still not established.
                    let kept = lease.as_deref().map(|tail| {
                        let Some(entry) = state.auths.get_mut(&snapshot.id) else {
                            return false;
                        };
                        if entry.auth.registration_epoch != epoch {
                            return false;
                        }
                        let now = OriginCheck::resolve(store_ref, Some(&entry.auth));
                        let live_same = tail_verdict(now.as_ref(), &tail.origin, written.as_ref())
                            == OriginProof::Same;
                        let read_same = tail_verdict(read.as_ref(), &tail.origin, written.as_ref())
                            == OriginProof::Same;
                        if !live_same && !self.permanently_ineligible(&entry.auth) {
                            entry.record_unresolved(&tail.origin, &tail.rotation);
                        }
                        live_same && read_same
                    });
                    if let Some(entry) = state.auths.get_mut(&snapshot.id)
                        && entry.auth.registration_epoch == epoch
                        && entry.discard_epoch == discard_epoch
                    {
                        let owed = entry.owed_save.is_some();
                        let policy = entry.policy_save.clone();
                        if same_record
                            && raw_origin
                            && kept.unwrap_or_else(|| {
                                self.shared
                                    .store
                                    .as_ref()
                                    .is_some_and(|store| store.binding_generation() == binding)
                            })
                            && compared
                                .as_ref()
                                .is_some_and(|before| same_record_identity(before, &entry.auth))
                            && let Some(disk) = &disk
                        {
                            clear_verified_partitions(
                                &mut entry.undurable,
                                &mut entry.undurable_baselines,
                                &mut entry.undurable_causes,
                                &mut entry.read_back_warned,
                                requested,
                                &entry.auth,
                                disk,
                            );
                        }
                        let undurable = entry.protected();
                        let live = Arc::make_mut(&mut entry.auth);
                        if let Some(disk) = disk.filter(|_| kept.unwrap_or(true)) {
                            let disk = if live.generation == generation {
                                Some(disk)
                            } else if same_record
                                && pending_save_covers_live(&snapshot, live)
                                && compared
                                    .as_ref()
                                    .is_some_and(|compared| same_record_identity(compared, live))
                            {
                                Some(super::merge::reconcile_persisted_codex(
                                    &snapshot, live, &disk,
                                ))
                            } else {
                                None
                            };
                            if let Some(mut disk) = disk {
                                overlay_undurable(&mut disk, live, undurable);
                                let previous = live.clone();
                                let changed = credentials_changed(live, &disk);
                                if owed && is_refresh {
                                    // A refused save left settings owed to the
                                    // file: take only its token keys, so the
                                    // live settings are not read back over.
                                    for key in crate::auth::codex_refresh::OUTER_KEYS
                                        .iter()
                                        .copied()
                                        .chain(["siwc"])
                                    {
                                        match disk.metadata.get(key) {
                                            Some(value) => {
                                                live.metadata.insert(key.to_owned(), value.clone());
                                            }
                                            None => {
                                                live.metadata.remove(key);
                                            }
                                        }
                                    }
                                } else {
                                    live.metadata = disk.metadata;
                                    live.disabled = disk.disabled;
                                    if live.disabled {
                                        live.status = Status::Disabled;
                                    } else if live.status == Status::Disabled {
                                        live.status = Status::Active;
                                    }
                                    live.proxy_url = disk.proxy_url;
                                    live.prefix = disk.prefix;
                                    live.attributes.retain(|key, _| {
                                        !key.starts_with("header:")
                                            || disk.attributes.contains_key(key)
                                    });
                                    live.attributes.extend(disk.attributes);
                                }
                                if let Some(policy) = &policy {
                                    policy.overlay(live);
                                }
                                if !same_observation_policy(&previous, live) {
                                    live.policy_generation = live.generation.saturating_add(1);
                                }
                                live.preserve_rejected_tokens_from(&previous, false);
                                if changed {
                                    live.credential_version =
                                        live.credential_version.saturating_add(1);
                                }
                            }
                        }
                        live.codex_pending_refresh
                            .retain(|patch| !patch.is_written());
                        release_saved_intent(live, &snapshot.codex_persist_intent, is_refresh);
                        live.generation = live.generation.saturating_add(1);
                        generation = live.generation;
                        snapshot = entry.auth.clone();
                        self.drop_unresolved_if_ineligible(entry);
                        state.sync_scheduler(self.models(), &snapshot.id, now);
                    }
                });
            }
            Ok(super::Persisted::Ineligible) => {
                let mut state = self.lock();
                if let Some(entry) = state.auths.get_mut(&snapshot.id)
                    && entry.auth.registration_epoch == epoch
                {
                    // A record with the state and no unwritten work is cleared too.
                    self.drop_unresolved_if_ineligible(entry);
                    if self.permanently_ineligible(&entry.auth)
                        && has_unwritten_refresh_work(&entry.auth)
                    {
                        discard_refresh_work(
                            &mut entry.discard_epoch,
                            &mut entry.undurable,
                            &mut entry.undurable_baselines,
                            &mut entry.undurable_causes,
                            self.shared.store.is_some(),
                            &[&entry.auth],
                        );
                        let live = Arc::make_mut(&mut entry.auth);
                        live.codex_pending_refresh.clear();
                        live.codex_persist_intent = Default::default();
                        snapshot = entry.auth.clone();
                    }
                }
            }
            Ok(super::Persisted::Skipped | super::Persisted::Deferred(_)) => {
                if !metadata_unchanged
                    && matches!(
                        snapshot.codex_persist_intent,
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(_)
                    )
                {
                    let mut state = self.lock();
                    if let Some(entry) = state.auths.get_mut(&snapshot.id)
                        && entry.auth.registration_epoch == epoch
                        && entry.discard_epoch == discard_epoch
                        && entry
                            .auth
                            .codex_persist_intent
                            .same_operation(&snapshot.codex_persist_intent)
                    {
                        let live = Arc::make_mut(&mut entry.auth);
                        // A skipped write is not completion. Keep eligible work
                        // for a later save; already-written operations are filtered.
                        live.codex_pending_refresh = pending_for_edit(
                            live,
                            &live.metadata,
                            self.permanently_ineligible(live),
                        );
                        live.codex_persist_intent = Default::default();
                    }
                }
            }
            Ok(super::Persisted::Written { .. }) => {}
        }
        if revision.is_some() {
            let state = self.lock();
            if state.auths.get(&snapshot.id).is_some_and(|entry| {
                entry.policy_save.is_none()
                    && same_record_identity(&snapshot, &entry.auth)
                    && entry.auth.policy_generation <= snapshot.policy_generation
            }) {
                let mark = lock(&self.shared.persist_locks)
                    .entry(snapshot.id.clone())
                    .or_default()
                    .clone();
                let mut saved = lock(&mark);
                *saved = (*saved).max((epoch, generation));
            }
        }
        drop(pending);
        drop(gate);
        self.flush_owed_save(&snapshot.id, epoch);
        self.schedule_policy_retry(&snapshot.id, epoch);
        Ok(Some((snapshot, generation)))
    }

    /// Removes a credential from the manager, and closes its executor's
    /// sessions (upstream's `Remove`). The store isn't touched: deleting the
    /// saved credential is the caller's job.
    pub fn remove(&self, id: &str) {
        self.remove_matching(id, None);
    }

    /// Removes only the observation's registration and origin. The check and
    /// removal share the state lock, so a replacement keeps its pending work.
    #[doc(hidden)]
    pub fn remove_observed(&self, expected: &Auth) -> bool {
        self.remove_matching(&expected.id, Some(expected))
    }

    fn remove_matching(&self, id: &str, expected: Option<&Auth>) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let gate = self.mutation_gate();
        let provider = {
            let mut guard = self.lock();
            let state = &mut *guard;
            if let Some(expected) = expected
                && !state.auths.get(id).is_some_and(|entry| {
                    entry.auth.registration_epoch == expected.registration_epoch
                        && equal_fold(entry.auth.provider.trim(), expected.provider.trim())
                        && entry.auth.file_name == expected.file_name
                        && entry.auth.attribute("path") == expected.attribute("path")
                })
            {
                return false;
            }
            state.save_sequence.remove(id);
            state.missing_file_deferrals.remove(id);
            let Some(existing) = state.auths.remove(id) else {
                return false;
            };
            state.pool_offsets.remove(id);
            if let Some(affinity) = state.affinity.as_mut() {
                affinity.invalidate_auth(id);
            }
            state.quota_checks.forget(id);
            state.quota_recovery.remove(id);
            state.sync_scheduler(self.models(), id, self.now());
            let slot = state.epochs.entry(id.to_owned()).or_insert(0);
            *slot = (*slot)
                .max(existing.auth.registration_epoch)
                .saturating_add(1);
            existing.auth.provider.trim().to_owned()
        };
        self.prune_idle_save_lock(id);
        drop(gate);
        self.queue_refresh_unschedule(id);
        cooldown_store::changed(self);
        if !provider.is_empty()
            && let Some(executor) = self.executor(&provider)
        {
            executor.close_auth_execution_sessions(id);
        }
        true
    }

    /// Replaces every credential with the store's (upstream's `Load`).
    /// Credentials without an ID or with an invalid weight are skipped.
    /// Without a store this does nothing.
    ///
    /// This discards unwritten pending refreshes and refresh intents. Use only
    /// for initialization or when no refresh work can be pending; waiting for
    /// saves to finish does not imply that those saves succeeded. Production
    /// configuration reloads edit existing credentials instead of calling this.
    ///
    /// It waits for changes being saved to land, and changes made meanwhile
    /// wait for it. The store is listed with the state lock released.
    /// Initialization/quiescent-only: reload is not an observational adoption API.
    pub fn load(&self) -> Result<(), ManagerError> {
        let Some(store) = &self.shared.store else {
            return Ok(());
        };
        let _reload = self.reload_gate();
        let items = store.list().map_err(ManagerError::Store)?;
        let mut guard = self.lock();
        let state = &mut *guard;
        let previous = std::mem::take(&mut state.auths);
        // open-ferry's capped quota rests go with the state replaced.
        for id in previous.keys() {
            state.quota_checks.forget(id);
            state.quota_recovery.remove(id);
        }
        for mut auth in items {
            if auth.id.is_empty() {
                continue;
            }
            normalize_credential_metadata(&mut auth.metadata);
            if validate_weight(&auth).is_err() {
                continue;
            }
            auth.ensure_index();
            let slot = state.epochs.entry(auth.id.clone()).or_insert(0);
            *slot = slot.saturating_add(1);
            auth.registration_epoch = *slot;
            auth.generation = 1;
            auth.policy_generation = 1;
            if let Some(prev) = previous.get(&auth.id) {
                auth.credential_version = auth.credential_version.max(prev.auth.credential_version);
                if credentials_changed(&prev.auth, &auth) {
                    auth.credential_version = auth.credential_version.saturating_add(1);
                }
            }
            if auth.credential_version == 0 {
                auth.credential_version = 1;
            }
            state.auths.insert(
                auth.id.clone(),
                Entry {
                    auth: Arc::new(auth),
                    refresh_failures: 0,
                    discard_epoch: 0,
                    undurable: Default::default(),
                    undurable_baselines: Default::default(),
                    unresolved_origin: None,
                    undurable_causes: Default::default(),
                    landing_unconfirmed: Default::default(),
                    owed_save: None,
                    policy_save: None,
                    written_configuration: 0,
                    read_back_warned: Default::default(),
                },
            );
        }
        for id in previous.keys() {
            if !state.auths.contains_key(id) {
                let slot = state.epochs.entry(id.clone()).or_insert(0);
                *slot = slot.saturating_add(1);
            }
        }
        // Upstream rebuilds its scheduler from the loaded credentials, which
        // starts its rotation over.
        state.selector.reset_scheduler();
        Ok(())
    }

    // runtime_only permanently discards existing refresh persistence work,
    // rather than deferring it. Removing the attribute permits future work;
    // it cannot resurrect discarded rotations or make a Configuration write
    // replace disk credentials. Fresh observations rebase only retained unwritten
    // operations; discarded work keeps rotated live partitions protected from
    // observations. The discarded-chain baseline sets without eviction supplement later
    // rotations, so a verified partition write repairs durability without
    // restoring consumed live tokens. A different disk identity is preserved
    // and leaves its marker/baseline sets intact. A token command can also repair it.
    // A save admitted before a discard may land, but its old epoch
    // cannot clear the marker: the next verified write or valid command must.
    // Opaque stores without read-back and no-store whole-record Replace are
    // compatibility exceptions that may restore stale tokens after cleanup.
    pub(crate) fn permanently_ineligible(&self, auth: &Auth) -> bool {
        self.shared.store.is_none()
            || (auth_kind(auth) == KIND_API_KEY && auth_source_kind(auth) == SOURCE_CONFIG)
            || auth
                .attributes
                .get("runtime_only")
                .is_some_and(|value| go_lower(value.trim()) == "true")
            || auth.metadata.is_empty()
    }

    /// Clears the entry's unresolved state when its record is now permanently
    /// ineligible: an ineligible record is never leased, so the state could
    /// never resolve and would block adoption for good. Marks are not touched.
    /// Returns whether it cleared.
    pub(crate) fn drop_unresolved_if_ineligible(&self, entry: &mut Entry) -> bool {
        if entry.unresolved_origin.is_some() && self.permanently_ineligible(&entry.auth) {
            entry.clear_unresolved();
            return true;
        }
        false
    }

    /// Saves a credential snapshot taken at (`epoch`, `generation`), unless a
    /// newer one was saved already (upstream's `persist`). Credentials from
    /// config API keys, runtime-only ones and ones without metadata aren't
    /// saved. Unsaved publication does not cover pending disk writes. Only
    /// proven observations and successful saves may advance the watermark.
    /// Async callers must offload Replace/adoption with run_persistence or
    /// spawn_blocking, including unsaved replacements and queued reload waits.
    pub(crate) fn persist(
        &self,
        auth: &Auth,
        epoch: u64,
        generation: u64,
        discard_epoch: u64,
        save: Save,
    ) -> Result<super::Persisted, ManagerError> {
        self.persist_bound(auth, epoch, generation, discard_epoch, save, None, false)
            .map(|(persisted, _)| persisted)
    }

    /// [`Self::persist`], saving a leased pass's tail to the lease's origin file
    /// when `lease` is set. The path the save wrote, as an origin, is returned
    /// beside the result.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn persist_bound(
        &self,
        auth: &Auth,
        epoch: u64,
        generation: u64,
        discard_epoch: u64,
        save: Save,
        lease: Option<&LeasedTail>,
        own_tail: bool,
    ) -> Result<(super::Persisted, Option<CodexOrigin>), ManagerError> {
        let mut written_origin = None;
        let persisted = self.persist_inner(
            auth,
            epoch,
            generation,
            discard_epoch,
            save,
            lease,
            own_tail,
            &mut written_origin,
        );
        if persisted.is_err() {
            let mut state = self.lock();
            if let Some(entry) = state.auths.get_mut(&auth.id)
                && entry.auth.registration_epoch == epoch
                && let Some(policy) = entry.policy_save.as_mut()
            {
                policy.retry_at.get_or_insert(self.now());
            }
        }
        let persisted = persisted?;
        Ok((persisted, written_origin))
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_inner(
        &self,
        auth: &Auth,
        epoch: u64,
        generation: u64,
        discard_epoch: u64,
        save: Save,
        lease: Option<&LeasedTail>,
        own_tail: bool,
        written_origin: &mut Option<CodexOrigin>,
    ) -> Result<super::Persisted, ManagerError> {
        #[cfg(test)]
        {
            let hook = lock(&self.shared.persist_hook).clone();
            if let Some(hook) = hook {
                hook(auth);
            }
        }
        let Some(store) = &self.shared.store else {
            return Ok(super::Persisted::Ineligible);
        };
        validate_weight(auth)
            .map_err(|err| ManagerError::InvalidWeight(format!("persist auth: {err}")))?;
        if self.permanently_ineligible(auth) {
            return Ok(super::Persisted::Ineligible);
        }
        // Not upstream's: fail like a save, before the per-id persist lock.
        #[cfg(feature = "blue-green-test")]
        if save == Save::Yes
            && (crate::gate::Operation::current_blocking().is_some()
                || (tokio::runtime::Handle::try_current().is_err()
                    && crate::gate::Operation::current().is_some()))
        {
            crate::test_hooks::barrier_blocking("persist.before").map_err(ManagerError::Store)?;
        }
        #[cfg(feature = "blue-green-test")]
        if save == Save::Failed {
            // A failed save must not advance the disk-originated watermark.
            return Err(ManagerError::Store(std::io::Error::other(
                "injected refresh save failure",
            )));
        }
        let id_lock = lock(&self.shared.persist_locks)
            .entry(auth.id.clone())
            .or_default()
            .clone();
        if save == Save::No {
            return Ok(super::Persisted::Skipped);
        }
        self.with_persist_save_lock(&auth.id, |save_lock| {
            let _saved = lock(save_lock);
            let mut withheld = super::UndurablePartitions::default();
            if save == Save::Yes {
                let state = self.lock();
                let Some(entry) = state.auths.get(&auth.id) else {
                    return Ok(super::Persisted::Skipped);
                };
                if entry.auth.registration_epoch != epoch {
                    return Ok(super::Persisted::Skipped);
                }
                if (entry.discard_epoch != discard_epoch && has_unwritten_refresh_work(auth))
                    || self.permanently_ineligible(&entry.auth)
                {
                    return Ok(super::Persisted::Ineligible);
                }
                withheld = entry.withheld();
                // Admission is final: a later policy change may discard pending
                // work, but this save may finish writing tokens live already holds.
            }
            #[cfg(test)]
            if save == Save::Yes {
                let hook = lock(&self.shared.persist_admitted_hook).clone();
                if let Some(hook) = hook {
                    hook(auth);
                }
            }
            let last = lock(&id_lock);
            let (last_epoch, last_generation) = *last;
            drop(last);
            let mut writing = auth.clone();
            let mut policy_rebased = false;
            // A configuration save queued before a fair observation must carry
            // the newly adopted disk fields too, not write its old token snapshot.
            if !matches!(writing.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
                let compared = self
                    .lock()
                    .auths
                    .get(&auth.id)
                    .filter(|entry| entry.policy_save.is_some())
                    .map(|entry| entry.auth.clone());
                let same_record = compared
                    .as_ref()
                    .is_some_and(|live| store.same_record(auth, live).unwrap_or(false));
                let state = self.lock();
                if let Some(entry) = state.auths.get(&auth.id)
                    && let Some(policy) = &entry.policy_save
                    && same_record
                    && compared
                        .as_ref()
                        .is_some_and(|before| same_record_identity(before, &entry.auth))
                    && entry.auth.registration_epoch == epoch
                {
                    writing = entry.auth.as_ref().clone();
                    policy.overlay(&mut writing);
                    policy_rebased = true;
                    writing.codex_persist_intent = CodexPersistIntent::Configuration;
                }
            }
            let generation = if policy_rebased {
                generation.max(writing.generation)
            } else {
                generation
            };
            if matches!(&writing.codex_persist_intent,
            crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) if patch.is_written())
            {
                writing.codex_persist_intent = Default::default();
            }
            writing
                .codex_pending_refresh
                .retain(|patch| !patch.is_written());
            // An unleased save leaves withheld partitions as the file holds
            // them, so it carries no operation that would rewrite them.
            if lease.is_none() {
                withhold_unresolved(&mut writing, withheld);
            }
            // Identity checks and storage below may perform I/O.
            if epoch < last_epoch {
                return Ok(super::Persisted::Skipped);
            }
            if epoch == last_epoch && generation < last_generation {
                // A newer configuration save preserves disk credentials. It cannot
                // discard a successful rotation merely because its whole snapshot
                // is older: only a scoped, baseline-checked patch may pass here.
                let scoped_refresh = save == Save::Yes
                    && store.supports_codex_refresh_patches()
                    && crate::auth::codex_refresh::is_codex_auth(auth)
                    && matches!(
                        &writing.codex_persist_intent,
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(_)
                    )
                    && (|| {
                        let compared = self.get(&auth.id)?;
                        let same_record = store.same_record(auth, &compared).unwrap_or(false);
                        let state = self.lock();
                        let entry = state.auths.get(&auth.id)?;
                        // Identity I/O is outside the state lock. Unrelated settings
                        // and call-count generations do not change the record's path.
                        Some(
                            entry.auth.registration_epoch == epoch
                                && same_record
                                && same_record_identity(&compared, &entry.auth)
                                && matches!(&writing.codex_persist_intent,
                            crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch)
                                if patch.can_apply_to(&entry.auth.metadata)),
                        )
                    })()
                    .unwrap_or(false);
                if !scoped_refresh {
                    return Ok(super::Persisted::Skipped);
                }
            }
            // The mark moves only once the save has succeeded: a save the gate
            // refused, or one that failed, must not make an older one that could
            // still be written look stale.
            let requested = requested_partitions(&writing);
            let binding = store.binding_generation();
            // Marks the save's refresh operations written as soon as its write
            // lands, while check (vi)'s hold still keeps out a pass of this id:
            // a pass that found one unwritten would commit it again over a
            // newer file. Returns whether any operation was unwritten.
            let land = |path: &str| {
                let unwritten = has_unwritten_refresh_work(&writing);
                if !path.is_empty() {
                    let refresh = match &writing.codex_persist_intent {
                        crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) => {
                            Some(patch)
                        }
                        _ => None,
                    };
                    for patch in writing.codex_pending_refresh.iter().chain(refresh) {
                        patch.mark_written();
                    }
                }
                unwritten
            };
            let (saved, unwritten) = if let Some(tail) = lease {
                let mut refused = |resolved: &CodexOrigin, proof: OriginProof| {
                    self.record_refused_tail(&auth.id, epoch, tail, resolved, proof);
                };
                match store
                    .save_bound(&writing, &tail.origin, &mut refused)
                    .map_err(ManagerError::Store)?
                {
                    Some(path) => {
                        let unwritten = land(&path);
                        (path, unwritten)
                    }
                    None => return Ok(super::Persisted::Skipped),
                }
            } else {
                // Check (vi)'s hold on the refresh mutex lives here, so it lasts
                // through the write and is released when this block ends.
                let mut held: Option<tokio::sync::OwnedMutexGuard<()>> = None;
                let mut guard = |dest: Option<&CodexOrigin>| {
                    self.save_guard(&writing, epoch, withheld, dest, own_tail, &mut held)
                };
                match store
                    .save_guarded(&writing, &mut guard)
                    .map_err(ManagerError::Store)?
                {
                    GuardedSave::Written(path) => {
                        let unwritten = land(&path);
                        (path, unwritten)
                    }
                    GuardedSave::Deferred(reason) => {
                        self.record_owed_save(&auth.id, epoch, generation, reason);
                        return Ok(super::Persisted::Deferred(reason));
                    }
                }
            };
            // The unleased block has ended, so check (vi)'s hold is dropped.
            #[cfg(test)]
            if lease.is_none() {
                let hook = lock(&self.shared.unleased_saved_hook).clone();
                if let Some(hook) = hook {
                    hook(auth);
                }
            }
            if saved.is_empty() {
                return Ok(super::Persisted::Skipped);
            }
            if lease.is_some() {
                *written_origin = CodexOrigin::of(std::path::Path::new(&saved)).ok();
            }
            // A file now exists, so a missing-file episode is over.
            if crate::auth::codex_refresh::is_codex_auth(&writing) {
                self.lock().missing_file_deferrals.remove(&auth.id);
            }
            if unwritten
                && self
                    .lock()
                    .auths
                    .get(&auth.id)
                    .is_some_and(|entry| entry.undurable.outer || entry.undurable.siwc)
            {
                let compared = self.get(&auth.id);
                let disk = store.read_back(&writing).ok().flatten();
                let origin = compared.as_ref().is_some_and(|live| {
                    store.same_record(&writing, live).unwrap_or(false)
                        && disk
                            .as_ref()
                            .is_some_and(|disk| store.same_record(live, disk).unwrap_or(false))
                });
                let mut state = self.lock();
                if let Some(entry) = state.auths.get_mut(&auth.id)
                    && entry.auth.registration_epoch == epoch
                    && entry.discard_epoch == discard_epoch
                    && store.binding_generation() == binding
                    && origin
                    && equal_fold(entry.auth.provider.trim(), writing.provider.trim())
                    && disk.as_ref().is_some_and(|disk| {
                        disk.id == writing.id
                            && equal_fold(disk.provider.trim(), writing.provider.trim())
                    })
                    && compared
                        .as_ref()
                        .is_some_and(|before| same_record_identity(before, &entry.auth))
                    && let Some(disk) = disk
                {
                    clear_verified_partitions(
                        &mut entry.undurable,
                        &mut entry.undurable_baselines,
                        &mut entry.undurable_causes,
                        &mut entry.read_back_warned,
                        requested,
                        &entry.auth,
                        &disk,
                    );
                }
            }
            // Only a Configuration save of the owed generation, or a later one,
            // covers what a refused save left owed to the file.
            if matches!(
                writing.codex_persist_intent,
                crate::auth::codex_refresh::CodexPersistIntent::Configuration
            ) {
                let mut state = self.lock();
                if let Some(entry) = state.auths.get_mut(&auth.id)
                    && entry.auth.registration_epoch == epoch
                {
                    entry.written_configuration = entry.written_configuration.max(generation);
                    if covers_owed(entry.owed_save, generation) {
                        entry.owed_save = None;
                    }
                }
            }
            if !matches!(writing.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
                let mut state = self.lock();
                if let Some(entry) = state.auths.get_mut(&auth.id)
                    && same_record_identity(&writing, &entry.auth)
                    && entry.policy_save.as_ref().is_some_and(|policy| {
                        generation >= policy.generation && policy.covered_by(&writing)
                    })
                {
                    entry.policy_save = None;
                }
            }
            let mut last = lock(&id_lock);
            *last = (*last).max((epoch, generation));
            Ok(super::Persisted::Written { requested })
        })
    }

    /// Runs a potentially persisting manager operation away from runtime workers.
    ///
    /// The entire operation, including credential-lock waits and its pending-save
    /// lifetime, runs on a blocking thread with the current operation's admission.
    /// Without a runtime it runs inline. Synchronous persistence APIs may block
    /// for the credential lock's bounded wait; async callers should use this API
    /// for saves. Async callers must also offload `update`/`update_unsaved`
    /// through this method, `Operation::run_blocking` or `spawn_blocking`: Replace
    /// waits for the adoption save mutex and reads the store. Settings-only
    /// `edit_unsaved` does not wait for an in-flight save, but may wait for a
    /// queued reload, which itself waits for in-flight saves.
    /// An unscoped caller on a draining gate gets `NotActive` before `work` runs;
    /// callers that must finish credential writes must retain their operation.
    pub async fn run_persistence<T, F>(&self, work: F) -> Result<T, ManagerError>
    where
        F: FnOnce(Self) -> T + Send + 'static,
        T: Send + 'static,
    {
        if tokio::runtime::Handle::try_current().is_err() {
            return Ok(work(self.clone()));
        }
        let operation = match crate::gate::Operation::current() {
            Some(operation) => operation,
            None => self
                .gate()
                .begin_operation()
                .map_err(ManagerError::NotActive)?,
        };
        let manager = self.clone();
        operation
            .run_blocking(move || work(manager))
            .await
            .map_err(|_| ManagerError::Store(std::io::Error::other("persistence task failed")))
    }

    // An unscoped execute finishing after drain starts may skip result counters,
    // cooldown/quota and success/failure state, affinity updates, projections and
    // their snapshot save. This does not carry a credential rotation or edit;
    // admitted operations still finish and persist their results during drain.
    pub(crate) async fn mark_call_result_async(
        &self,
        result: &CallResult,
        session: Option<&Session>,
    ) {
        self.record_result_async(result, session, false).await;
    }

    // Like mark_call_result_async, an unscoped draining caller may skip the
    // result count/generation, error event and snapshot save, never a rotation.
    pub(crate) async fn record_availability_neutral_result_async(&self, result: &CallResult) {
        self.record_result_async(result, None, true).await;
    }

    async fn record_result_async(
        &self,
        result: &CallResult,
        session: Option<&Session>,
        neutral: bool,
    ) {
        let record = || {
            if neutral {
                self.record_availability_neutral_result(result);
            } else {
                self.mark_call_result(result, session);
            }
        };
        // Preserve run_persistence's no-runtime behavior, including synchronous
        // callers' existing admission rules.
        if tokio::runtime::Handle::try_current().is_err() {
            record();
            return;
        }
        let operation = crate::gate::Operation::current()
            .map(Ok)
            .unwrap_or_else(|| {
                self.gate()
                    .begin_operation()
                    .map_err(ManagerError::NotActive)
            });
        let outcome = match operation {
            Ok(operation) => {
                operation
                    .scope(async {
                        // Match spawn_blocking's panic boundary. Unwinding
                        // drops/poisons the same guards before this catch;
                        // manager locks recover poison on both paths.
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            self.try_record_result(result, session, neutral)
                        })) {
                            Ok(true) => return Ok(()),
                            Ok(false) => {}
                            Err(_) => {
                                return Err(ManagerError::Store(std::io::Error::other(
                                    "persistence task failed",
                                )));
                            }
                        }
                        let result = result.clone();
                        let session = session.cloned();
                        self.run_persistence(move |manager| {
                            if neutral {
                                manager.record_availability_neutral_result(&result);
                            } else {
                                manager.mark_call_result(&result, session.as_ref());
                            }
                        })
                        .await
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            if neutral {
                tracing::warn!("record neutral call result failed: {error}");
            } else {
                tracing::warn!("record call result failed: {error}");
            }
        }
    }

    /// Only immutable, permanently ineligible snapshots may finish inline.
    /// Check eligibility and mutate under the SAME state lock so a concurrent
    /// credential edit cannot turn this into runtime-worker disk IO. Reuse the
    /// reload permit through the state mutation and (ineligible) persistence
    /// tail, retaining pending-save counts and sequence watermarks without IO.
    /// The tail releases it BEFORE model projections and affinity publication,
    /// just as on the blocking path. Those later short lock acquisitions can
    /// wait behind a reload's state rebuild on a runtime worker. Dispatch
    /// already takes the same state lock synchronously (execute.rs's
    /// execute_unary/execute_streaming selection and resolver snapshots).
    /// Keeping the old release point preserves reload ordering and avoids
    /// extending shared admission across publication callbacks.
    /// A queued/active reload or contended admission falls back before mutation.
    /// The surrounding operation scope retains the usual drain admission on
    /// both paths; no await/cancellation point separates admission and mutation.
    fn try_record_result(
        &self,
        result: &CallResult,
        session: Option<&Session>,
        neutral: bool,
    ) -> bool {
        let Some(gate) = self.shared.load_gate.try_acquire_shared() else {
            return false;
        };
        let now = self.now();
        let model_key = if neutral {
            String::new()
        } else {
            canonical_model_key(&result.model)
        };
        let Some(state) = super::try_lock(&self.shared.state) else {
            return false;
        };
        if state
            .auths
            .get(&result.auth_id)
            .is_some_and(|entry| !self.permanently_ineligible(&entry.auth))
        {
            return false;
        }
        let operation = crate::gate::Operation::current();
        let admission = ResultAdmission {
            gate,
            operation,
            state,
            now,
            inline_ineligible: true,
        };
        if neutral {
            self.record_neutral_result_admitted(result, admission);
        } else if self.record_result_admitted(result, admission, model_key) {
            self.update_result_affinity(result, session);
        }
        true
    }

    /// The guard of an unleased save. Runs inside the store, under the write
    /// permit and the destination's credential lock, before the store's save
    /// mutex. Takes the state lock once, does no I/O, and calls only
    /// `codex_origin`. `admitted` is what the save was admitted to withhold.
    fn save_guard(
        &self,
        writing: &Auth,
        epoch: u64,
        admitted: super::UndurablePartitions,
        dest: Option<&CodexOrigin>,
        own_tail: bool,
        held: &mut Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<WithheldPartitions, SaveDeferral> {
        if !crate::auth::codex_refresh::is_codex_auth(writing) {
            return Ok(WithheldPartitions::default());
        }
        // (vi) No refresh pass of this id is in flight. Not waited for: the
        // guard is kept in `held` until the write has returned, so a pass
        // cannot start meanwhile and the save cannot recreate a file a pass
        // moved away from.
        //
        // A refresh pass's own unleased tail is exempt (`own_tail`): the pass
        // holds lock 1 from its snapshot through that save, so no other pass
        // can be in flight, and this one has finished its send. Only
        // `finish_update` sets the flag, for `is_refresh` without a lease.
        if !own_tail {
            let refresh_lock = lock(&self.shared.refresh_locks)
                .entry(writing.id.clone())
                .or_default()
                .clone();
            match refresh_lock.try_lock_owned() {
                Ok(guard) => *held = Some(guard),
                Err(_) => return Err(SaveDeferral::RefreshInFlight),
            }
        }
        let state = self.lock();
        let Some(entry) = state.auths.get(&writing.id) else {
            return Err(SaveDeferral::RegistrationChanged);
        };
        if entry.auth.registration_epoch != epoch {
            return Err(SaveDeferral::RegistrationChanged);
        }
        if let Some(dest) = dest
            && OriginCheck::resolve(self.shared.store.as_deref(), Some(&entry.auth)).as_ref()
                != Some(dest)
        {
            return Err(SaveDeferral::OriginMoved);
        }
        let now = entry.withheld();
        if (now.outer && !admitted.outer) || (now.siwc && !admitted.siwc) {
            return Err(SaveDeferral::StateChanged);
        }
        let intent = writing.codex_persist_intent.refresh_patch();
        if writing
            .codex_pending_refresh
            .iter()
            .chain(intent)
            .any(|patch| patch.is_written())
        {
            return Err(SaveDeferral::Stale);
        }
        let live = &entry.auth.metadata;
        if !crate::auth::codex_refresh::same_version(&writing.metadata, live)
            || !crate::auth::codex_refresh::same_version(
                &nested_siwc(&writing.metadata),
                &nested_siwc(live),
            )
        {
            return Err(SaveDeferral::Stale);
        }
        Ok(WithheldPartitions {
            outer: admitted.outer,
            siwc: admitted.siwc,
        })
    }

    /// Notes that a save was refused and its change is still owed to the file.
    /// A registration change owes nothing: the new registration saves itself.
    /// Nor does a generation a written Configuration save already covers:
    /// a flush can read the flag, wait for the save lock while that save
    /// writes and clears it, and then be refused behind a pass that has
    /// already looked for the flag, so recording it would leave it owed
    /// with nothing to flush it.
    fn record_owed_save(&self, id: &str, epoch: u64, generation: u64, reason: SaveDeferral) {
        if reason == SaveDeferral::RegistrationChanged {
            return;
        }
        {
            let mut state = self.lock();
            if let Some(entry) = state.auths.get_mut(id)
                && entry.auth.registration_epoch == epoch
                && generation > entry.written_configuration
            {
                let version = entry
                    .owed_save
                    .map_or(generation, |owed| owed.version.max(generation));
                entry.owed_save = Some(super::OwedSave { reason, version });
            }
        }
        tracing::warn!("credential save deferred for {id}: {reason:?}");
    }

    /// Fence 2: a leased tail's save was refused at the store. `resolved` is
    /// the file the record resolved to, with `proof` of it against the origin.
    fn record_refused_tail(
        &self,
        id: &str,
        epoch: u64,
        tail: &LeasedTail,
        resolved: &CodexOrigin,
        proof: OriginProof,
    ) {
        let mut guard = self.lock();
        let state = &mut *guard;
        let Some(entry) = state.auths.get_mut(id) else {
            return;
        };
        if entry.auth.registration_epoch != epoch || self.permanently_ineligible(&entry.auth) {
            return;
        }
        let live = OriginCheck::resolve(self.shared.store.as_deref(), Some(&entry.auth));
        if live.as_ref() == Some(&tail.origin) {
            return;
        }
        if proof == OriginProof::Different && live.as_ref() == Some(resolved) {
            mark_undurable(
                &tail.rotation,
                &mut entry.undurable,
                &mut entry.undurable_baselines,
                &mut entry.undurable_causes,
                &super::MarkCause::Different,
            );
        } else {
            entry.record_unresolved(&tail.origin, &tail.rotation);
        }
    }

    /// Saves again what a refused save left owed, once, after the caller's
    /// permits are released. It never clears the flag: only a covering
    /// Configuration save does.
    pub(super) fn flush_owed_save(&self, id: &str, epoch: u64) {
        let _gate = self.mutation_gate();
        let (snapshot, discard_epoch) = {
            let state = self.lock();
            let Some(entry) = state.auths.get(id) else {
                return;
            };
            let Some(owed) = entry.owed_save else {
                return;
            };
            let withheld = entry.withheld();
            let state_caused = matches!(
                owed.reason,
                SaveDeferral::MissingFile
                    | SaveDeferral::MissingPartition
                    | SaveDeferral::StateChanged
            );
            if entry.auth.registration_epoch != epoch
                || ((withheld.outer || withheld.siwc) && state_caused)
            {
                return;
            }
            (entry.auth.clone(), entry.discard_epoch)
        };
        if let Err(error) = self.persist(
            &snapshot,
            epoch,
            snapshot.generation,
            discard_epoch,
            Save::Yes,
        ) {
            log_persist_failure(&snapshot, "failed to persist owed save", &error);
        }
    }

    fn schedule_policy_retry(&self, id: &str, epoch: u64) {
        let Some(operation) = self.gate().begin_operation().ok() else {
            return;
        };
        let ticket = {
            let mut state = self.lock();
            let Some(entry) = state.auths.get_mut(id) else {
                return;
            };
            if entry.auth.registration_epoch != epoch {
                return;
            }
            let Some(policy) = entry.policy_save.as_mut() else {
                return;
            };
            if policy.retry_scheduled || policy.retry_at.is_none_or(|at| at > self.now()) {
                return;
            }
            policy.retry_scheduled = true;
            policy.retry_ticket = policy.generation;
            policy.retry_ticket
        };
        let manager = self.clone();
        let id = id.to_owned();
        let retry_id = id.clone();
        let retry = move || {
            for delay in [100, 500, 2000] {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let gate = manager.mutation_gate();
                let snapshot = {
                    let state = manager.lock();
                    state
                        .auths
                        .get(&id)
                        .filter(|entry| {
                            entry.auth.registration_epoch == epoch
                                && entry.policy_save.as_ref().is_some_and(|policy| {
                                    policy.retry_scheduled && policy.retry_ticket == ticket
                                })
                        })
                        .map(|entry| (entry.auth.clone(), entry.discard_epoch))
                };
                let Some((snapshot, discard_epoch)) = snapshot else {
                    return;
                };
                // A whole-record/configuration save, using current disk-adopted tokens.
                let mut writing = snapshot.as_ref().clone();
                writing.codex_persist_intent = CodexPersistIntent::Configuration;
                let _ = manager.persist(
                    &writing,
                    epoch,
                    writing.generation,
                    discard_epoch,
                    Save::Yes,
                );
                drop(gate);
                if manager.lock().auths.get(&id).is_none_or(|entry| {
                    entry.auth.registration_epoch != epoch
                        || entry
                            .policy_save
                            .as_ref()
                            .is_none_or(|policy| policy.retry_ticket != ticket)
                }) {
                    return;
                }
            }
            let mut state = manager.lock();
            if let Some(entry) = state.auths.get_mut(&id)
                && entry.auth.registration_epoch == epoch
                && let Some(policy) = entry.policy_save.as_mut()
                && policy.retry_ticket == ticket
            {
                policy.retry_scheduled = false;
                policy.retry_at = Some(manager.now() + chrono::TimeDelta::seconds(30));
            }
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            drop(operation.run_blocking(retry));
        } else {
            // No Tokio runtime exists here. The dedicated worker still carries
            // its admission through scope_blocking; failure leaves the debt owed.
            match std::thread::Builder::new()
                .name("open-ferry-policy-retry".into())
                .spawn(move || operation.scope_blocking(retry))
            {
                Ok(worker) => drop(worker),
                Err(_) => {
                    let mut state = self.lock();
                    if let Some(entry) = state.auths.get_mut(&retry_id)
                        && entry.auth.registration_epoch == epoch
                        && let Some(policy) = entry.policy_save.as_mut()
                        && policy.retry_ticket == ticket
                    {
                        policy.retry_scheduled = false;
                        policy.retry_at = Some(self.now() + chrono::TimeDelta::seconds(30));
                    }
                    drop(state);
                    tracing::warn!("policy-save retry worker could not start");
                }
            }
        }
    }

    pub(super) fn reconcile_pending_save(
        &self,
        snapshot: &Auth,
        discard_epoch: u64,
        requested: super::UndurablePartitions,
    ) {
        let Some(store) = self.shared.store.as_ref() else {
            return;
        };
        #[cfg(test)]
        {
            let hook = lock(&self.shared.reconcile_hook).clone();
            if let Some(hook) = hook {
                hook(snapshot);
            }
        }
        self.with_persist_save_lock(&snapshot.id, |save_lock| {
            // Keep coverage flags stable until the captured disk is reconciled.
            let _saved = lock(save_lock);
            let Some(compared) = self.get(&snapshot.id) else {
                return;
            };
            let binding = store.binding_generation();
            let same_record = store.same_record(snapshot, &compared).unwrap_or(false);
            let disk = store.read_back(snapshot).ok().flatten();
            let raw_origin = disk.as_ref().is_some_and(|disk| {
                disk.id == snapshot.id
                    && equal_fold(disk.provider.trim(), snapshot.provider.trim())
                    && store.same_record(snapshot, disk).unwrap_or(false)
            });
            #[cfg(test)]
            {
                let hook = lock(&self.shared.read_back_hook).clone();
                if let Some(hook) = hook {
                    hook(snapshot);
                }
            }
            let mut state = self.lock();
            let Some(entry) = state.auths.get_mut(&snapshot.id) else {
                return;
            };
            if !same_record
                || entry.auth.registration_epoch != snapshot.registration_epoch
                || entry.discard_epoch != discard_epoch
                || !pending_save_covers_live(snapshot, &entry.auth)
                || !entry.auth.codex_pending_refresh.iter().any(|live| {
                    snapshot
                        .codex_pending_refresh
                        .iter()
                        .any(|saved| live.same_operation(saved))
                })
                || !same_record_identity(&entry.auth, &compared)
            {
                return;
            }
            if raw_origin
                && store.binding_generation() == binding
                && let Some(disk) = &disk
            {
                clear_verified_partitions(
                    &mut entry.undurable,
                    &mut entry.undurable_baselines,
                    &mut entry.undurable_causes,
                    &mut entry.read_back_warned,
                    requested,
                    &entry.auth,
                    disk,
                );
            }
            let undurable = entry.protected();
            let policy = entry.policy_save.clone();
            let live = Arc::make_mut(&mut entry.auth);
            if same_record && let Some(disk) = disk {
                let previous = live.clone();
                let mut persisted = super::merge::reconcile_persisted_codex(snapshot, live, &disk);
                overlay_undurable(&mut persisted, live, undurable);
                if credentials_changed(live, &persisted) {
                    live.credential_version = live.credential_version.saturating_add(1);
                }
                live.metadata = persisted.metadata;
                live.attributes = persisted.attributes;
                live.disabled = persisted.disabled;
                if live.disabled {
                    live.status = Status::Disabled;
                } else if live.status == Status::Disabled {
                    live.status = Status::Active;
                }
                live.prefix = persisted.prefix;
                live.proxy_url = persisted.proxy_url;
                if let Some(policy) = &policy {
                    policy.overlay(live);
                }
                if !same_observation_policy(&previous, live) {
                    live.policy_generation = live.generation.saturating_add(1);
                }
                live.preserve_rejected_tokens_from(&previous, false);
                live.generation = live.generation.saturating_add(1);
            }
            live.codex_pending_refresh
                .retain(|patch| !patch.is_written());
            release_saved_intent(live, &snapshot.codex_persist_intent, false);
            self.drop_unresolved_if_ineligible(entry);
            state.sync_scheduler(self.models(), &snapshot.id, self.now());
        });
    }

    fn persist_detached(
        &self,
        snapshot: Arc<Auth>,
        discard_epoch: u64,
        pending: Option<PendingSave>,
        gate: super::ReloadGuard,
        operation: Option<crate::gate::Operation>,
    ) {
        self.persist_result(snapshot, discard_epoch, pending, gate, operation, false);
    }

    fn persist_result(
        &self,
        snapshot: Arc<Auth>,
        discard_epoch: u64,
        pending: Option<PendingSave>,
        gate: super::ReloadGuard,
        operation: Option<crate::gate::Operation>,
        inline_ineligible: bool,
    ) {
        debug_assert!(!inline_ineligible || self.permanently_ineligible(&snapshot));
        // Preserve detachment/refusal warnings for all synchronous callers.
        // Only a fast-path result, admitted with an immutable ineligible
        // snapshot, may complete its tail inline on a runtime worker.
        let detach = self.shared.store.is_some()
            && crate::auth::codex_refresh::is_codex_auth(&snapshot)
            && !inline_ineligible;
        let manager = self.clone();
        let work = move || {
            let mut deferred = false;
            match manager.persist(
                &snapshot,
                snapshot.registration_epoch,
                snapshot.generation,
                discard_epoch,
                Save::Yes,
            ) {
                Err(error) => {
                    log_persist_failure(&snapshot, "failed to persist credential state", &error)
                }
                Ok(super::Persisted::Written { requested })
                    if !snapshot.codex_pending_refresh.is_empty() =>
                {
                    manager.reconcile_pending_save(&snapshot, discard_epoch, requested);
                }
                Ok(super::Persisted::Deferred(_)) => deferred = true,
                Ok(_) => {}
            }
            drop(pending);
            drop(gate);
            if deferred {
                manager.flush_owed_save(&snapshot.id, snapshot.registration_epoch);
            }
            manager.schedule_policy_retry(&snapshot.id, snapshot.registration_epoch);
        };
        if detach
            && tokio::runtime::Handle::try_current().is_ok()
            && crate::gate::Operation::current_blocking().is_none()
        {
            if let Some(operation) = operation {
                // The task owns both admission and the reload/pending-save permits.
                // Dropping its handle does not cancel persistence.
                drop(operation.run_blocking(work));
            } else {
                tracing::warn!("credential state save refused: operation gate is not open");
            }
        } else {
            work();
        }
    }

    /// Records a call's outcome on its credential: counts it, cools the
    /// model or the credential down after a failure, clears it after a
    /// success, saves the credential, and publishes its models' availability
    /// (upstream's `MarkResult`).
    pub fn mark_result(&self, result: &CallResult) {
        self.mark_call_result(result, None);
    }

    /// [`mark_result`](Self::mark_result) for a call of `session`, whose
    /// bindings the outcome then updates, whether or not the credential is
    /// still there (upstream's `MarkResult` and `updateSessionAffinity`).
    /// An earlier registration is fenced. An older credential version still
    /// applies when its captured active bearer is unchanged; otherwise a 401
    /// can only remember a still-present nested bearer, without changing affinity.
    pub(crate) fn mark_call_result(&self, result: &CallResult, session: Option<&Session>) {
        if !self.record_result(result) {
            return;
        }
        self.update_result_affinity(result, session);
    }

    fn update_result_affinity(&self, result: &CallResult, session: Option<&Session>) {
        if let Some(session) = session
            && !result.auth_id.is_empty()
        {
            let now = self.now();
            if let Some(affinity) = self.lock().affinity.as_mut() {
                affinity.on_result(session, result, "mixed", now);
            }
        }
    }

    /// The credential side of [`mark_result`](Self::mark_result). False
    /// when a stale outcome was excluded from counters and affinity. A
    /// stale-version 401 can still mark a matching nested bearer as rejected.
    fn record_result(&self, result: &CallResult) -> bool {
        if result.auth_id.is_empty() {
            return true;
        }
        let operation =
            crate::gate::Operation::current().or_else(|| self.gate().begin_operation().ok());
        let gate = self.mutation_gate();
        let now = self.now();
        let model_key = canonical_model_key(&result.model);
        self.record_result_admitted(
            result,
            ResultAdmission {
                gate,
                operation,
                state: self.lock(),
                now,
                inline_ineligible: false,
            },
            model_key,
        )
    }

    fn record_result_admitted(
        &self,
        result: &CallResult,
        admission: ResultAdmission<'_>,
        mut model_key: String,
    ) -> bool {
        let ResultAdmission {
            gate,
            operation,
            state: mut guard,
            now,
            inline_ineligible,
        } = admission;
        if result.auth_id.is_empty() {
            return true;
        }
        let schedule_unauthorized_refresh;
        let (snapshot, _epoch, generation, discard_epoch, pending) = {
            let state = &mut *guard;
            let Some(entry) = state.auths.get_mut(&result.auth_id) else {
                return true;
            };
            if is_stale_result(result, &entry.auth) {
                let stale_epoch = result.registration_epoch > 0
                    && result.registration_epoch < entry.auth.registration_epoch;
                if !stale_epoch && super::scoped::rejects_access_token(result) {
                    let auth = Arc::make_mut(&mut entry.auth);
                    if auth.record_rejected_token(&result.access_token) {
                        auth.generation = auth.generation.saturating_add(1);
                        auth.updated_at = Some(now);
                        state.sync_scheduler(self.models(), &result.auth_id, now);
                    }
                }
                return false;
            }
            if model_key.is_empty() && !result.route_model.trim().is_empty() {
                let resolver = Resolver {
                    settings: &state.settings,
                    oauth: &state.oauth,
                };
                model_key = resolver.selection_model_key_for_auth(&entry.auth, &result.route_model);
                if model_key.is_empty() {
                    model_key = canonical_model_key(&result.route_model);
                }
            }
            let auth = Arc::make_mut(&mut entry.auth);
            let was_terminal = has_unauthorized_auth_failure(auth);
            count_result(auth, result, now);
            // open-ferry's cap on long quota rests (see `quota_check`).
            let rests = state.quota_checks.live_rests(&auth.id, &model_key, now);
            // A merge's mark ends only once nothing it could have touched is
            // still ahead, judged before this result changes any of it.
            let rest_live = state.quota_checks.rest_live(&auth.id, now);
            state.quota_recovery.settle_merge_mark(auth, rest_live, now);
            state.quota_checks.before_result(auth);
            let dispatched = self.current_execution_seq();
            let (_, recovery_scope) = super::quota_recovery::scope_of(result, &model_key);
            let scope_before = super::quota_recovery::scope_deadline(auth, &recovery_scope);
            let (stale, own) = apply_result(
                &state.settings,
                auth,
                result,
                &model_key,
                now,
                dispatched,
                rests,
            );
            // The scope's deadline as the outcome set it, before the quota
            // check's cap shortens it to the next check.
            let scope_after = super::quota_recovery::scope_deadline(auth, &recovery_scope);
            let cap = state.settings.quota_check_after;
            schedule_unauthorized_refresh =
                super::refresh::schedule_unauthorized_refresh(auth, result, now, was_terminal);
            state
                .quota_checks
                .after_result(cap, auth, result, &model_key, now, stale);
            // What a later usage read may end (see `quota_recovery`).
            state.quota_recovery.observe(
                auth,
                result,
                &model_key,
                own,
                (scope_before, scope_after),
                stale,
                now,
            );
            auth.updated_at = Some(now);
            auth.generation = auth.generation.saturating_add(1);
            let committed = (
                entry.auth.clone(),
                entry.auth.registration_epoch,
                entry.auth.generation,
                entry.discard_epoch,
            );
            state.sync_scheduler(self.models(), &result.auth_id, now);
            let pending = self.pending_save(state, &result.auth_id, Save::Yes);
            (committed.0, committed.1, committed.2, committed.3, pending)
        };
        drop(guard);
        self.persist_result(
            snapshot.clone(),
            discard_epoch,
            pending,
            gate,
            operation,
            inline_ineligible,
        );
        cooldown_store::changed(self);
        self.publish_projections(&snapshot, generation, now, true);
        self.publish_error_event(result, &snapshot);
        if schedule_unauthorized_refresh {
            self.queue_refresh_reschedule(&result.auth_id);
        }
        true
    }

    /// Records an outcome that says nothing about the credential's health:
    /// it is counted and the generation moves, and the credential is saved
    /// (upstream's `recordAvailabilityNeutralResult`). One from an earlier
    /// credential version or registration is dropped.
    pub(crate) fn record_availability_neutral_result(&self, result: &CallResult) {
        if result.auth_id.is_empty() {
            return;
        }
        let operation =
            crate::gate::Operation::current().or_else(|| self.gate().begin_operation().ok());
        let gate = self.mutation_gate();
        let now = self.now();
        self.record_neutral_result_admitted(
            result,
            ResultAdmission {
                gate,
                operation,
                state: self.lock(),
                now,
                inline_ineligible: false,
            },
        );
    }

    fn record_neutral_result_admitted(&self, result: &CallResult, admission: ResultAdmission<'_>) {
        let ResultAdmission {
            gate,
            operation,
            state: mut guard,
            now,
            inline_ineligible,
        } = admission;
        if result.auth_id.is_empty() {
            return;
        }
        let (snapshot, _epoch, _generation, discard_epoch, pending) = {
            let state = &mut *guard;
            let Some(entry) = state.auths.get_mut(&result.auth_id) else {
                return;
            };
            if is_stale_result(result, &entry.auth) {
                return;
            }
            let auth = Arc::make_mut(&mut entry.auth);
            count_result(auth, result, now);
            auth.updated_at = Some(now);
            auth.generation = auth.generation.saturating_add(1);
            let committed = (
                entry.auth.clone(),
                entry.auth.registration_epoch,
                entry.auth.generation,
                entry.discard_epoch,
            );
            let pending = self.pending_save(state, &result.auth_id, Save::Yes);
            (committed.0, committed.1, committed.2, committed.3, pending)
        };
        drop(guard);
        self.persist_result(
            snapshot.clone(),
            discard_epoch,
            pending,
            gate,
            operation,
            inline_ineligible,
        );
        self.publish_error_event(result, &snapshot);
    }

    /// Clears a credential's quota and cooldowns and puts its models back in
    /// rotation (upstream's `ResetQuota`). Returns the credential and the
    /// models cleared, or `None` when no credential has the ID. A failed
    /// save is returned after the reset took effect.
    pub fn reset_quota(&self, id: &str) -> Result<Option<QuotaReset>, ManagerError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Other("auth id is required".into()));
        }
        let now = self.now();
        let registered: Vec<String> = self
            .models()
            .models_for_client(id)
            .iter()
            .filter(|model| !model.trim().is_empty())
            .map(|model| canonical_model_key(model))
            .collect();
        let gate = self.mutation_gate();
        let (snapshot, models, epoch, generation, discard_epoch, pending) = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let Some(entry) = state.auths.get_mut(id) else {
                return Ok(None);
            };
            let auth = Arc::make_mut(&mut entry.auth);
            let (models, cleared) = reset_quota(auth, &registered, now);
            state.quota_checks.forget(id);
            state.quota_recovery.reset(id);
            let bumps = if cleared { 2 } else { 1 };
            auth.generation = auth.generation.saturating_add(bumps);
            let committed = (
                entry.auth.clone(),
                models,
                entry.auth.registration_epoch,
                entry.auth.generation,
                entry.discard_epoch,
            );
            state.sync_scheduler(self.models(), id, now);
            let pending = self.pending_save(state, id, Save::Yes);
            (
                committed.0,
                committed.1,
                committed.2,
                committed.3,
                committed.4,
                pending,
            )
        };
        let persisted = self.persist(&snapshot, epoch, generation, discard_epoch, Save::Yes);
        drop(pending);
        drop(gate);
        cooldown_store::changed(self);
        self.publish_projections(&snapshot, generation, now, false);
        persisted.map(|_| {
            Some(QuotaReset {
                auth: snapshot,
                models,
            })
        })
    }

    /// Aligns a credential's model states with the models the registry now
    /// lists for it, then publishes their availability (upstream's
    /// `ReconcileRegistryModelStates`). Active cooldowns stay; stale errors
    /// reset; states for models it no longer serves go.
    pub fn reconcile_registry_model_states(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let operation =
            crate::gate::Operation::current().or_else(|| self.gate().begin_operation().ok());
        let gate = self.mutation_gate();
        let now = self.now();
        let models = self.models();
        let mut committed = None;
        let mut pending = None;
        {
            let mut guard = self.lock();
            let state = &mut *guard;
            let Some(entry) = state.auths.get_mut(id) else {
                return;
            };
            let resolver = Resolver {
                settings: &state.settings,
                oauth: &state.oauth,
            };
            for _ in 0..RECONCILE_ATTEMPTS {
                let (supported, reg_epoch) = models.models_and_epoch_for_client(id);
                let (states, changed, merged) =
                    reconcile_model_states(resolver, &entry.auth, &supported, now);
                if models.client_registration_epoch(id) != reg_epoch {
                    continue;
                }
                // A cooldown folded into its target model is one no claim of
                // quota recovery was recorded against.
                // The mark's bound comes from what the credential held before
                // the merge folded its layers together.
                let rest_reset = state.quota_checks.latest_reset(id);
                let bound = super::quota_recovery::merge_bound(&entry.auth, rest_reset, now);
                for (alias, target) in &merged {
                    state.quota_recovery.merged(id, alias, target, bound);
                }
                let auth = Arc::make_mut(&mut entry.auth);
                auth.model_states = states;
                if changed {
                    update_aggregated_availability(auth, now);
                    if !has_model_error(auth, now) {
                        auth.last_error = None;
                        auth.status_message.clear();
                        auth.status = Status::Active;
                    }
                    auth.updated_at = Some(now);
                    auth.generation = auth.generation.saturating_add(1);
                }
                committed = Some((
                    entry.auth.clone(),
                    entry.auth.registration_epoch,
                    entry.auth.generation,
                    entry.discard_epoch,
                    changed,
                    supported,
                    reg_epoch,
                ));
                break;
            }
            if let Some((.., changed, _, _)) = &committed {
                state.sync_scheduler(models, id, now);
                if *changed {
                    pending = self.pending_save(state, id, Save::Yes);
                }
            }
        }
        let Some((snapshot, _epoch, generation, discard_epoch, changed, supported, reg_epoch)) =
            committed
        else {
            return;
        };
        if changed {
            self.persist_detached(snapshot.clone(), discard_epoch, pending, gate, operation);
        } else {
            drop(pending);
            drop(gate);
        }
        cooldown_store::changed(self);
        let (settings, oauth) = self.resolver_parts();
        let resolver = Resolver {
            settings: &settings,
            oauth: &oauth,
        };
        let projections = projections_for(resolver, &snapshot, &supported, now);
        models.apply_client_model_projections(id, reg_epoch, generation, &projections);
    }

    /// Publishes the availability of each of the credential's registered
    /// models, as of `generation`. With `only_if_any`, nothing is published
    /// when the credential has no registered models.
    pub(crate) fn publish_projections(
        &self,
        auth: &Auth,
        generation: u64,
        now: Timestamp,
        only_if_any: bool,
    ) {
        let (supported, reg_epoch) = self.models().models_and_epoch_for_client(&auth.id);
        let (settings, oauth) = self.resolver_parts();
        let resolver = Resolver {
            settings: &settings,
            oauth: &oauth,
        };
        let projections = projections_for(resolver, auth, &supported, now);
        if only_if_any && projections.is_empty() {
            return;
        }
        self.models()
            .apply_client_model_projections(&auth.id, reg_epoch, generation, &projections);
    }

    /// The executor that serves `auth`, under the state lock.
    pub(crate) fn executor_for(state: &State, auth: &Auth) -> Option<Arc<dyn ProviderExecutor>> {
        lookup_executor(&state.executors, &executor_key_from_auth(auth))
    }
}

/// Drop bookkeeping only after recording every unwritten operation's partitions.
/// Both pending and intent are inspected; identity deduplication avoids treating
/// the same operation as independent work. No-store registrations have no markers.
fn discard_refresh_work(
    epoch: &mut u64,
    undurable: &mut super::UndurablePartitions,
    baselines: &mut super::UndurableBaselines,
    causes: &mut super::MarkCauses,
    has_store: bool,
    sources: &[&Auth],
) {
    use crate::auth::codex_refresh::CodexPersistIntent;
    let mut operations = Vec::new();
    for auth in sources {
        let intent = match &auth.codex_persist_intent {
            CodexPersistIntent::Refresh(patch) => Some(patch),
            _ => None,
        };
        for patch in auth.codex_pending_refresh.iter().chain(intent) {
            if !patch.is_written() {
                if !operations.iter().any(
                    |older: &&crate::auth::codex_refresh::CredentialRefreshPatch| {
                        older.same_operation(patch)
                    },
                ) {
                    operations.push(patch);
                }
                // Shared operation identity dedupes the epoch bookkeeping, not
                // richer chain baselines attached to another clone.
                if has_store {
                    mark_undurable(
                        patch,
                        undurable,
                        baselines,
                        causes,
                        &super::MarkCause::Different,
                    );
                }
            }
        }
    }
    if !operations.is_empty() {
        *epoch = epoch.saturating_add(1);
    }
}

/// Marks each partition `patch` includes, and records its durable baselines.
/// A partition's `ReadBack` cause is kept, never relabelled.
pub(crate) fn mark_undurable(
    patch: &crate::auth::codex_refresh::CredentialRefreshPatch,
    undurable: &mut super::UndurablePartitions,
    baselines: &mut super::UndurableBaselines,
    causes: &mut super::MarkCauses,
    cause: &super::MarkCause,
) {
    mark_undurable_except(
        patch,
        undurable,
        baselines,
        causes,
        cause,
        super::UndurablePartitions::default(),
    );
}

/// [`mark_undurable`], skipping each partition `except` names.
pub(crate) fn mark_undurable_except(
    patch: &crate::auth::codex_refresh::CredentialRefreshPatch,
    undurable: &mut super::UndurablePartitions,
    baselines: &mut super::UndurableBaselines,
    causes: &mut super::MarkCauses,
    cause: &super::MarkCause,
    except: super::UndurablePartitions,
) {
    use crate::auth::CodexCredentialPartition;
    for (partition, skipped, marked, recorded, slot) in [
        (
            CodexCredentialPartition::Outer,
            except.outer,
            &mut undurable.outer,
            &mut baselines.outer,
            &mut causes.outer,
        ),
        (
            CodexCredentialPartition::Siwc,
            except.siwc,
            &mut undurable.siwc,
            &mut baselines.siwc,
            &mut causes.siwc,
        ),
    ] {
        if !skipped && patch.includes_partition(partition) {
            for baseline in patch.durable_baselines(partition) {
                crate::auth::codex_refresh::record_durable_baseline(recorded, &baseline);
            }
            *marked = true;
            if !matches!(slot, Some(super::MarkCause::ReadBack { .. })) {
                *slot = Some(cause.clone());
            }
        }
    }
}

pub(crate) fn overlay_undurable(
    target: &mut Auth,
    live: &Auth,
    marked: super::UndurablePartitions,
) {
    if marked.outer {
        for key in crate::auth::codex_refresh::OUTER_KEYS {
            match live.metadata.get(*key) {
                Some(value) => {
                    target.metadata.insert((*key).to_owned(), value.clone());
                }
                None => {
                    target.metadata.remove(*key);
                }
            }
        }
    }
    if marked.siwc {
        if let Some(source) = live
            .metadata
            .get("siwc")
            .and_then(serde_json::Value::as_object)
        {
            let nested = target
                .metadata
                .entry("siwc")
                .or_insert_with(|| serde_json::json!({}));
            if !nested.is_object() {
                *nested = serde_json::json!({});
            }
            let nested = nested.as_object_mut().expect("SIWC object");
            for key in crate::auth::codex_refresh::SIWC_KEYS {
                match source.get(*key) {
                    Some(value) => {
                        nested.insert((*key).to_owned(), value.clone());
                    }
                    None => {
                        nested.remove(*key);
                    }
                }
            }
        } else if let Some(nested) = target
            .metadata
            .get_mut("siwc")
            .and_then(serde_json::Value::as_object_mut)
        {
            for key in crate::auth::codex_refresh::SIWC_KEYS {
                nested.remove(*key);
            }
        }
    }
}

/// The nested `siwc` object of a metadata map (empty without one).
fn nested_siwc(
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    metadata
        .get("siwc")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Strips from `writing` every unwritten operation that touches a withheld
/// partition: the unleased save must not rewrite what the file holds. A
/// touching Refresh intent becomes a Configuration save. Whether anything was
/// dropped.
pub(crate) fn withhold_unresolved(
    writing: &mut Auth,
    withheld: super::UndurablePartitions,
) -> bool {
    use crate::auth::CodexCredentialPartition::{Outer, Siwc};
    if !withheld.outer && !withheld.siwc {
        return false;
    }
    let touches = |patch: &CredentialRefreshPatch| {
        !patch.is_written()
            && ((withheld.outer && patch.includes_partition(Outer))
                || (withheld.siwc && patch.includes_partition(Siwc)))
    };
    let before = writing.codex_pending_refresh.len();
    writing
        .codex_pending_refresh
        .retain(|patch| !touches(patch));
    let mut dropped = writing.codex_pending_refresh.len() != before;
    if matches!(&writing.codex_persist_intent, CodexPersistIntent::Refresh(patch) if touches(patch))
    {
        writing.codex_persist_intent = Default::default();
        dropped = true;
    }
    dropped
}

/// Releases the intent a save carried from the live record. A Refresh intent
/// the save did not write stays owed as a pending operation; the failure it
/// answers is cleared only by a written patch in refresh mode.
pub(crate) fn release_saved_intent(live: &mut Auth, saved: &CodexPersistIntent, is_refresh: bool) {
    if !live.codex_persist_intent.same_operation(saved) {
        return;
    }
    if let CodexPersistIntent::Refresh(patch) = &live.codex_persist_intent
        && !patch.is_written()
        && !live
            .codex_pending_refresh
            .iter()
            .any(|pending| pending.same_operation(patch))
    {
        live.codex_pending_refresh.push(patch.clone());
    }
    live.codex_persist_intent = Default::default();
    if is_refresh
        && let CodexPersistIntent::Refresh(patch) = saved
        && patch.is_written()
        && live
            .codex_refresh_failure
            .as_ref()
            .is_none_or(|failure| patch.includes_partition(failure.partition))
    {
        live.codex_refresh_failure = None;
    }
}

fn requested_partitions(requested: &Auth) -> super::UndurablePartitions {
    use crate::auth::{CodexCredentialPartition, codex_refresh::CodexPersistIntent};
    let intent = match &requested.codex_persist_intent {
        CodexPersistIntent::Refresh(patch) => Some(patch),
        _ => None,
    };
    let includes = |partition| {
        requested
            .codex_pending_refresh
            .iter()
            .chain(intent)
            .any(|patch| !patch.is_written() && patch.includes_partition(partition))
    };
    super::UndurablePartitions {
        outer: includes(CodexCredentialPartition::Outer),
        siwc: includes(CodexCredentialPartition::Siwc),
    }
}

/// The partitions of `requested` whose live token fields equal the disk's.
pub(crate) fn verified_partitions(
    requested: super::UndurablePartitions,
    live: &serde_json::Map<String, serde_json::Value>,
    disk: &serde_json::Map<String, serde_json::Value>,
) -> super::UndurablePartitions {
    let live_siwc = live.get("siwc").and_then(serde_json::Value::as_object);
    let disk_siwc = disk.get("siwc").and_then(serde_json::Value::as_object);
    super::UndurablePartitions {
        outer: requested.outer
            && crate::auth::codex_refresh::OUTER_KEYS
                .iter()
                .all(|key| live.get(*key) == disk.get(*key)),
        siwc: requested.siwc
            && crate::auth::codex_refresh::SIWC_KEYS.iter().all(|key| {
                live_siwc.and_then(|m| m.get(*key)) == disk_siwc.and_then(|m| m.get(*key))
            }),
    }
}

/// Clears the mark, baselines and cause of each requested partition the disk
/// verifies.
pub(crate) fn clear_verified_partitions(
    marked: &mut super::UndurablePartitions,
    baselines: &mut super::UndurableBaselines,
    causes: &mut super::MarkCauses,
    read_back_warned: &mut super::UndurablePartitions,
    requested: super::UndurablePartitions,
    live: &Auth,
    disk: &Auth,
) {
    let verified = verified_partitions(requested, &live.metadata, &disk.metadata);
    if verified.outer {
        read_back_warned.outer = false;
        marked.outer = false;
        baselines.outer.clear();
        causes.outer = None;
    }
    if verified.siwc {
        read_back_warned.siwc = false;
        marked.siwc = false;
        baselines.siwc.clear();
        causes.siwc = None;
    }
}

/// Incoming settings remain last-writer-wins; only the token partitions are read.
fn copy_observed_credentials(target: &mut Auth, record: &Auth) {
    for key in crate::auth::codex_refresh::OUTER_KEYS {
        match record.metadata.get(*key) {
            Some(value) => {
                target.metadata.insert((*key).to_owned(), value.clone());
            }
            None => {
                target.metadata.remove(*key);
            }
        }
    }
    if let Some(fresh) = record
        .metadata
        .get("siwc")
        .and_then(serde_json::Value::as_object)
    {
        let nested = target
            .metadata
            .entry("siwc")
            .or_insert_with(|| serde_json::json!({}));
        if !nested.is_object() {
            *nested = serde_json::json!({});
        }
        let nested = nested.as_object_mut().expect("SIWC object");
        for key in crate::auth::codex_refresh::SIWC_KEYS {
            match fresh.get(*key) {
                Some(value) => {
                    nested.insert((*key).to_owned(), value.clone());
                }
                None => {
                    nested.remove(*key);
                }
            }
        }
    } else {
        target.metadata.remove("siwc");
    }
}

/// Fields a credential or operator edit owns; runtime health is excluded.
pub(super) fn same_observation_policy(left: &Auth, right: &Auth) -> bool {
    left.disabled == right.disabled
        && (left.status == Status::Disabled) == (right.status == Status::Disabled)
        && left.metadata == right.metadata
        && left.attributes == right.attributes
        && left.prefix == right.prefix
        && left.label == right.label
        && left.proxy_url == right.proxy_url
}

/// Compares registration and lexical record identity without filesystem I/O.
fn same_record_identity(a: &Auth, b: &Auth) -> bool {
    a.registration_epoch == b.registration_epoch
        && a.id == b.id
        && a.file_name.trim() == b.file_name.trim()
        && a.trimmed_attribute(crate::auth::classification::ATTRIBUTE_PATH)
            == b.trimmed_attribute(crate::auth::classification::ATTRIBUTE_PATH)
}

/// Logs a credential save that failed. A save the gate refused is expected
/// while a switch is under way, and every request's outcome would log one, so
/// it is logged at debug.
fn log_persist_failure(auth: &Auth, what: &str, err: &ManagerError) {
    let refused = matches!(err, ManagerError::NotActive(_))
        || matches!(err, ManagerError::Store(io)
            if io.get_ref().is_some_and(|inner| inner.is::<crate::gate::NotActive>()));
    if refused {
        tracing::debug!(auth_id = %auth.id, "{what} refused: the gate is not open");
    } else {
        let mut secrets = crate::observe::redact::Secrets::new();
        secrets.add_auth(auth);
        let error = secrets.text(err.to_string(), crate::observe::redact::Policy::Disk);
        tracing::warn!(
            auth_id = %auth.id,
            provider = %auth.provider,
            "{what}: {error}"
        );
    }
}

// An older save cannot reconcile over a newer pending result merely because
// its token identity is unchanged (expiry-only rotations are independent work).
fn pending_save_covers_live(snapshot: &Auth, live: &Auth) -> bool {
    let covered = |operation: &crate::auth::codex_refresh::CredentialRefreshPatch| {
        operation.is_written()
            || matches!(&snapshot.codex_persist_intent,
                crate::auth::codex_refresh::CodexPersistIntent::Refresh(saved)
                    if operation.same_operation(saved))
            || snapshot
                .codex_pending_refresh
                .iter()
                .any(|saved| operation.same_operation(saved))
    };
    live.codex_pending_refresh.iter().all(&covered)
        && match &live.codex_persist_intent {
            crate::auth::codex_refresh::CodexPersistIntent::Refresh(operation) => {
                covered(operation)
            }
            _ => true,
        }
}

fn has_unwritten_refresh_work(auth: &Auth) -> bool {
    auth.codex_pending_refresh
        .iter()
        .any(|patch| !patch.is_written())
        || matches!(&auth.codex_persist_intent,
            crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) if !patch.is_written())
}

// Whether a written Configuration save of `generation` covers the owed change:
// only a save of the owed generation, or a later one, does.
fn covers_owed(owed: Option<super::OwedSave>, generation: u64) -> bool {
    owed.is_some_and(|owed| generation >= owed.version)
}

// Pending memory is bounded by refreshes between successful saves. No cap may
// silently discard an unwritten rotation; written flags are pruned on edits/save.
pub(crate) fn pending_for_edit(
    auth: &Auth,
    metadata: &serde_json::Map<String, serde_json::Value>,
    ineligible: bool,
) -> Vec<crate::auth::codex_refresh::CredentialRefreshPatch> {
    if ineligible {
        return Vec::new();
    }
    let mut pending = auth.codex_pending_refresh.clone();
    if let crate::auth::codex_refresh::CodexPersistIntent::Refresh(patch) =
        &auth.codex_persist_intent
        && !patch.is_written()
        && !pending.iter().any(|older| older.same_operation(patch))
    {
        pending.push(patch.clone());
    }
    pending
        .iter()
        .enumerate()
        .filter_map(|(index, patch)| {
            if patch.is_written() {
                None
            } else {
                patch.pruned_after_following(&pending[index + 1..], metadata)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_version_4() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
        assert_ne!(new_uuid(), id);
    }

    /// A store whose records all resolve to one settable path; no file exists.
    struct PathStore(Mutex<Option<std::path::PathBuf>>);

    impl PathStore {
        fn at(path: Option<&str>) -> Self {
            Self(Mutex::new(path.map(Into::into)))
        }
    }

    impl AuthStore for PathStore {
        fn list(&self) -> std::io::Result<Vec<Auth>> {
            Ok(Vec::new())
        }

        fn save(&self, _auth: &Auth) -> std::io::Result<String> {
            Ok(String::new())
        }

        fn delete(&self, _id: &str) -> std::io::Result<()> {
            Ok(())
        }

        fn codex_origin(&self, _auth: &Auth) -> std::io::Result<Option<CodexOrigin>> {
            match &*lock(&self.0) {
                Some(path) => CodexOrigin::of(path).map(Some),
                None => Ok(None),
            }
        }
    }

    fn origin(path: &str) -> CodexOrigin {
        CodexOrigin::of(std::path::Path::new(path)).unwrap()
    }

    fn verdict(
        live: Option<&str>,
        check: &OriginCheck,
        pinned: &CodexOrigin,
    ) -> Result<OriginProof, OriginMoved> {
        let store = PathStore::at(live);
        check.verdict(Some(&store), pinned, &Auth::default())
    }

    #[test]
    fn origin_resolution_is_none_without_a_store_a_record_or_an_identity() {
        let store = PathStore::at(Some("p/a.json"));
        let record = Auth::default();
        assert_eq!(
            OriginCheck::resolve(Some(&store), Some(&record)),
            Some(origin("p/a.json"))
        );
        assert_eq!(OriginCheck::resolve(None, Some(&record)), None);
        assert_eq!(OriginCheck::resolve(Some(&store), None), None);
        let opaque = PathStore::at(None);
        assert_eq!(OriginCheck::resolve(Some(&opaque), Some(&record)), None);
    }

    #[test]
    fn a_verdict_is_same_when_live_names_the_pinned_origin() {
        let pinned = origin("p/a.json");
        for last in [false, true] {
            let check = OriginCheck::proved(Some(origin("q/b.json")), OriginProof::Different, last);
            assert!(matches!(
                verdict(Some("p/a.json"), &check, &pinned),
                Ok(OriginProof::Same)
            ));
        }
    }

    #[test]
    fn a_verdict_is_unproven_when_live_does_not_resolve() {
        let pinned = origin("p/a.json");
        let check = OriginCheck::proved(Some(origin("q/b.json")), OriginProof::Different, false);
        assert!(matches!(
            verdict(None, &check, &pinned),
            Ok(OriginProof::Unproven)
        ));
    }

    #[test]
    fn a_verdict_carries_the_proof_made_for_the_current_origin() {
        let pinned = origin("p/a.json");
        for proof in [
            OriginProof::Same,
            OriginProof::Different,
            OriginProof::Unproven,
        ] {
            let check = OriginCheck::proved(Some(origin("q/b.json")), proof, false);
            assert!(matches!(verdict(Some("q/b.json"), &check, &pinned), Ok(got) if got == proof));
        }
    }

    #[test]
    fn a_verdict_for_an_unproved_path_is_moved_until_the_last_attempt() {
        let pinned = origin("p/a.json");
        for current in [Some(origin("q/b.json")), None] {
            let check = OriginCheck::proved(current.clone(), OriginProof::Different, false);
            assert!(matches!(
                verdict(Some("r/c.json"), &check, &pinned),
                Err(OriginMoved)
            ));
            let last = OriginCheck::proved(current, OriginProof::Different, true);
            assert!(matches!(
                verdict(Some("r/c.json"), &last, &pinned),
                Ok(OriginProof::Unproven)
            ));
        }
    }

    #[test]
    fn the_tail_verdict_is_same_only_for_the_origin_or_the_written_path() {
        let pinned = origin("p/a.json");
        let written = origin("q/b.json");
        let other = origin("r/c.json");
        assert_eq!(
            tail_verdict(Some(&pinned), &pinned, None),
            OriginProof::Same
        );
        assert_eq!(
            tail_verdict(Some(&written), &pinned, Some(&written)),
            OriginProof::Same
        );
        assert_eq!(
            tail_verdict(Some(&written), &pinned, None),
            OriginProof::Unproven
        );
        assert_eq!(
            tail_verdict(Some(&other), &pinned, Some(&written)),
            OriginProof::Unproven
        );
        assert_eq!(
            tail_verdict(None, &pinned, Some(&written)),
            OriginProof::Unproven
        );
    }

    fn rotation(before: &str, after: &str) -> CredentialRefreshPatch {
        let meta = |token: &str| {
            serde_json::json!({"access_token": token})
                .as_object()
                .cloned()
                .unwrap()
        };
        CredentialRefreshPatch::new(&meta(before), &meta(after), true, false)
    }

    fn outer_failure() -> crate::auth::CodexRefreshFailure {
        crate::auth::CodexRefreshFailure {
            partition: crate::auth::CodexCredentialPartition::Outer,
            error: crate::exec::ExecError::upstream(500, "refresh failed"),
        }
    }

    fn with_intent(intent: CodexPersistIntent) -> Auth {
        Auth {
            codex_persist_intent: intent,
            ..Auth::default()
        }
    }

    #[test]
    fn release_ignores_an_intent_the_live_record_does_not_hold() {
        let saved = CodexPersistIntent::Refresh(rotation("a", "b"));
        let other = CodexPersistIntent::Refresh(rotation("a", "b"));
        let mut live = with_intent(other.clone());
        release_saved_intent(&mut live, &saved, true);
        assert!(live.codex_persist_intent == other);
        assert!(live.codex_pending_refresh.is_empty());
    }

    #[test]
    fn release_resets_a_saved_configuration_intent() {
        let saved = CodexPersistIntent::OuterLogin;
        let mut live = with_intent(CodexPersistIntent::OuterLogin);
        release_saved_intent(&mut live, &saved, true);
        assert!(live.codex_persist_intent == CodexPersistIntent::Configuration);
        assert!(live.codex_pending_refresh.is_empty());
    }

    #[test]
    fn release_keeps_an_unwritten_refresh_as_pending_and_its_failure() {
        let patch = rotation("a", "b");
        let saved = CodexPersistIntent::Refresh(patch.clone());
        let mut live = with_intent(saved.clone());
        live.codex_refresh_failure = Some(outer_failure());
        release_saved_intent(&mut live, &saved, true);
        assert!(live.codex_persist_intent == CodexPersistIntent::Configuration);
        assert_eq!(live.codex_pending_refresh.len(), 1);
        assert!(live.codex_pending_refresh[0].same_operation(&patch));
        assert!(live.codex_refresh_failure.is_some());
        // Releasing it again does not queue it twice.
        live.codex_persist_intent = saved.clone();
        release_saved_intent(&mut live, &saved, true);
        assert_eq!(live.codex_pending_refresh.len(), 1);
    }

    #[test]
    fn release_clears_the_failure_only_for_a_written_patch_in_refresh_mode() {
        let patch = rotation("a", "b");
        patch.mark_written();
        let saved = CodexPersistIntent::Refresh(patch);
        let failure = outer_failure();
        let mut live = with_intent(saved.clone());
        live.codex_refresh_failure = Some(failure.clone());
        release_saved_intent(&mut live, &saved, false);
        assert!(live.codex_refresh_failure.is_some());
        assert!(live.codex_pending_refresh.is_empty());
        let mut live = with_intent(saved.clone());
        live.codex_refresh_failure = Some(failure);
        release_saved_intent(&mut live, &saved, true);
        assert!(live.codex_refresh_failure.is_none());
        assert!(live.codex_pending_refresh.is_empty());
    }

    #[test]
    fn only_a_save_of_the_owed_generation_or_later_covers_it() {
        // B-37 (d): a Configuration save older than the refused one leaves it owed.
        let owed = Some(super::super::OwedSave {
            reason: SaveDeferral::RefreshInFlight,
            version: 5,
        });
        assert!(!covers_owed(owed, 4));
        assert!(covers_owed(owed, 5));
        assert!(covers_owed(owed, 6));
        assert!(!covers_owed(None, 5));
    }
}
