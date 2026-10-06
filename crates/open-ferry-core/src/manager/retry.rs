// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_selection.go (the
// request retry rounds: retrySettings through waitForCooldown) (v8.0.15,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Retry rounds: after every credential in a round failed, whether to start
//! another round, and how long to wait for a cooldown to end first.
//!
//! A round may start again when the error is one a credential can recover
//! from (403, 408, 429, 5xx gateway errors, or a transport failure), some
//! credential still has rounds left under `request_retry` (or its own
//! override), and that credential is ready or recovers within
//! `max_retry_interval`.
//!
//! Deviations from upstream:
//! - Of upstream's eligibility filters only the free-plan rule is ported
//!   (see `policy`): required auth kinds aren't, and a credential policy
//!   only narrows its own pick, which has no retry rounds.
//! - Credentials are checked in ID order, where Go's map order is random;
//!   only the shortest wait is kept, so the outcome is the same.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use super::Entry;
use super::classify::{
    ErrView, is_request_invalid_error, is_transient_transport_error, retry_after_from_error,
};
use super::cooldown::{MIN_QUOTA_COOLDOWN_FLOOR, cooldown_disabled_for_auth};
use super::credential::{is_zero, request_retry_override};
use super::policy::Eligibility;
use super::select::{
    BlockReason, Selection, availability_block, executor_key_from_auth, is_auth_blocked_for_model,
};
use super::text::{canonical_model_key, go_lower};
use crate::auth::{Auth, AuthError, Status, Timestamp};
use crate::exec::ExecError;

/// The most random delay added to a cooldown wait
/// (upstream's `cooldownWaitJitterCap`).
const COOLDOWN_WAIT_JITTER_CAP: Duration = Duration::from_secs(2);

/// How many extra rounds a credential takes: its own `request_retry`, else
/// the default (upstream's `effectiveRequestRetryLimit`).
pub(crate) fn effective_request_retry_limit(auth: &Auth, default_retry: usize) -> i64 {
    request_retry_override(auth).unwrap_or_else(|| i64::try_from(default_retry).unwrap_or(i64::MAX))
}

/// The credentials a round leaves out because they have no rounds left
/// (upstream's `requestRetryRoundExclusions`).
pub(crate) fn request_retry_round_exclusions(
    auths: &BTreeMap<String, Entry>,
    round: usize,
    default_retry: usize,
) -> HashSet<String> {
    let mut excluded = HashSet::new();
    if round == 0 {
        return excluded;
    }
    let round = i64::try_from(round).unwrap_or(i64::MAX);
    for entry in auths.values() {
        let auth = &entry.auth;
        if auth.id.trim().is_empty() {
            continue;
        }
        if effective_request_retry_limit(auth, default_retry) < round {
            excluded.insert(auth.id.clone());
        }
    }
    excluded
}

/// Whether a status is one a credential can recover from in a later round
/// (upstream's `isCredentialRetryRoundStatus`).
pub(crate) fn is_credential_retry_round_status(status: u16) -> bool {
    matches!(status, 403 | 408 | 429 | 500 | 502 | 503 | 504)
}

/// Whether an error may start another round (upstream's
/// `isRequestRetryRoundError`).
pub(crate) fn is_request_retry_round_error(err: &ExecError) -> bool {
    is_credential_retry_round_status(err.status) || is_transient_transport_error(ErrView::Exec(err))
}

/// Upstream's `credentialRetryRoundStateEligible`.
fn credential_retry_round_state_eligible(
    last_error: Option<&AuthError>,
    quota_exceeded: bool,
) -> bool {
    match last_error {
        None => quota_exceeded,
        Some(err) => is_credential_retry_round_status(err.http_status),
    }
}

fn credential_quota_active(auth: &Auth, now: Timestamp) -> bool {
    auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && auth.quota.next_recover_at.is_some_and(|t| t > now)
}

/// Whether a credential can take a later round for `model`, and when it
/// recovers if it is cooling down (upstream's
/// `retryRoundAvailabilityForAuth`).
pub(crate) fn retry_round_availability_for_auth(
    auth: &Auth,
    model: &str,
    now: Timestamp,
) -> (bool, Option<Timestamp>) {
    let (blocked, reason, next) = is_auth_blocked_for_model(auth, model, now);
    if !blocked {
        return (true, None);
    }
    if is_zero(next) || reason == BlockReason::Disabled {
        return (false, None);
    }
    if credential_quota_active(auth, now) {
        return (
            credential_retry_round_state_eligible(auth.last_error.as_ref(), true),
            next,
        );
    }
    let model_key = canonical_model_key(model);
    if !model_key.is_empty() && !auth.model_states.is_empty() {
        let mut matched_blocked = false;
        for (state_model, state) in &auth.model_states {
            if canonical_model_key(state_model) != model_key {
                continue;
            }
            if state.status == Status::Disabled {
                return (false, None);
            }
            let (state_blocked, _, state_next) = availability_block(
                state.unavailable,
                state.quota.exceeded,
                state.next_retry_after,
                state.quota.next_recover_at,
                now,
            );
            if !state_blocked {
                continue;
            }
            matched_blocked = true;
            if is_zero(state_next)
                || !credential_retry_round_state_eligible(
                    state.last_error.as_ref(),
                    state.quota.exceeded,
                )
            {
                return (false, None);
            }
        }
        if matched_blocked {
            return (true, next);
        }
    }
    if !credential_retry_round_state_eligible(auth.last_error.as_ref(), auth.quota.exceeded) {
        return (false, None);
    }
    (true, next)
}

/// What a retry decision looks at: the call's providers, model and pinned
/// credential, the round just finished, and the credentials it tried.
pub(crate) struct RetryQuery<'a> {
    /// The call's normalized providers.
    pub(crate) providers: &'a [String],
    /// The model credentials are picked by.
    pub(crate) model: &'a str,
    /// The pinned credential, or empty.
    pub(crate) pinned: &'a str,
    /// The round that just failed, from 0.
    pub(crate) attempt: usize,
    /// The default number of extra rounds.
    pub(crate) default_retry: usize,
    /// The credentials the failed round tried.
    pub(crate) attempted: &'a HashSet<String>,
    /// What else narrows the credentials the call may pick.
    pub(crate) eligibility: Eligibility,
}

impl RetryQuery<'_> {
    fn provider_set(&self) -> HashSet<String> {
        self.providers
            .iter()
            .map(|p| go_lower(p.trim()))
            .filter(|p| !p.is_empty())
            .collect()
    }

    /// The credentials a later round could use, with the model to check
    /// each against (the filters shared by upstream's `retryAllowed` and
    /// `closestCooldownWaitWithAttempted`).
    fn candidates<'s>(
        &'s self,
        selection: &'s Selection<'s>,
        provider_set: &'s HashSet<String>,
    ) -> impl Iterator<Item = (&'s Auth, String)> + 's {
        let attempt = i64::try_from(self.attempt).unwrap_or(i64::MAX);
        selection.auths.values().filter_map(move |entry| {
            let auth: &Auth = &entry.auth;
            if auth.disabled || auth.status == Status::Disabled {
                return None;
            }
            if !self.pinned.is_empty() && auth.id != self.pinned {
                return None;
            }
            if !self.eligibility.allows(auth) {
                return None;
            }
            if !provider_set.contains(&executor_key_from_auth(auth)) {
                return None;
            }
            if !self.model.is_empty() && !selection.auth_supports_route_model(auth, self.model) {
                return None;
            }
            if attempt >= effective_request_retry_limit(auth, self.default_retry) {
                return None;
            }
            let check_model = if self.model.trim().is_empty() {
                self.model.to_owned()
            } else {
                selection
                    .resolver
                    .selection_model_for_auth(auth, self.model)
            };
            Some((auth, check_model))
        })
    }
}

/// Whether any credential could take another round (upstream's
/// `retryAllowed`).
fn retry_allowed(selection: &Selection<'_>, query: &RetryQuery<'_>) -> bool {
    if query.providers.is_empty() {
        return false;
    }
    let provider_set = query.provider_set();
    if provider_set.is_empty() {
        return false;
    }
    query
        .candidates(selection, &provider_set)
        .any(|(auth, model)| retry_round_availability_for_auth(auth, &model, selection.now).0)
}

/// The shortest wait until a credential could take another round, if any
/// could (upstream's `closestCooldownWaitWithAttempted`). A credential the
/// failed round tried after a 429 waits at least the quota floor, unless
/// cooling is off for it.
pub(super) fn closest_cooldown_wait(
    selection: &Selection<'_>,
    query: &RetryQuery<'_>,
    status: u16,
) -> Option<Duration> {
    if query.providers.is_empty() {
        return None;
    }
    let now = selection.now;
    let settings = selection.resolver.settings;
    let provider_set = query.provider_set();
    let mut min_wait: Option<Duration> = None;
    let mut offer = |wait: Duration| {
        if min_wait.is_none_or(|min| wait < min) {
            min_wait = Some(wait);
        }
    };
    for (auth, check_model) in query.candidates(selection, &provider_set) {
        let (eligible, next) = retry_round_availability_for_auth(auth, &check_model, now);
        if !eligible {
            continue;
        }
        let next = next.filter(|_| !is_zero(next));
        let was_attempted = query.attempted.contains(&auth.id);
        if !was_attempted || cooldown_disabled_for_auth(settings, auth) || status != 429 {
            let Some(next) = next else {
                return Some(Duration::ZERO);
            };
            // A wait already past is skipped, as upstream skips a negative one.
            let Ok(wait) = (next - now).to_std() else {
                continue;
            };
            offer(wait);
            continue;
        }
        // Tried in the round that just failed with 429, and cooling is on:
        // it must not trigger an immediate retry round.
        let wait = next
            .and_then(|next| (next - now).to_std().ok())
            .unwrap_or(Duration::ZERO)
            .max(MIN_QUOTA_COOLDOWN_FLOOR);
        offer(wait);
    }
    min_wait
}

/// Whether to start another round after `err`, and how long to wait first
/// (upstream's `shouldRetryAfterErrorWithAttempted`, without Home).
/// `max_wait` limits only positive waits; zero means no waiting, which still
/// allows a round that can start at once.
pub(crate) fn should_retry_after_error(
    selection: &Selection<'_>,
    query: &RetryQuery<'_>,
    err: &ExecError,
    max_wait: Duration,
) -> Option<Duration> {
    let status = err.status;
    if status == 200 {
        return None;
    }
    if is_request_invalid_error(ErrView::Exec(err)) {
        return None;
    }
    if !is_request_retry_round_error(err) || !retry_allowed(selection, query) {
        return None;
    }
    let too_long = |wait: Duration| !wait.is_zero() && (max_wait.is_zero() || wait > max_wait);
    if let Some(wait) = closest_cooldown_wait(selection, query, status) {
        return (!too_long(wait)).then_some(wait);
    }
    if let Some(retry_after) = retry_after_from_error(ErrView::Exec(err)) {
        return (!too_long(retry_after)).then_some(retry_after);
    }
    Some(Duration::ZERO)
}

/// A cooldown wait with up to a quarter (at most two seconds) of random
/// delay added, so waiting calls don't all wake at once; never past
/// `max_wait` (upstream's `jitteredCooldownWait`).
pub(crate) fn jittered_cooldown_wait(wait: Duration, max_wait: Duration) -> Duration {
    if wait.is_zero() {
        return wait;
    }
    let mut range = (wait / 4).min(COOLDOWN_WAIT_JITTER_CAP);
    if !max_wait.is_zero() && range > max_wait.saturating_sub(wait) {
        range = max_wait.saturating_sub(wait);
    }
    let nanos = u64::try_from(range.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return wait;
    }
    wait.saturating_add(Duration::from_nanos(rand::random_range(0..nanos)))
}

/// Sleeps for a jittered cooldown wait (upstream's `waitForCooldown`).
pub(crate) async fn wait_for_cooldown(wait: Duration, max_wait: Duration) {
    if wait.is_zero() {
        return;
    }
    tokio::time::sleep(jittered_cooldown_wait(wait, max_wait)).await;
}
