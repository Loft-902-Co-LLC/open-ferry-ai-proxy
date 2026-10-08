//! open-ferry's cap on long quota rests (`routing.quota.check-after`).
//!
//! A quota answer, a 429 the cooldowns read as a quota, rests a credential
//! or one of its models until the provider's reset, which can be days
//! away. With the cap on, a rest longer than the cap lasts the cap instead,
//! and then one call is let through to check. While that check is in
//! flight no other call picks the credential for that rest. A success ends
//! the rest. Another quota answer doubles the wait, never past the
//! provider's reset: a wait that would reach the reset becomes the rest the
//! cooldowns gave, and from the reset on their own rules apply. Any other
//! outcome, or a check that never reports one (a client that left a
//! stream), lets the next call check.
//!
//! The rests are kept in a table beside the credentials, by credential and
//! scope: the whole credential (a credential-wide quota, or a quota answer
//! with no model) or one model, by its canonical key. The cooldowns are
//! written as upstream writes them; afterwards, the times of the rest that
//! fall between the check and the provider's reset are brought back to the
//! check. So the selector, the cooldown views and the saved cooldowns all
//! see the check time, and the table keeps the provider's reset. A
//! check claims its rest when its credential is picked: a lease then holds
//! the scope until the provider's reset. The lease is taken off before the
//! check's outcome is applied, so the cooldowns read the provider's answer
//! as they would without the cap, and put back after it while the check is
//! still in flight.
//!
//! The table keeps each rest's wait, check time and provider reset, and the
//! claim of a check in flight. `save-cooldown-status` saves the first three
//! with the cooldown they belong to (see `cooldown_store`), so a restart
//! keeps the doubling; a check in flight at the save is due at once after
//! the restart. A credential's rests are forgotten when it is removed,
//! registered again or reloaded from the store, as its cooldowns are
//! replaced then. Turning the cap off forgets the rests as each
//! credential's next outcome is recorded, and stops claims and saves at
//! once; the times already brought back stay.
//!
//! Deviations from upstream:
//! - The whole module is open-ferry's own. Upstream rests a credential
//!   until the provider's reset, has no setting for a cap, and ignores
//!   `routing.quota`. There is no parity suite, as upstream has nothing to
//!   compare with; `tests/quota_check.rs` runs the manager against fake
//!   executors.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use super::cooldown::{CallResult, add};
use super::cooldown_store::QuotaCheckRecord;
use super::text::canonical_model_key;
use super::{Entry, Manager, Settings, Shared, State, lock};
use crate::auth::{Auth, ModelState, Timestamp};

/// The scope of a rest of the whole credential.
const CREDENTIAL: &str = "";
const QUOTA: &str = "quota";
const CREDENTIAL_QUOTA: &str = "credential_quota";

/// The capped rests, by credential ID and scope.
#[derive(Debug, Default)]
pub(crate) struct QuotaChecks {
    rests: BTreeMap<(String, String), Rest>,
    last_claim: u64,
}

/// One capped rest.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rest {
    /// How long the rest lasts from the last quota answer.
    wait: Duration,
    /// When one call is let through to check.
    check_at: Timestamp,
    /// The provider's reset: the end of the rest the cooldowns gave.
    reset_at: Timestamp,
    /// The check in flight.
    claim: Option<Claim>,
}

/// A check in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Claim {
    id: u64,
    /// The lease's end and what it replaced, while it holds.
    lease: Option<(Timestamp, Saved)>,
}

/// What a lease replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Saved {
    Credential {
        unavailable: bool,
        next_retry_after: Option<Timestamp>,
        exceeded: bool,
        reason: String,
        next_recover_at: Option<Timestamp>,
    },
    /// Each model state's name, `unavailable` and `next_retry_after`.
    Models(Vec<(String, bool, Option<Timestamp>)>),
}

/// A capped quota rest, as the management API shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaCheck {
    /// The model's canonical key, or empty for the whole credential.
    pub model_key: String,
    /// When one call is next let through to check; a check in flight has
    /// its start's due time.
    pub next_check_at: Timestamp,
    /// The provider's reset, from which the cap no longer applies.
    pub provider_reset_at: Timestamp,
    /// The rest's length since the last quota answer.
    pub wait: Duration,
    /// Whether a check is in flight.
    pub checking: bool,
}

/// A check let through: dropping it, once the check's outcome is recorded
/// or the call is gone, lets the next call check if the outcome didn't
/// settle the rest.
pub(crate) struct CheckClaim {
    shared: Weak<Shared>,
    auth_id: String,
    scopes: Vec<String>,
    id: u64,
}

impl Drop for CheckClaim {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let now = (shared.clock)();
        let mut guard = lock(&shared.state);
        let state = &mut *guard;
        if state
            .quota_checks
            .release(&mut state.auths, &self.auth_id, &self.scopes, self.id)
        {
            state.sync_scheduler(shared.models.as_ref(), &self.auth_id, now);
        }
    }
}

/// Whether a quota state is a quota rest.
fn is_quota_rest(exceeded: bool, reason: &str) -> bool {
    exceeded && (reason == QUOTA || reason == CREDENTIAL_QUOTA)
}

/// Whether the credential's own quota fields are part of `scope`'s rest: a
/// credential's own rest, or, for a model, the rest its models' make up.
fn auth_in_scope(auth: &Auth, scope: &str) -> bool {
    if scope == CREDENTIAL {
        is_quota_rest(auth.quota.exceeded, &auth.quota.reason)
    } else {
        auth.quota.exceeded && auth.quota.reason == QUOTA
    }
}

/// Whether a model's state is part of `scope`'s rest.
fn state_in_scope(name: &str, state: &ModelState, scope: &str) -> bool {
    is_quota_rest(state.quota.exceeded, &state.quota.reason)
        && (scope == CREDENTIAL || canonical_model_key(name) == scope)
}

/// The end of `scope`'s rest on `auth`, if after `now`. A model's rest is
/// read from its states alone: the credential's own fields gather all its
/// models'.
fn rest_end(auth: &Auth, scope: &str, now: Timestamp) -> Option<Timestamp> {
    let mut times = Vec::new();
    if scope == CREDENTIAL && auth_in_scope(auth, scope) {
        times.extend([auth.next_retry_after, auth.quota.next_recover_at]);
    }
    for (name, state) in &auth.model_states {
        if state_in_scope(name, state, scope) {
            times.extend([state.next_retry_after, state.quota.next_recover_at]);
        }
    }
    times.into_iter().flatten().filter(|t| *t > now).max()
}

/// Brings the times of `scope`'s rest that fall after `check_at`, up to
/// `end`, back to `check_at`.
fn retime(auth: &mut Auth, scope: &str, check_at: Timestamp, end: Timestamp) {
    let pull = |time: &mut Option<Timestamp>| {
        if time.is_some_and(|t| t > check_at && t <= end) {
            *time = Some(check_at);
        }
    };
    if auth_in_scope(auth, scope) {
        pull(&mut auth.next_retry_after);
        pull(&mut auth.quota.next_recover_at);
    }
    for (name, state) in &mut auth.model_states {
        if state_in_scope(name, state, scope) {
            pull(&mut state.next_retry_after);
            pull(&mut state.quota.next_recover_at);
        }
    }
}

/// Holds `scope` on `auth` until `until`, and returns what that replaced:
/// for the credential, a credential-wide quota; for a model, its states'
/// cooldowns.
fn lease(auth: &mut Auth, scope: &str, until: Timestamp) -> Saved {
    if scope == CREDENTIAL {
        let saved = Saved::Credential {
            unavailable: auth.unavailable,
            next_retry_after: auth.next_retry_after,
            exceeded: auth.quota.exceeded,
            reason: auth.quota.reason.clone(),
            next_recover_at: auth.quota.next_recover_at,
        };
        auth.unavailable = true;
        auth.next_retry_after = Some(until);
        auth.quota.exceeded = true;
        CREDENTIAL_QUOTA.clone_into(&mut auth.quota.reason);
        auth.quota.next_recover_at = Some(until);
        return saved;
    }
    let mut saved = Vec::new();
    for (name, state) in &mut auth.model_states {
        if canonical_model_key(name) == scope {
            saved.push((name.clone(), state.unavailable, state.next_retry_after));
            state.unavailable = true;
            state.next_retry_after = Some(until);
        }
    }
    Saved::Models(saved)
}

/// Takes a lease ending at `until` off `auth`, where nothing replaced it
/// since.
fn unlease(auth: &mut Auth, until: Timestamp, saved: Saved) {
    match saved {
        Saved::Credential {
            unavailable,
            next_retry_after,
            exceeded,
            reason,
            next_recover_at,
        } => {
            if auth.next_retry_after == Some(until) {
                auth.unavailable = unavailable;
                auth.next_retry_after = next_retry_after;
            }
            if auth.quota.next_recover_at == Some(until) && auth.quota.reason == CREDENTIAL_QUOTA {
                auth.quota.exceeded = exceeded;
                auth.quota.reason = reason;
                auth.quota.next_recover_at = next_recover_at;
            }
        }
        Saved::Models(states) => {
            for (name, unavailable, next_retry_after) in states {
                if let Some(state) = auth.model_states.get_mut(&name)
                    && state.next_retry_after == Some(until)
                {
                    state.unavailable = unavailable;
                    state.next_retry_after = next_retry_after;
                }
            }
        }
    }
}

/// The scope a result's quota answer rests.
fn scope_of(result: &CallResult, model_key: &str) -> String {
    let key = canonical_model_key(model_key);
    if result.credential_scope || key.is_empty() {
        CREDENTIAL.to_owned()
    } else {
        key
    }
}

/// `to - from`, or zero if `to` isn't later.
fn between(from: Timestamp, to: Timestamp) -> Duration {
    (to - from).to_std().unwrap_or_default()
}

impl QuotaChecks {
    fn key(id: &str, scope: &str) -> (String, String) {
        (id.to_owned(), scope.to_owned())
    }

    /// The scopes resting on credential `id`.
    fn scopes(&self, id: &str) -> Vec<String> {
        self.rests
            .range(Self::key(id, CREDENTIAL)..)
            .take_while(|((rest_id, _), _)| rest_id == id)
            .map(|((_, scope), _)| scope.clone())
            .collect()
    }

    /// Forgets credential `id`'s rests.
    pub(crate) fn forget(&mut self, id: &str) {
        for scope in self.scopes(id) {
            self.rests.remove(&Self::key(id, &scope));
        }
    }

    /// Forgets credential `id`'s rests that reached the provider's reset.
    fn forget_ended(&mut self, id: &str, now: Timestamp) {
        for scope in self.scopes(id) {
            let key = Self::key(id, &scope);
            if self
                .rests
                .get(&key)
                .is_some_and(|rest| now >= rest.reset_at)
            {
                self.rests.remove(&key);
            }
        }
    }

    /// Takes the leases of `auth`'s checks in flight off, before an outcome
    /// is applied to it.
    pub(crate) fn before_result(&mut self, auth: &mut Auth) {
        for scope in self.scopes(&auth.id) {
            let lease = self
                .rests
                .get_mut(&Self::key(&auth.id, &scope))
                .and_then(|rest| rest.claim.as_mut())
                .and_then(|claim| claim.lease.take());
            if let Some((until, saved)) = lease {
                unlease(auth, until, saved);
            }
        }
    }

    /// Caps, doubles or ends `auth`'s rests by the outcome the cooldowns
    /// just applied, and puts the leases of checks still in flight back.
    pub(crate) fn after_result(
        &mut self,
        cap: Duration,
        auth: &mut Auth,
        result: &CallResult,
        model_key: &str,
        now: Timestamp,
    ) {
        if cap.is_zero() {
            self.forget(&auth.id);
            return;
        }
        self.forget_ended(&auth.id, now);
        let scope = scope_of(result, model_key);
        if result.success {
            self.rests.remove(&Self::key(&auth.id, CREDENTIAL));
            self.rests.remove(&Self::key(&auth.id, &scope));
        } else if result
            .error
            .as_ref()
            .is_some_and(|err| err.http_status == 429)
        {
            // A check's answer is its rest's, whatever scope it gave.
            let mut scopes: Vec<String> = self
                .scopes(&auth.id)
                .into_iter()
                .filter(|claimed| {
                    self.rests
                        .get(&Self::key(&auth.id, claimed))
                        .is_some_and(|rest| rest.claim.is_some())
                })
                .collect();
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
            for scope in scopes {
                self.rest_again(cap, auth, &scope, now);
            }
        }
        for scope in self.scopes(&auth.id) {
            if let Some(rest) = self.rests.get_mut(&Self::key(&auth.id, &scope))
                && let Some(claim) = rest.claim.as_mut()
                && claim.lease.is_none()
            {
                claim.lease = Some((rest.reset_at, lease(auth, &scope, rest.reset_at)));
            }
        }
    }

    /// Applies a quota answer to `scope`'s rest on `auth`.
    fn rest_again(&mut self, cap: Duration, auth: &mut Auth, scope: &str, now: Timestamp) {
        let key = Self::key(&auth.id, scope);
        let Some(end) = rest_end(auth, scope, now) else {
            self.rests.remove(&key);
            return;
        };
        let left = between(now, end);
        if left <= cap {
            self.rests.remove(&key);
            return;
        }
        let rest = match self.rests.get(&key) {
            None => Rest {
                wait: cap,
                check_at: add(now, cap),
                reset_at: end,
                claim: None,
            },
            Some(rest) if rest.claim.is_some() || now >= rest.check_at => {
                let wait = rest.wait.saturating_mul(2);
                if wait >= left {
                    self.rests.remove(&key);
                    return;
                }
                Rest {
                    wait,
                    check_at: add(now, wait),
                    reset_at: end,
                    claim: None,
                }
            }
            Some(rest) => {
                if end <= rest.check_at {
                    self.rests.remove(&key);
                    return;
                }
                Rest {
                    reset_at: end,
                    ..rest.clone()
                }
            }
        };
        tracing::debug!(
            model = scope,
            wait_secs = rest.wait.as_secs(),
            reset_in_secs = left.as_secs(),
            "quota rest capped"
        );
        retime(auth, scope, rest.check_at, end);
        self.rests.insert(key, rest);
    }

    /// Ends the claim `id` on credential `auth_id`'s `scopes`, taking its
    /// lease off. Returns whether a lease was taken off.
    fn release(
        &mut self,
        auths: &mut BTreeMap<String, Entry>,
        auth_id: &str,
        scopes: &[String],
        id: u64,
    ) -> bool {
        let mut released = false;
        for scope in scopes {
            let Some(rest) = self.rests.get_mut(&Self::key(auth_id, scope)) else {
                continue;
            };
            if rest.claim.as_ref().is_none_or(|claim| claim.id != id) {
                continue;
            }
            if let Some(Claim {
                lease: Some((until, saved)),
                ..
            }) = rest.claim.take()
                && let Some(entry) = auths.get_mut(auth_id)
            {
                unlease(Arc::make_mut(&mut entry.auth), until, saved);
                released = true;
            }
        }
        released
    }

    /// The rests to save with the cooldowns, by credential and scope: none
    /// while the cap is off, and only those short of the provider's reset.
    pub(crate) fn records(
        &self,
        settings: &Settings,
        now: Timestamp,
    ) -> BTreeMap<(String, String), QuotaCheckRecord> {
        if settings.quota_check_after.is_zero() {
            return BTreeMap::new();
        }
        self.rests
            .iter()
            .filter(|(_, rest)| now < rest.reset_at)
            .map(|(key, rest)| {
                let record = QuotaCheckRecord {
                    wait: rest.wait,
                    check_at: rest.check_at,
                    reset_at: rest.reset_at,
                };
                (key.clone(), record)
            })
            .collect()
    }

    /// Puts a saved rest back on `auth` once the cooldown of `model` (empty
    /// for the credential's own) it was saved with is restored. The times
    /// of that rest between its check and the provider's reset, those of a
    /// check in flight at the save, are brought back to the check, so the
    /// check is due then. A rest already in the table, or one with no quota
    /// rest under it, is left as it is.
    pub(crate) fn restore(
        &mut self,
        settings: &Settings,
        auth: &mut Auth,
        model: &str,
        record: &QuotaCheckRecord,
        now: Timestamp,
    ) {
        if settings.quota_check_after.is_zero()
            || record.reset_at <= now
            || record.check_at >= record.reset_at
        {
            return;
        }
        let scope = canonical_model_key(model);
        let key = Self::key(&auth.id, &scope);
        if self.rests.contains_key(&key) || rest_end(auth, &scope, now).is_none() {
            return;
        }
        retime(auth, &scope, record.check_at, record.reset_at);
        let rest = Rest {
            wait: record.wait.max(Duration::from_secs(1)),
            check_at: record.check_at,
            reset_at: record.reset_at,
            claim: None,
        };
        self.rests.insert(key, rest);
    }

    /// Credential `id`'s rests, for the management API.
    fn view(&self, id: &str, now: Timestamp) -> Vec<QuotaCheck> {
        self.rests
            .range(Self::key(id, CREDENTIAL)..)
            .take_while(|((rest_id, _), _)| rest_id == id)
            .filter(|(_, rest)| now < rest.reset_at)
            .map(|((_, scope), rest)| QuotaCheck {
                model_key: scope.clone(),
                next_check_at: rest.check_at,
                provider_reset_at: rest.reset_at,
                wait: rest.wait,
                checking: rest.claim.is_some(),
            })
            .collect()
    }
}

/// Claims the checks due on credential `id` for the scopes a call would
/// rest under: the whole credential, and `model_keys`, the models it would
/// try. Each one claimed is leased until its provider's reset, so no other
/// call picks it, until the returned claim is dropped.
pub(crate) fn claim(
    state: &mut State,
    shared: &Arc<Shared>,
    id: &str,
    model_keys: impl IntoIterator<Item = String>,
    now: Timestamp,
) -> Option<Arc<CheckClaim>> {
    if state.settings.quota_check_after.is_zero() {
        return None;
    }
    let checks = &mut state.quota_checks;
    checks.forget_ended(id, now);
    let mut scopes = vec![CREDENTIAL.to_owned()];
    for key in model_keys {
        let key = canonical_model_key(&key);
        if !key.is_empty() && !scopes.contains(&key) {
            scopes.push(key);
        }
    }
    scopes.retain(|scope| {
        checks
            .rests
            .get(&QuotaChecks::key(id, scope))
            .is_some_and(|rest| rest.claim.is_none() && now >= rest.check_at)
    });
    if scopes.is_empty() {
        return None;
    }
    let auth = Arc::make_mut(&mut state.auths.get_mut(id)?.auth);
    checks.last_claim = checks.last_claim.wrapping_add(1);
    let claim_id = checks.last_claim;
    let mut claimed = Vec::new();
    for scope in scopes {
        let key = QuotaChecks::key(id, &scope);
        let Some(rest) = checks.rests.get_mut(&key) else {
            continue;
        };
        let saved = lease(auth, &scope, rest.reset_at);
        if matches!(&saved, Saved::Models(states) if states.is_empty()) {
            // The model's state is gone, and the rest with it.
            checks.rests.remove(&key);
            continue;
        }
        rest.claim = Some(Claim {
            id: claim_id,
            lease: Some((rest.reset_at, saved)),
        });
        claimed.push(scope);
    }
    if claimed.is_empty() {
        return None;
    }
    tracing::debug!(scopes = claimed.len(), "quota check let through");
    state.sync_scheduler(shared.models.as_ref(), id, now);
    Some(Arc::new(CheckClaim {
        shared: Arc::downgrade(shared),
        auth_id: id.to_owned(),
        scopes: claimed,
        id: claim_id,
    }))
}

impl Manager {
    /// Credential `id`'s capped quota rests (`routing.quota.check-after`,
    /// open-ferry's own): when each next lets a call through to check.
    /// Empty while the cap is off.
    pub fn quota_checks(&self, id: &str) -> Vec<QuotaCheck> {
        let now = self.now();
        let state = self.lock();
        if state.settings.quota_check_after.is_zero() || !state.auths.contains_key(id) {
            return Vec::new();
        }
        state.quota_checks.view(id, now)
    }
}
