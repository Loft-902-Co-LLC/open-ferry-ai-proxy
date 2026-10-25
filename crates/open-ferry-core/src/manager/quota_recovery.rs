//! A confirmed upstream usage reset ends a stale quota cooldown (open-ferry
//! issue #8).
//!
//! A Claude 429 for a weekly or session limit cools a credential down until
//! the reset its headers name, or a computed backoff. Nothing here ever
//! looked at the provider's own usage report again, so a limit that was
//! lifted early (a plan change, a reset the account got) kept the credential
//! out of rotation until the clock ran out.
//!
//! The rule is narrow on purpose. A fresh, successful read of the credential's
//! own usage ([`UsageReading`], taken by the management API from Claude's
//! OAuth usage endpoint) ends a cooldown only when:
//!
//! - the cooldown came from a Claude 429 whose headers named exactly which
//!   windows were rejected ([`Recoveries::observe`] freezes them at that
//!   moment; the quota snapshot is replaced by every later response, so it
//!   can't be consulted later);
//! - the read shows both base windows (`five_hour`, `seven_day`) and every
//!   window the 429 named as valid and below their limit;
//! - nothing else cooled the credential down since the 429, and the read
//!   began after it (see "Fences");
//! - the read is recent, by the response's own `Date`.
//!
//! Plans, display buckets and anything ambiguous prove nothing: a claim that
//! the headers can't name is never recorded, and a cooldown without a claim
//! (a legacy one restored from a `.cds` file, a 429 of another provider or
//! shape) is left to expire as before.
//!
//! Only the scope the 429 recorded is cleared. A credential-wide cooldown is
//! unwrapped from the model states beneath it, which come back with their own
//! limits; a model's own cooldown is dropped without touching the credential
//! wrap over it. The quota-check rests and leases of that scope go with it.
//! It never calls the broad manual reset, so other cooldowns stay.
//!
//! Independent holds, including a no-model 429 or 503, stay separate from a
//! model-derived credential aggregate. Each keeps its own deadline, not the
//! merged quota deadline; an unknown deadline fails closed. While the
//! credential has a hold of its own that is live (or whose end is unknown), a
//! model's recovery is refused and changes nothing: the credential's fields
//! are merged with its models', so taking the model out from under the hold
//! could not give the hold back as it was, and selection skips the credential
//! until the hold ends anyway. Once it ends (it expires, a success or a
//! manual reset clears it), a fresh read recovers the model. Reset, removal,
//! registration and every clearing of the cooldowns discard old holds while
//! preserving the fence against reads already in flight.
//!
//! # Fences
//!
//! A reading is applied only against a [`QuotaRecoveryToken`] taken before
//! the read started. It is refused when the credential was replaced or
//! re-registered since, when any failure that cooled it down was recorded
//! since the claim was (a newer 429, a 401, a 503 on any model), when the
//! saved cooldowns were restored or the credential reset, or when the
//! reading's `Date` predates the claim or the token, or when what shapes the
//! request (proxy, attributes such as the headers and base URL, the quota
//! probe) was edited since. A failure on one model fences that model's claim;
//! a credential-wide one (its own quota, a rejected token) fences them all.
//! A failure no claim can be told apart from (a 503, a 429 of unknown
//! windows) fences the claim but does not end it: the windows it names stay
//! owed until a later recognized 429 merges its own in, and for as long as
//! the cooldown layer they were recorded for stands.
//!
//! Claims and the record of what was recovered are runtime only. A cooldown
//! restored from the saved state after a restart has no claim, so it fails
//! closed. In process, a recovery leaves a mark that stops a restore of an
//! older saved cooldown (cooldown storage turned off, then on) from bringing
//! it back; an authoritative restore (a blue-green handover) clears the
//! marks and makes the file the authority. A crash before the debounced
//! store flushes the recovery can bring the saved cooldown back, as it can
//! any other change.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::TimeDelta;
use http::HeaderMap;
use sha2::{Digest, Sha256};

use super::classify::should_skip_credential_cooldown;
use super::cooldown::{
    CallResult, has_model_error, is_credential_quota_active, recoverable_failure_retry_after,
    reset_model_state, unwrap_expired_quota, update_aggregated_availability,
};
use super::lifecycle::Save;
use super::quota_signals::apply_cooldown_fields;
use super::text::canonical_model_key;
use super::{Manager, ManagerError, Settings};
use crate::auth::{Auth, QuotaState, Status, Timestamp};

/// The reading may not be older than this, or further ahead, by its `Date`.
const MAX_SKEW: TimeDelta = TimeDelta::seconds(120);
/// `Date` has a second's resolution.
const DATE_GRACE: TimeDelta = TimeDelta::seconds(1);

/// A usage window of the OAuth usage report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UsageWindow {
    FiveHour,
    SevenDay,
    SevenDaySonnet,
    SevenDayOpus,
}

/// One window of a usage reading, as the provider reported it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindowReading {
    /// Percent used, 0 to 100.
    pub utilization_percent: f64,
    /// When the window resets, if it said.
    pub resets_at: Option<Timestamp>,
}

/// A fresh read of a credential's own usage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReading {
    /// When the provider says it answered (its `Date`); a reading without
    /// one, or one a cache or proxy aged (`Age`), proves nothing.
    pub observed_at: Option<Timestamp>,
    /// The windows that were present and well formed; a missing or
    /// malformed one is left out.
    pub windows: BTreeMap<UsageWindow, UsageWindowReading>,
}

/// What a read was started against, taken before it began.
#[derive(Clone, Debug)]
pub struct QuotaRecoveryToken {
    auth_id: String,
    registration_epoch: u64,
    credential_version: u64,
    /// What shapes the read's request (see [`shaping`]): an edit of it keeps
    /// the credential version, and the read was made with the old one.
    shaping: [u8; 32],
    era: u64,
    seq: u64,
    captured_at: Timestamp,
}

impl QuotaRecoveryToken {
    /// Whether the token was taken for this very credential, as `auth`
    /// holds it (not one that replaced it since).
    pub fn matches(&self, auth: &Auth) -> bool {
        auth.id == self.auth_id
            && auth.registration_epoch == self.registration_epoch
            && auth.credential_version == self.credential_version
            && shaping(auth) == self.shaping
    }
}

/// A digest of what shapes the requests made with `auth` beyond its keys and
/// tokens, which the credential version covers: the proxy, the attributes
/// (the base URL and the extra headers among them) and the quota probe. Only
/// a digest is kept, as the token is logged and these hold secrets. The
/// cooldowns, counts and the like are runtime state, not part of it.
fn shaping(auth: &Auth) -> [u8; 32] {
    let mut digest = Sha256::new();
    let mut field = |text: &[u8]| {
        digest.update((text.len() as u64).to_le_bytes());
        digest.update(text);
    };
    field(auth.proxy_url.as_bytes());
    for (name, value) in &auth.attributes {
        field(name.as_bytes());
        field(value.as_bytes());
    }
    field(b"quota_probe");
    if let Some(probe) = auth.metadata.get("quota_probe") {
        field(probe.to_string().as_bytes());
    }
    // What Claude's executor reads to choose its authentication headers
    // (`claude/request.rs`, `uses_bearer` and `auth_kind`): the record's
    // `auth_kind`, and whether each field that implies OAuth holds anything.
    // Their values are not hashed (a refresh rewrites `expires_at`), only
    // what decides the choice; the token itself is `credential_version`'s.
    field(b"auth_kind");
    if let Some(kind) = auth.metadata_str("auth_kind") {
        field(kind.as_bytes());
    }
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ] {
        field(&[u8::from(
            auth.metadata_str(key)
                .is_some_and(|value| !value.trim().is_empty()),
        )]);
    }
    field(&[u8::from(
        auth.metadata
            .get("token")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|token| !token.is_empty()),
    )]);
    digest.finalize().into()
}

/// What a reading ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaRecovery {
    /// Whether the credential-wide cooldown ended.
    pub credential: bool,
    /// The canonical keys of the models whose own cooldown ended.
    pub models: Vec<String>,
}

impl QuotaRecovery {
    /// Whether anything ended.
    pub fn is_empty(&self) -> bool {
        !self.credential && self.models.is_empty()
    }
}

/// A 429's frozen provenance: the windows its headers rejected.
#[derive(Clone, Debug)]
struct Claim {
    windows: Vec<UsageWindow>,
    recorded_at: Timestamp,
    /// The sequence number of the failure that recorded it.
    seq: u64,
}

/// How long a cooldown no usage read can speak for lasts: until the time, or
/// for an unknown while (`None`, which fails closed).
type Hold = Option<Timestamp>;

/// The scope a failure applied to: whether it is the whole credential's, and
/// the model's canonical key (empty for the credential).
pub(crate) fn scope_of(result: &CallResult, model_key: &str) -> (bool, String) {
    let credential = result.credential_scope || canonical_model_key(model_key).is_empty();
    let scope = if credential {
        String::new()
    } else {
        canonical_model_key(model_key)
    };
    (credential, scope)
}

/// When the cooldowns of `scope` of `auth` end, the latest of them.
pub(crate) fn scope_deadline(auth: &Auth, scope: &str) -> Option<Timestamp> {
    if scope.is_empty() {
        return auth.next_retry_after.max(auth.quota.next_recover_at);
    }
    auth.model_states
        .iter()
        .filter(|(name, _)| canonical_model_key(name) == scope)
        .map(|(_, state)| state.next_retry_after.max(state.quota.next_recover_at))
        .max()
        .flatten()
}

/// What a failure that no usage read can speak for added to the scope's
/// cooldown: its own deadline, not the merged one (a later cooldown keeps an
/// earlier, longer one standing). `before` and `after` are the scope's
/// deadline around the failure.
fn own_hold(
    settings: &Settings,
    result: &CallResult,
    status: u16,
    now: Timestamp,
    before: Option<Timestamp>,
    after: Option<Timestamp>,
) -> Hold {
    match status {
        408 | 500 | 502 | 503 | 504 | 520..=526 => {
            // Cooling off leaves no hold at all.
            recoverable_failure_retry_after(settings, now, result.retry_after, false).or(Some(now))
        }
        _ => match after {
            None => Some(now),
            Some(after) if before.is_none_or(|before| after > before) => Some(after),
            // The failure's own time is hidden behind an older, longer one.
            Some(_) => None,
        },
    }
}

/// Runtime state of the recovery, kept with the manager's state.
#[derive(Debug, Default)]
pub(crate) struct Recoveries {
    /// By credential and scope (empty for the whole credential).
    claims: BTreeMap<(String, String), Claim>,
    /// Numbers the failures that touched cooldowns, across credentials.
    seq: u64,
    /// The last failure that touched one model's cooldowns.
    touched: BTreeMap<(String, String), u64>,
    /// The last failure that touched a whole credential's: its own quota, a
    /// rejected token, a reset, a replacement.
    touched_all: BTreeMap<String, u64>,
    /// The last credential-wide 429: it fences a read in flight, but leaves
    /// the claims of models, whose own layers it keeps, as they were.
    wrapped: BTreeMap<String, u64>,
    /// Scopes a failure no usage read can speak for put a cooldown on, with
    /// how long those cooldowns last (each failure's own contribution).
    foreign: BTreeMap<(String, String), Hold>,
    /// When a scope was last recovered, against a restore of older records.
    recovered: BTreeMap<(String, String), Timestamp>,
    /// Moves when the saved cooldowns replace the live ones.
    era: u64,
}

impl Recoveries {
    fn next(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    /// Notes a failure that touched the cooldowns of the whole credential.
    fn touch_all(&mut self, id: &str) -> u64 {
        let seq = self.next();
        self.touched_all.insert(id.to_owned(), seq);
        seq
    }

    /// Notes a failure that touched one model's cooldowns.
    fn touch(&mut self, id: &str, scope: &str) -> u64 {
        let seq = self.next();
        self.touched.insert((id.to_owned(), scope.to_owned()), seq);
        seq
    }

    /// Notes a credential-wide 429.
    fn touch_wrapped(&mut self, id: &str) -> u64 {
        let seq = self.next();
        self.wrapped.insert(id.to_owned(), seq);
        seq
    }

    fn touched_all(&self, id: &str) -> u64 {
        self.touched_all.get(id).copied().unwrap_or(0)
    }

    fn wrapped(&self, id: &str) -> u64 {
        self.wrapped.get(id).copied().unwrap_or(0)
    }

    /// Whether nothing touched `scope` of `id` after the claim `claim` was
    /// recorded. A credential-wide 429 ends the credential's claim and no
    /// model's.
    fn untouched_since(&self, id: &str, scope: &str, claim: &Claim) -> bool {
        let (model, wrapped) = if scope.is_empty() {
            (0, self.wrapped(id))
        } else {
            let key = (id.to_owned(), scope.to_owned());
            (self.touched.get(&key).copied().unwrap_or(0), 0)
        };
        self.touched_all(id) <= claim.seq && model <= claim.seq && wrapped <= claim.seq
    }

    /// Whether a cooldown no usage read can speak for is on `scope` of `id`
    /// at `now`, or may be.
    pub(crate) fn foreign_live(&self, id: &str, scope: &str, now: Timestamp) -> bool {
        self.foreign
            .get(&(id.to_owned(), scope.to_owned()))
            .is_some_and(|hold| hold.is_none_or(|until| until > now))
    }

    /// Adds a foreign cooldown to `key`: the longest of them counts, and an
    /// unknown one stays unknown.
    fn hold(&mut self, key: (String, String), hold: Hold) {
        let merged = match self.foreign.get(&key) {
            None => hold,
            Some(old) => (*old).zip(hold).map(|(old, new)| old.max(new)),
        };
        self.foreign.insert(key, merged);
    }

    /// Notes the outcome just applied to `auth`: any failure that touched its
    /// cooldowns fences the claims, and a Claude 429 records its own.
    /// `deadlines` is the deadline of the outcome's scope before and after
    /// it, as the outcome set them: not as the quota check's cap shortened
    /// them to its next check, which is no end of the provider's cooldown.
    pub(crate) fn observe(
        &mut self,
        settings: &Settings,
        auth: &Auth,
        result: &CallResult,
        model_key: &str,
        deadlines: (Option<Timestamp>, Option<Timestamp>),
        now: Timestamp,
    ) {
        let (before, after) = deadlines;
        // A claim whose cooldown layer the outcome ended (a success, an
        // expiry) is not owed any more, and must not be joined by a later one.
        self.claims
            .retain(|(id, scope), _| id != &auth.id || recorded(auth, scope, scope.is_empty()));
        // The same for a cooldown nothing could have recovered.
        self.foreign
            .retain(|(id, scope), _| id != &auth.id || blocked(auth, scope));
        if result.success {
            return;
        }
        if should_skip_credential_cooldown(result.error.as_ref()) {
            return;
        }
        let status = result.error.as_ref().map_or(0, |error| error.http_status);
        let (credential, scope) = scope_of(result, model_key);
        // A rejected token touches every scope, a credential-wide cooldown
        // fences the reads in flight; another failure touches its model's
        // alone.
        let seq = if matches!(status, 401..=403) {
            self.touch_all(&auth.id)
        } else if credential {
            self.touch_wrapped(&auth.id)
        } else {
            self.touch(&auth.id, &scope)
        };
        let key = (auth.id.clone(), scope.clone());
        let own = || own_hold(settings, result, status, now, before, after);
        // A claim stays through a failure it can't be told apart from: its
        // cooldown layer stands (the retain above ends it when it doesn't),
        // and the windows it names stay owed. The failure's sequence number
        // fences it until a later recognized 429 merges its windows in.
        if status != 429 {
            self.hold(key, own());
            return;
        }
        let windows = (result.provider.trim().eq_ignore_ascii_case("claude")
            && recorded(auth, &scope, credential))
        .then(|| windows_from_headers(&result.response_headers, credential))
        .flatten();
        match windows {
            Some(mut windows) => {
                // A later 429 may leave a longer cooldown of an earlier one
                // standing (see `cooldown.rs`), so what it rejected stays
                // owed until a reading clears it too.
                if let Some(previous) = self.claims.get(&key) {
                    windows.extend(previous.windows.iter().copied());
                    windows.sort_unstable();
                    windows.dedup();
                }
                self.claims.insert(
                    key,
                    Claim {
                        windows,
                        recorded_at: now,
                        seq,
                    },
                );
            }
            None => self.hold(key, own()),
        }
    }

    /// Forgets everything of credential `id`: it was replaced, removed or
    /// reset, and a read started before can't speak for it. The cooldowns it
    /// had went with it, so the foreign ones noted no longer taint a claim
    /// recorded after; the fence against reads in flight stays.
    pub(crate) fn forget(&mut self, id: &str) {
        self.claims.retain(|(claim_id, _), _| claim_id != id);
        self.foreign.retain(|(foreign_id, _), _| foreign_id != id);
        self.touch_all(id);
    }

    /// Notes that saved cooldowns of `scopes` of `id` were put back: no claim
    /// or read started before speaks for them. The marks of earlier
    /// recoveries stay, against the next restore.
    pub(crate) fn restored(&mut self, id: &str, scopes: &[String]) {
        for scope in scopes {
            self.claims.remove(&(id.to_owned(), scope.clone()));
            // A cooldown that came back from the file has no provenance.
            self.hold((id.to_owned(), scope.clone()), None);
            if scope.is_empty() {
                self.touch_all(id);
            } else {
                self.touch(id, scope);
            }
        }
    }

    /// Forgets everything, for a restore that makes the saved cooldowns the
    /// authority.
    pub(crate) fn clear(&mut self) {
        self.claims.clear();
        self.foreign.clear();
        self.recovered.clear();
        self.era = self.era.saturating_add(1);
    }

    /// Whether a saved aggregate of `id`'s models' cooldowns (the credential's
    /// own derived record), last changed at `updated_at`, is older than a
    /// recovery of one of its models.
    pub(crate) fn supersedes_derived(&self, id: &str, updated_at: Option<Timestamp>) -> bool {
        self.recovered.iter().any(|((recovered_id, scope), at)| {
            recovered_id == id && !scope.is_empty() && updated_at.is_none_or(|saved| saved <= *at)
        })
    }

    /// Whether a saved cooldown of `scopes` of `id`, last changed at
    /// `updated_at`, is older than a recovery of one of them.
    pub(crate) fn supersedes(
        &self,
        id: &str,
        scopes: &[String],
        updated_at: Option<Timestamp>,
    ) -> bool {
        scopes.iter().any(|scope| {
            self.recovered
                .get(&(id.to_owned(), scope.clone()))
                .is_some_and(|at| updated_at.is_none_or(|saved| saved <= *at))
        })
    }
}

/// Whether anything holds `scope` of `auth` back, of whatever cause.
fn blocked(auth: &Auth, scope: &str) -> bool {
    if scope.is_empty() {
        return auth.unavailable || auth.quota.exceeded;
    }
    auth.model_states
        .iter()
        .filter(|(name, _)| canonical_model_key(name) == scope)
        .any(|(_, state)| {
            state.unavailable
                || state.quota.exceeded
                || state.next_retry_after.is_some()
                || state.last_error.is_some()
                || state.credential_quota_shadow.is_some()
        })
}

/// Whether the cooldown `result` should have recorded is on `auth`.
fn recorded(auth: &Auth, scope: &str, credential: bool) -> bool {
    if credential {
        return auth.quota.exceeded && auth.quota.reason == "credential_quota";
    }
    auth.model_states
        .iter()
        .filter(|(name, _)| canonical_model_key(name) == scope)
        .any(|(_, state)| {
            state.quota.exceeded
                || state
                    .credential_quota_shadow
                    .as_deref()
                    .is_some_and(|own| own.quota.exceeded)
        })
}

/// One header's value, lower-cased: `None` when its values disagree or are
/// not text, `Some(None)` when it is absent.
fn value(headers: &HeaderMap, name: &str) -> Option<Option<String>> {
    let mut seen: Option<String> = None;
    for raw in headers.get_all(name) {
        let text = raw.to_str().ok()?.trim().to_ascii_lowercase();
        match &seen {
            Some(prev) if *prev != text => return None,
            _ => seen = Some(text),
        }
    }
    Some(seen)
}

/// The windows a Claude 429's headers rejected, or `None` when they can't be
/// told apart for sure: a status other than `rejected` overall, a status
/// value or window not known, or a representative claim that names a window
/// that wasn't rejected. A credential-wide cooldown may name only the base
/// windows.
fn windows_from_headers(headers: &HeaderMap, credential: bool) -> Option<Vec<UsageWindow>> {
    const PREFIX: &str = "anthropic-ratelimit-unified-";
    if value(headers, "anthropic-ratelimit-unified-status")?.as_deref() != Some("rejected") {
        return None;
    }
    let mut rejected = BTreeSet::new();
    for name in headers.keys() {
        let name = name.as_str();
        let Some(middle) = name
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix("-status"))
        else {
            continue;
        };
        let status = value(headers, name)??;
        let window = match middle {
            "5h" => Some(UsageWindow::FiveHour),
            "7d" => Some(UsageWindow::SevenDay),
            _ => None,
        };
        match (status.as_str(), window, middle) {
            ("rejected", Some(window), _) => {
                rejected.insert(window);
            }
            ("allowed" | "allowed_warning", ..) => {}
            // Overage that is exhausted is a fact about overage use after a
            // window limit, which the window's own reading settles.
            ("rejected", None, "overage") => {}
            _ => return None,
        }
    }
    match value(headers, "anthropic-ratelimit-unified-representative-claim")?.as_deref() {
        None => {}
        Some("five_hour") if rejected.contains(&UsageWindow::FiveHour) => {}
        Some("seven_day") if rejected.contains(&UsageWindow::SevenDay) => {}
        Some("seven_day_sonnet") if !credential => {
            rejected.insert(UsageWindow::SevenDaySonnet);
        }
        Some("seven_day_opus") if !credential => {
            rejected.insert(UsageWindow::SevenDayOpus);
        }
        Some(_) => return None,
    }
    (!rejected.is_empty()).then(|| rejected.into_iter().collect())
}

/// Whether a window reads below its limit with a reset that is still to come
/// (or, unused, none).
fn healthy(window: &UsageWindowReading, now: Timestamp) -> bool {
    let percent = window.utilization_percent;
    if !percent.is_finite() || !(0.0..100.0).contains(&percent) {
        return false;
    }
    match window.resets_at {
        Some(reset) => reset > now,
        None => percent == 0.0,
    }
}

/// Whether `reading`, answered at `observed`, is recent at `now`, was not
/// answered before `token` was taken, and shows both base windows healthy
/// (they speak for every claim).
fn fresh(
    reading: &UsageReading,
    observed: Timestamp,
    token: &QuotaRecoveryToken,
    now: Timestamp,
) -> bool {
    (now - observed).abs() <= MAX_SKEW
        && observed + DATE_GRACE >= token.captured_at
        && [UsageWindow::FiveHour, UsageWindow::SevenDay]
            .iter()
            .all(|window| {
                reading
                    .windows
                    .get(window)
                    .is_some_and(|reading| healthy(reading, now))
            })
}

impl Manager {
    /// What a usage read about to start is applied against (see the module's
    /// fences). `None` when no credential has the ID.
    pub fn quota_recovery_token(&self, id: &str) -> Option<QuotaRecoveryToken> {
        let id = id.trim();
        let now = self.now();
        let guard = self.lock();
        let auth = &guard.auths.get(id)?.auth;
        Some(QuotaRecoveryToken {
            auth_id: id.to_owned(),
            registration_epoch: auth.registration_epoch,
            credential_version: auth.credential_version,
            shaping: shaping(auth),
            era: guard.quota_recovery.era,
            seq: guard.quota_recovery.seq,
            captured_at: now,
        })
    }

    /// Ends the quota cooldowns of the credential `token` was taken for that
    /// `reading` confirms are over (see the module). Returns what ended;
    /// nothing is also a success. A failed save is returned after the change
    /// took effect.
    pub fn apply_quota_usage_recovery(
        &self,
        token: &QuotaRecoveryToken,
        reading: &UsageReading,
    ) -> Result<QuotaRecovery, ManagerError> {
        let none = QuotaRecovery::default();
        let Some(operation) =
            crate::gate::Operation::current().or_else(|| self.gate().begin_operation().ok())
        else {
            return Ok(none);
        };
        let Some(observed) = reading.observed_at else {
            return Ok(none);
        };
        let gate = self.mutation_gate();
        if !fresh(reading, observed, token, self.now()) {
            return Ok(none);
        }
        let id = token.auth_id.as_str();
        let (snapshot, recovery, epoch, generation, discard_epoch, pending, now) = {
            let mut guard = self.lock();
            let state = &mut *guard;
            // Judged again under the lock, at the moment of the change: time
            // spent waiting for the gate or the lock must not age a reading
            // or a reset into acceptance.
            let now = self.now();
            if !fresh(reading, observed, token, now) {
                return Ok(none);
            }
            if state.quota_recovery.era != token.era
                || state.quota_recovery.touched_all(id) > token.seq
                || state.quota_recovery.wrapped(id) > token.seq
            {
                return Ok(none);
            }
            let Some(entry) = state.auths.get_mut(id) else {
                return Ok(none);
            };
            {
                let auth = &entry.auth;
                if auth.registration_epoch != token.registration_epoch
                    || auth.credential_version != token.credential_version
                    || shaping(auth) != token.shaping
                    || auth.disabled
                    || auth.status == Status::Disabled
                    || super::classify::has_unauthorized_auth_failure(auth)
                {
                    return Ok(none);
                }
            }
            let confirmed: Vec<String> = state
                .quota_recovery
                .claims
                .range((id.to_owned(), String::new())..)
                .take_while(|((claim_id, _), _)| claim_id == id)
                .filter(|((_, scope), claim)| {
                    !state.quota_recovery.foreign_live(id, scope, now)
                        // A model stays in cooldown while the credential has
                        // a hold of its own (live, or of unknown end).
                        && !state.quota_recovery.foreign_live(id, "", now)
                        && claim.seq <= token.seq
                        && state.quota_recovery.untouched_since(id, scope, claim)
                        && observed + DATE_GRACE >= claim.recorded_at
                        && claim.windows.iter().all(|window| {
                            reading
                                .windows
                                .get(window)
                                .is_some_and(|reading| healthy(reading, now))
                        })
                })
                .map(|((_, scope), _)| scope.clone())
                .collect();
            if confirmed.is_empty() {
                return Ok(none);
            }
            let auth = Arc::make_mut(&mut entry.auth);
            let mut recovery = QuotaRecovery::default();
            state.quota_checks.before_result(auth);
            let mut scopes = Vec::new();
            // The credential first: its models come out from under it with
            // their own limits, which a model's claim may then end.
            if confirmed.iter().any(String::is_empty)
                && auth.quota.exceeded
                && auth.quota.reason == "credential_quota"
            {
                // A model's own layer under the wrap may have been capped to
                // its next check; it comes out with the provider's reset.
                state.quota_checks.uncap_shadows(auth, now);
                clear_credential(auth, now);
                recovery.credential = true;
                scopes.push(String::new());
            }
            for scope in confirmed.iter().filter(|scope| !scope.is_empty()) {
                if clear_model(auth, scope, now) {
                    recovery.models.push(scope.clone());
                    scopes.push(scope.clone());
                }
            }
            if scopes.is_empty() {
                // Nothing matched: put the leases back as they were.
                state.quota_checks.recovered(auth, &[], now);
                return Ok(none);
            }
            for scope in &scopes {
                let key = (id.to_owned(), scope.clone());
                state.quota_recovery.claims.remove(&key);
                state.quota_recovery.recovered.insert(key, now);
            }
            state.quota_checks.recovered(auth, &scopes, now);
            let bumps = 2;
            auth.updated_at = Some(now);
            auth.generation = auth.generation.saturating_add(bumps);
            let committed = (
                entry.auth.clone(),
                recovery,
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
                now,
            )
        };
        let persisted = self.persist(&snapshot, epoch, generation, discard_epoch, Save::Yes);
        drop(pending);
        drop(gate);
        super::cooldown_store::changed(self);
        self.publish_projections(&snapshot, generation, now, false);
        drop(operation);
        persisted.map(|_| recovery)
    }
}

/// Takes the credential-wide quota cooldown off `auth` and every model out
/// from under it, to the model's own layer when it still holds.
fn clear_credential(auth: &mut Auth, now: Timestamp) {
    auth.unavailable = false;
    auth.next_retry_after = None;
    apply_cooldown_fields(&mut auth.quota, QuotaState::default());
    for state in auth.model_states.values_mut() {
        if state.credential_quota_propagated || state.quota.reason == "credential_quota" {
            unwrap_expired_quota(state, now);
        }
    }
    update_aggregated_availability(auth, now);
    if !has_model_error(auth, now) {
        if auth
            .last_error
            .as_ref()
            .is_some_and(|error| error.http_status == 429)
        {
            auth.last_error = None;
            auth.status_message.clear();
        }
        if !auth.unavailable && auth.last_error.is_none() {
            auth.status = Status::Active;
        }
    }
    auth.cooldown_recorded_seq = 0;
}

/// Takes the model `scope`'s own quota cooldown off `auth`, leaving the
/// credential-wide one over it, and says whether there was one. The caller
/// has refused it while the credential has a hold of its own.
fn clear_model(auth: &mut Auth, scope: &str, now: Timestamp) -> bool {
    let wrap_live = is_credential_quota_active(&auth.quota, now);
    let wrap_next = auth.quota.next_recover_at;
    let mut cleared = false;
    for (name, state) in &mut auth.model_states {
        if canonical_model_key(name) != scope {
            continue;
        }
        let mut wrapped =
            state.credential_quota_propagated || state.quota.reason == "credential_quota";
        if wrapped && !wrap_live {
            unwrap_expired_quota(state, now);
            wrapped = state.credential_quota_propagated || state.quota.reason == "credential_quota";
        }
        if wrapped {
            if state.credential_quota_shadow.take().is_some() {
                // What shows outside is the wrap alone again.
                state.next_retry_after = wrap_next;
                state.quota.next_recover_at = wrap_next;
                state.updated_at = Some(now);
                cleared = true;
            }
        } else if state.quota.exceeded && state.quota.reason == "quota" {
            reset_model_state(state, now);
            cleared = true;
        }
    }
    if cleared {
        update_aggregated_availability(auth, now);
        // The credential's own quota, derived from its models', outlives the
        // last of them: derive it again from none.
        if auth.quota.exceeded
            && auth.quota.reason == "quota"
            && !auth.model_states.values().any(|state| state.quota.exceeded)
        {
            apply_cooldown_fields(&mut auth.quota, QuotaState::default());
            update_aggregated_availability(auth, now);
        }
        if !has_model_error(auth, now)
            && !auth.unavailable
            && auth
                .last_error
                .as_ref()
                .is_some_and(|error| error.http_status == 429)
        {
            auth.last_error = None;
            auth.status_message.clear();
            auth.status = Status::Active;
        }
    }
    cleared
}
