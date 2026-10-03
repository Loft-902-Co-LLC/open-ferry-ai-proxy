// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_lifecycle.go
// (RegisterExecutor, UnregisterExecutor, Register, Update,
// UpdateRefreshedAuth, Remove, Load and persist), MarkResult,
// recordAvailabilityNeutralResult, ResetQuota and clearDisabledCooldownStates
// in sdk/cliproxy/auth/conductor_cooldown.go, and
// ReconcileRegistryModelStates in sdk/cliproxy/auth/conductor_selection.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Adding, changing and removing credentials and executors, recording call
//! outcomes, and saving credentials to the store.
//!
//! Every change makes a new [`Auth`] snapshot and bumps the credential's
//! generation; registering an ID again bumps its epoch. A save is skipped
//! when a newer (epoch, generation) of the same credential was already
//! saved, so saves never go backwards. [`Manager::register_unsaved`] and
//! [`Manager::update_unsaved`] change a credential without saving it, for
//! one just read from its file or the config (upstream's `WithSkipPersist`).
//!
//! Deviations from upstream:
//! - The registration epoch and generation live on the manager's entry;
//!   an update can't carry its own, so it always takes the live ones.
//! - A replaced credential starts with no `invalid_grant` failures counted;
//!   upstream keeps whatever count the caller's copy held.
//! - Saves happen after the state lock is released.
//! - `load` doesn't reschedule refreshes, as upstream; start the refresh
//!   loop after loading. It restarts the rotation, as upstream's scheduler
//!   rebuild does.
//! - A new ID is a random version 4 UUID made with `rand`.
//! - Not ported: the auth index, hooks, the scheduler index, API-key model
//!   alias rebuilds, the cooldown state store, plugin virtual credentials,
//!   the Meta key mint save inside the lock, result policies, quota
//!   observation from response headers and the success and failure
//!   counters.

use std::sync::Arc;

use super::classify::{ErrView, has_unauthorized_auth_failure, is_unauthorized_error};
use super::cooldown::{
    CallResult, apply_result, clear_cooldown_state_for_auth, cooldown_disabled_for_auth,
    has_model_error, is_credential_quota_active, normalize_model_states, projections_for,
    reconcile_model_states, reset_quota, update_aggregated_availability,
};
use super::credential::{
    KIND_API_KEY, SOURCE_CONFIG, auth_kind, auth_source_kind, credentials_changed, validate_weight,
};
use super::merge::{merge_refreshed_auth, normalize_credential_metadata};
use super::models::Resolver;
use super::refresh::clear_unauthorized_model_states;
use super::select::{executor_key_from_auth, lookup_executor};
use super::text::{canonical_model_key, equal_fold, go_lower};
use super::{Entry, Manager, ManagerError, State, lock};
use crate::auth::{Auth, Status, Timestamp};
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

/// Clears the cooldowns of every credential that no longer cools down:
/// cooling is off for it under the current settings, or it is disabled
/// (upstream's `clearDisabledCooldownStates`). Returns the IDs of the
/// credentials it changed.
pub(crate) fn clear_disabled_cooldown_states(state: &mut State, now: Timestamp) -> Vec<String> {
    let settings = state.settings.clone();
    let mut cleared = Vec::new();
    for (id, entry) in &mut state.auths {
        if !cooldown_disabled_for_auth(&settings, &entry.auth) && !is_disabled(&entry.auth) {
            continue;
        }
        if clear_cooldown_state_for_auth(Arc::make_mut(&mut entry.auth), now) {
            entry.generation += 1;
            cleared.push(id.clone());
        }
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
}

/// Whether an update is a plain replacement or a refresh result to merge.
enum UpdateMode<'a> {
    Replace,
    /// The credential the refresh started from, and its epoch then.
    Refresh {
        base: &'a Auth,
        base_epoch: u64,
    },
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
    /// A credential without an ID gets a random one. The credential is saved
    /// to the store; a failed save is logged, not returned.
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
        let snapshot = Arc::new(auth);
        let epoch = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let existing_epoch = state.auths.get(&snapshot.id).map_or(0, |entry| entry.epoch);
            let slot = state.epochs.entry(snapshot.id.clone()).or_insert(0);
            *slot = (*slot).max(existing_epoch).saturating_add(1);
            let epoch = *slot;
            state.auths.insert(
                snapshot.id.clone(),
                Entry {
                    auth: snapshot.clone(),
                    epoch,
                    generation: 1,
                    refresh_failures: 0,
                },
            );
            state.sync_scheduler(self.models(), &snapshot.id, now);
            epoch
        };
        self.queue_refresh_reschedule(&snapshot.id);
        if let Err(err) = self.persist(&snapshot, epoch, 1, save) {
            tracing::warn!(
                auth_id = %snapshot.id,
                provider = %snapshot.provider,
                "failed to persist registered auth: {err}"
            );
        }
        Ok(snapshot)
    }

    /// Replaces a registered credential (upstream's `Update`). Returns
    /// `None` when no credential has its ID.
    ///
    /// While neither the old nor the new credential is disabled, the new
    /// one keeps the old model states when it has none, an active
    /// credential-wide quota cooldown carries over, and new tokens clear a
    /// recorded 401.
    pub fn update(&self, auth: Auth) -> Result<Option<Arc<Auth>>, ManagerError> {
        Ok(self
            .update_internal(UpdateMode::Replace, auth, Save::Yes)?
            .map(|(snapshot, _)| snapshot))
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
    pub(crate) fn update_refreshed(
        &self,
        base: &Auth,
        base_epoch: u64,
        updated: Auth,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        self.update_internal(UpdateMode::Refresh { base, base_epoch }, updated, Save::Yes)
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
        let now = self.now();
        let (snapshot, epoch, generation) = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let Some(existing) = state.auths.get(&auth.id) else {
                return Ok(None);
            };
            let existing_auth = existing.auth.clone();
            let existing_epoch = existing.epoch;
            let existing_generation = existing.generation;
            let existing_failures = existing.refresh_failures;
            let slot = state.epochs.entry(auth.id.clone()).or_insert(0);
            *slot = (*slot).max(existing_epoch);
            let live_epoch = *slot;
            let refresh_failures = match mode {
                UpdateMode::Replace => 0,
                UpdateMode::Refresh { base, base_epoch } => {
                    if existing_epoch != base_epoch {
                        return Err(ManagerError::Other(format!(
                            "update auth {}: stale registration epoch {base_epoch} != {existing_epoch}",
                            auth.id
                        )));
                    }
                    auth = merge_refreshed_auth(base, &existing_auth, &auth, now);
                    normalize_credential_metadata(&mut auth.metadata);
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
            let mut generation = existing_generation.saturating_add(1);
            if !is_disabled(&existing_auth) && !is_disabled(&auth) {
                if auth.model_states.is_empty() && !existing_auth.model_states.is_empty() {
                    auth.model_states = existing_auth.model_states.clone();
                }
                if credentials_changed(&existing_auth, &auth) {
                    let new_unauthorized = auth
                        .last_error
                        .as_ref()
                        .is_some_and(|err| is_unauthorized_error(ErrView::Auth(err)));
                    if has_unauthorized_auth_failure(&existing_auth) || new_unauthorized {
                        auth.unavailable = false;
                        auth.last_error = None;
                        auth.status_message.clear();
                        auth.status = Status::Active;
                    }
                    clear_unauthorized_model_states(&mut auth, now);
                }
                if is_credential_quota_active(&existing_auth.quota, now) {
                    auth.unavailable = existing_auth.unavailable;
                    auth.next_retry_after = existing_auth.next_retry_after;
                    auth.quota = existing_auth.quota.clone();
                    if auth.status == Status::Active {
                        auth.status = existing_auth.status;
                    }
                }
            }
            auth.updated_at = Some(now);
            normalize_model_states(&mut auth.model_states);
            if (cooldown_disabled_for_auth(&state.settings, &auth) || is_disabled(&auth))
                && clear_cooldown_state_for_auth(&mut auth, now)
            {
                generation = generation.saturating_add(1);
            }
            let snapshot = Arc::new(auth);
            state.auths.insert(
                snapshot.id.clone(),
                Entry {
                    auth: snapshot.clone(),
                    epoch: live_epoch,
                    generation,
                    refresh_failures,
                },
            );
            state.sync_scheduler(self.models(), &snapshot.id, now);
            (snapshot, live_epoch, generation)
        };
        self.queue_refresh_reschedule(&snapshot.id);
        if let Err(err) = self.persist(&snapshot, epoch, generation, save) {
            tracing::warn!(
                auth_id = %snapshot.id,
                provider = %snapshot.provider,
                "failed to persist updated auth: {err}"
            );
        }
        Ok(Some((snapshot, generation)))
    }

    /// Removes a credential from the manager, and closes its executor's
    /// sessions (upstream's `Remove`). The store isn't touched: deleting the
    /// saved credential is the caller's job.
    pub fn remove(&self, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        let provider = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let Some(existing) = state.auths.remove(id) else {
                return;
            };
            state.pool_offsets.remove(id);
            state.sync_scheduler(self.models(), id, self.now());
            let slot = state.epochs.entry(id.to_owned()).or_insert(0);
            *slot = (*slot).max(existing.epoch).saturating_add(1);
            existing.auth.provider.trim().to_owned()
        };
        self.queue_refresh_unschedule(id);
        if !provider.is_empty()
            && let Some(executor) = self.executor(&provider)
        {
            Self::close_all_sessions(&*executor);
        }
    }

    /// Replaces every credential with the store's (upstream's `Load`).
    /// Credentials without an ID or with an invalid weight are skipped.
    /// Without a store this does nothing.
    pub fn load(&self) -> Result<(), ManagerError> {
        let Some(store) = &self.shared.store else {
            return Ok(());
        };
        let mut guard = self.lock();
        let items = store.list().map_err(ManagerError::Store)?;
        let state = &mut *guard;
        let previous = std::mem::take(&mut state.auths);
        for mut auth in items {
            if auth.id.is_empty() {
                continue;
            }
            normalize_credential_metadata(&mut auth.metadata);
            if validate_weight(&auth).is_err() {
                continue;
            }
            let slot = state.epochs.entry(auth.id.clone()).or_insert(0);
            *slot = slot.saturating_add(1);
            let epoch = *slot;
            state.auths.insert(
                auth.id.clone(),
                Entry {
                    auth: Arc::new(auth),
                    epoch,
                    generation: 1,
                    refresh_failures: 0,
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

    /// Saves a credential snapshot taken at (`epoch`, `generation`), unless a
    /// newer one was saved already (upstream's `persist`). Credentials from
    /// config API keys, runtime-only ones and ones without metadata aren't
    /// saved.
    pub(crate) fn persist(
        &self,
        auth: &Auth,
        epoch: u64,
        generation: u64,
        save: Save,
    ) -> Result<(), ManagerError> {
        let Some(store) = &self.shared.store else {
            return Ok(());
        };
        validate_weight(auth)
            .map_err(|err| ManagerError::InvalidWeight(format!("persist auth: {err}")))?;
        if auth_kind(auth) == KIND_API_KEY && auth_source_kind(auth) == SOURCE_CONFIG {
            return Ok(());
        }
        if auth
            .attributes
            .get("runtime_only")
            .is_some_and(|value| go_lower(value.trim()) == "true")
        {
            return Ok(());
        }
        if auth.metadata.is_empty() {
            return Ok(());
        }
        let id_lock = lock(&self.shared.persist_locks)
            .entry(auth.id.clone())
            .or_default()
            .clone();
        let mut last = lock(&id_lock);
        let (last_epoch, last_generation) = *last;
        if epoch < last_epoch || (epoch == last_epoch && generation < last_generation) {
            return Ok(());
        }
        *last = (epoch, generation);
        if save == Save::No {
            return Ok(());
        }
        store.save(auth).map(drop).map_err(ManagerError::Store)
    }

    /// Records a call's outcome on its credential: cools the model or the
    /// credential down after a failure, clears it after a success, saves the
    /// credential, and publishes its models' availability (upstream's
    /// `MarkResult`).
    pub fn mark_result(&self, result: &CallResult) {
        if result.auth_id.is_empty() {
            return;
        }
        let now = self.now();
        let mut model_key = canonical_model_key(&result.model);
        let (snapshot, epoch, generation) = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let Some(entry) = state.auths.get_mut(&result.auth_id) else {
                return;
            };
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
            apply_result(&state.settings, auth, result, &model_key, now);
            auth.updated_at = Some(now);
            entry.generation = entry.generation.saturating_add(1);
            let committed = (entry.auth.clone(), entry.epoch, entry.generation);
            state.sync_scheduler(self.models(), &result.auth_id, now);
            committed
        };
        let _ = self.persist(&snapshot, epoch, generation, Save::Yes);
        self.publish_projections(&snapshot, generation, now, true);
    }

    /// Records an outcome that says nothing about the credential's health:
    /// only the generation moves, and the credential is saved (upstream's
    /// `recordAvailabilityNeutralResult`).
    pub(crate) fn record_availability_neutral_result(&self, result: &CallResult) {
        if result.auth_id.is_empty() {
            return;
        }
        let now = self.now();
        let (snapshot, epoch, generation) = {
            let mut state = self.lock();
            let Some(entry) = state.auths.get_mut(&result.auth_id) else {
                return;
            };
            Arc::make_mut(&mut entry.auth).updated_at = Some(now);
            entry.generation = entry.generation.saturating_add(1);
            (entry.auth.clone(), entry.epoch, entry.generation)
        };
        let _ = self.persist(&snapshot, epoch, generation, Save::Yes);
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
        let (snapshot, models, epoch, generation) = {
            let mut state = self.lock();
            let Some(entry) = state.auths.get_mut(id) else {
                return Ok(None);
            };
            let (models, cleared) = reset_quota(Arc::make_mut(&mut entry.auth), &registered, now);
            let bumps = if cleared { 2 } else { 1 };
            entry.generation = entry.generation.saturating_add(bumps);
            let committed = (entry.auth.clone(), models, entry.epoch, entry.generation);
            state.sync_scheduler(self.models(), id, now);
            committed
        };
        let persisted = self.persist(&snapshot, epoch, generation, Save::Yes);
        self.publish_projections(&snapshot, generation, now, false);
        persisted.map(|()| {
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
        let now = self.now();
        let models = self.models();
        let mut committed = None;
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
                let (states, changed) =
                    reconcile_model_states(resolver, &entry.auth, &supported, now);
                if models.client_registration_epoch(id) != reg_epoch {
                    continue;
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
                    entry.generation = entry.generation.saturating_add(1);
                }
                committed = Some((
                    entry.auth.clone(),
                    entry.epoch,
                    entry.generation,
                    changed,
                    supported,
                    reg_epoch,
                ));
                break;
            }
            if committed.is_some() {
                state.sync_scheduler(models, id, now);
            }
        }
        let Some((snapshot, epoch, generation, changed, supported, reg_epoch)) = committed else {
            return;
        };
        if changed && let Err(err) = self.persist(&snapshot, epoch, generation, Save::Yes) {
            tracing::warn!(
                auth_id = %snapshot.id,
                "failed to persist auth changes during model state reconciliation: {err}"
            );
        }
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
}
