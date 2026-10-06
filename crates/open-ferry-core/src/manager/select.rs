// Ported from CLIProxyAPI sdk/cliproxy/auth/scheduler.go, selector.go (the
// round-robin, fill-first and weighted selectors and the availability
// checks), conductor_selection.go (pickNextMixed and its legacy path) and
// the unavailable errors in errors.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Picking a credential for a call.
//!
//! The usual path is upstream's scheduler: for each provider and model, the
//! ready credentials grouped by priority, picked from the highest priority
//! by round robin, fill first or smooth weighted round robin. When a
//! credential routes the model under another name (a prefix or an alias),
//! the legacy path checks each credential against its own name for the
//! model instead.
//!
//! Deviations from upstream:
//! - The scheduler isn't a cache of the credentials' states. Each pick
//!   works them out there and then and keeps only the rotation cursors.
//!   The cursors are reconciled as upstream's rebuild does whenever the
//!   manager changes a credential, so a cooldown that starts and ends
//!   between two picks still drops the weighted credit upstream drops, and
//!   again when a pick finds the entries changed by time alone (a cooldown
//!   that ran out).
//! - Model states are checked in key order, where Go's map order is random.
//! - The scheduler's cursor maps are capped at 4096 keys and cleared when
//!   full; upstream's grow without bound.
//! - Plugin schedulers, session affinity and required auth kinds aren't
//!   ported. The only eligibility filter is the free-plan rule (see
//!   `policy`); a credential policy has its own pick.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::Entry;
use super::classify::{auth_error_text, has_unauthorized_auth_failure};
use super::credential::{is_zero, priority, websockets_enabled, weight};
use super::models::{Resolver, openai_compatible_provider_key};
use super::policy::Eligibility;
use super::settings::RoutingStrategy;
use super::summary::extract_upstream_error_summary;
use super::text::{canonical_model_key, equal_fold, go_lower, parse_suffix};
use crate::auth::{Auth, ModelState, Status, Timestamp};
use crate::exec::{ErrorKind, ExecError};
use crate::executor::ProviderExecutor;
use crate::registry::ModelRegistry;

pub(crate) use crate::executor::CLOSE_ALL_EXECUTION_SESSIONS;

/// The most keys a cursor map holds before it is cleared.
pub(super) const MAX_CURSOR_KEYS: usize = 4096;

/// The most entries a weighted state keeps for credentials outside the
/// candidates (upstream's `maxSmoothWeightedStateEntries`).
pub(super) const MAX_SMOOTH_WEIGHTED_STATE_ENTRIES: usize = 1024;

/// What the manager needs from the model registry: the models each
/// credential serves, and somewhere to publish their availability.
pub trait ClientModels: Send + Sync + 'static {
    /// The IDs of the models registered for credential `client_id`.
    fn models_for_client(&self, client_id: &str) -> Vec<String>;

    /// The models registered for `client_id`, with the registration epoch
    /// they belong to.
    fn models_and_epoch_for_client(&self, client_id: &str) -> (Vec<String>, u64) {
        (self.models_for_client(client_id), 0)
    }

    /// The registration epoch of `client_id`'s models (upstream's
    /// `ClientRegistrationEpoch`).
    fn client_registration_epoch(&self, client_id: &str) -> u64 {
        self.models_and_epoch_for_client(client_id).1
    }

    /// Whether `client_id` serves `model_id` (upstream's
    /// `ClientSupportsModel`).
    fn client_supports_model(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        self.models_for_client(client_id)
            .iter()
            .any(|id| equal_fold(id.trim(), model_id))
    }

    /// Publishes whether each of `client_id`'s models is usable, for the
    /// credential state at `generation` under registration `epoch`. Returns
    /// whether the registry took them.
    fn apply_client_model_projections(
        &self,
        client_id: &str,
        epoch: u64,
        generation: u64,
        projections: &[ModelProjection],
    ) -> bool {
        let _ = (client_id, epoch, generation, projections);
        false
    }
}

impl ClientModels for ModelRegistry {
    fn models_for_client(&self, client_id: &str) -> Vec<String> {
        ClientModels::models_and_epoch_for_client(self, client_id).0
    }

    fn models_and_epoch_for_client(&self, client_id: &str) -> (Vec<String>, u64) {
        let (models, epoch) = ModelRegistry::models_and_epoch_for_client(self, client_id);
        (models.into_iter().map(|model| model.id).collect(), epoch)
    }

    fn client_registration_epoch(&self, client_id: &str) -> u64 {
        ModelRegistry::client_registration_epoch(self, client_id)
    }

    fn client_supports_model(&self, client_id: &str, model_id: &str) -> bool {
        ModelRegistry::client_supports_model(self, client_id, model_id)
    }

    fn apply_client_model_projections(
        &self,
        client_id: &str,
        epoch: u64,
        generation: u64,
        projections: &[ModelProjection],
    ) -> bool {
        ModelRegistry::apply_client_model_projections(
            self,
            client_id,
            epoch,
            generation,
            projections,
        )
    }
}

/// Whether a credential can serve one model now (upstream's
/// `ClientModelProjection`).
pub use crate::registry::ClientModelProjection as ModelProjection;

/// Why a credential is blocked for a model (upstream's `blockReason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockReason {
    None,
    Cooldown,
    Disabled,
    Other,
}

fn after(time: Option<Timestamp>, now: Timestamp) -> bool {
    time.is_some_and(|t| t > now)
}

/// Whether `auth` is blocked for `model`, why, and until when (upstream's
/// `isAuthBlockedForModel`).
pub(crate) fn is_auth_blocked_for_model(
    auth: &Auth,
    model: &str,
    now: Timestamp,
) -> (bool, BlockReason, Option<Timestamp>) {
    if auth.disabled || auth.status == Status::Disabled {
        return (true, BlockReason::Disabled, None);
    }
    if has_unauthorized_auth_failure(auth) {
        return (true, BlockReason::Other, None);
    }
    if let Some(exp) = auth.access_token_expiration_time()
        && !is_zero(Some(exp))
        && exp <= now
    {
        return (true, BlockReason::Other, None);
    }
    if auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && after(auth.quota.next_recover_at, now)
    {
        return (true, BlockReason::Cooldown, auth.quota.next_recover_at);
    }
    if !model.is_empty() {
        if auth.model_states.is_empty() {
            return availability_block(
                auth.unavailable,
                auth.quota.exceeded,
                auth.next_retry_after,
                auth.quota.next_recover_at,
                now,
            );
        }
        let key = canonical_model_key(model);
        let mut matched = false;
        let mut blocked = false;
        let mut reason = BlockReason::None;
        let mut next_retry: Option<Timestamp> = None;
        for (name, state) in &auth.model_states {
            if canonical_model_key(name) != key {
                continue;
            }
            matched = true;
            if state.status == Status::Disabled {
                return (true, BlockReason::Disabled, None);
            }
            let (state_blocked, state_reason, next) = availability_block(
                state.unavailable,
                state.quota.exceeded,
                state.next_retry_after,
                state.quota.next_recover_at,
                now,
            );
            if !state_blocked {
                continue;
            }
            let Some(next) = next else {
                return (true, state_reason, None);
            };
            if !blocked
                || Some(next) > next_retry
                || (Some(next) == next_retry && state_reason == BlockReason::Cooldown)
            {
                blocked = true;
                reason = state_reason;
                next_retry = Some(next);
            }
        }
        if matched {
            return (blocked, reason, next_retry);
        }
        return (false, BlockReason::None, None);
    }
    let mut quota_exceeded = auth.quota.exceeded;
    if !auth.model_states.is_empty() && auth.quota.reason != "credential_quota" && !auth.unavailable
    {
        quota_exceeded = false;
    }
    availability_block(
        auth.unavailable,
        quota_exceeded,
        auth.next_retry_after,
        auth.quota.next_recover_at,
        now,
    )
}

/// Upstream's `availabilityBlock`.
pub(crate) fn availability_block(
    unavailable: bool,
    quota_exceeded: bool,
    next_retry_after: Option<Timestamp>,
    next_recover_at: Option<Timestamp>,
    now: Timestamp,
) -> (bool, BlockReason, Option<Timestamp>) {
    if !unavailable && !quota_exceeded {
        return (false, BlockReason::None, None);
    }
    let has_recovery_time = !is_zero(next_retry_after) || !is_zero(next_recover_at);
    let mut next: Option<Timestamp> = None;
    for candidate in [next_retry_after, next_recover_at].into_iter().flatten() {
        if candidate > now && next.is_none_or(|n| candidate > n) {
            next = Some(candidate);
        }
    }
    if let Some(next) = next {
        let reason = if quota_exceeded {
            BlockReason::Cooldown
        } else {
            BlockReason::Other
        };
        return (true, reason, Some(next));
    }
    if has_recovery_time {
        return (false, BlockReason::None, None);
    }
    (true, BlockReason::Other, None)
}

/// Upstream's `canonicalSchedulingProvider`.
pub(crate) fn canonical_scheduling_provider(key: &str) -> String {
    let lower = go_lower(key.trim());
    match lower.as_str() {
        "kimi.com" => "kimi".into(),
        "kimi.ai" => "kimi-ai".into(),
        _ => lower,
    }
}

/// The executor a credential runs on (upstream's `executorKeyFromAuth`).
pub(crate) fn executor_key_from_auth(auth: &Auth) -> String {
    let compat_name = auth.attributes.get("compat_name").map_or("", |v| v.trim());
    if !compat_name.is_empty() {
        let provider_key = auth.attributes.get("provider_key").map_or("", |v| v.trim());
        let key = if provider_key.is_empty() {
            compat_name
        } else {
            provider_key
        };
        return openai_compatible_provider_key(key);
    }
    if equal_fold(auth.provider.trim(), "openai-compatibility") {
        let label = auth.label.trim();
        let key = if label.is_empty() {
            "openai-compatibility"
        } else {
            label
        };
        return openai_compatible_provider_key(key);
    }
    let provider = go_lower(auth.provider.trim());
    match provider.as_str() {
        "kimi.com" => "kimi".into(),
        "kimi.ai" => "kimi-ai".into(),
        _ => provider,
    }
}

/// The executor registered for a provider (upstream's `executorLocked`).
pub(crate) fn lookup_executor(
    executors: &HashMap<String, Arc<dyn ProviderExecutor>>,
    provider: &str,
) -> Option<Arc<dyn ProviderExecutor>> {
    let provider = provider.trim();
    if provider.is_empty() {
        return None;
    }
    if let Some(executor) = executors.get(provider) {
        return Some(executor.clone());
    }
    let lower = go_lower(provider);
    if lower != provider
        && let Some(executor) = executors.get(&lower)
    {
        return Some(executor.clone());
    }
    match lower.as_str() {
        "kimi-ai" | "kimi.ai" | "kimi.com" => executors.get("kimi").cloned(),
        _ => None,
    }
}

/// The call's providers, lower-cased, trimmed and deduplicated (upstream's
/// `normalizeProviders`).
pub(crate) fn normalize_providers(providers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(providers.len());
    for provider in providers {
        let p = go_lower(provider.trim());
        if p.is_empty() || out.contains(&p) {
            continue;
        }
        out.push(p);
    }
    out
}

/// Upstream's `normalizeProviderKeys`.
fn normalize_provider_keys(providers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(providers.len());
    for provider in providers {
        let key = canonical_scheduling_provider(provider);
        if key.is_empty() || out.contains(&key) {
            continue;
        }
        out.push(key);
    }
    out
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The summary of a cause, when there is one to give.
fn cause_summary(cause: Option<&str>) -> Option<String> {
    let summary = extract_upstream_error_summary(cause?);
    (!summary.is_empty()).then_some(summary)
}

fn with_cause(mut err: ExecError, cause: Option<&str>) -> ExecError {
    if let Some(summary) = cause_summary(cause) {
        err.cause = Some(summary);
    }
    err
}

/// `auth_not_found` with the last candidate error behind it.
pub(crate) fn auth_not_found_with_cause(cause: Option<&str>) -> ExecError {
    with_cause(ExecError::auth_not_found(), cause)
}

/// Upstream's `newAuthUnavailableErrorWithCause`: retryable with a wait
/// when a credential frees up later, plain otherwise.
pub(crate) fn auth_unavailable_with_cause(
    next: Option<Timestamp>,
    now: Timestamp,
    cause: Option<&str>,
) -> ExecError {
    let err = match next {
        Some(next) if next > now => {
            ExecError::auth_unavailable((next - now).to_std().unwrap_or(Duration::ZERO))
        }
        _ => ExecError::new(ErrorKind::AuthUnavailable, "no auth available"),
    };
    with_cause(err, cause)
}

/// Every candidate's credential was rejected: the client must sign in again
/// (upstream's `NewTerminalAuthError`).
fn terminal_auth_unavailable(cause: Option<&str>) -> ExecError {
    let err = ExecError::new(ErrorKind::AuthUnavailable, "no auth available")
        .with_status(503)
        .with_terminal_auth();
    with_cause(err, cause)
}

/// Every candidate is cooling down (upstream's
/// `newModelCooldownErrorWithCause`).
fn model_cooldown_with_cause(
    model: &str,
    provider: &str,
    next: Timestamp,
    now: Timestamp,
    cause: Option<&str>,
) -> ExecError {
    let reset_in = (next - now).to_std().unwrap_or(Duration::ZERO);
    let summary = cause_summary(cause);
    ExecError::model_cooldown(model, Some(provider), reset_in, summary.as_deref())
}

/// One candidate error and when it happened.
#[derive(Clone, Debug)]
struct Latest {
    time: Option<Timestamp>,
    id: String,
    text: String,
}

impl Latest {
    fn offer(slot: &mut Option<Latest>, candidate: Latest) {
        let newer = match slot {
            None => true,
            Some(current) => {
                candidate.time > current.time
                    || (candidate.time == current.time && candidate.id > current.id)
            }
        };
        if newer {
            *slot = Some(candidate);
        }
    }
}

/// The latest model and credential errors among candidates (upstream's
/// `candidateErrorsLocked` and `latestCandidateErrorForModel`).
#[derive(Clone, Debug, Default)]
struct CandidateErrors {
    model: Option<Latest>,
    auth: Option<Latest>,
}

impl CandidateErrors {
    /// Takes in a candidate, whose model error is in its state for `model`,
    /// else for the model's canonical key.
    fn offer(&mut self, auth: &Auth, model: &str) {
        if !auth.model_states.is_empty() {
            let state = auth
                .model_states
                .get(model)
                .or_else(|| auth.model_states.get(&canonical_model_key(model)));
            if let Some((text, time)) = state.and_then(state_error) {
                Latest::offer(
                    &mut self.model,
                    Latest {
                        time: time.or(auth.updated_at),
                        id: auth.id.clone(),
                        text,
                    },
                );
            }
        }
        let auth_err = match &auth.last_error {
            Some(err) => Some(auth_error_text(err)),
            None if !auth.status_message.trim().is_empty() => Some(auth.status_message.clone()),
            None => None,
        };
        if let Some(text) = auth_err {
            Latest::offer(
                &mut self.auth,
                Latest {
                    time: auth.updated_at,
                    id: auth.id.clone(),
                    text,
                },
            );
        }
    }

    fn merge(&mut self, other: CandidateErrors) {
        if let Some(model) = other.model {
            Latest::offer(&mut self.model, model);
        }
        if let Some(auth) = other.auth {
            Latest::offer(&mut self.auth, auth);
        }
    }

    /// The model error, else the credential error.
    fn last(&self) -> Option<&str> {
        self.model
            .as_ref()
            .or(self.auth.as_ref())
            .map(|latest| latest.text.as_str())
    }

    fn auth_text(&self) -> Option<&str> {
        self.auth.as_ref().map(|latest| latest.text.as_str())
    }
}

fn state_error(state: &ModelState) -> Option<(String, Option<Timestamp>)> {
    if let Some(err) = &state.last_error {
        return Some((auth_error_text(err), state.updated_at));
    }
    if !state.status_message.trim().is_empty() {
        return Some((state.status_message.clone(), state.updated_at));
    }
    None
}

/// Counts over the candidates a pick could have used.
#[derive(Clone, Copy, Debug, Default)]
struct Summary {
    total: usize,
    cooldown: usize,
    unauthorized: usize,
    earliest: Option<Timestamp>,
}

impl Summary {
    fn merge(&mut self, other: Summary) {
        self.total += other.total;
        self.cooldown += other.cooldown;
        self.unauthorized += other.unauthorized;
        if let Some(next) = other.earliest
            && self.earliest.is_none_or(|e| next < e)
        {
            self.earliest = Some(next);
        }
    }

    /// The error for a pick that found nothing (upstream's
    /// `unavailableErrorLocked`).
    fn error(
        self,
        errors: &CandidateErrors,
        model: &str,
        provider: &str,
        now: Timestamp,
    ) -> ExecError {
        let last = errors.last();
        if self.total == 0 {
            return auth_not_found_with_cause(last);
        }
        if self.cooldown == self.total
            && let Some(earliest) = self.earliest
        {
            let provider = if provider == "mixed" { "" } else { provider };
            return model_cooldown_with_cause(model, provider, earliest, now, last);
        }
        if self.unauthorized == self.total {
            return terminal_auth_unavailable(errors.auth_text().or(last));
        }
        auth_unavailable_with_cause(self.earliest, now, last)
    }
}

// ---------------------------------------------------------------------------
// Smooth weighted round robin
// ---------------------------------------------------------------------------

/// Credits for smooth weighted round robin (upstream's
/// `smoothWeightedState`).
#[derive(Clone, Debug, Default)]
pub(crate) struct SmoothWeighted {
    pub(super) current: Option<HashMap<String, i64>>,
    pub(super) weights: HashMap<String, i64>,
}

impl SmoothWeighted {
    /// Takes in the candidates' weights, keeping credits unless a weight
    /// changed (upstream's `prepare`).
    pub(super) fn prepare(&mut self, weights: &HashMap<String, i64>) {
        if self.current.is_none() || weights_config_changed(&self.weights, weights) {
            self.current = Some(HashMap::with_capacity(weights.len()));
        }
        for (id, weight) in weights {
            self.weights.insert(id.clone(), *weight);
        }
        let current_len = self.current.as_ref().map_or(0, HashMap::len);
        if current_len <= MAX_SMOOTH_WEIGHTED_STATE_ENTRIES
            && self.weights.len() <= MAX_SMOOTH_WEIGHTED_STATE_ENTRIES
        {
            return;
        }
        if let Some(current) = &mut self.current {
            current.retain(|id, _| weights.contains_key(id));
        }
        self.weights.retain(|id, _| weights.contains_key(id));
    }

    /// Picks among `(id, weight)` candidates in order (upstream's
    /// `pickSmoothWeightedAuth`).
    pub(super) fn pick<'e>(
        &mut self,
        candidates: impl Iterator<Item = (&'e str, i64)>,
    ) -> Option<&'e str> {
        let current = self.current.get_or_insert_with(HashMap::new);
        let mut picked: Option<(&str, i64)> = None;
        let mut total: i64 = 0;
        for (id, weight) in candidates {
            if weight <= 0 {
                continue;
            }
            let credit = current.entry(id.to_owned()).or_insert(0);
            *credit = credit.saturating_add(weight);
            total = total.saturating_add(weight);
            if picked.is_none_or(|(_, best)| *credit > best) {
                picked = Some((id, *credit));
            }
        }
        let (id, _) = picked?;
        if let Some(credit) = current.get_mut(id) {
            *credit = credit.saturating_sub(total);
        }
        Some(id)
    }
}

/// Whether a credential in both vectors changed weight (upstream's
/// `weightsConfigChanged`).
fn weights_config_changed(left: &HashMap<String, i64>, right: &HashMap<String, i64>) -> bool {
    if left.is_empty() {
        return false;
    }
    right
        .iter()
        .any(|(id, weight)| left.get(id).is_some_and(|previous| previous != weight))
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// A scheduled credential's state for a model (upstream's
/// `scheduledState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchedState {
    Ready,
    Cooldown,
    Disabled,
    Blocked,
}

/// One credential in a provider and model's shard.
struct Sched<'a> {
    auth: &'a Arc<Auth>,
    provider: String,
    state: SchedState,
    next: Option<Timestamp>,
    priority: i64,
    weight: i64,
    ws: bool,
}

/// What a shard rebuild depends on, per entry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Signature {
    id: String,
    state: SchedState,
    next: Option<Timestamp>,
    priority: i64,
    ws: bool,
}

impl Signature {
    fn of(entry: &Sched<'_>) -> Self {
        Self {
            id: entry.auth.id.clone(),
            state: entry.state,
            next: entry.next,
            priority: entry.priority,
            ws: entry.ws,
        }
    }
}

#[derive(Debug, Default)]
struct ViewCursor {
    last_picked: String,
    weighted: SmoothWeighted,
}

#[derive(Debug, Default)]
struct BucketCursors {
    all: ViewCursor,
    ws: ViewCursor,
}

#[derive(Debug, Default)]
pub(super) struct ShardCursors {
    signature: Vec<Signature>,
    buckets: HashMap<i64, BucketCursors>,
}

/// The rotation state picks keep between calls.
#[derive(Debug, Default)]
pub(crate) struct SelectorState {
    pub(super) shards: HashMap<(String, String), ShardCursors>,
    mixed_cursors: HashMap<String, usize>,
    mixed_weighted: HashMap<String, SmoothWeighted>,
    legacy_last_picked: HashMap<String, String>,
    legacy_weighted: HashMap<String, SmoothWeighted>,
}

impl SelectorState {
    /// Forgets the state a routing strategy change replaces: the
    /// mixed-provider cursors and the legacy selector's.
    pub(crate) fn reset_strategy(&mut self) {
        self.mixed_cursors.clear();
        self.mixed_weighted.clear();
        self.legacy_last_picked.clear();
        self.legacy_weighted.clear();
    }

    /// Forgets what upstream's scheduler rebuild forgets: the per-shard and
    /// mixed-provider cursors. The legacy selector's state stays.
    pub(crate) fn reset_scheduler(&mut self) {
        self.shards.clear();
        self.mixed_cursors.clear();
        self.mixed_weighted.clear();
    }

    fn shard(&mut self, provider: &str, model_key: &str) -> &mut ShardCursors {
        let key = (provider.to_owned(), model_key.to_owned());
        if !self.shards.contains_key(&key) && self.shards.len() >= MAX_CURSOR_KEYS {
            self.shards.clear();
        }
        self.shards.entry(key).or_default()
    }
}

impl super::State {
    /// Upstream's scheduler update after the manager changed credential
    /// `id` (see [`Selection::sync_auth`]). Call it with the lock held,
    /// after the change.
    pub(crate) fn sync_scheduler(&mut self, models: &dyn ClientModels, id: &str, now: Timestamp) {
        let selection = Selection {
            auths: &self.auths,
            executors: &self.executors,
            models,
            resolver: Resolver {
                settings: &self.settings,
                oauth: &self.oauth,
            },
            strategy: self.settings.routing_strategy,
            now,
        };
        selection.sync_auth(&mut self.selector, id);
    }
}

fn capped<'m, V: Default>(map: &'m mut HashMap<String, V>, key: &str) -> &'m mut V {
    if !map.contains_key(key) && map.len() >= MAX_CURSOR_KEYS {
        map.clear();
    }
    map.entry(key.to_owned()).or_default()
}

/// The ready entries of one priority, all or only those with WebSockets.
fn view<'s, 'a>(shard: &'s [Sched<'a>], priority: i64, ws: bool) -> Vec<&'s Sched<'a>> {
    shard
        .iter()
        .filter(|e| e.state == SchedState::Ready && e.priority == priority && (!ws || e.ws))
        .collect()
}

fn weight_vector(
    flat: &[&Sched<'_>],
    predicate: Option<&dyn Fn(&Sched<'_>) -> bool>,
) -> HashMap<String, i64> {
    flat.iter()
        .filter(|e| e.weight > 0 && predicate.is_none_or(|p| p(e)))
        .map(|e| (e.auth.id.clone(), e.weight))
        .collect()
}

/// Upstream's `restoreReadyViewCursors`.
fn restore(cursor: &mut ViewCursor, flat: &[&Sched<'_>]) {
    let weights = weight_vector(flat, None);
    let keep = cursor
        .weighted
        .current
        .as_ref()
        .is_some_and(|c| !c.is_empty())
        && !weights_config_changed(&cursor.weighted.weights, &weights);
    if !keep {
        cursor.weighted = SmoothWeighted::default();
        return;
    }
    let previous = cursor.weighted.current.take().unwrap_or_default();
    let current = flat
        .iter()
        .filter_map(|e| {
            previous
                .get(&e.auth.id)
                .map(|credit| (e.auth.id.clone(), *credit))
        })
        .collect();
    cursor.weighted.current = Some(current);
    cursor.weighted.weights = weights;
}

impl ShardCursors {
    /// Reconciles the cursors with the shard's entries, as upstream's
    /// `rebuildIndexesLocked` does when an entry changes.
    fn sync(&mut self, shard: &[Sched<'_>]) {
        let signature: Vec<Signature> = shard.iter().map(Signature::of).collect();
        if signature == self.signature {
            return;
        }
        self.signature = signature;
        let ready: HashSet<i64> = shard
            .iter()
            .filter(|e| e.state == SchedState::Ready)
            .map(|e| e.priority)
            .collect();
        self.buckets.retain(|priority, _| ready.contains(priority));
        for (priority, bucket) in &mut self.buckets {
            restore(&mut bucket.all, &view(shard, *priority, false));
            restore(&mut bucket.ws, &view(shard, *priority, true));
        }
    }
}

/// Distinct priorities with ready entries, highest first.
fn priority_order(shard: &[Sched<'_>]) -> Vec<i64> {
    let mut order: Vec<i64> = shard
        .iter()
        .filter(|e| e.state == SchedState::Ready)
        .map(|e| e.priority)
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    order.dedup();
    order
}

/// Upstream's `highestReadyPriorityLocked`.
fn highest_ready_priority(
    shard: &[Sched<'_>],
    prefer_ws: bool,
    predicate: &dyn Fn(&Sched<'_>) -> bool,
) -> Option<i64> {
    let order = priority_order(shard);
    if prefer_ws
        && let Some(priority) = order
            .iter()
            .find(|p| view(shard, **p, true).iter().any(|e| predicate(e)))
    {
        return Some(*priority);
    }
    order
        .into_iter()
        .find(|p| view(shard, *p, false).iter().any(|e| predicate(e)))
}

/// The view a pick at `priority` uses, and whether it is the WebSocket one.
fn pick_view<'s, 'a>(
    shard: &'s [Sched<'a>],
    prefer_ws: bool,
    priority: i64,
    predicate: &dyn Fn(&Sched<'_>) -> bool,
) -> (Vec<&'s Sched<'a>>, bool) {
    if prefer_ws {
        let ws = view(shard, priority, true);
        if ws.iter().any(|e| predicate(e)) {
            return (ws, true);
        }
    }
    (view(shard, priority, false), false)
}

/// Upstream's `scheduledSuccessorIndex`.
pub(super) fn successor_index(ids: &[&str], last: &str) -> usize {
    if last.is_empty() {
        return 0;
    }
    ids.iter().position(|id| *id > last).unwrap_or(0)
}

/// Upstream's `pickReadyAtPriorityLocked`.
fn pick_ready_at_priority<'s, 'a>(
    shard: &'s [Sched<'a>],
    cursors: &mut ShardCursors,
    prefer_ws: bool,
    priority: i64,
    strategy: RoutingStrategy,
    predicate: &dyn Fn(&Sched<'_>) -> bool,
) -> Option<&'s Sched<'a>> {
    let (flat, ws) = pick_view(shard, prefer_ws, priority, predicate);
    if flat.is_empty() {
        return None;
    }
    let bucket = cursors.buckets.entry(priority).or_default();
    let cursor = if ws { &mut bucket.ws } else { &mut bucket.all };
    match strategy {
        RoutingStrategy::FillFirst => flat.into_iter().find(|e| predicate(e)),
        RoutingStrategy::Weighted => {
            cursor
                .weighted
                .prepare(&weight_vector(&flat, Some(predicate)));
            let id = cursor.weighted.pick(
                flat.iter()
                    .filter(|e| predicate(e))
                    .map(|e| (e.auth.id.as_str(), e.weight)),
            )?;
            flat.into_iter().find(|e| e.auth.id == id)
        }
        RoutingStrategy::RoundRobin => {
            let ids: Vec<&str> = flat.iter().map(|e| e.auth.id.as_str()).collect();
            let start = successor_index(&ids, &cursor.last_picked);
            for offset in 0..flat.len() {
                let entry = flat[(start + offset) % flat.len()];
                if predicate(entry) {
                    cursor.last_picked = entry.auth.id.clone();
                    return Some(entry);
                }
            }
            None
        }
    }
}

/// Upstream's `readyCountAtPriorityLocked`.
fn ready_count_at_priority(
    shard: &[Sched<'_>],
    priority: i64,
    predicate: &dyn Fn(&Sched<'_>) -> bool,
) -> usize {
    view(shard, priority, false)
        .iter()
        .filter(|e| predicate(e))
        .count()
}

/// Counts and errors over a shard's candidates (upstream's
/// `availabilitySummaryLocked` and `candidateErrorsLocked`).
fn shard_summary(
    shard: &[Sched<'_>],
    model: &str,
    predicate: &dyn Fn(&Sched<'_>) -> bool,
) -> (Summary, CandidateErrors) {
    let mut summary = Summary::default();
    let mut errors = CandidateErrors::default();
    for entry in shard.iter().filter(|e| predicate(e)) {
        summary.total += 1;
        if has_unauthorized_auth_failure(entry.auth) {
            summary.unauthorized += 1;
        }
        if entry.state == SchedState::Cooldown {
            summary.cooldown += 1;
        }
        if matches!(entry.state, SchedState::Cooldown | SchedState::Blocked)
            && let Some(next) = entry.next
            && summary.earliest.is_none_or(|e| next < e)
        {
            summary.earliest = Some(next);
        }
        errors.offer(entry.auth, model);
    }
    (summary, errors)
}

/// What a pick needs to know about the call.
pub(crate) struct PickArgs<'a> {
    /// The route model.
    pub(crate) model: &'a str,
    /// The credential the call is pinned to, trimmed, or empty.
    pub(crate) pinned: &'a str,
    /// Whether the client is on a WebSocket.
    pub(crate) downstream_websocket: bool,
    /// Credentials already tried.
    pub(crate) tried: &'a HashSet<String>,
    /// What else narrows the credentials the call may pick.
    pub(crate) eligibility: Eligibility,
}

/// A picked credential with its executor.
pub(crate) struct Picked {
    pub(crate) auth: Arc<Auth>,
    pub(crate) executor: Arc<dyn ProviderExecutor>,
    pub(crate) provider: String,
}

/// The manager state a pick reads.
pub(crate) struct Selection<'a> {
    pub(crate) auths: &'a BTreeMap<String, Entry>,
    pub(crate) executors: &'a HashMap<String, Arc<dyn ProviderExecutor>>,
    pub(crate) models: &'a dyn ClientModels,
    pub(crate) resolver: Resolver<'a>,
    pub(crate) strategy: RoutingStrategy,
    pub(crate) now: Timestamp,
}

fn schedulable(auth: &Auth) -> bool {
    !auth.disabled && auth.status != Status::Disabled && !auth.id.trim().is_empty()
}

impl<'a> Selection<'a> {
    /// The shard for a provider and model: the schedulable credentials on
    /// that executor that serve the model, by ID.
    fn shard(&self, provider: &str, model_key: &str) -> Vec<Sched<'a>> {
        let mut out = Vec::new();
        for entry in self.auths.values() {
            let auth = &entry.auth;
            if !schedulable(auth) || executor_key_from_auth(auth) != provider {
                continue;
            }
            let models = if model_key.is_empty() {
                Vec::new()
            } else {
                self.models.models_for_client(&auth.id)
            };
            out.extend(self.sched(auth, provider, model_key, &models));
        }
        out
    }

    /// Credential `auth`'s entry in the shard for `provider` and
    /// `model_key`, given the models the registry lists for it, or `None`
    /// when it isn't in that shard.
    fn sched(
        &self,
        auth: &'a Arc<Auth>,
        provider: &str,
        model_key: &str,
        models: &[String],
    ) -> Option<Sched<'a>> {
        if provider.is_empty() || !schedulable(auth) || executor_key_from_auth(auth) != provider {
            return None;
        }
        if !model_key.is_empty() && !models.iter().any(|m| canonical_model_key(m) == model_key) {
            return None;
        }
        let (blocked, reason, next) = is_auth_blocked_for_model(auth, model_key, self.now);
        let (state, next) = match (blocked, reason) {
            (false, _) => (SchedState::Ready, None),
            (true, BlockReason::Cooldown) => (SchedState::Cooldown, next),
            (true, BlockReason::Disabled) => (SchedState::Disabled, None),
            (true, _) => (SchedState::Blocked, next),
        };
        Some(Sched {
            auth,
            provider: provider.to_owned(),
            state,
            next,
            priority: priority(auth),
            weight: weight(auth),
            ws: websockets_enabled(auth),
        })
    }

    /// Reconciles the cursors of each shard credential `id` is or was in
    /// after it changed, as upstream's scheduler rebuilds a shard when one
    /// of its entries changes (`upsertEntryLocked`, `removeEntryLocked`).
    /// Shards where the credential's entry is unchanged are left alone.
    pub(crate) fn sync_auth(&self, state: &mut SelectorState, id: &str) {
        let auth = self.auths.get(id).map(|entry| &entry.auth);
        let provider = auth
            .map(|auth| executor_key_from_auth(auth))
            .unwrap_or_default();
        let models = match auth {
            Some(auth) if !provider.is_empty() && schedulable(auth) => {
                self.models.models_for_client(id)
            }
            _ => Vec::new(),
        };
        let stale: Vec<(String, String)> = state
            .shards
            .iter()
            .filter(|((shard_provider, model_key), cursors)| {
                let old = cursors.signature.iter().find(|s| s.id == id);
                let new = auth
                    .filter(|_| *shard_provider == provider)
                    .and_then(|auth| self.sched(auth, &provider, model_key, &models))
                    .map(|entry| Signature::of(&entry));
                old != new.as_ref()
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            let shard = self.shard(&key.0, &key.1);
            if let Some(cursors) = state.shards.get_mut(&key) {
                cursors.sync(&shard);
            }
        }
    }

    fn predicate<'p>(
        &self,
        args: &'p PickArgs<'p>,
        pinned: &'p str,
    ) -> impl Fn(&Sched<'_>) -> bool + 'p {
        let require_weight = self.strategy == RoutingStrategy::Weighted;
        move |entry: &Sched<'_>| {
            if !args.eligibility.allows(entry.auth) {
                return false;
            }
            if require_weight && entry.weight <= 0 {
                return false;
            }
            if !pinned.is_empty() && entry.auth.id != pinned {
                return false;
            }
            !args.tried.contains(&entry.auth.id)
        }
    }

    /// Upstream's `pickSingle`.
    fn pick_single(
        &self,
        state: &mut SelectorState,
        provider: &str,
        args: &PickArgs<'_>,
    ) -> Result<Arc<Auth>, ExecError> {
        let provider_key = canonical_scheduling_provider(provider);
        let model_key = canonical_model_key(args.model);
        let prefer_ws = args.downstream_websocket
            && matches!(provider_key.as_str(), "codex" | "xai")
            && args.pinned.is_empty();
        let shard = self.shard(&provider_key, &model_key);
        let cursors = state.shard(&provider_key, &model_key);
        cursors.sync(&shard);
        let predicate = self.predicate(args, args.pinned);
        if let Some(priority) = highest_ready_priority(&shard, prefer_ws, &predicate)
            && let Some(picked) = pick_ready_at_priority(
                &shard,
                cursors,
                prefer_ws,
                priority,
                self.strategy,
                &predicate,
            )
        {
            return Ok(picked.auth.clone());
        }
        let (summary, errors) = shard_summary(&shard, args.model, &predicate);
        Err(summary.error(&errors, args.model, provider, self.now))
    }

    /// Upstream's `authScheduler.pickMixed`.
    fn pick_mixed(
        &self,
        state: &mut SelectorState,
        providers: &[String],
        args: &PickArgs<'_>,
    ) -> Result<(Arc<Auth>, String), ExecError> {
        let normalized = normalize_provider_keys(providers);
        if normalized.is_empty() {
            return Err(ExecError::new(
                ErrorKind::ProviderNotFound,
                "no provider supplied",
            ));
        }
        if normalized.len() == 1 {
            let key = normalized[0].clone();
            let picked = self.pick_single(state, &key, args)?;
            return Ok((picked, key));
        }
        let model_key = canonical_model_key(args.model);
        if !args.pinned.is_empty() {
            let provider_key = self
                .auths
                .get(args.pinned)
                .filter(|entry| schedulable(&entry.auth))
                .map(|entry| executor_key_from_auth(&entry.auth))
                .unwrap_or_default();
            if provider_key.is_empty() || !normalized.contains(&provider_key) {
                return Err(ExecError::auth_not_found());
            }
            let shard = self.shard(&provider_key, &model_key);
            let cursors = state.shard(&provider_key, &model_key);
            cursors.sync(&shard);
            let predicate = self.predicate(args, args.pinned);
            if let Some(priority) = highest_ready_priority(&shard, false, &predicate)
                && let Some(picked) = pick_ready_at_priority(
                    &shard,
                    cursors,
                    false,
                    priority,
                    self.strategy,
                    &predicate,
                )
            {
                return Ok((picked.auth.clone(), provider_key));
            }
            let (summary, errors) = shard_summary(&shard, args.model, &predicate);
            return Err(summary.error(&errors, args.model, "mixed", self.now));
        }

        let predicate = self.predicate(args, "");
        let shards: Vec<Vec<Sched<'a>>> = normalized
            .iter()
            .map(|provider| self.shard(provider, &model_key))
            .collect();
        for (provider, shard) in normalized.iter().zip(&shards) {
            state.shard(provider, &model_key).sync(shard);
        }
        let unavailable = || {
            let mut summary = Summary::default();
            let mut errors = CandidateErrors::default();
            for shard in &shards {
                let (s, e) = shard_summary(shard, args.model, &predicate);
                summary.merge(s);
                errors.merge(e);
            }
            summary.error(&errors, args.model, "mixed", self.now)
        };
        let Some(best) = shards
            .iter()
            .filter_map(|shard| highest_ready_priority(shard, false, &predicate))
            .max()
        else {
            return Err(unavailable());
        };

        match self.strategy {
            RoutingStrategy::FillFirst => {
                for (provider, shard) in normalized.iter().zip(&shards) {
                    let cursors = state.shard(provider, &model_key);
                    if let Some(picked) = pick_ready_at_priority(
                        shard,
                        cursors,
                        false,
                        best,
                        self.strategy,
                        &predicate,
                    ) {
                        return Ok((picked.auth.clone(), provider.clone()));
                    }
                }
                Err(unavailable())
            }
            RoutingStrategy::Weighted => {
                let cursor_key = format!("{}:{model_key}", normalized.join(","));
                let mut entries: Vec<&Sched<'a>> = shards
                    .iter()
                    .flat_map(|shard| view(shard, best, false))
                    .collect();
                entries.sort_by(|a, b| a.auth.id.cmp(&b.auth.id));
                let weighted = capped(&mut state.mixed_weighted, &cursor_key);
                weighted.prepare(&weight_vector(&entries, Some(&predicate)));
                let picked = weighted
                    .pick(
                        entries
                            .iter()
                            .filter(|e| predicate(e))
                            .map(|e| (e.auth.id.as_str(), e.weight)),
                    )
                    .and_then(|id| entries.iter().find(|e| e.auth.id == id));
                match picked {
                    Some(entry) => Ok((entry.auth.clone(), entry.provider.clone())),
                    None => Err(unavailable()),
                }
            }
            RoutingStrategy::RoundRobin => {
                let cursor_key = format!("{}:{model_key}", normalized.join(","));
                let weights: Vec<usize> = shards
                    .iter()
                    .map(|shard| ready_count_at_priority(shard, best, &predicate))
                    .collect();
                let mut starts = Vec::with_capacity(weights.len());
                let mut ends = Vec::with_capacity(weights.len());
                let mut total = 0usize;
                for weight in &weights {
                    starts.push(total);
                    total += weight;
                    ends.push(total);
                }
                if total == 0 {
                    return Err(unavailable());
                }
                let start_slot = state.mixed_cursors.get(&cursor_key).copied().unwrap_or(0) % total;
                let Some(start) =
                    (0..weights.len()).find(|i| weights[*i] > 0 && start_slot < ends[*i])
                else {
                    return Err(unavailable());
                };
                let mut slot = start_slot;
                for offset in 0..normalized.len() {
                    let index = (start + offset) % normalized.len();
                    if weights[index] == 0 {
                        continue;
                    }
                    if index != start {
                        slot = starts[index];
                    }
                    let provider = &normalized[index];
                    let cursors = state.shard(provider, &model_key);
                    let Some(picked) = pick_ready_at_priority(
                        &shards[index],
                        cursors,
                        false,
                        best,
                        RoutingStrategy::RoundRobin,
                        &predicate,
                    ) else {
                        continue;
                    };
                    *capped(&mut state.mixed_cursors, &cursor_key) = slot + 1;
                    return Ok((picked.auth.clone(), provider.clone()));
                }
                Err(unavailable())
            }
        }
    }

    /// Whether a credential serves the route model, under its own name for
    /// it if need be (upstream's `authSupportsRouteModel`).
    pub(crate) fn auth_supports_route_model(&self, auth: &Auth, route_model: &str) -> bool {
        let route_key = canonical_model_key(route_model);
        if route_key.is_empty() {
            return true;
        }
        if self.models.client_supports_model(&auth.id, &route_key) {
            return true;
        }
        let selection_key = self
            .resolver
            .selection_model_key_for_auth(auth, route_model);
        !selection_key.is_empty()
            && selection_key != route_key
            && self.models.client_supports_model(&auth.id, &selection_key)
    }

    /// Picks the next credential for a call across providers (upstream's
    /// `pickNextMixed`). `providers` are the call's normalized providers.
    pub(crate) fn pick_next_mixed(
        &self,
        state: &mut SelectorState,
        providers: &[String],
        args: &PickArgs<'_>,
    ) -> Result<Picked, ExecError> {
        let mut eligible: Vec<String> = Vec::with_capacity(providers.len());
        for provider in providers {
            let key = canonical_scheduling_provider(provider);
            if key.is_empty() || eligible.contains(&key) {
                continue;
            }
            if lookup_executor(self.executors, &key).is_none() {
                continue;
            }
            eligible.push(key);
        }
        if eligible.is_empty() {
            return Err(ExecError::auth_not_found());
        }
        if !args.model.trim().is_empty() {
            let route_key = canonical_model_key(args.model);
            let route_aware = self.auths.values().any(|entry| {
                let auth = &entry.auth;
                !auth.disabled
                    && eligible.contains(&canonical_scheduling_provider(&executor_key_from_auth(
                        auth,
                    )))
                    && args.eligibility.allows(auth)
                    && !args.tried.contains(&auth.id)
                    && self.resolver.selection_model_key_for_auth(auth, args.model) != route_key
            });
            if route_aware {
                return self.pick_next_mixed_legacy(state, providers, args);
            }
        }
        let (auth, provider) = self.pick_mixed(state, &eligible, args)?;
        let executor = lookup_executor(self.executors, &provider).ok_or_else(|| {
            ExecError::new(ErrorKind::ExecutorNotFound, "executor not registered")
        })?;
        Ok(Picked {
            auth,
            executor,
            provider,
        })
    }

    /// The legacy pick, which checks each credential against its own name
    /// for the model (upstream's `pickNextMixedLegacy`).
    pub(super) fn pick_next_mixed_legacy(
        &self,
        state: &mut SelectorState,
        providers: &[String],
        args: &PickArgs<'_>,
    ) -> Result<Picked, ExecError> {
        let provider_set: HashSet<String> = providers
            .iter()
            .map(|p| canonical_scheduling_provider(p))
            .filter(|p| !p.is_empty())
            .collect();
        if provider_set.is_empty() {
            return Err(ExecError::new(
                ErrorKind::ProviderNotFound,
                "no provider supplied",
            ));
        }
        let mut model_key = args.model.trim();
        if !model_key.is_empty() {
            let base = parse_suffix(model_key).0;
            if !base.is_empty() {
                model_key = base.trim();
            }
        }
        let candidates: Vec<&Arc<Auth>> = self
            .auths
            .values()
            .map(|entry| &entry.auth)
            .filter(|auth| {
                if auth.disabled || (!args.pinned.is_empty() && auth.id != args.pinned) {
                    return false;
                }
                if !args.eligibility.allows(auth) {
                    return false;
                }
                let key = executor_key_from_auth(auth);
                !key.is_empty()
                    && provider_set.contains(&key)
                    && !args.tried.contains(&auth.id)
                    && lookup_executor(self.executors, &key).is_some()
                    && (model_key.is_empty() || self.auth_supports_route_model(auth, args.model))
            })
            .collect();
        if candidates.is_empty() {
            return Err(ExecError::auth_not_found());
        }
        let available = self.available_auths_for_route_model(&candidates, "mixed", args.model)?;
        let selected = self.pick_legacy(state, &available, "mixed", args.model)?;
        let provider = executor_key_from_auth(selected);
        let executor = lookup_executor(self.executors, &provider).ok_or_else(|| {
            ExecError::new(ErrorKind::ExecutorNotFound, "executor not registered")
        })?;
        Ok(Picked {
            auth: selected.clone(),
            executor,
            provider,
        })
    }

    /// The built-in selector's pick among the `available` credentials, with
    /// its rotation kept under `scope`, the provider or `mixed` (upstream's
    /// `RoundRobinSelector`, `FillFirstSelector` and
    /// `WeightedRoundRobinSelector` as the legacy picks call them).
    pub(super) fn pick_legacy<'c>(
        &self,
        state: &mut SelectorState,
        available: &[&'c Arc<Auth>],
        scope: &str,
        model: &str,
    ) -> Result<&'c Arc<Auth>, ExecError> {
        let selected = match self.strategy {
            RoutingStrategy::FillFirst => available.first().copied(),
            RoutingStrategy::RoundRobin => {
                let key = format!("{scope}:");
                let ids: Vec<&str> = available.iter().map(|a| a.id.as_str()).collect();
                let last = capped(&mut state.legacy_last_picked, &key);
                let picked = available.get(successor_index(&ids, last)).copied();
                if let Some(auth) = picked {
                    *last = auth.id.clone();
                }
                picked
            }
            RoutingStrategy::Weighted => {
                let positive: Vec<&Arc<Auth>> = available
                    .iter()
                    .copied()
                    .filter(|a| weight(a) > 0)
                    .collect();
                if positive.is_empty() {
                    return Err(ExecError::new(
                        ErrorKind::AuthNotFound,
                        "no auth candidates",
                    ));
                }
                let key = format!("{scope}:{}", canonical_model_key(model));
                let weighted = capped(&mut state.legacy_weighted, &key);
                let weights: HashMap<String, i64> =
                    positive.iter().map(|a| (a.id.clone(), weight(a))).collect();
                weighted.prepare(&weights);
                let id = weighted.pick(positive.iter().map(|a| (a.id.as_str(), weight(a))));
                match id.and_then(|id| positive.iter().find(|a| a.id == id)) {
                    Some(auth) => Some(*auth),
                    None => {
                        return Err(ExecError::new(
                            ErrorKind::AuthUnavailable,
                            "no auth available with positive weight",
                        ));
                    }
                }
            }
        };
        selected.ok_or_else(|| ExecError::new(ErrorKind::AuthNotFound, "selector returned no auth"))
    }

    /// The highest priority ready candidates, by ID, or why there are none
    /// (upstream's `availableAuthsForRouteModel`).
    pub(super) fn available_auths_for_route_model<'c>(
        &self,
        candidates: &[&'c Arc<Auth>],
        provider: &str,
        route_model: &str,
    ) -> Result<Vec<&'c Arc<Auth>>, ExecError> {
        if candidates.is_empty() {
            return Err(ExecError::new(
                ErrorKind::AuthNotFound,
                "no auth candidates",
            ));
        }
        let now = self.now;
        let mut by_priority: BTreeMap<i64, Vec<&'c Arc<Auth>>> = BTreeMap::new();
        let mut cooldown = 0usize;
        let mut unauthorized = 0usize;
        let mut earliest: Option<Timestamp> = None;
        for candidate in candidates {
            let check_model = self
                .resolver
                .selection_model_for_auth(candidate, route_model);
            let (blocked, reason, next) = is_auth_blocked_for_model(candidate, &check_model, now);
            if !blocked {
                by_priority
                    .entry(priority(candidate))
                    .or_default()
                    .push(candidate);
                continue;
            }
            if reason == BlockReason::Cooldown {
                cooldown += 1;
            }
            if reason != BlockReason::Disabled
                && let Some(next) = next
                && next > now
                && earliest.is_none_or(|e| next < e)
            {
                earliest = Some(next);
            }
            if has_unauthorized_auth_failure(candidate) {
                unauthorized += 1;
            }
        }
        if let Some((_, mut best)) = by_priority.pop_last() {
            best.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(best);
        }
        let mut errors = CandidateErrors::default();
        for candidate in candidates {
            let check_model = self
                .resolver
                .selection_model_for_auth(candidate, route_model);
            errors.offer(candidate, &check_model);
        }
        let last = errors.last();
        if cooldown == candidates.len()
            && let Some(earliest) = earliest
        {
            let provider = if provider == "mixed" { "" } else { provider };
            return Err(model_cooldown_with_cause(
                route_model,
                provider,
                earliest,
                now,
                last,
            ));
        }
        if unauthorized == candidates.len() {
            let mut latest: Option<Latest> = None;
            for candidate in candidates {
                if !has_unauthorized_auth_failure(candidate) {
                    continue;
                }
                if let Some(err) = &candidate.last_error {
                    Latest::offer(
                        &mut latest,
                        Latest {
                            time: candidate.updated_at,
                            id: candidate.id.clone(),
                            text: auth_error_text(err),
                        },
                    );
                }
            }
            let cause = latest.as_ref().map(|l| l.text.as_str()).or(last);
            return Err(terminal_auth_unavailable(cause));
        }
        Err(auth_unavailable_with_cause(earliest, now, last))
    }
}
