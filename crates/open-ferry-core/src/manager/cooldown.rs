// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cooldown.go (MarkResult,
// isStaleExecutionResult, the model-state helpers, applyAuthFailureState, the quota helpers,
// clearCooldownStateForAuth and ResetQuota), clientModelProjectionForAuth in
// sdk/cliproxy/auth/conductor_models.go, ReconcileRegistryModelStates in
// sdk/cliproxy/auth/conductor_selection.go and applyCooldownFields in
// sdk/cliproxy/auth/quota_signals.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What a call's outcome does to a credential: the cooldowns that keep a
//! failing credential or model out of rotation, and how they clear.
//!
//! A failure for a model cools down only that model, unless the provider
//! says the whole credential is out of quota. A failure with no model cools
//! down the credential. Cooling can be turned off globally, per
//! OpenAI-compatible provider or per credential.
//!
//! Deviations from upstream:
//! - A model's own cooldown beneath a credential-wide quota cooldown is
//!   kept (upstream's Devin quota change, b946d54e). Upstream's Devin-only
//!   `isDevinQuotaExhaustionMessage` checks are treated as false, so only a
//!   `credential_scope` failure wraps the sibling models; the revalidation
//!   scheduling and quota restoration that only Devin triggers aren't
//!   ported.
//! - Model states are walked in key order, where Go's map order is random.
//! - The backoff level saturates where Go's shift would overflow.
//! - Result policies and hooks aren't ported.
//! - The provider's response headers come in the [`CallResult`], where
//!   upstream's `MarkResult` reads them from the request's context.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use http::HeaderMap;

use super::classify::{
    CODE_FORCE_COOLDOWN, has_unauthorized_auth_failure, is_cloudflare_challenge_result_error,
    is_invalid_grant_result_error, is_model_support_result_error, should_skip_credential_cooldown,
};
use super::credential::{disable_cooling_override, is_zero};
use super::models::{Resolver, resolve_openai_compat_config};
use super::quota_signals::{apply_cooldown_fields, merge_quota_observation};
use super::select::ModelProjection;
use super::settings::Settings;
use super::text::{canonical_model_key, go_lower};
use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status, Timestamp};

/// The first quota cooldown (upstream's `quotaBackoffBase`).
pub(crate) const QUOTA_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// The longest quota cooldown (upstream's `quotaBackoffMax`).
pub(crate) const QUOTA_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
/// The shortest cooldown a provider's retry hint can set (upstream's
/// `minQuotaCooldownFloor`).
pub(crate) const MIN_QUOTA_COOLDOWN_FLOOR: Duration = Duration::from_secs(10);
/// The default cooldown after a transient failure (upstream's
/// `transientErrorCooldown`).
pub(crate) const TRANSIENT_ERROR_COOLDOWN: Duration = Duration::from_secs(60);

const THIRTY_MINUTES: Duration = Duration::from_secs(30 * 60);
const TWELVE_HOURS: Duration = Duration::from_secs(12 * 60 * 60);

/// The outcome of one call on a credential (upstream's `Result`).
#[derive(Clone, Debug, Default)]
pub struct CallResult {
    /// The credential.
    pub auth_id: String,
    /// The provider it ran on.
    pub provider: String,
    /// The model whose state the outcome belongs to, or empty for the whole
    /// credential.
    pub model: String,
    /// The model the client asked for.
    pub route_model: String,
    /// Whether the call succeeded.
    pub success: bool,
    /// The failure, when it didn't.
    pub error: Option<AuthError>,
    /// The provider's hint for when to try again.
    pub retry_after: Option<Duration>,
    /// Whether the provider said the whole credential is out of quota.
    pub credential_scope: bool,
    /// The headers of the provider's response, for the quota snapshot:
    /// those of the last attempt, or of the stream the call read.
    pub response_headers: HeaderMap,
    /// Leaves the quota snapshot alone, as for a token count, whose
    /// response says nothing of the credential's quota (upstream's
    /// `SkipQuotaObservation`).
    pub skip_quota_observation: bool,
    /// The [`Auth::credential_version`] of the credential the call ran with,
    /// or 0 when not known (upstream's `CredentialVersion`).
    pub credential_version: u64,
    /// The [`Auth::registration_epoch`] of the credential the call ran
    /// with, or 0 when not known (upstream's `RegistrationEpoch`).
    pub registration_epoch: u64,
    /// The manager's sequence stamp for the dispatch, or 0 when not known (a
    /// result recorded directly): it orders the call against cooldowns
    /// recorded meanwhile, so a success dispatched before a cooldown can't
    /// clear it. Zero is exempt from that rule (upstream's `ExecutionSeq`).
    pub execution_seq: u64,
}

/// Whether `result` came from tokens or an API key `current` no longer has,
/// or from an earlier registration of its ID (upstream's
/// `isStaleExecutionResult`). A result that doesn't say which version it
/// ran with counts as stale once the credential's secrets were replaced.
pub(crate) fn is_stale_result(result: &CallResult, current: &Auth) -> bool {
    let stale_version = result.credential_version < current.credential_version
        && (result.credential_version > 0 || current.credential_version > 1);
    let stale_epoch =
        result.registration_epoch > 0 && result.registration_epoch < current.registration_epoch;
    stale_version || stale_epoch
}

/// `t` plus `d`, saturating at the latest time chrono can hold.
pub(crate) fn add(t: Timestamp, d: Duration) -> Timestamp {
    TimeDelta::from_std(d)
        .ok()
        .and_then(|d| t.checked_add_signed(d))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

fn after(t: Option<Timestamp>, now: Timestamp) -> bool {
    t.is_some_and(|t| t > now)
}

/// Whether a credential-wide quota cooldown is still running.
pub(crate) fn is_credential_quota_active(quota: &QuotaState, now: Timestamp) -> bool {
    quota.exceeded && quota.reason == "credential_quota" && after(quota.next_recover_at, now)
}

/// Whether cooling is off for `auth`: its own override, else its
/// OpenAI-compatible provider's, else the global setting (upstream's
/// `cooldownDisabledForAuth`).
pub(crate) fn cooldown_disabled_for_auth(settings: &Settings, auth: &Auth) -> bool {
    if let Some(disabled) = disable_cooling_override(auth) {
        return disabled;
    }
    if let Some(disabled) = provider_cooling_override(settings, auth) {
        return disabled;
    }
    settings.disable_cooling
}

/// Upstream's `providerCoolingOverrideForAuth`.
fn provider_cooling_override(settings: &Settings, auth: &Auth) -> Option<bool> {
    let provider = go_lower(auth.provider.trim());
    if provider.is_empty() {
        return None;
    }
    let mut provider_key = auth.attributes.get("provider_key").map_or("", |v| v.trim());
    let compat_name = auth.attributes.get("compat_name").map_or("", |v| v.trim());
    if provider_key.is_empty() && compat_name.is_empty() && provider != "openai-compatibility" {
        return None;
    }
    if provider_key.is_empty() {
        provider_key = &provider;
    }
    resolve_openai_compat_config(settings, provider_key, compat_name, &provider)?.disable_cooling
}

/// When to retry after a transient failure: the provider's hint, else the
/// configured cooldown (upstream's `recoverableFailureRetryAfterWithHint`).
pub(crate) fn recoverable_failure_retry_after(
    settings: &Settings,
    now: Timestamp,
    retry_after: Option<Duration>,
    disable_cooling: bool,
) -> Option<Timestamp> {
    if disable_cooling {
        return None;
    }
    let seconds = settings.transient_error_cooldown_seconds;
    if seconds < 0 {
        return None;
    }
    if let Some(hint) = retry_after.filter(|d| !d.is_zero()) {
        return Some(add(now, hint));
    }
    if seconds == 0 {
        return Some(add(now, TRANSIENT_ERROR_COOLDOWN));
    }
    Some(add(now, Duration::from_secs(seconds.unsigned_abs())))
}

/// The next quota cooldown and backoff level after `prev_level` (upstream's
/// `nextQuotaCooldown`).
pub(crate) fn next_quota_cooldown(prev_level: u32, disable_cooling: bool) -> (Duration, u32) {
    if disable_cooling {
        return (Duration::ZERO, prev_level);
    }
    let cooldown = 1u32
        .checked_shl(prev_level)
        .and_then(|factor| QUOTA_BACKOFF_BASE.checked_mul(factor))
        .unwrap_or(QUOTA_BACKOFF_MAX)
        .max(QUOTA_BACKOFF_BASE);
    if cooldown >= QUOTA_BACKOFF_MAX {
        return (QUOTA_BACKOFF_MAX, prev_level);
    }
    (cooldown, prev_level.saturating_add(1))
}

/// The recovery time and backoff level for a quota failure; a failure while
/// the window is still open reuses it (upstream's
/// `quotaCooldownAfterFailure`).
pub(crate) fn quota_cooldown_after_failure(
    quota: &QuotaState,
    now: Timestamp,
) -> (Option<Timestamp>, u32) {
    if after(quota.next_recover_at, now) {
        return (quota.next_recover_at, quota.backoff_level);
    }
    let (cooldown, level) = next_quota_cooldown(quota.backoff_level, false);
    let next = (!cooldown.is_zero()).then(|| add(now, cooldown));
    (next, level)
}

/// Upstream's `nextCloudflareCooldown`.
pub(crate) fn next_cloudflare_cooldown(
    level: u32,
    disable_cooling: bool,
    now: Timestamp,
) -> (Option<Timestamp>, u32) {
    if disable_cooling {
        return (None, level);
    }
    let (cooldown, next_level) = next_quota_cooldown(level, false);
    let cooldown = cooldown.max(Duration::from_secs(10));
    (Some(add(now, cooldown)), next_level)
}

// ---------------------------------------------------------------------------
// Model states
// ---------------------------------------------------------------------------

/// The state kept for `model`, if any (upstream's `existingModelState`).
pub(crate) fn existing_model_state<'a>(auth: &'a Auth, model: &str) -> Option<&'a ModelState> {
    let key = canonical_model_key(model);
    if key.is_empty() {
        return None;
    }
    auth.model_states.get(&key)
}

/// Makes sure a state exists for `model` and returns its key (upstream's
/// `ensureModelState`).
pub(crate) fn ensure_model_state(auth: &mut Auth, model: &str) -> Option<String> {
    let key = canonical_model_key(model);
    if key.is_empty() {
        return None;
    }
    normalize_model_states(&mut auth.model_states);
    auth.model_states
        .entry(key.clone())
        .or_insert_with(|| ModelState {
            status: Status::Active,
            ..ModelState::default()
        });
    Some(key)
}

/// Keys every state by its canonical model, merging duplicates (upstream's
/// `normalizeModelStates`). Returns whether anything changed.
pub(crate) fn normalize_model_states(states: &mut BTreeMap<String, ModelState>) -> bool {
    if states.is_empty() {
        return false;
    }
    let mut changed = false;
    let mut normalized: BTreeMap<String, ModelState> = BTreeMap::new();
    for (model, state) in std::mem::take(states) {
        let mut key = canonical_model_key(&model);
        if key.is_empty() {
            key = model.trim().to_owned();
        }
        if key != model {
            changed = true;
        }
        match normalized.get_mut(&key) {
            Some(existing) => {
                merge_model_state(existing, &state);
                changed = true;
            }
            None => {
                normalized.insert(key, state);
            }
        }
    }
    *states = normalized;
    changed
}

/// Merges `source` into `target`, keeping the stricter of each cooldown
/// (upstream's `mergeModelState`).
pub(crate) fn merge_model_state(target: &mut ModelState, source: &ModelState) {
    let source_newer = source.updated_at > target.updated_at;
    let (preferred, fallback) = if source_newer {
        (source, &*target)
    } else {
        (&*target, source)
    };
    let mut merged = ModelState {
        status: preferred.status,
        status_message: preferred.status_message.clone(),
        unavailable: target.unavailable || source.unavailable,
        next_retry_after: target.next_retry_after.max(source.next_retry_after),
        last_error: preferred.last_error.clone(),
        quota: QuotaState {
            exceeded: target.quota.exceeded || source.quota.exceeded,
            reason: preferred.quota.reason.clone(),
            next_recover_at: target
                .quota
                .next_recover_at
                .max(source.quota.next_recover_at),
            backoff_level: target.quota.backoff_level.max(source.quota.backoff_level),
            ..QuotaState::default()
        },
        updated_at: target.updated_at.max(source.updated_at),
        // The strongest watermark of either side, so merging a restored or
        // migrated state into a live one can't reopen the stale-success race.
        // Both are zero across a restart, where a restored cooldown stays
        // clearable by any success.
        cooldown_recorded_seq: target
            .cooldown_recorded_seq
            .max(source.cooldown_recorded_seq),
        credential_quota_shadow: None,
        credential_quota_propagated: false,
    };
    // The newer snapshot wins whole, the preferred state's on a tie.
    merged.quota = merge_quota_observation(merged.quota, &fallback.quota);
    merged.quota = merge_quota_observation(merged.quota, &preferred.quota);
    if merged.status_message.is_empty() {
        merged.status_message = fallback.status_message.clone();
    }
    if merged.last_error.is_none() {
        merged.last_error = fallback.last_error.clone();
    }
    if merged.quota.reason.is_empty() {
        merged.quota.reason = fallback.quota.reason.clone();
    }
    if target.status == Status::Disabled || source.status == Status::Disabled {
        merged.status = Status::Disabled;
    } else if merged.unavailable || merged.quota.exceeded {
        merged.status = Status::Error;
    }
    // The own layer and the wrap's mark come from either side (the newer
    // wins) while the merged state is wrapped. The reporting model carries
    // the outer reason "quota", so a wrap is told by the mark and the shadow
    // as well as by the propagated reason.
    if merged.quota.reason == "credential_quota"
        || merged.quota.reason == "quota"
        || target.credential_quota_propagated
        || source.credential_quota_propagated
        || target.credential_quota_shadow.is_some()
        || source.credential_quota_shadow.is_some()
    {
        let shadow = preferred
            .credential_quota_shadow
            .as_ref()
            .or(fallback.credential_quota_shadow.as_ref());
        merged.credential_quota_shadow = shadow.map(|shadow| Box::new(shadow.clone_shallow()));
        merged.credential_quota_propagated =
            target.credential_quota_propagated || source.credential_quota_propagated;
    }
    *target = merged;
}

/// Upstream's `resetModelState`.
pub(crate) fn reset_model_state(state: &mut ModelState, now: Timestamp) {
    state.unavailable = false;
    state.status = Status::Active;
    state.status_message.clear();
    state.next_retry_after = None;
    state.last_error = None;
    apply_cooldown_fields(&mut state.quota, QuotaState::default());
    state.cooldown_recorded_seq = 0;
    state.credential_quota_shadow = None;
    state.credential_quota_propagated = false;
    state.updated_at = Some(now);
}

/// Takes a model state out from under a credential-wide quota cooldown that
/// has run out: its own layer comes back if it is still live, and a clean
/// state otherwise, and the mark of the wrap goes either way so that a later
/// wrap snapshots fresh state (upstream's inline normalization in
/// `MarkResult`).
fn unwrap_expired_quota(state: &mut ModelState, now: Timestamp) {
    match state.credential_quota_shadow.take() {
        Some(shadow) if is_model_state_active_cooldown(&shadow, now) => {
            // The shadow's own watermark comes with it.
            *state = shadow.clone_shallow();
        }
        _ => reset_model_state(state, now),
    }
}

/// Whether a state still holds a model out (upstream's
/// `isModelStateActiveCooldown`).
pub(crate) fn is_model_state_active_cooldown(state: &ModelState, now: Timestamp) -> bool {
    state.status == Status::Disabled
        || after(state.next_retry_after, now)
        || after(state.quota.next_recover_at, now)
        || (state.quota.exceeded && is_zero(state.quota.next_recover_at))
}

/// Whether a state records nothing (upstream's `modelStateIsClean`).
pub(crate) fn model_state_is_clean(state: &ModelState) -> bool {
    state.status == Status::Active
        && !state.unavailable
        && state.status_message.is_empty()
        && is_zero(state.next_retry_after)
        && state.last_error.is_none()
        && !state.quota.exceeded
        && state.quota.reason.is_empty()
        && is_zero(state.quota.next_recover_at)
        && state.quota.backoff_level == 0
}

/// Derives the credential's availability from its model states (upstream's
/// `updateAggregatedAvailability`).
pub(crate) fn update_aggregated_availability(auth: &mut Auth, now: Timestamp) {
    // A credential whose tokens were both rejected stays out of rotation
    // until they change; a model's result doesn't bring it back.
    if has_unauthorized_auth_failure(auth) {
        auth.unavailable = true;
        return;
    }
    if is_credential_quota_active(&auth.quota, now) {
        auth.unavailable = true;
        return;
    }
    if auth.model_states.is_empty() {
        clear_aggregated_availability(auth);
        return;
    }
    let mut all_unavailable = true;
    let mut earliest_retry: Option<Timestamp> = None;
    let mut quota_exceeded = false;
    let mut quota_recover: Option<Timestamp> = None;
    let mut max_backoff = 0u32;
    for state in auth.model_states.values_mut() {
        let mut state_unavailable = false;
        if state.status == Status::Disabled {
            state_unavailable = true;
        } else if state.unavailable {
            if is_zero(state.next_retry_after) {
                state_unavailable = false;
            } else if after(state.next_retry_after, now) {
                state_unavailable = true;
                if is_zero(earliest_retry) || state.next_retry_after < earliest_retry {
                    earliest_retry = state.next_retry_after;
                }
            } else {
                state.unavailable = false;
                state.next_retry_after = None;
            }
        }
        if !state_unavailable {
            all_unavailable = false;
        }
        if state.quota.exceeded {
            quota_exceeded = true;
            if is_zero(quota_recover)
                || (!is_zero(state.quota.next_recover_at)
                    && state.quota.next_recover_at < quota_recover)
            {
                quota_recover = state.quota.next_recover_at;
            }
            max_backoff = max_backoff.max(state.quota.backoff_level);
        }
    }
    auth.unavailable = all_unavailable;
    auth.next_retry_after = if all_unavailable {
        earliest_retry
    } else {
        None
    };
    if quota_exceeded {
        auth.quota.exceeded = true;
        auth.quota.reason = "quota".into();
        if auth.quota.next_recover_at > quota_recover {
            quota_recover = auth.quota.next_recover_at;
        }
        auth.quota.next_recover_at = quota_recover;
        auth.quota.backoff_level = max_backoff;
    } else if auth.quota.exceeded && after(auth.quota.next_recover_at, now) {
        // An active credential-wide quota cooldown stays.
    } else {
        apply_cooldown_fields(&mut auth.quota, QuotaState::default());
    }
}

/// Upstream's `clearAggregatedAvailability`.
fn clear_aggregated_availability(auth: &mut Auth) {
    auth.unavailable = false;
    auth.next_retry_after = None;
    apply_cooldown_fields(&mut auth.quota, QuotaState::default());
}

/// Whether any model state still records a failure (upstream's
/// `hasModelError`).
pub(crate) fn has_model_error(auth: &Auth, now: Timestamp) -> bool {
    auth.model_states.values().any(|state| {
        state.last_error.is_some()
            || (state.status == Status::Error
                && state.unavailable
                && (is_zero(state.next_retry_after) || after(state.next_retry_after, now)))
    })
}

/// Upstream's `clearAuthStateOnSuccess`. A credential out of rotation for a
/// rejected token stays out.
pub(crate) fn clear_auth_state_on_success(auth: &mut Auth, now: Timestamp) {
    if has_unauthorized_auth_failure(auth) {
        auth.unavailable = true;
        return;
    }
    auth.unavailable = false;
    auth.status = Status::Active;
    auth.status_message.clear();
    apply_cooldown_fields(&mut auth.quota, QuotaState::default());
    auth.last_error = None;
    auth.next_retry_after = None;
    auth.cooldown_recorded_seq = 0;
    auth.updated_at = Some(now);
}

/// Whether the credential carries a live cooldown of its own, recorded
/// without a model (upstream's `isAuthActiveCooldown`).
fn is_auth_active_cooldown(auth: &Auth, now: Timestamp) -> bool {
    if auth.disabled || auth.status == Status::Disabled {
        return false;
    }
    after(auth.next_retry_after, now)
        || after(auth.quota.next_recover_at, now)
        || (auth.quota.exceeded && is_zero(auth.quota.next_recover_at))
}

/// Clears every cooldown on the credential and its models (upstream's
/// `clearCooldownStateForAuth`). Returns whether anything changed; the
/// caller bumps the generation when it did. A credential out of rotation
/// for a rejected token is left as it is.
pub(crate) fn clear_cooldown_state_for_auth(auth: &mut Auth, now: Timestamp) -> bool {
    if has_unauthorized_auth_failure(auth) {
        return false;
    }
    let mut changed = false;
    if auth.unavailable
        || !is_zero(auth.next_retry_after)
        || auth.quota.exceeded
        || !is_zero(auth.quota.next_recover_at)
    {
        auth.unavailable = false;
        auth.next_retry_after = None;
        apply_cooldown_fields(&mut auth.quota, QuotaState::default());
        auth.updated_at = Some(now);
        changed = true;
    }
    for state in auth.model_states.values_mut() {
        if state.unavailable
            || !is_zero(state.next_retry_after)
            || state.quota.exceeded
            || !is_zero(state.quota.next_recover_at)
            || state.credential_quota_shadow.is_some()
            || state.credential_quota_propagated
        {
            state.unavailable = false;
            state.next_retry_after = None;
            apply_cooldown_fields(&mut state.quota, QuotaState::default());
            state.credential_quota_shadow = None;
            state.credential_quota_propagated = false;
            state.updated_at = Some(now);
            changed = true;
        }
    }
    if !auth.model_states.is_empty() {
        update_aggregated_availability(auth, now);
    }
    if changed {
        auth.updated_at = Some(now);
    }
    changed
}

/// Applies a call's outcome to a credential, as upstream's `MarkResult` does
/// under its lock. `model_key` is the canonical model the outcome belongs
/// to, or empty.
///
/// A credential out of rotation for a rejected token stays out: a call that
/// was already running when it was taken out only updates its model's
/// state.
///
/// `dispatched` is the latest execution sequence issued
/// ([`Manager::current_execution_seq`](super::Manager)): the watermark a
/// cooldown recorded now takes, so a success from a call dispatched at or
/// before it leaves that cooldown be.
pub(crate) fn apply_result(
    settings: &Settings,
    auth: &mut Auth,
    result: &CallResult,
    model_key: &str,
    now: Timestamp,
    dispatched: u64,
    rests: LiveRests,
) -> StaleSuccess {
    let was_terminal_unauthorized = has_unauthorized_auth_failure(auth);
    let stale = stale_success(
        auth,
        result,
        model_key,
        now,
        was_terminal_unauthorized,
        rests,
    );
    apply_outcome(
        settings,
        auth,
        result,
        model_key,
        now,
        was_terminal_unauthorized,
        dispatched,
        stale,
    );
    if was_terminal_unauthorized {
        auth.unavailable = true;
        auth.status = Status::Error;
        auth.next_refresh_after = None;
        auth.next_retry_after = None;
    }
    if !result.skip_quota_observation {
        observe_quota(auth, result, model_key, now);
    }
    stale
}

/// Which cooldown a success left be because it proved nothing about
/// recovery (see [`apply_result`]); all false when it was taken as recovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StaleSuccess {
    /// An active credential-wide quota stayed: a success is no recovery
    /// from it.
    pub(crate) quota: bool,
    /// The model's cooldown stayed: the call predated it.
    pub(crate) model: bool,
    /// The credential's own cooldown stayed: the call predated it.
    pub(crate) credential: bool,
}

/// Which scopes of a result still have a provider reset that holds, by the
/// quota-check rests. The cap shortens a long cooldown's timestamps to the
/// check's time, so the cooldown can look over while the provider's quota
/// is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LiveRests {
    /// The credential's rest.
    pub(crate) credential: bool,
    /// The result's model's rest.
    pub(crate) model: bool,
}

/// Takes the quota snapshot of the response behind `result` into the
/// credential and into the state of the model it was for, when that state
/// exists: a call doesn't make one only to keep a snapshot.
fn observe_quota(auth: &mut Auth, result: &CallResult, model_key: &str, now: Timestamp) {
    let headers = &result.response_headers;
    auth.quota
        .observe_response_headers_for_provider(&result.provider, headers, now);
    let key = canonical_model_key(model_key);
    if key.is_empty() {
        return;
    }
    if let Some(state) = auth.model_states.get_mut(&key) {
        state
            .quota
            .observe_response_headers_for_provider(&result.provider, headers, now);
    }
}

/// Whether a success proves nothing about recovery from a live cooldown
/// (see [`StaleSuccess`]), given which of the scopes' provider resets still
/// hold (`rests`, from the quota-check rests: the cap rewrites a long
/// cooldown's timestamps, so they may have run out while the reset hasn't).
fn stale_success(
    auth: &Auth,
    result: &CallResult,
    model_key: &str,
    now: Timestamp,
    was_terminal_unauthorized: bool,
    rests: LiveRests,
) -> StaleSuccess {
    if !result.success || was_terminal_unauthorized {
        return StaleSuccess::default();
    }
    let predates = |recorded: u64| result.execution_seq > 0 && result.execution_seq <= recorded;
    // A success is no recovery from an active credential quota, whatever
    // call it came from. Past the cap's check time the quota is held by its
    // rest alone, and then only a call that predates it is no recovery (the
    // check's own success is).
    let recorded = auth
        .model_states
        .values()
        .map(|state| state.cooldown_recorded_seq)
        .fold(auth.cooldown_recorded_seq, u64::max);
    let quota = (auth.quota.reason == "credential_quota" && after(auth.quota.next_recover_at, now))
        || (rests.credential && recorded > 0 && predates(recorded));
    let model = !model_key.is_empty()
        && existing_model_state(auth, model_key).is_some_and(|existing| {
            (is_model_state_active_cooldown(existing, now) || rests.model)
                && existing.cooldown_recorded_seq > 0
                && predates(existing.cooldown_recorded_seq)
        });
    let credential = model_key.is_empty()
        && is_auth_active_cooldown(auth, now)
        && auth.cooldown_recorded_seq > 0
        && predates(auth.cooldown_recorded_seq);
    StaleSuccess {
        quota,
        model,
        credential,
    }
}

/// The body of [`apply_result`].
#[allow(clippy::too_many_arguments)]
fn apply_outcome(
    settings: &Settings,
    auth: &mut Auth,
    result: &CallResult,
    model_key: &str,
    now: Timestamp,
    was_terminal_unauthorized: bool,
    dispatched: u64,
    stale: StaleSuccess,
) {
    if result.success {
        // A success from a call dispatched before a still-live cooldown was
        // recorded predates the failure and proves nothing about recovery,
        // so it causes no state change at all.
        if was_terminal_unauthorized {
            if !model_key.is_empty()
                && let Some(key) = ensure_model_state(auth, model_key)
                && let Some(state) = auth.model_states.get_mut(&key)
            {
                reset_model_state(state, now);
            }
        } else if stale.quota {
            // An active credential-scoped cooldown stays.
        } else if stale.model || stale.credential {
            // So does one recorded after this call was dispatched.
        } else if !model_key.is_empty() {
            if let Some(key) = ensure_model_state(auth, model_key)
                && let Some(state) = auth.model_states.get_mut(&key)
            {
                reset_model_state(state, now);
            }
            update_aggregated_availability(auth, now);
            if !has_model_error(auth, now) {
                auth.last_error = None;
                auth.status_message.clear();
                auth.status = Status::Active;
            }
        } else {
            clear_auth_state_on_success(auth, now);
        }
        return;
    }
    let force_cooldown = result
        .error
        .as_ref()
        .is_some_and(|err| err.code == CODE_FORCE_COOLDOWN);
    if model_key.is_empty() {
        if was_terminal_unauthorized {
            return;
        }
        let disable_cooling = cooldown_disabled_for_auth(settings, auth) && !force_cooldown;
        apply_auth_failure_state(
            settings,
            auth,
            result.error.as_ref(),
            result.retry_after,
            now,
            disable_cooling,
        );
        auth.cooldown_recorded_seq = dispatched;
        return;
    }
    if should_skip_credential_cooldown(result.error.as_ref()) {
        return;
    }
    let disable_cooling = cooldown_disabled_for_auth(settings, auth) && !force_cooldown;
    let Some(key) = ensure_model_state(auth, model_key) else {
        return;
    };
    let Some(mut state) = auth.model_states.remove(&key) else {
        return;
    };
    // A credential-wide quota cooldown still in force, as this call finds it.
    let wrap_live =
        auth.quota.reason == "credential_quota" && after(auth.quota.next_recover_at, now);
    // A wrap that has run out is taken off before this result applies, so
    // that its own layer comes back and a later wrap snapshots fresh state.
    if (state.credential_quota_propagated || state.quota.reason == "credential_quota")
        && !is_credential_quota_active(&auth.quota, now)
    {
        unwrap_expired_quota(&mut state, now);
    }
    // While the credential-wide cooldown wraps the model, a failure of its
    // own is applied to its own layer, so that its deadline and backoff
    // never inherit the credential's, and the outer fields are composed from
    // both layers below. A credential-wide failure refreshes the outer layer
    // and is never the model's own.
    let saved_outer = state.clone_shallow();
    let own_layer_pending = !result.credential_scope && wrap_live;
    if own_layer_pending {
        state = match state.credential_quota_shadow.take() {
            Some(shadow) => shadow.clone_shallow(),
            None => ModelState::default(),
        };
    }
    state.unavailable = true;
    state.status = Status::Error;
    state.updated_at = Some(now);
    let prev_retry_after = state.next_retry_after;
    if let Some(err) = &result.error {
        state.last_error = Some(err.clone());
        state.status_message = err.message.clone();
        if !was_terminal_unauthorized {
            auth.last_error = Some(err.clone());
            auth.status_message = err.message.clone();
        }
    }
    let err = result.error.as_ref();
    let status = err.map_or(0, |e| e.http_status);
    let retry_hint = result.retry_after.filter(|d| !d.is_zero());
    if err.is_some_and(is_model_support_result_error) {
        state.next_retry_after = if disable_cooling {
            None
        } else {
            Some(add(now, retry_hint.unwrap_or(TWELVE_HOURS)))
        };
    } else if err.is_some_and(is_cloudflare_challenge_result_error) {
        let (next, level) =
            next_cloudflare_cooldown(state.quota.backoff_level, disable_cooling, now);
        state.next_retry_after = next;
        state.status_message = "cloudflare challenge".into();
        if auth.last_error.is_some() && !was_terminal_unauthorized {
            auth.status_message = "cloudflare challenge".into();
        }
        apply_cooldown_fields(
            &mut state.quota,
            QuotaState {
                exceeded: true,
                reason: "cloudflare challenge".into(),
                next_recover_at: next,
                backoff_level: level,
                ..QuotaState::default()
            },
        );
    } else if err.is_some_and(is_invalid_grant_result_error) {
        state.next_retry_after = (!disable_cooling).then(|| add(now, THIRTY_MINUTES));
    } else {
        match status {
            401..=403 => {
                state.next_retry_after = (!disable_cooling).then(|| add(now, THIRTY_MINUTES));
            }
            404 => {
                state.next_retry_after = if disable_cooling {
                    None
                } else {
                    Some(add(now, retry_hint.unwrap_or(TWELVE_HOURS)))
                };
            }
            429 => {
                let mut next: Option<Timestamp> = None;
                let mut credential_next: Option<Timestamp> = None;
                let mut level = state.quota.backoff_level;
                let auth_credential_quota =
                    auth.quota.exceeded && auth.quota.reason == "credential_quota";
                if result.credential_scope {
                    level = if auth_credential_quota {
                        auth.quota.backoff_level
                    } else {
                        0
                    };
                }
                if !disable_cooling {
                    if let Some(retry_after) = result.retry_after {
                        next = Some(add(now, retry_after.max(MIN_QUOTA_COOLDOWN_FLOOR)));
                    } else {
                        let mut quota_for_failure = state.quota.clone();
                        if result.credential_scope {
                            if auth_credential_quota {
                                quota_for_failure = auth.quota.clone();
                            } else {
                                quota_for_failure.next_recover_at = None;
                                quota_for_failure.backoff_level = 0;
                            }
                        }
                        (next, level) = quota_cooldown_after_failure(&quota_for_failure, now);
                    }
                    credential_next = next;
                    if state.quota.exceeded && state.quota.next_recover_at > next {
                        next = state.quota.next_recover_at;
                    }
                }
                state.next_retry_after = next;
                apply_cooldown_fields(
                    &mut state.quota,
                    QuotaState {
                        exceeded: true,
                        reason: "quota".into(),
                        next_recover_at: next,
                        backoff_level: level,
                        ..QuotaState::default()
                    },
                );
                // A failure of the model's own re-derives its own layer below,
                // so a stale one goes. A credential-wide report keeps the
                // layer it finds, and only refreshes the outer one.
                if !result.credential_scope {
                    state.credential_quota_shadow = None;
                } else if !state.credential_quota_propagated
                    && saved_outer.quota.reason != "credential_quota"
                {
                    // The reporting model's first wrap keeps the block of its
                    // own that is still live, an ordinary 429 of the model
                    // included. An outer `credential_quota` instead proves
                    // an earlier wrap, which is never the model's own.
                    state.credential_quota_shadow = after(prev_retry_after, now).then(|| {
                        let mut own = saved_outer.clone_shallow();
                        own.next_retry_after = prev_retry_after;
                        own.unavailable = true;
                        own.status = Status::Error;
                        Box::new(own)
                    });
                }
                if result.credential_scope && !disable_cooling {
                    for other in auth.model_states.values_mut() {
                        // A wrap that has run out comes off first, so that a
                        // stale mark can't keep a fresh block from being kept.
                        if (other.credential_quota_propagated
                            || other.quota.reason == "credential_quota")
                            && !is_credential_quota_active(&auth.quota, now)
                        {
                            unwrap_expired_quota(other, now);
                        }
                        // The first wrap keeps the sibling's live block of its
                        // own beneath the credential's; a healthy or already
                        // wrapped sibling has none. The mark stops a later
                        // wrap from taking its own earlier layer for one.
                        let already_wrapped = other.credential_quota_propagated
                            || other.quota.reason == "credential_quota";
                        if !already_wrapped {
                            other.credential_quota_shadow =
                                is_model_state_active_cooldown(other, now)
                                    .then(|| Box::new(other.clone_shallow()));
                        }
                        other.credential_quota_propagated = true;
                        other.unavailable = true;
                        other.status = Status::Error;
                        let mut other_quota_next = credential_next;
                        if other.quota.exceeded && other.quota.next_recover_at > other_quota_next {
                            other_quota_next = other.quota.next_recover_at;
                        }
                        let mut other_retry_after = other_quota_next;
                        if !is_zero(other.next_retry_after)
                            && other.next_retry_after > other_retry_after
                        {
                            other_retry_after = other.next_retry_after;
                        }
                        other.next_retry_after = other_retry_after;
                        apply_cooldown_fields(
                            &mut other.quota,
                            QuotaState {
                                exceeded: true,
                                reason: "credential_quota".into(),
                                next_recover_at: other_quota_next,
                                backoff_level: level,
                                ..QuotaState::default()
                            },
                        );
                    }
                    if !was_terminal_unauthorized {
                        auth.unavailable = true;
                        let mut auth_next = credential_next;
                        if auth_credential_quota && auth.quota.next_recover_at > auth_next {
                            auth_next = auth.quota.next_recover_at;
                        }
                        auth.quota.exceeded = true;
                        auth.quota.reason = "credential_quota".into();
                        auth.quota.next_recover_at = auth_next;
                        auth.quota.backoff_level = level;
                        auth.next_retry_after = auth_next;
                        // The reporting model is wrapped too, so a later
                        // credential-wide report from another model can't
                        // keep its state as a block of its own.
                        state.credential_quota_propagated = true;
                    }
                }
            }
            408 | 500 | 502 | 503 | 504 | 520..=526 => {
                state.next_retry_after = recoverable_failure_retry_after(
                    settings,
                    now,
                    result.retry_after,
                    disable_cooling,
                );
                state.unavailable = state.next_retry_after.is_some();
            }
            _ => {
                state.next_retry_after =
                    recoverable_failure_retry_after(settings, now, None, disable_cooling);
                state.unavailable = state.next_retry_after.is_some();
            }
        }
    }
    if disable_cooling && is_zero(state.next_retry_after) && is_zero(state.quota.next_recover_at) {
        state.unavailable = false;
        state.quota.exceeded = false;
    }
    if force_cooldown && is_zero(state.next_retry_after) {
        state.next_retry_after = Some(add(now, TRANSIENT_ERROR_COOLDOWN));
        state.unavailable = true;
    }
    if !is_zero(state.next_retry_after)
        && prev_retry_after > state.next_retry_after
        && after(prev_retry_after, now)
    {
        state.next_retry_after = prev_retry_after;
    }
    // The dispatch watermark of the cooldown just recorded. A credential-wide
    // one reached the credential and every sibling state above, so they take
    // it too; a sibling's later, longer model cooldown re-stamps only its own.
    state.cooldown_recorded_seq = dispatched;
    if own_layer_pending {
        // The outer fields are composed from the wrap, which was saved, and
        // from the model's own layer, which has just been updated in place.
        let own = state.clone_shallow();
        let mut outer = saved_outer;
        outer.credential_quota_shadow =
            is_model_state_active_cooldown(&own, now).then(|| Box::new(own));
        outer.cooldown_recorded_seq = dispatched;
        // Wrapped by definition: marked, so that a later credential-wide
        // report can't take it for an unwrapped state.
        outer.credential_quota_propagated = true;
        outer.unavailable = true;
        outer.status = Status::Error;
        outer.updated_at = Some(now);
        outer.last_error.clone_from(&state.last_error);
        outer.status_message.clone_from(&state.status_message);
        if state.next_retry_after > outer.next_retry_after {
            outer.next_retry_after = state.next_retry_after;
        }
        if auth.quota.next_recover_at > outer.next_retry_after {
            outer.next_retry_after = auth.quota.next_recover_at;
        }
        if !(outer.quota.exceeded && outer.quota.reason == "credential_quota") {
            apply_cooldown_fields(
                &mut outer.quota,
                QuotaState {
                    exceeded: true,
                    reason: "credential_quota".into(),
                    next_recover_at: auth.quota.next_recover_at,
                    backoff_level: auth.quota.backoff_level,
                    ..QuotaState::default()
                },
            );
        }
        state = outer;
    }
    if result.credential_scope && !disable_cooling && status == 429 {
        auth.cooldown_recorded_seq = dispatched;
        for other in auth.model_states.values_mut() {
            other.cooldown_recorded_seq = dispatched;
        }
    }
    auth.model_states.insert(key, state);
    auth.status = Status::Error;
    update_aggregated_availability(auth, now);
}

/// Cools the whole credential down after a failure with no model (upstream's
/// `applyAuthFailureState`).
pub(crate) fn apply_auth_failure_state(
    settings: &Settings,
    auth: &mut Auth,
    err: Option<&AuthError>,
    retry_after: Option<Duration>,
    now: Timestamp,
    disable_cooling: bool,
) {
    let prev_retry_after = auth.next_retry_after;
    if should_skip_credential_cooldown(err) {
        return;
    }
    auth.unavailable = true;
    auth.status = Status::Error;
    auth.updated_at = Some(now);
    if let Some(err) = err {
        auth.last_error = Some(err.clone());
        if !err.message.is_empty() {
            auth.status_message = err.message.clone();
        }
    }
    let status = err.map_or(0, |e| e.http_status);
    let retry_hint = retry_after.filter(|d| !d.is_zero());
    if err.is_some_and(is_cloudflare_challenge_result_error) {
        auth.status_message = "cloudflare challenge".into();
        let (next, level) =
            next_cloudflare_cooldown(auth.quota.backoff_level, disable_cooling, now);
        apply_cooldown_fields(
            &mut auth.quota,
            QuotaState {
                exceeded: true,
                reason: "cloudflare challenge".into(),
                next_recover_at: next,
                backoff_level: level,
                ..QuotaState::default()
            },
        );
        auth.next_retry_after = next;
    } else if err.is_some_and(is_invalid_grant_result_error) {
        auth.status_message = "invalid_grant".into();
        auth.next_retry_after = (!disable_cooling).then(|| add(now, THIRTY_MINUTES));
    } else {
        match status {
            401 => {
                auth.status_message = "unauthorized".into();
                auth.next_retry_after = (!disable_cooling).then(|| add(now, THIRTY_MINUTES));
            }
            402 | 403 => {
                auth.status_message = "payment_required".into();
                auth.next_retry_after = (!disable_cooling).then(|| add(now, THIRTY_MINUTES));
            }
            404 => {
                auth.status_message = "not_found".into();
                auth.next_retry_after = if disable_cooling {
                    None
                } else {
                    Some(add(now, retry_hint.unwrap_or(TWELVE_HOURS)))
                };
            }
            429 => {
                auth.status_message = "quota exhausted".into();
                auth.quota.exceeded = true;
                auth.quota.reason = "quota".into();
                let mut next: Option<Timestamp> = None;
                if !disable_cooling {
                    if let Some(retry_after) = retry_after {
                        next = Some(add(now, retry_after.max(MIN_QUOTA_COOLDOWN_FLOOR)));
                    } else {
                        let (n, level) = quota_cooldown_after_failure(&auth.quota, now);
                        next = n;
                        auth.quota.backoff_level = level;
                    }
                    if auth.quota.exceeded && auth.quota.next_recover_at > next {
                        next = auth.quota.next_recover_at;
                    }
                }
                auth.quota.next_recover_at = next;
                auth.next_retry_after = next;
            }
            408 | 500 | 502 | 503 | 504 | 520..=526 => {
                auth.status_message = "transient upstream error".into();
                auth.next_retry_after =
                    recoverable_failure_retry_after(settings, now, retry_after, disable_cooling);
                auth.unavailable = auth.next_retry_after.is_some();
            }
            _ => {
                if auth.status_message.is_empty() {
                    auth.status_message = "request failed".into();
                }
                auth.next_retry_after =
                    recoverable_failure_retry_after(settings, now, None, disable_cooling);
                auth.unavailable = auth.next_retry_after.is_some();
            }
        }
    }
    if !is_zero(auth.next_retry_after)
        && prev_retry_after > auth.next_retry_after
        && after(prev_retry_after, now)
    {
        auth.next_retry_after = prev_retry_after;
    }
    if err.is_some_and(|e| e.code == CODE_FORCE_COOLDOWN) && is_zero(auth.next_retry_after) {
        auth.next_retry_after = Some(add(now, TRANSIENT_ERROR_COOLDOWN));
        auth.unavailable = true;
    }
    if disable_cooling && is_zero(auth.next_retry_after) && is_zero(auth.quota.next_recover_at) {
        auth.unavailable = false;
        auth.quota.exceeded = false;
    }
}

/// Why a cooldown holds: the quota reason, the status message, or the
/// error (upstream's `cooldownReason`).
pub(crate) fn cooldown_reason(
    status_message: &str,
    quota: &QuotaState,
    last_error: Option<&AuthError>,
) -> String {
    let reason = quota.reason.trim();
    if !reason.is_empty() {
        return reason.to_owned();
    }
    let message = status_message.trim();
    if !message.is_empty() {
        return message.to_owned();
    }
    if let Some(err) = last_error {
        let code = err.code.trim();
        if !code.is_empty() {
            return code.to_owned();
        }
        let message = err.message.trim();
        if !message.is_empty() {
            return message.to_owned();
        }
    }
    String::new()
}

/// Whether `auth` can serve `route_model` now, for the registry (upstream's
/// `clientModelProjectionForAuth`).
pub(crate) fn client_model_projection_for_auth(
    resolver: Resolver<'_>,
    auth: &Auth,
    route_model: &str,
    now: Timestamp,
) -> ModelProjection {
    let target = route_model.trim();
    if target.is_empty() {
        return ModelProjection::default();
    }
    let mut key = resolver.selection_model_key_for_auth(auth, target);
    if key.is_empty() {
        key = canonical_model_key(target);
    }
    let state = existing_model_state(auth, &key);
    let mut suspended = auth.disabled || auth.status == Status::Disabled;
    if is_credential_quota_active(&auth.quota, now) {
        suspended = true;
    }
    let mut quota_exceeded = false;
    let mut reason = String::new();
    if let Some(state) = state {
        if state.status == Status::Disabled
            || state.unavailable
            || (!is_zero(state.next_retry_after) && after(state.next_retry_after, now))
        {
            suspended = true;
        }
        if state.quota.exceeded
            && (is_zero(state.quota.next_recover_at) || after(state.quota.next_recover_at, now))
        {
            quota_exceeded = true;
        }
        if suspended {
            reason = cooldown_reason(
                &state.status_message,
                &state.quota,
                state.last_error.as_ref(),
            );
        }
    }
    if auth.model_states.is_empty() && auth.unavailable && after(auth.next_retry_after, now) {
        suspended = true;
    }
    if suspended && reason.is_empty() {
        reason = cooldown_reason(&auth.status_message, &auth.quota, auth.last_error.as_ref());
    }
    ModelProjection {
        model_id: target.to_owned(),
        suspended,
        suspend_reason: reason,
        quota_exceeded,
    }
}

/// Projections for each of a credential's registered models.
pub(crate) fn projections_for(
    resolver: Resolver<'_>,
    auth: &Auth,
    models: &[String],
    now: Timestamp,
) -> Vec<ModelProjection> {
    models
        .iter()
        .filter(|model| !model.trim().is_empty())
        .map(|model| client_model_projection_for_auth(resolver, auth, model, now))
        .collect()
}

/// Upstream's `dedupeStrings`.
pub(crate) fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    if values.len() < 2 {
        return values;
    }
    let mut seen = HashSet::with_capacity(values.len());
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let value = value.trim().to_owned();
        if value.is_empty() || !seen.insert(value.clone()) {
            continue;
        }
        out.push(value);
    }
    out
}

/// Clears a credential's quota and cooldowns, as upstream's `ResetQuota`
/// does under its lock. Returns the models it cleared, falling back to the
/// credential's registered models (`registered`, canonical), and whether
/// [`clear_cooldown_state_for_auth`] changed anything (upstream bumps the
/// generation for that, and once more for the reset).
pub(crate) fn reset_quota(
    auth: &mut Auth,
    registered: &[String],
    now: Timestamp,
) -> (Vec<String>, bool) {
    let mut models: Vec<String> = Vec::new();
    for (key, state) in auth.model_states.iter_mut() {
        if key.trim().is_empty() {
            continue;
        }
        models.push(key.clone());
        reset_model_state(state, now);
    }
    let cleared = clear_cooldown_state_for_auth(auth, now);
    if cleared {
        if models.is_empty() {
            models.extend(registered.iter().cloned());
        }
    } else if !auth.model_states.is_empty() {
        update_aggregated_availability(auth, now);
    }
    if models.is_empty() {
        models.extend(registered.iter().cloned());
    }
    let models = dedupe_strings(models);
    if !auth.disabled
        && auth.status != Status::Disabled
        && !has_model_error(auth, now)
        && !has_unauthorized_auth_failure(auth)
    {
        auth.last_error = None;
        auth.status_message.clear();
        auth.status = Status::Active;
    }
    auth.updated_at = Some(now);
    (models, cleared)
}

/// Aligns a credential's model states with its registered models: active
/// cooldowns stay, stale errors reset, states for models it no longer
/// serves go, and legacy alias states move to their target (upstream's
/// `ReconcileRegistryModelStates`, without the lock). Returns the new
/// states and whether they changed.
pub(crate) fn reconcile_model_states(
    resolver: Resolver<'_>,
    auth: &Auth,
    supported: &[String],
    now: Timestamp,
) -> (BTreeMap<String, ModelState>, bool) {
    let mut states = auth.model_states.clone();
    let mut changed = normalize_model_states(&mut states);
    let target_key = |route_id: &str| {
        let key = resolver.selection_model_key_for_auth(auth, route_id);
        if key.is_empty() {
            canonical_model_key(route_id)
        } else {
            key
        }
    };
    if !states.is_empty() && !supported.is_empty() {
        let mut authoritative: HashSet<String> = HashSet::new();
        let mut route_to_target: BTreeMap<String, String> = BTreeMap::new();
        for model in supported {
            let route_id = model.trim();
            if route_id.is_empty() {
                continue;
            }
            let canonical_route = canonical_model_key(route_id);
            let target = target_key(route_id);
            if !target.is_empty() {
                authoritative.insert(target.clone());
            }
            if !canonical_route.is_empty() {
                route_to_target.insert(canonical_route, target);
            }
        }
        for model in supported {
            let route_id = model.trim();
            if route_id.is_empty() {
                continue;
            }
            let canonical_route = canonical_model_key(route_id);
            let target = route_to_target
                .get(&canonical_route)
                .cloned()
                .unwrap_or_default();
            if target.is_empty() || canonical_route.is_empty() {
                continue;
            }
            if canonical_route == target || authoritative.contains(&canonical_route) {
                continue;
            }
            let mut alias_keys = vec![canonical_route.clone()];
            if route_id != canonical_route {
                alias_keys.push(route_id.to_owned());
            }
            for alias in alias_keys {
                let Some(state) = states.remove(&alias) else {
                    continue;
                };
                if !model_state_is_clean(&state) || is_model_state_active_cooldown(&state, now) {
                    match states.get_mut(&target) {
                        Some(existing) => merge_model_state(existing, &state),
                        None => {
                            states.insert(target.clone(), state);
                        }
                    }
                }
                changed = true;
            }
        }
    }
    let supported_keys: HashSet<String> = supported
        .iter()
        .filter(|model| !model.trim().is_empty())
        .map(|model| target_key(model))
        .filter(|key| !key.is_empty())
        .collect();
    let keys: Vec<String> = states.keys().cloned().collect();
    for key in keys {
        let mut base = canonical_model_key(&key);
        if base.is_empty() {
            base = key.trim().to_owned();
        }
        if !supported_keys.contains(&base) {
            states.remove(&key);
            changed = true;
            continue;
        }
        let Some(state) = states.get_mut(&key) else {
            continue;
        };
        if model_state_is_clean(state) || is_model_state_active_cooldown(state, now) {
            continue;
        }
        reset_model_state(state, now);
        changed = true;
    }
    (states, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap_or_default()
    }

    #[test]
    fn next_quota_cooldown_doubles_until_the_cap() {
        assert_eq!(next_quota_cooldown(0, false), (Duration::from_secs(1), 1));
        assert_eq!(next_quota_cooldown(3, false), (Duration::from_secs(8), 4));
        assert_eq!(next_quota_cooldown(11, false), (QUOTA_BACKOFF_MAX, 11));
        assert_eq!(next_quota_cooldown(200, false), (QUOTA_BACKOFF_MAX, 200));
        assert_eq!(next_quota_cooldown(4, true), (Duration::ZERO, 4));
    }

    #[test]
    fn quota_cooldown_reuses_an_open_window() {
        let open = QuotaState {
            exceeded: true,
            next_recover_at: Some(add(now(), Duration::from_secs(5))),
            backoff_level: 3,
            ..QuotaState::default()
        };
        assert_eq!(
            quota_cooldown_after_failure(&open, now()),
            (open.next_recover_at, 3)
        );
        let closed = QuotaState {
            backoff_level: 2,
            ..QuotaState::default()
        };
        assert_eq!(
            quota_cooldown_after_failure(&closed, now()),
            (Some(add(now(), Duration::from_secs(4))), 3)
        );
    }

    #[test]
    fn merge_model_state_keeps_the_stricter_cooldown() {
        let early = Some(now());
        let late = Some(add(now(), Duration::from_secs(60)));
        let mut target = ModelState {
            status: Status::Active,
            next_retry_after: late,
            updated_at: early,
            ..ModelState::default()
        };
        let source = ModelState {
            status: Status::Active,
            status_message: "boom".into(),
            unavailable: true,
            next_retry_after: early,
            updated_at: late,
            ..ModelState::default()
        };
        merge_model_state(&mut target, &source);
        assert_eq!(target.next_retry_after, late);
        assert_eq!(target.status_message, "boom");
        assert!(target.unavailable);
        assert_eq!(target.status, Status::Error);
        assert_eq!(target.updated_at, late);
    }

    #[test]
    fn aggregate_quota_recover_is_the_earliest_non_zero() {
        let mut auth = Auth::default();
        let t1 = Some(add(now(), Duration::from_secs(30)));
        let t2 = Some(add(now(), Duration::from_secs(90)));
        for (name, at) in [("a", None), ("b", t2), ("c", t1)] {
            auth.model_states.insert(
                name.into(),
                ModelState {
                    quota: QuotaState {
                        exceeded: true,
                        next_recover_at: at,
                        backoff_level: 2,
                        ..QuotaState::default()
                    },
                    ..ModelState::default()
                },
            );
        }
        update_aggregated_availability(&mut auth, now());
        assert!(auth.quota.exceeded);
        assert_eq!(auth.quota.reason, "quota");
        assert_eq!(auth.quota.next_recover_at, t1);
        assert_eq!(auth.quota.backoff_level, 2);
        assert!(!auth.unavailable);
    }
}
