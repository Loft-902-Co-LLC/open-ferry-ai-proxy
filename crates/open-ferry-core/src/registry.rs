// Ported from CLIProxyAPI internal/registry/model_registry.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The model registry: which models each credential serves, and which of
//! them are available now.
//!
//! Each credential registers as a client, with a provider and its models.
//! A model is listed while at least one client serves it. A client that hit
//! its quota for a model is taken out for five minutes, and a client can be
//! suspended for a model until it is resumed. The credential manager mirrors
//! its own state into the registry with
//! [`ModelRegistry::apply_client_model_projections`].
//!
//! [`definitions`] holds the static model catalog, and [`registration`]
//! works out which models a credential serves.
//!
//! Deviations from upstream:
//! - There is no global registry; callers share a [`ModelRegistry`].
//! - Model lists aren't cached; they are worked out on each call, with the
//!   same result.
//! - Orders upstream leaves to Go's map iteration are fixed: model lists come
//!   sorted by ID, [`ModelRegistry::available_models_by_provider`] takes each
//!   model's details from the client with the lowest ID, and
//!   [`ModelRegistry::first_available_model_for`] keeps models without a
//!   `created` time last and ties in ID order.
//! - No logging and no registration hooks (upstream's `ModelRegistryHook`,
//!   used by plugins, which aren't ported).
//! - Web search capability (`SupportsWebSearch`, `NativeCapabilities`,
//!   `GetResponsesWebSearchCapability`, `ApplyClientModelCapabilities`) isn't
//!   ported; nor are the Gemini-only fields `inputTokenLimit`,
//!   `outputTokenLimit` and `supportedGenerationMethods`, so Gemini model
//!   lists leave them out.
//! - `LookupModelInfo` and `ModelOverrideHeaders` aren't ported. The latter
//!   serves the catalog's `override_header`, which forces a client identity
//!   and is left out by policy.
//! - A registration's `LastUpdated` time isn't kept; nothing reads it.

pub mod definitions;
mod json;
pub mod registration;

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{self, AtomicU64};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use open_ferry_translate::go;
use serde_json::{Map, Value};

use crate::auth::equal_fold;
use crate::exec::ProviderId;
use crate::models::{ModelCatalog, ModelInfo};

pub use definitions::{CatalogError, CodexPlan, StaticCatalog};
pub use registration::{
    ApiKeyEntry, AuthModels, ConfiguredModel, ModelAlias, ModelSetting, RegistrationRules,
};

/// The input token limit Claude model lists show when a model has none.
pub const DEFAULT_CLAUDE_MAX_INPUT_TOKENS: u64 = 200_000;
/// The output token limit Claude model lists show when a model has none.
pub const DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// How long a quota mark takes a client out of a model.
const QUOTA_WINDOW: Duration = Duration::from_secs(5 * 60);

/// The models of every registered client, and their availability (upstream's
/// `ModelRegistry`). Safe to share between threads.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    state: RwLock<State>,
    /// Counts client registrations and removals.
    registration_epoch: AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    /// Registrations by model ID.
    models: BTreeMap<String, ModelRegistration>,
    /// Each client's model IDs, one per model it registered, duplicates kept.
    client_models: HashMap<String, Vec<String>>,
    /// Each client's own details for its models.
    client_model_infos: HashMap<String, HashMap<String, ModelInfo>>,
    /// Each client's provider, when it has one.
    client_providers: BTreeMap<String, String>,
    /// Counts each client's registrations and removals.
    client_epochs: HashMap<String, u64>,
    /// The newest projection generation applied to each client.
    client_generations: HashMap<String, u64>,
    /// Counts changes to registrations and availability.
    generation: u64,
}

/// One model and the clients that serve it (upstream's `ModelRegistration`).
#[derive(Clone, Debug, Default)]
struct ModelRegistration {
    /// The details last registered.
    info: ModelInfo,
    /// The details last registered under each provider.
    info_by_provider: HashMap<String, ModelInfo>,
    /// How many registrations serve the model.
    count: i64,
    /// When each client last hit its quota for the model.
    quota_exceeded_clients: HashMap<String, Instant>,
    /// How many registrations serve the model under each provider.
    providers: HashMap<String, i64>,
    /// Suspended clients, with the reason.
    suspended_clients: HashMap<String, String>,
}

/// The state the credential manager wants for one of a client's models
/// (upstream's `ClientModelProjection`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientModelProjection {
    /// The model.
    pub model_id: String,
    /// Whether the client is suspended for the model.
    pub suspended: bool,
    /// Why, when suspended. A reason of `quota` counts as a cooldown.
    pub suspend_reason: String,
    /// Whether the client hit its quota for the model.
    pub quota_exceeded: bool,
}

/// Why [`ModelRegistry::first_available_model_for`] found no model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirstModelError {
    /// No model is listed for the handler type.
    NoModels(String),
    /// Models are listed, but none has a client free to serve it.
    NoAvailableClients(String),
}

impl fmt::Display for FirstModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoModels(handler) => {
                write!(f, "no models available for handler type: {handler}")
            }
            Self::NoAvailableClients(handler) => write!(
                f,
                "no available clients for any model in handler type: {handler}"
            ),
        }
    }
}

impl std::error::Error for FirstModelError {}

impl ModelRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump_registration_epoch(&self) {
        // fetch_add wraps on overflow.
        self.registration_epoch
            .fetch_add(1, atomic::Ordering::SeqCst);
    }

    /// Counts changes to registrations and availability (upstream's
    /// `GetGeneration`).
    pub fn generation(&self) -> u64 {
        self.read().generation
    }

    /// Counts client registrations and removals.
    pub fn registration_epoch(&self) -> u64 {
        self.registration_epoch.load(atomic::Ordering::SeqCst)
    }

    /// Registers `client_id`'s models under `provider`, replacing what it
    /// registered before (upstream's `RegisterClient`). A model listed twice
    /// counts twice. With no models, the client is unregistered.
    ///
    /// Re-registering clears the client's quota marks and suspensions for the
    /// models it keeps.
    pub fn register_client(&self, client_id: &str, provider: &str, models: &[ModelInfo]) {
        let mut state = self.write();
        let provider = go::to_lower(provider);

        let mut raw_ids: Vec<String> = Vec::with_capacity(models.len());
        let mut unique_ids: Vec<String> = Vec::with_capacity(models.len());
        let mut new_models: HashMap<String, &ModelInfo> = HashMap::with_capacity(models.len());
        let mut new_counts: HashMap<String, i64> = HashMap::with_capacity(models.len());
        for model in models {
            if model.id.is_empty() {
                continue;
            }
            raw_ids.push(model.id.clone());
            *new_counts.entry(model.id.clone()).or_insert(0) += 1;
            if new_models.contains_key(&model.id) {
                continue;
            }
            new_models.insert(model.id.clone(), model);
            unique_ids.push(model.id.clone());
        }

        if unique_ids.is_empty() {
            self.unregister_locked(&mut state, client_id);
            state.client_models.remove(client_id);
            state.client_model_infos.remove(client_id);
            state.client_providers.remove(client_id);
            state.invalidate();
            return;
        }

        bump(&mut state.client_epochs, client_id);
        state.client_generations.insert(client_id.to_owned(), 0);
        self.bump_registration_epoch();

        let old_provider = state
            .client_providers
            .get(client_id)
            .cloned()
            .unwrap_or_default();
        let provider_changed = old_provider != provider;
        let Some(old_models) = state.client_models.get(client_id).cloned() else {
            for id in &raw_ids {
                if let Some(model) = new_models.get(id) {
                    state.add_model_registration(id, &provider, model);
                }
            }
            state.set_client(client_id, &provider, raw_ids, &new_models);
            state.invalidate();
            return;
        };

        let mut old_counts: HashMap<String, i64> = HashMap::with_capacity(old_models.len());
        for id in &old_models {
            *old_counts.entry(id.clone()).or_insert(0) += 1;
        }
        let count_of =
            |counts: &HashMap<String, i64>, id: &str| counts.get(id).copied().unwrap_or(0);
        let added: HashSet<&str> = unique_ids
            .iter()
            .filter(|id| count_of(&old_counts, id) == 0)
            .map(String::as_str)
            .collect();
        let removed: Vec<&String> = old_counts
            .keys()
            .filter(|id| count_of(&new_counts, id) == 0)
            .collect();

        // Move the models kept across a provider change off the old provider
        // first.
        if provider_changed && !old_provider.is_empty() {
            for (id, &new_count) in &new_counts {
                let old_count = count_of(&old_counts, id);
                if new_count == 0 || old_count == 0 {
                    continue;
                }
                let to_remove = new_count.min(old_count);
                if let Some(registration) = state.models.get_mut(id)
                    && let Some(&count) = registration.providers.get(&old_provider)
                {
                    if count <= to_remove {
                        registration.providers.remove(&old_provider);
                        registration.info_by_provider.remove(&old_provider);
                    } else {
                        registration
                            .providers
                            .insert(old_provider.clone(), count - to_remove);
                    }
                }
            }
        }

        for id in &removed {
            for _ in 0..count_of(&old_counts, id) {
                state.remove_model_registration(client_id, id, &old_provider);
            }
        }
        for (id, &old_count) in &old_counts {
            let new_count = count_of(&new_counts, id);
            if new_count == 0 || old_count <= new_count {
                continue;
            }
            for _ in 0..old_count - new_count {
                state.remove_model_registration(client_id, id, &old_provider);
            }
        }
        for (id, &new_count) in &new_counts {
            let old_count = count_of(&old_counts, id);
            if new_count <= old_count {
                continue;
            }
            if let Some(model) = new_models.get(id) {
                for _ in 0..new_count - old_count {
                    state.add_model_registration(id, &provider, model);
                }
            }
        }

        for id in &unique_ids {
            let (Some(model), Some(registration)) = (new_models.get(id), state.models.get_mut(id))
            else {
                continue;
            };
            registration.info = (*model).clone();
            if !provider.is_empty() {
                registration
                    .info_by_provider
                    .insert(provider.clone(), (*model).clone());
            }
            // Cooldowns and suspensions belong to the old registration.
            registration.quota_exceeded_clients.remove(client_id);
            registration.suspended_clients.remove(client_id);
            if provider_changed && !provider.is_empty() && !added.contains(id.as_str()) {
                let overlap = count_of(&new_counts, id).min(count_of(&old_counts, id));
                if overlap > 0 {
                    *registration.providers.entry(provider.clone()).or_insert(0) += overlap;
                }
            }
        }

        state.set_client(client_id, &provider, raw_ids, &new_models);
        state.invalidate();
    }

    /// Removes `client_id` and its models (upstream's `UnregisterClient`).
    pub fn unregister_client(&self, client_id: &str) {
        let mut state = self.write();
        self.unregister_locked(&mut state, client_id);
        state.invalidate();
    }

    fn unregister_locked(&self, state: &mut State, client_id: &str) {
        bump(&mut state.client_epochs, client_id);
        bump(&mut state.client_generations, client_id);
        self.bump_registration_epoch();

        let provider = state.client_providers.remove(client_id);
        let Some(models) = state.client_models.remove(client_id) else {
            return;
        };
        for id in &models {
            let Some(registration) = state.models.get_mut(id) else {
                continue;
            };
            registration.count -= 1;
            registration.quota_exceeded_clients.remove(client_id);
            registration.suspended_clients.remove(client_id);
            if let Some(provider) = &provider {
                registration.release_provider(provider);
            }
            if registration.count <= 0 {
                state.models.remove(id);
            }
        }
        state.client_model_infos.remove(client_id);
    }

    /// Marks `client_id` as over its quota for `model_id`, which takes it out
    /// for five minutes (upstream's `SetModelQuotaExceeded`).
    pub fn set_model_quota_exceeded(&self, client_id: &str, model_id: &str) {
        let mut state = self.write();
        if let Some(registration) = state.models.get_mut(model_id) {
            registration
                .quota_exceeded_clients
                .insert(client_id.to_owned(), Instant::now());
            state.invalidate();
        }
    }

    /// Clears `client_id`'s quota mark for `model_id` (upstream's
    /// `ClearModelQuotaExceeded`).
    pub fn clear_model_quota_exceeded(&self, client_id: &str, model_id: &str) {
        let mut state = self.write();
        if let Some(registration) = state.models.get_mut(model_id) {
            registration.quota_exceeded_clients.remove(client_id);
            state.invalidate();
        }
    }

    /// Applies the credential manager's view of `client_id`'s models, if
    /// `epoch` is the client's current registration epoch and `generation`
    /// isn't older than the last one applied (upstream's
    /// `ApplyClientModelProjections`). Returns whether it was applied: a
    /// client that isn't registered, or projections that name none of its
    /// models, are rejected.
    pub fn apply_client_model_projections(
        &self,
        client_id: &str,
        epoch: u64,
        generation: u64,
        projections: &[ClientModelProjection],
    ) -> bool {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return false;
        }
        let mut state = self.write();
        let state = &mut *state;
        let Some(client_models) = state.client_models.get(client_id) else {
            return false;
        };
        if client_models.is_empty()
            || state.client_epochs.get(client_id).copied().unwrap_or(0) != epoch
            || generation
                < state
                    .client_generations
                    .get(client_id)
                    .copied()
                    .unwrap_or(0)
        {
            return false;
        }
        let registered: HashSet<&str> = client_models
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .collect();
        fn owned<'a>(
            registered: &HashSet<&str>,
            projection: &'a ClientModelProjection,
        ) -> Option<&'a str> {
            let id = projection.model_id.trim();
            (!id.is_empty() && registered.contains(id)).then_some(id)
        }
        let any_valid = projections
            .iter()
            .filter_map(|projection| owned(&registered, projection))
            .any(|id| state.models.contains_key(id));
        if !any_valid {
            return false;
        }

        state
            .client_generations
            .insert(client_id.to_owned(), generation);
        let now = Instant::now();
        let mut changed = false;
        for projection in projections {
            let Some(id) = owned(&registered, projection) else {
                continue;
            };
            let Some(registration) = state.models.get_mut(id) else {
                continue;
            };
            if projection.suspended {
                if registration.suspended_clients.get(client_id) != Some(&projection.suspend_reason)
                {
                    registration
                        .suspended_clients
                        .insert(client_id.to_owned(), projection.suspend_reason.clone());
                    changed = true;
                }
            } else if registration.suspended_clients.remove(client_id).is_some() {
                changed = true;
            }
            if projection.quota_exceeded {
                if !registration.quota_exceeded_clients.contains_key(client_id) {
                    registration
                        .quota_exceeded_clients
                        .insert(client_id.to_owned(), now);
                    changed = true;
                }
            } else if registration
                .quota_exceeded_clients
                .remove(client_id)
                .is_some()
            {
                changed = true;
            }
        }
        if changed {
            state.invalidate();
        }
        true
    }

    /// Takes `client_id` out of `model_id` until it is resumed (upstream's
    /// `SuspendClientModel`). A reason of `quota` counts as a cooldown. An
    /// existing suspension keeps its reason.
    pub fn suspend_client_model(&self, client_id: &str, model_id: &str, reason: &str) {
        if client_id.is_empty() || model_id.is_empty() {
            return;
        }
        let mut state = self.write();
        let Some(registration) = state.models.get_mut(model_id) else {
            return;
        };
        if registration.suspended_clients.contains_key(client_id) {
            return;
        }
        registration
            .suspended_clients
            .insert(client_id.to_owned(), reason.to_owned());
        state.invalidate();
    }

    /// Ends a suspension (upstream's `ResumeClientModel`).
    pub fn resume_client_model(&self, client_id: &str, model_id: &str) {
        if client_id.is_empty() || model_id.is_empty() {
            return;
        }
        let mut state = self.write();
        let Some(registration) = state.models.get_mut(model_id) else {
            return;
        };
        if registration.suspended_clients.remove(client_id).is_some() {
            state.invalidate();
        }
    }

    /// Whether `client_id` registered `model_id`, ignoring case and
    /// surrounding whitespace (upstream's `ClientSupportsModel`).
    pub fn client_supports_model(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        let state = self.read();
        state
            .client_models
            .get(client_id)
            .is_some_and(|ids| ids.iter().any(|id| equal_fold(id.trim(), model_id)))
    }

    /// Whether `client_id` is suspended for `model_id` (upstream's
    /// `IsModelSuspendedForClient`).
    pub fn is_model_suspended_for_client(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        let state = self.read();
        state
            .models
            .get(model_id)
            .is_some_and(|registration| registration.suspended_clients.contains_key(client_id))
    }

    /// Whether `client_id` has a quota mark for `model_id`, however old
    /// (upstream's `IsModelQuotaExceededForClient`).
    pub fn is_model_quota_exceeded_for_client(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        let state = self.read();
        state
            .models
            .get(model_id)
            .is_some_and(|registration| registration.quota_exceeded_clients.contains_key(client_id))
    }

    /// The available models, as a model list for `handler_type` shows them:
    /// `openai`, `claude`, `gemini`, or any other value for a generic list
    /// (upstream's `GetAvailableModels`). Sorted by model ID.
    pub fn available_model_maps(&self, handler_type: &str) -> Vec<Map<String, Value>> {
        self.available_model_maps_at(handler_type, Instant::now())
    }

    fn available_model_maps_at(&self, handler_type: &str, now: Instant) -> Vec<Map<String, Value>> {
        let state = self.read();
        state
            .models
            .values()
            .filter(|registration| registration.available(now))
            .map(|registration| model_to_map(&registration.info, handler_type))
            .collect()
    }

    /// The details of each available model, sorted by ID (upstream's
    /// `GetAvailableModelInfos`).
    pub fn available_model_infos(&self) -> Vec<ModelInfo> {
        self.available_model_infos_at(Instant::now())
    }

    fn available_model_infos_at(&self, now: Instant) -> Vec<ModelInfo> {
        let state = self.read();
        let mut infos: Vec<ModelInfo> = state
            .models
            .values()
            .filter(|registration| registration.available(now))
            .map(|registration| registration.info.clone())
            .collect();
        infos.sort_by(|a, b| a.id.trim().cmp(b.id.trim()));
        infos
    }

    /// The models available through `provider`'s clients, counting only those
    /// clients' quota marks and suspensions (upstream's
    /// `GetAvailableModelsByProvider`). Each model has its details as the
    /// first client registered them. Sorted by model ID.
    pub fn available_models_by_provider(&self, provider: &str) -> Vec<ModelInfo> {
        self.available_models_by_provider_at(provider, Instant::now())
    }

    fn available_models_by_provider_at(&self, provider: &str, now: Instant) -> Vec<ModelInfo> {
        let provider = go::to_lower(provider.trim());
        if provider.is_empty() {
            return Vec::new();
        }
        let state = self.read();

        struct Entry<'a> {
            count: i64,
            info: Option<&'a ModelInfo>,
        }
        let mut entries: BTreeMap<&str, Entry<'_>> = BTreeMap::new();
        for (client_id, client_provider) in &state.client_providers {
            if *client_provider != provider {
                continue;
            }
            let Some(ids) = state.client_models.get(client_id) else {
                continue;
            };
            let client_infos = state.client_model_infos.get(client_id);
            for id in ids {
                let id = id.trim();
                if id.is_empty() {
                    continue;
                }
                let entry = entries.entry(id).or_insert(Entry {
                    count: 0,
                    info: None,
                });
                entry.count += 1;
                if entry.info.is_none() {
                    entry.info = client_infos
                        .and_then(|infos| infos.get(id))
                        .or_else(|| state.models.get(id).map(|registration| &registration.info));
                }
            }
        }

        let of_provider = |client_id: &String| {
            !client_id.is_empty() && state.client_providers.get(client_id) == Some(&provider)
        };
        let mut result = Vec::new();
        for (id, entry) in entries {
            if entry.count <= 0 {
                continue;
            }
            let registration = state.models.get(id);
            let mut tally = Tally::default();
            if let Some(registration) = registration {
                tally.expired = registration
                    .quota_exceeded_clients
                    .iter()
                    .filter(|(client_id, mark)| {
                        of_provider(client_id) && within_window(**mark, now)
                    })
                    .count() as i64;
                for (client_id, reason) in &registration.suspended_clients {
                    if of_provider(client_id) {
                        tally.add_suspension(registration, client_id, reason, now);
                    }
                }
            }
            if tally.available(entry.count) {
                if let Some(info) = entry.info {
                    result.push(info.clone());
                } else if let Some(registration) = registration {
                    result.push(registration.info.clone());
                }
            }
        }
        result
    }

    /// How many clients can serve `model_id` now: those without a recent
    /// quota mark or a suspension (upstream's `GetModelCount`).
    pub fn model_count(&self, model_id: &str) -> usize {
        self.model_count_at(model_id, Instant::now())
    }

    fn model_count_at(&self, model_id: &str, now: Instant) -> usize {
        let state = self.read();
        let Some(registration) = state.models.get(model_id) else {
            return 0;
        };
        let expired = registration
            .quota_exceeded_clients
            .values()
            .filter(|mark| within_window(**mark, now))
            .count() as i64;
        // A suspended client with a recent quota mark is already counted.
        let suspended = registration
            .suspended_clients
            .keys()
            .filter(|client_id| !registration.has_recent_mark(client_id, now))
            .count() as i64;
        usize::try_from(registration.count - expired - suspended).unwrap_or(0)
    }

    /// The providers that serve `model_id`, most registrations first, then by
    /// name (upstream's `GetModelProviders`).
    pub fn providers_for_model(&self, model_id: &str) -> Vec<ProviderId> {
        let state = self.read();
        let Some(registration) = state.models.get(model_id) else {
            return Vec::new();
        };
        let mut providers: Vec<(&String, i64)> = registration
            .providers
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(name, count)| (name, *count))
            .collect();
        providers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        providers
            .into_iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// `model_id`'s details as last registered under `provider`, or as last
    /// registered at all when `provider` is empty or doesn't serve it
    /// (upstream's `GetModelInfo`).
    pub fn model_info(&self, model_id: &str, provider: &str) -> Option<ModelInfo> {
        let state = self.read();
        let registration = state.models.get(model_id)?;
        if !provider.is_empty()
            && registration
                .providers
                .get(provider)
                .is_some_and(|count| *count > 0)
            && let Some(info) = registration.info_by_provider.get(provider)
        {
            return Some(info.clone());
        }
        Some(registration.info.clone())
    }

    /// Drops quota marks older than five minutes (upstream's
    /// `CleanupExpiredQuotas`).
    pub fn cleanup_expired_quotas(&self) {
        self.cleanup_expired_quotas_at(Instant::now());
    }

    fn cleanup_expired_quotas_at(&self, now: Instant) {
        let mut state = self.write();
        let mut removed = false;
        for registration in state.models.values_mut() {
            let before = registration.quota_exceeded_clients.len();
            registration
                .quota_exceeded_clients
                .retain(|_, mark| within_window(*mark, now));
            removed |= registration.quota_exceeded_clients.len() != before;
        }
        if removed {
            state.invalidate();
        }
    }

    /// The newest available model with a client free to serve it, by its
    /// `created` time in `handler_type`'s model list (upstream's
    /// `GetFirstAvailableModel`). Lists that show no `created` time keep
    /// model ID order.
    pub fn first_available_model_for(&self, handler_type: &str) -> Result<String, FirstModelError> {
        self.first_available_model_at(handler_type, Instant::now())
    }

    fn first_available_model_at(
        &self,
        handler_type: &str,
        now: Instant,
    ) -> Result<String, FirstModelError> {
        let mut models = self.available_model_maps_at(handler_type, now);
        if models.is_empty() {
            return Err(FirstModelError::NoModels(handler_type.to_owned()));
        }
        let created = |model: &Map<String, Value>| model.get("created").and_then(Value::as_i64);
        models.sort_by(|a, b| match (created(a), created(b)) {
            (Some(a), Some(b)) => b.cmp(&a),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
        models
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .find(|id| self.model_count_at(id, now) > 0)
            .map(str::to_owned)
            .ok_or_else(|| FirstModelError::NoAvailableClients(handler_type.to_owned()))
    }

    /// The models `client_id` registered, each once, with the client's own
    /// details, and its registration epoch (upstream's
    /// `GetModelsAndEpochForClient`).
    pub fn models_and_epoch_for_client(&self, client_id: &str) -> (Vec<ModelInfo>, u64) {
        let state = self.read();
        let epoch = state.client_epochs.get(client_id).copied().unwrap_or(0);
        let Some(ids) = state.client_models.get(client_id) else {
            return (Vec::new(), epoch);
        };
        let client_infos = state.client_model_infos.get(client_id);
        let mut seen = HashSet::new();
        let mut models = Vec::with_capacity(ids.len());
        for id in ids {
            if !seen.insert(id.as_str()) {
                continue;
            }
            if let Some(info) = client_infos.and_then(|infos| infos.get(id)) {
                models.push(info.clone());
            } else if let Some(registration) = state.models.get(id) {
                models.push(registration.info.clone());
            }
        }
        (models, epoch)
    }

    /// The models `client_id` registered (upstream's `GetModelsForClient`).
    pub fn models_for_client(&self, client_id: &str) -> Vec<ModelInfo> {
        self.models_and_epoch_for_client(client_id).0
    }

    /// `client_id`'s registration epoch, which changes each time it registers
    /// or is removed (upstream's `ClientRegistrationEpoch`).
    pub fn client_registration_epoch(&self, client_id: &str) -> u64 {
        self.read()
            .client_epochs
            .get(client_id)
            .copied()
            .unwrap_or(0)
    }
}

impl ModelCatalog for ModelRegistry {
    fn model_providers(&self, model: &str) -> Vec<ProviderId> {
        self.providers_for_model(model)
    }

    fn first_available_model(&self) -> Option<String> {
        // Upstream's `auto` resolution asks with an empty handler type.
        self.first_available_model_for("").ok()
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        self.available_model_infos()
    }

    fn model_info(&self, model: &str, provider: &str) -> Option<ModelInfo> {
        Self::model_info(self, model, provider)
    }
}

impl State {
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn add_model_registration(&mut self, id: &str, provider: &str, model: &ModelInfo) {
        if let Some(registration) = self.models.get_mut(id) {
            registration.count += 1;
            registration.info = model.clone();
            if !provider.is_empty() {
                *registration
                    .providers
                    .entry(provider.to_owned())
                    .or_insert(0) += 1;
                registration
                    .info_by_provider
                    .insert(provider.to_owned(), model.clone());
            }
            return;
        }
        let mut registration = ModelRegistration {
            info: model.clone(),
            count: 1,
            ..ModelRegistration::default()
        };
        if !provider.is_empty() {
            registration.providers.insert(provider.to_owned(), 1);
            registration
                .info_by_provider
                .insert(provider.to_owned(), model.clone());
        }
        self.models.insert(id.to_owned(), registration);
    }

    fn remove_model_registration(&mut self, client_id: &str, id: &str, provider: &str) {
        let Some(registration) = self.models.get_mut(id) else {
            return;
        };
        registration.count = (registration.count - 1).max(0);
        registration.quota_exceeded_clients.remove(client_id);
        registration.suspended_clients.remove(client_id);
        if !provider.is_empty() {
            registration.release_provider(provider);
        }
        if registration.count <= 0 {
            self.models.remove(id);
        }
    }

    /// Records the client's models and provider after a registration.
    fn set_client(
        &mut self,
        client_id: &str,
        provider: &str,
        raw_ids: Vec<String>,
        models: &HashMap<String, &ModelInfo>,
    ) {
        self.client_models.insert(client_id.to_owned(), raw_ids);
        let infos = models
            .iter()
            .map(|(id, model)| (id.clone(), (*model).clone()))
            .collect();
        self.client_model_infos.insert(client_id.to_owned(), infos);
        if provider.is_empty() {
            self.client_providers.remove(client_id);
        } else {
            self.client_providers
                .insert(client_id.to_owned(), provider.to_owned());
        }
    }
}

impl ModelRegistration {
    /// Takes one registration off `provider`.
    fn release_provider(&mut self, provider: &str) {
        if let Some(&count) = self.providers.get(provider) {
            if count <= 1 {
                self.providers.remove(provider);
                self.info_by_provider.remove(provider);
            } else {
                self.providers.insert(provider.to_owned(), count - 1);
            }
        }
    }

    fn has_recent_mark(&self, client_id: &str, now: Instant) -> bool {
        self.quota_exceeded_clients
            .get(client_id)
            .is_some_and(|mark| within_window(*mark, now))
    }

    /// Whether the model should be listed (upstream's
    /// `modelRegistrationAvailability`).
    fn available(&self, now: Instant) -> bool {
        let mut tally = Tally {
            expired: self
                .quota_exceeded_clients
                .values()
                .filter(|mark| within_window(**mark, now))
                .count() as i64,
            ..Tally::default()
        };
        for (client_id, reason) in &self.suspended_clients {
            tally.add_suspension(self, client_id, reason, now);
        }
        tally.available(self.count)
    }
}

/// Clients out of a model, by why.
#[derive(Default)]
struct Tally {
    /// Clients with a quota mark from the last five minutes.
    expired: i64,
    /// Clients suspended for quota.
    cooldown: i64,
    /// Clients suspended for anything else.
    other: i64,
    /// Clients suspended for anything else that also have a recent quota
    /// mark, so are counted twice.
    quota_and_other: i64,
}

impl Tally {
    fn add_suspension(
        &mut self,
        registration: &ModelRegistration,
        client_id: &str,
        reason: &str,
        now: Instant,
    ) {
        if equal_fold(reason, "quota") {
            self.cooldown += 1;
            return;
        }
        self.other += 1;
        if registration.has_recent_mark(client_id, now) {
            self.quota_and_other += 1;
        }
    }

    /// Whether a model with `count` registrations is listed. A model whose
    /// clients are only cooling down stays listed.
    fn available(&self, count: i64) -> bool {
        let effective = (count - self.expired - self.other + self.quota_and_other).max(0);
        effective > 0 || (count > 0 && (self.expired > 0 || self.cooldown > 0) && self.other == 0)
    }
}

fn within_window(mark: Instant, now: Instant) -> bool {
    mark.checked_add(QUOTA_WINDOW).is_none_or(|end| now < end)
}

fn bump(counters: &mut HashMap<String, u64>, key: &str) {
    let counter = counters.entry(key.to_owned()).or_insert(0);
    *counter = counter.wrapping_add(1);
}

/// `model` as a model list for `handler_type` shows it (upstream's
/// `convertModelToMap`). Keys are sorted, as Go writes a map's.
fn model_to_map(model: &ModelInfo, handler_type: &str) -> Map<String, Value> {
    let mut map: BTreeMap<&str, Value> = BTreeMap::new();
    let strings = |values: &[String]| Value::from(values.to_vec());
    match handler_type {
        "openai" => {
            map.insert("id", model.id.clone().into());
            map.insert("object", "model".into());
            map.insert("owned_by", model.owned_by.clone().into());
            if model.created > 0 {
                map.insert("created", model.created.into());
            }
            for (key, value) in [
                ("type", &model.model_type),
                ("display_name", &model.display_name),
                ("version", &model.version),
                ("description", &model.description),
            ] {
                if !value.is_empty() {
                    map.insert(key, value.clone().into());
                }
            }
            for (key, value) in [
                ("context_length", model.context_length),
                ("max_context_length", model.max_context_length),
                ("max_completion_tokens", model.max_completion_tokens),
            ] {
                if value > 0 {
                    map.insert(key, value.into());
                }
            }
            if !model.supported_parameters.is_empty() {
                map.insert("supported_parameters", strings(&model.supported_parameters));
            }
        }
        "claude" => {
            map.insert("id", model.id.clone().into());
            map.insert("object", "model".into());
            map.insert("owned_by", model.owned_by.clone().into());
            if model.created > 0 {
                map.insert("created_at", rfc3339(model.created).into());
            }
            map.insert("type", "model".into());
            let display_name = if model.display_name.is_empty() {
                &model.id
            } else {
                &model.display_name
            };
            map.insert("display_name", display_name.clone().into());
            let or_default = |value: u64, default: u64| if value > 0 { value } else { default };
            map.insert(
                "max_input_tokens",
                or_default(model.context_length, DEFAULT_CLAUDE_MAX_INPUT_TOKENS).into(),
            );
            map.insert(
                "max_tokens",
                or_default(
                    model.max_completion_tokens,
                    DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS,
                )
                .into(),
            );
        }
        "gemini" => {
            let name = if model.name.is_empty() {
                &model.id
            } else {
                &model.name
            };
            map.insert("name", name.clone().into());
            for (key, value) in [
                ("version", &model.version),
                ("displayName", &model.display_name),
                ("description", &model.description),
            ] {
                if !value.is_empty() {
                    map.insert(key, value.clone().into());
                }
            }
            for (key, value) in [
                ("inputTokenLimit", model.input_token_limit),
                ("outputTokenLimit", model.output_token_limit),
            ] {
                if value > 0 {
                    map.insert(key, value.into());
                }
            }
            for (key, values) in [
                (
                    "supportedGenerationMethods",
                    &model.supported_generation_methods,
                ),
                (
                    "supportedInputModalities",
                    &model.supported_input_modalities,
                ),
                (
                    "supportedOutputModalities",
                    &model.supported_output_modalities,
                ),
            ] {
                if !values.is_empty() {
                    map.insert(key, strings(values));
                }
            }
        }
        _ => {
            map.insert("id", model.id.clone().into());
            map.insert("object", "model".into());
            if !model.owned_by.is_empty() {
                map.insert("owned_by", model.owned_by.clone().into());
            }
            if !model.model_type.is_empty() {
                map.insert("type", model.model_type.clone().into());
            }
            if model.created != 0 {
                map.insert("created", model.created.into());
            }
        }
    }
    map.into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Unix seconds as Go's `time.RFC3339` writes them in UTC, such as
/// `2026-02-18T00:00:00Z`.
fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // Go writes a negative year as `-` and at least four digits.
    let year = if year < 0 {
        format!("-{:04}", year.unsigned_abs())
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, after Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Both are small and positive.
    (year, month as u32, day as u32)
}
