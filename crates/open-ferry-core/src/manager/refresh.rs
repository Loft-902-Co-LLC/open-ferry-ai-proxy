// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_refresh.go and
// sdk/cliproxy/auth/auto_refresh_loop.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Token refresh: the background loop that refreshes OAuth credentials
//! before they expire, the refresh after a 401, and forced refreshes.
//!
//! The loop keeps a queue of when each credential is next due. A credential
//! is due a lead time before its token expires (the executor's
//! [`refresh_lead`](crate::executor::ProviderExecutor::refresh_lead)), or
//! every `refresh_interval_seconds` when the credential sets one. Due
//! credentials go to a pool of workers, which call the executor's
//! [`refresh`](crate::executor::ProviderExecutor::refresh) and fold the result
//! into the live credential, then save it. A failed refresh backs off: five
//! minutes, or doubling from one minute up to thirty after `invalid_grant`.
//! Dropping the last manager handle stops the loop, as stopping it does.
//!
//! An access token the provider refused with a 401, on a credential that
//! doesn't say when it expires, counts as expired until a refresh works or
//! the tokens change, so the credential stays out of rotation meanwhile. A
//! canceled refresh is tried again a second later. A refresh doesn't put its
//! tokens back over ones replaced while it ran.
//!
//! Deviations from upstream:
//! - The executor's refresh lead replaces upstream's registry of leads per
//!   provider; a credential whose executor isn't registered isn't
//!   scheduled until one is.
//! - Runtime refresh evaluators aren't ported.
//! - Stopping the loop lets refreshes already running finish; upstream
//!   cancels their context.
//! - Credentials due at the same moment are taken in ID order.
//! - A job is matched by a sequence number, where upstream compares
//!   pointers.
//! - The refresh call gets the credential as it is; there is no request
//!   proxy override to strip.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc, watch};

use super::classify::{
    ErrView, has_disabled_invalid_grant_failure, has_unauthorized_auth_failure,
    is_invalid_grant_error, is_request_scoped_error, is_unauthorized_error,
    refresh_error_from_error,
};
use super::cooldown::{
    add, is_credential_quota_active, reset_model_state, update_aggregated_availability,
};
use super::credential::{
    KIND_API_KEY, access_token, auth_kind, credentials_changed, has_refresh_credential, is_zero,
    last_refresh_timestamp, preferred_interval, refresh_token,
};
use super::lifecycle::{
    LeasedDisposition, LeasedTail, ORIGIN_CHECK_ATTEMPTS, OriginCheck, OriginMoved,
    clear_verified_partitions, mark_undurable, mark_undurable_except, overlay_undurable,
    pending_for_edit, verified_partitions,
};
use super::merge::{ResponseCredentials, merge_refreshed_auth_with, normalize_credential_metadata};
use super::text::{equal_fold, go_lower};
use super::{
    Entry, Manager, ManagerError, MarkCause, Shared, State, UndurableBaselines,
    UndurablePartitions, UnresolvedOrigin, lock,
};
use crate::auth::classification::ATTRIBUTE_PATH;
use crate::auth::codex_refresh::{
    CodexPartitions, CredentialRefreshPatch, OUTER_KEYS, SIWC_KEYS, disk_rotated_partitions,
    is_codex_auth, same_version,
};
use crate::auth::{
    Auth, AuthError, CodexCredentialPartition, CodexLease, CodexLeaseToken, CodexOrigin,
    CodexPersisted, OriginProof, Status, Timestamp,
};
use crate::exec::{ErrorKind, ExecError};
use crate::executor::{AdoptedPartitions, ProviderExecutor};
use crate::gate::{GateState, NotActive};
use serde_json::{Map, Value};

/// How often the loop checks when no interval is given
/// (upstream's `refreshCheckInterval`).
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How many credentials refresh at once by default
/// (upstream's `refreshMaxConcurrency`).
const REFRESH_MAX_CONCURRENCY: usize = 16;
/// How long a queued refresh holds the credential's next check
/// (upstream's `refreshPendingBackoff`).
const REFRESH_PENDING_BACKOFF: Duration = Duration::from_secs(60);
/// The wait after a failed refresh (upstream's `refreshFailureBackoff`).
const REFRESH_FAILURE_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Refresh well before Codex's nominal ten-day JWT expiry, since upstream can
/// invalidate the bearer earlier. SIWC retains its separate five-minute lead.
pub const CODEX_OUTER_REFRESH_LEAD: Duration = Duration::from_secs(96 * 3600);
/// The first wait after `invalid_grant` (upstream's
/// `invalidGrantBackoffBase`).
const INVALID_GRANT_BACKOFF_BASE: Duration = Duration::from_secs(60);
/// The longest wait after `invalid_grant` (upstream's
/// `invalidGrantBackoffMax`).
const INVALID_GRANT_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
/// The wait when a refresh succeeded but the credential still looks due, so
/// the loop doesn't spin (upstream's `refreshIneffectiveBackoff`).
const REFRESH_INEFFECTIVE_BACKOFF: Duration = Duration::from_secs(30);
/// The wait after a refresh that was canceled before it finished.
const REFRESH_CANCELED_RETRY: Duration = Duration::from_secs(1);
/// The longest the loop sleeps at once, so it notices a clock jump after
/// the machine sleeps (upstream's `maxRefreshTimerWait`).
const MAX_REFRESH_TIMER_WAIT: Duration = Duration::from_secs(30);
/// How many deferrals in a row an unresolved origin makes before the
/// credential's refresh hold is shown (see [`Entry::refresh_hold`]).
pub(super) const ORIGIN_UNRESOLVED_SURFACE_AFTER: u32 = 10;

static NEXT_LOOP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_JOB_SEQ: AtomicU64 = AtomicU64::new(1);

/// A refresh queued or running for one credential (upstream's
/// `authRefreshJob`).
#[derive(Clone, Debug)]
pub(crate) struct RefreshJob {
    pub(crate) id: String,
    /// The registration the job belongs to.
    pub(crate) epoch: u64,
    /// The `next_refresh_after` the job set, which it clears when done.
    pub(crate) pending_until: Timestamp,
    /// The loop that queued it.
    pub(crate) loop_id: u64,
    /// Tells this job from a later one for the same credential.
    seq: u64,
    /// Whether a worker took it.
    pub(crate) running: bool,
    /// The job exists only because the entry has an unresolved origin: its
    /// token is not due. Fixed when the job is queued.
    pub(crate) resolution_only: bool,
}

/// The outcome of a forced refresh for one credential (upstream's
/// `ForceRefreshResult`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForceRefreshResult {
    /// The credential.
    pub id: String,
    /// Whether the refresh worked.
    pub success: bool,
    /// Why it didn't.
    pub error: Option<String>,
    /// Not upstream's: whether it wasn't tried because the gate isn't open.
    pub not_active: bool,
}

/// The loop's queue: credentials by when they are next due.
#[derive(Default)]
pub(super) struct Queue {
    order: BTreeSet<(Timestamp, String)>,
    pub(super) index: HashMap<String, Timestamp>,
    dirty: HashSet<String>,
}

/// What the loop shares with the manager (upstream's `authAutoRefreshLoop`
/// fields).
pub(crate) struct LoopShared {
    id: u64,
    interval: Duration,
    pub(super) queue: Mutex<Queue>,
    wake: Notify,
}

/// The running loop, kept by the manager. Dropping it stops the loop.
pub(crate) struct RefreshLoopHandle {
    pub(super) shared: Arc<LoopShared>,
    pub(super) stop: watch::Sender<bool>,
}

impl RefreshLoopHandle {
    pub(super) fn stop(&self) {
        self.stop.send_replace(true);
    }
}

impl LoopShared {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            id: NEXT_LOOP_ID.fetch_add(1, Ordering::Relaxed),
            interval,
            queue: Mutex::new(Queue::default()),
            wake: Notify::new(),
        }
    }

    /// Marks a credential for a fresh look, and wakes the loop (upstream's
    /// `queueReschedule`).
    fn queue_reschedule(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        lock(&self.queue).dirty.insert(id.to_owned());
        self.wake.notify_one();
    }

    pub(super) fn upsert(&self, id: &str, next: Timestamp) {
        if id.is_empty() {
            return;
        }
        let mut queue = lock(&self.queue);
        if let Some(old) = queue.index.insert(id.to_owned(), next) {
            queue.order.remove(&(old, id.to_owned()));
        }
        queue.order.insert((next, id.to_owned()));
    }

    fn remove(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let mut queue = lock(&self.queue);
        queue.dirty.remove(id);
        if let Some(old) = queue.index.remove(id) {
            queue.order.remove(&(old, id.to_owned()));
        }
    }

    fn rebuild(&self, entries: Vec<(String, Timestamp)>) {
        let mut queue = lock(&self.queue);
        queue.order.clear();
        queue.index.clear();
        for (id, next) in entries {
            queue.order.insert((next, id.clone()));
            queue.index.insert(id, next);
        }
    }

    /// How long to sleep until the next credential is due, if any is
    /// queued (upstream's `nextWait`).
    pub(super) fn next_wait(&self, now: Timestamp) -> Option<Duration> {
        let next = lock(&self.queue).order.first().map(|(next, _)| *next)?;
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        Some(wait.min(MAX_REFRESH_TIMER_WAIT))
    }

    /// Takes every credential due by `now` off the queue (upstream's
    /// `popDue`).
    pub(super) fn pop_due(&self, now: Timestamp) -> Vec<String> {
        let mut queue = lock(&self.queue);
        let mut due = Vec::new();
        while queue.order.first().is_some_and(|(next, _)| *next <= now) {
            if let Some((_, id)) = queue.order.pop_first() {
                queue.index.remove(&id);
                due.push(id);
            }
        }
        due
    }

    fn drain_dirty(&self) -> Vec<String> {
        let mut queue = lock(&self.queue);
        queue.dirty.drain().collect()
    }
}

fn delta(d: Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

/// `t` minus `d`, saturating at the earliest time chrono can hold.
fn sub(t: Timestamp, d: Duration) -> Timestamp {
    TimeDelta::from_std(d)
        .ok()
        .and_then(|d| t.checked_sub_signed(d))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn non_zero(t: Option<Timestamp>) -> Option<Timestamp> {
    t.filter(|t| !is_zero(Some(*t)))
}

fn last_refresh(auth: &Auth) -> Option<Timestamp> {
    non_zero(auth.last_refreshed_at).or_else(|| last_refresh_timestamp(auth))
}

fn refresh_held(auth: &Auth, now: Timestamp) -> bool {
    auth.next_refresh_after.is_some_and(|t| now < t)
}

fn is_disabled(auth: &Auth) -> bool {
    auth.disabled || auth.status == Status::Disabled
}

/// A committed stream's 401 cannot retry inline. Reject its current bearer and
/// wake background refresh, preserving an existing failure backoff and the
/// terminal refresh-token failure state. Called under the manager state lock.
pub(super) fn schedule_unauthorized_refresh(
    auth: &mut Auth,
    result: &super::CallResult,
    now: Timestamp,
    was_terminal: bool,
) -> bool {
    let unauthorized = super::scoped::rejects_access_token(result);
    if unauthorized {
        auth.record_rejected_token(&result.access_token);
    }
    if was_terminal || !has_refresh_credential(auth) || !unauthorized {
        return false;
    }
    if non_zero(auth.next_refresh_after).is_none() {
        auth.next_refresh_after = Some(
            if non_zero(auth.last_refreshed_at)
                .is_some_and(|last| now - last < delta(REFRESH_FAILURE_BACKOFF))
            {
                add(now, REFRESH_FAILURE_BACKOFF)
            } else {
                now
            },
        );
    }
    true
}

/// Whether a credential should refresh now (upstream's `shouldRefresh`).
/// `lead` is how long before expiry its executor refreshes, if it does.
pub(crate) fn should_refresh(auth: &Auth, lead: Option<Duration>, now: Timestamp) -> bool {
    if has_unauthorized_auth_failure(auth) || has_disabled_invalid_grant_failure(auth) {
        return false;
    }
    if refresh_held(auth, now) {
        return false;
    }
    if auth.outer_refresh_due_at(now, lead).is_some() || auth.siwc_refresh_due_at(now).is_some() {
        return [
            auth.outer_refresh_due_at(now, lead),
            auth.siwc_refresh_due_at(now),
        ]
        .into_iter()
        .flatten()
        .any(|due| due <= now);
    }
    let last = last_refresh(auth);
    let expiry = non_zero(auth.expiration_time());
    if let Some(interval) = preferred_interval(auth).filter(|d| !d.is_zero()) {
        if let Some(expiry) = expiry
            && (expiry <= now || expiry - now <= delta(interval))
        {
            return true;
        }
        let Some(last) = last else {
            return true;
        };
        return now - last >= delta(interval);
    }
    let Some(lead) = lead else {
        return false;
    };
    if lead.is_zero() {
        return expiry.is_some_and(|expiry| now > expiry);
    }
    if let Some(expiry) = expiry {
        return expiry - now <= delta(lead);
    }
    if let Some(last) = last {
        return now - last >= delta(lead);
    }
    true
}

/// When to look at a credential next, or none when it never refreshes
/// (upstream's `nextRefreshCheckAt`).
pub(crate) fn next_refresh_check_at(
    now: Timestamp,
    auth: &Auth,
    lead: Option<Duration>,
) -> Option<Timestamp> {
    if has_unauthorized_auth_failure(auth) || has_disabled_invalid_grant_failure(auth) {
        return None;
    }
    if let Some(held) = auth.next_refresh_after
        && now < held
    {
        return Some(held);
    }
    if auth.outer_refresh_due_at(now, lead).is_some() || auth.siwc_refresh_due_at(now).is_some() {
        return [
            auth.outer_refresh_due_at(now, lead),
            auth.siwc_refresh_due_at(now),
        ]
        .into_iter()
        .flatten()
        .min();
    }
    if auth_kind(auth) == KIND_API_KEY {
        return None;
    }
    let last = last_refresh(auth);
    let expiry = non_zero(auth.expiration_time());
    if let Some(pref) = preferred_interval(auth).filter(|d| !d.is_zero()) {
        let mut next = None;
        if let Some(expiry) = expiry {
            if expiry <= now || expiry - now <= delta(pref) {
                return Some(now);
            }
            next = Some(sub(expiry, pref));
        }
        let Some(last) = last else {
            return Some(now);
        };
        let candidate = add(last, pref);
        let next = next.map_or(candidate, |next: Timestamp| next.min(candidate));
        return Some(next.max(now));
    }
    let lead = lead?;
    if let Some(expiry) = expiry {
        return Some(sub(expiry, lead).max(now));
    }
    if let Some(last) = last {
        return Some(add(last, lead).max(now));
    }
    Some(now)
}

/// [`next_refresh_check_at`] for a registered entry: an entry with an
/// unresolved origin is checked now, unless a deferral holds it. I/O-free;
/// called under the state lock.
fn next_check_for_entry(
    now: Timestamp,
    entry: &Entry,
    lead: Option<Duration>,
) -> Option<Timestamp> {
    // None: a top-level 401 or a disabled invalid_grant.
    let next = next_refresh_check_at(now, &entry.auth, lead)?;
    if entry.unresolved_origin.is_some() && !refresh_held(&entry.auth, now) {
        return Some(now);
    }
    Some(next)
}

impl Auth {
    /// Due time for the refreshable outer Codex partition. The outer metadata
    /// refresh timestamp wins over runtime time so SIWC rotations cannot
    /// postpone the outer interval. `None` lead uses Codex's 96-hour default.
    pub fn outer_refresh_due_at(
        &self,
        now: Timestamp,
        lead: Option<Duration>,
    ) -> Option<Timestamp> {
        if !crate::auth::codex_refresh::is_codex_auth(self) || refresh_token(self).is_empty() {
            return None;
        }
        let token = self.outer_access_token();
        if token.is_empty() || self.rejected_access_token == token {
            return Some(now);
        }
        if self.codex_refresh_failure.as_ref().is_some_and(|failure| {
            failure.partition == crate::auth::CodexCredentialPartition::Outer
        }) {
            return Some(now);
        }
        let last = last_refresh_timestamp(self).or(self.last_refreshed_at);
        let expiry = non_zero(self.outer_expiration_time());
        let due = if let Some(interval) = preferred_interval(self).filter(|d| !d.is_zero()) {
            let Some(last) = last else {
                return Some(now);
            };
            let interval_due = add(last, interval);
            expiry.map_or(interval_due, |expiry| {
                sub(expiry, interval).min(interval_due)
            })
        } else {
            let lead = lead.unwrap_or(CODEX_OUTER_REFRESH_LEAD);
            if lead.is_zero() && expiry.is_none() {
                return None;
            }
            expiry
                .map(|expiry| sub(expiry, lead))
                .or_else(|| last.map(|last| add(last, lead)))
                .unwrap_or(now)
        };
        Some(due.max(now))
    }
}

/// The wait after `failures` refreshes in a row failed with `invalid_grant`
/// (upstream's `invalidGrantBackoffDuration`).
pub(crate) fn invalid_grant_backoff(failures: u32) -> Duration {
    if failures <= 1 {
        return INVALID_GRANT_BACKOFF_BASE;
    }
    let shift = (failures - 1).min(10);
    INVALID_GRANT_BACKOFF_BASE
        .saturating_mul(1 << shift)
        .min(INVALID_GRANT_BACKOFF_MAX)
}

/// Resets the model states whose last failure was a 401, and returns their
/// models (upstream's `clearUnauthorizedModelStates`). A model still out of
/// quota only loses the 401: its quota cooldown runs on.
pub(crate) fn clear_unauthorized_model_states(auth: &mut Auth, now: Timestamp) -> Vec<String> {
    let mut resumed = Vec::new();
    let mut changed = false;
    for (model, state) in &mut auth.model_states {
        let unauthorized = state.last_error.as_ref().is_some_and(|err| {
            err.http_status == 401
                || equal_fold(&err.code, "unauthorized")
                || is_unauthorized_error(ErrView::Auth(err))
        }) || go_lower(&state.status_message).contains("unauthorized");
        if !unauthorized {
            continue;
        }
        changed = true;
        if state.quota.exceeded
            && (is_zero(state.quota.next_recover_at)
                || state.quota.next_recover_at.is_some_and(|t| t > now))
        {
            state.last_error = None;
            if go_lower(&state.status_message).contains("unauthorized") {
                state.status_message.clone_from(&state.quota.reason);
            }
            state.updated_at = Some(now);
            continue;
        }
        reset_model_state(state, now);
        resumed.push(model.clone());
    }
    if changed {
        update_aggregated_availability(auth, now);
    }
    resumed
}

/// What to do with the loop's queue after a failed refresh.
enum AfterFailure {
    Reschedule,
    Unschedule,
    Nothing,
}

/// The refresh that failed, for [`Manager::record_refresh_failure`].
struct FailedRefresh<'a> {
    /// The registration it started from.
    base_epoch: u64,
    /// The access token the provider rejected, or empty.
    failed_token: &'a str,
    /// Whether it was a forced refresh.
    force: bool,
    /// The Codex partition that failed while the other one rotated, which
    /// the record keeps so the next check retries it. `None` when the whole
    /// refresh failed.
    partition: Option<crate::auth::CodexCredentialPartition>,
}

/// A failed partition of a Codex refresh whose other partition rotated, held
/// until it can be recorded against the live credential.
struct PartialFailure {
    failure: crate::auth::CodexRefreshFailure,
    /// The failed partition's tokens as the refresh left them: the failure
    /// only counts while the live credential still holds those.
    baseline: Option<crate::auth::codex_refresh::CredentialRefreshPatch>,
    secrets: crate::observe::redact::Secrets,
}

/// The status message of a credential whose tokens were both rejected.
const TERMINAL_UNAUTHORIZED_MESSAGE: &str = "unauthorized (refresh token invalid)";

/// The error recorded on a credential whose tokens were both rejected: a
/// 401, so it stays out of rotation (see [`has_unauthorized_auth_failure`]).
fn terminal_unauthorized_error(err: &ExecError) -> AuthError {
    AuthError {
        code: "unauthorized".into(),
        message: ErrView::Exec(err).text(),
        http_status: 401,
        ..AuthError::default()
    }
}

/// The loop's own handle to the manager, which doesn't keep the loop going.
fn upgrade(weak: &Weak<Shared>) -> Option<Manager> {
    weak.upgrade().map(|shared| Manager {
        shared,
        _owner: None,
    })
}

fn is_stopped(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow() || stop.has_changed().is_err()
}

async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

async fn sleep_for(wait: Option<Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending::<()>().await,
    }
}

/// The loop itself (upstream's `run` and `loop`). It and its workers hold
/// the manager weakly, and their own handles don't count as the manager's:
/// dropping the last handle given out stops them.
pub(super) async fn run_loop(
    weak: Weak<Shared>,
    shared: Arc<LoopShared>,
    mut stop: watch::Receiver<bool>,
    jobs: mpsc::Sender<RefreshJob>,
) {
    loop {
        let (wait, gate) = {
            let Some(manager) = upgrade(&weak) else {
                return;
            };
            (shared.next_wait(manager.now()), manager.gate())
        };
        // Not upstream's: a process that isn't the active one doesn't refresh,
        // so the loop pops no due credential while the gate isn't open, and
        // parks until it opens or the loop stops.
        if gate.state() != GateState::Open {
            tokio::select! {
                biased;
                () = stopped(&mut stop) => break,
                () = gate.wait_open() => {}
            }
            continue;
        }
        tokio::select! {
            biased;
            () = stopped(&mut stop) => break,
            () = shared.wake.notified() => {
                let Some(manager) = upgrade(&weak) else {
                    return;
                };
                let now = manager.now();
                manager.apply_dirty(&shared, now);
            }
            () = sleep_for(wait) => {
                let Some(manager) = upgrade(&weak) else {
                    return;
                };
                let now = manager.now();
                manager.handle_due(&shared, &jobs, &stop, now);
                manager.apply_dirty(&shared, now);
            }
        }
    }
    if let Some(manager) = upgrade(&weak) {
        manager.release_queued_jobs(shared.id);
    }
}

/// One refresh worker (upstream's `worker`).
pub(super) async fn run_worker(
    weak: Weak<Shared>,
    jobs: Arc<tokio::sync::Mutex<mpsc::Receiver<RefreshJob>>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let job = {
            let mut jobs = jobs.lock().await;
            tokio::select! {
                biased;
                () = stopped(&mut stop) => return,
                job = jobs.recv() => job,
            }
        };
        let Some(job) = job else {
            return;
        };
        let Some(manager) = upgrade(&weak) else {
            return;
        };
        let mut retry_at = None;
        if manager.begin_refresh_job(&job, &stop)
            && let Err(ManagerError::NotActive(_)) = manager
                .refresh_with(&job.id, "", job.epoch, false, job.resolution_only)
                .await
        {
            // Refused by a gate that closed since the job was queued: try
            // again after a pause, not at once.
            retry_at = Some(add(manager.now(), REFRESH_CHECK_INTERVAL));
        }
        manager.finish_refresh_job(&job, retry_at, false);
    }
}

impl Manager {
    /// Starts refreshing credentials in the background, checking at least
    /// every `interval` (5 seconds when zero), with `refresh_workers`
    /// refreshes at once (upstream's `StartAutoRefresh`). A loop already
    /// running stops first. Fails outside a Tokio runtime.
    pub fn start_auto_refresh(&self, interval: Duration) -> Result<(), ManagerError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| ManagerError::Other("auto refresh needs a tokio runtime".into()))?;
        let interval = if interval.is_zero() {
            REFRESH_CHECK_INTERVAL
        } else {
            interval
        };
        if let Some(previous) = lock(&self.shared.refresh_loop).take() {
            previous.stop();
        }
        let workers = self.refresh_workers();
        let shared = Arc::new(LoopShared::new(interval));
        let (stop_tx, stop_rx) = watch::channel(false);
        let (jobs_tx, jobs_rx) = mpsc::channel(workers.saturating_mul(4).max(64));
        *lock(&self.shared.refresh_loop) = Some(RefreshLoopHandle {
            shared: shared.clone(),
            stop: stop_tx,
        });
        self.rebuild_refresh_queue(&shared, self.now());
        let jobs_rx = Arc::new(tokio::sync::Mutex::new(jobs_rx));
        // The workers and the loop write nothing: they only queue and pick
        // credentials, and each refresh they start begins its own operation
        // (`refresh_with`).
        for _ in 0..workers {
            #[allow(clippy::disallowed_methods)]
            runtime.spawn(run_worker(
                Arc::downgrade(&self.shared),
                jobs_rx.clone(),
                stop_rx.clone(),
            ));
        }
        #[allow(clippy::disallowed_methods)]
        runtime.spawn(run_loop(
            Arc::downgrade(&self.shared),
            shared,
            stop_rx,
            jobs_tx,
        ));
        Ok(())
    }

    /// Stops the background refresh loop, if one is running (upstream's
    /// `StopAutoRefresh`). Refreshes already running finish.
    pub fn stop_auto_refresh(&self) {
        if let Some(handle) = lock(&self.shared.refresh_loop).take() {
            handle.stop();
        }
    }

    /// Upstream's `refreshWorkers`.
    pub(super) fn refresh_workers(&self) -> usize {
        match self.settings().refresh_workers {
            0 => REFRESH_MAX_CONCURRENCY,
            workers => workers,
        }
    }

    pub(super) fn refresh_loop_shared(&self) -> Option<Arc<LoopShared>> {
        lock(&self.shared.refresh_loop)
            .as_ref()
            .map(|handle| handle.shared.clone())
    }

    /// Asks the running loop to look at a credential again (upstream's
    /// `queueRefreshReschedule`).
    pub(crate) fn queue_refresh_reschedule(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        if let Some(shared) = self.refresh_loop_shared() {
            shared.queue_reschedule(id);
        }
    }

    /// Takes a credential off the running loop's queue (upstream's
    /// `queueRefreshUnschedule`).
    pub(crate) fn queue_refresh_unschedule(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        if let Some(shared) = self.refresh_loop_shared() {
            shared.remove(id);
        }
    }

    /// How long before expiry the credential's executor refreshes.
    fn refresh_lead_for(state: &State, auth: &Auth) -> Option<Duration> {
        Self::executor_for(state, auth).and_then(|executor| executor.refresh_lead())
    }

    fn next_check_for(&self, id: &str, now: Timestamp) -> Option<Timestamp> {
        let state = self.lock();
        let entry = state.auths.get(id)?;
        next_check_for_entry(now, entry, Self::refresh_lead_for(&state, &entry.auth))
    }

    /// Upstream's `rebuild`.
    pub(super) fn rebuild_refresh_queue(&self, shared: &LoopShared, now: Timestamp) {
        let entries: Vec<(String, Timestamp)> = {
            let state = self.lock();
            state
                .auths
                .iter()
                .filter_map(|(id, entry)| {
                    let lead = Self::refresh_lead_for(&state, &entry.auth);
                    next_check_for_entry(now, entry, lead).map(|next| (id.clone(), next))
                })
                .collect()
        };
        shared.rebuild(entries);
    }

    /// Upstream's `applyDirty`.
    pub(super) fn apply_dirty(&self, shared: &LoopShared, now: Timestamp) {
        for id in shared.drain_dirty() {
            match self.next_check_for(&id, now) {
                Some(next) => shared.upsert(&id, next),
                None => shared.remove(&id),
            }
        }
    }

    /// Upstream's `handleDue`.
    pub(super) fn handle_due(
        &self,
        shared: &LoopShared,
        jobs: &mpsc::Sender<RefreshJob>,
        stop: &watch::Receiver<bool>,
        now: Timestamp,
    ) {
        for id in shared.pop_due(now) {
            self.handle_due_auth(shared, jobs, stop, now, &id);
        }
    }

    /// Upstream's `handleDueAuth`: requeues a credential that isn't due
    /// yet, and hands a due one to the workers.
    fn handle_due_auth(
        &self,
        shared: &LoopShared,
        jobs: &mpsc::Sender<RefreshJob>,
        stop: &watch::Receiver<bool>,
        now: Timestamp,
        id: &str,
    ) {
        if id.is_empty() {
            return;
        }
        let (next, due, resolution_only, has_executor, epoch) = {
            let state = self.lock();
            let Some(entry) = state.auths.get(id) else {
                return;
            };
            let executor = Self::executor_for(&state, &entry.auth);
            let lead = executor
                .as_ref()
                .and_then(|executor| executor.refresh_lead());
            let token_due = should_refresh(&entry.auth, lead, now);
            // Due for the unresolved state alone, not for its token.
            let unresolved = entry.unresolved_origin.is_some() && !refresh_held(&entry.auth, now);
            (
                next_check_for_entry(now, entry, lead),
                token_due || unresolved,
                !token_due && unresolved,
                executor.is_some(),
                entry.auth.registration_epoch,
            )
        };
        let Some(next) = next else {
            shared.remove(id);
            return;
        };
        if !due {
            shared.upsert(id, next);
            return;
        }
        if !has_executor {
            shared.upsert(id, add(now, shared.interval));
            return;
        }
        let Some(job) = self.mark_refresh_pending_with(shared.id, id, epoch, now, resolution_only)
        else {
            match self.next_check_for(id, now) {
                Some(next) if next <= now => shared.upsert(id, add(now, shared.interval)),
                Some(next) => shared.upsert(id, next),
                None => shared.remove(id),
            }
            return;
        };
        if !is_stopped(stop) && jobs.try_send(job.clone()).is_ok() {
            return;
        }
        // A full queue must not hold the loop or leave a pending job behind.
        let retry_at = add(now, shared.interval);
        self.finish_refresh_job(&job, Some(retry_at), false);
        shared.upsert(id, retry_at);
    }

    /// Records a queued refresh and holds the credential's next check while
    /// it waits (upstream's `markRefreshPending`).
    #[cfg(test)]
    pub(crate) fn mark_refresh_pending(
        &self,
        loop_id: u64,
        id: &str,
        epoch: u64,
        now: Timestamp,
    ) -> Option<RefreshJob> {
        self.mark_refresh_pending_with(loop_id, id, epoch, now, false)
    }

    /// Records a queued refresh and holds the credential's next check while
    /// it waits, noting whether the job exists only for the entry's
    /// unresolved origin.
    fn mark_refresh_pending_with(
        &self,
        loop_id: u64,
        id: &str,
        epoch: u64,
        now: Timestamp,
        resolution_only: bool,
    ) -> Option<RefreshJob> {
        let job = {
            let mut guard = self.lock();
            let state = &mut *guard;
            let entry = state.auths.get_mut(id)?;
            if entry.auth.registration_epoch != epoch
                || has_unauthorized_auth_failure(&entry.auth)
                || has_disabled_invalid_grant_failure(&entry.auth)
            {
                return None;
            }
            if state.refresh_jobs.contains_key(id) || refresh_held(&entry.auth, now) {
                return None;
            }
            let job = RefreshJob {
                id: id.to_owned(),
                epoch,
                pending_until: add(now, REFRESH_PENDING_BACKOFF),
                loop_id,
                seq: NEXT_JOB_SEQ.fetch_add(1, Ordering::Relaxed),
                running: false,
                resolution_only,
            };
            state.refresh_jobs.insert(id.to_owned(), job.clone());
            let auth = Arc::make_mut(&mut entry.auth);
            auth.next_refresh_after = Some(job.pending_until);
            auth.updated_at = Some(now);
            auth.generation = auth.generation.saturating_add(1);
            job
        };
        self.queue_refresh_reschedule(id);
        Some(job)
    }

    /// Marks a job running, unless the loop stopped, the job was replaced,
    /// or the credential was registered again (upstream's
    /// `beginRefreshJob`).
    fn begin_refresh_job(&self, job: &RefreshJob, stop: &watch::Receiver<bool>) -> bool {
        let mut guard = self.lock();
        let state = &mut *guard;
        if is_stopped(stop) {
            return false;
        }
        let same_epoch = state
            .auths
            .get(&job.id)
            .is_some_and(|entry| entry.auth.registration_epoch == job.epoch);
        match state.refresh_jobs.get_mut(&job.id) {
            Some(stored) if stored.seq == job.seq && same_epoch => {
                stored.running = true;
                true
            }
            _ => false,
        }
    }

    /// Drops a job and, unless a newer outcome replaced it, its hold on the
    /// credential's next check, which becomes `retry_at` (upstream's
    /// `finishRefreshJob`). With `queued_only`, a running job stays.
    fn finish_refresh_job(&self, job: &RefreshJob, retry_at: Option<Timestamp>, queued_only: bool) {
        let now = self.now();
        {
            let mut guard = self.lock();
            let state = &mut *guard;
            match state.refresh_jobs.get(&job.id) {
                Some(stored) if stored.seq == job.seq => {
                    if queued_only && stored.running {
                        return;
                    }
                }
                _ => return,
            }
            state.refresh_jobs.remove(&job.id);
            if let Some(entry) = state.auths.get_mut(&job.id)
                && entry.auth.registration_epoch == job.epoch
                && entry.auth.next_refresh_after == Some(job.pending_until)
            {
                let auth = Arc::make_mut(&mut entry.auth);
                auth.next_refresh_after = retry_at;
                auth.updated_at = Some(now);
                auth.generation = auth.generation.saturating_add(1);
            }
        }
        self.queue_refresh_reschedule(&job.id);
    }

    /// Lets go of the jobs a stopped loop queued that no worker took
    /// (upstream's `releaseQueuedJobs`).
    fn release_queued_jobs(&self, loop_id: u64) {
        let jobs: Vec<RefreshJob> = self
            .lock()
            .refresh_jobs
            .values()
            .filter(|job| job.loop_id == loop_id && !job.running)
            .cloned()
            .collect();
        for job in jobs {
            self.finish_refresh_job(&job, None, true);
        }
    }

    /// Refreshes a credential once after a 401, so the call can try it
    /// again (upstream's `tryRefreshAfterUnauthorized`). Returns the
    /// refreshed credential, or `None` when it didn't refresh.
    #[cfg(test)]
    pub(crate) async fn try_refresh_after_unauthorized(
        &self,
        auth: &Arc<Auth>,
        err: &ExecError,
        already_tried: bool,
    ) -> Option<Arc<Auth>> {
        self.try_refresh_after_unauthorized_for_request(
            auth,
            err,
            already_tried,
            &http::HeaderMap::new(),
        )
        .await
    }

    pub(crate) async fn try_refresh_after_unauthorized_for_request(
        &self,
        auth: &Arc<Auth>,
        err: &ExecError,
        already_tried: bool,
        headers: &http::HeaderMap,
    ) -> Option<Arc<Auth>> {
        // A request-scoped failure is about this request, not the token.
        if is_request_scoped_error(ErrView::Exec(err)) {
            return None;
        }
        if !is_unauthorized_error(ErrView::Exec(err)) {
            return None;
        }
        if super::scoped::match_request_scoped_error_action(auth, err, &self.settings()).is_some() {
            return None;
        }
        let failed_token = auth.access_token_for_request(headers).to_owned();
        self.mark_rejected_access_token_at_epoch(&auth.id, &failed_token, auth.registration_epoch);
        if already_tried || !has_refresh_credential(auth) {
            return None;
        }
        if has_unauthorized_auth_failure(auth) {
            return None;
        }
        tracing::debug!(
            auth_id = %auth.id,
            provider = %auth.provider,
            "unauthorized response, refreshing credentials before fallback"
        );
        match self
            .refresh_at_epoch(&auth.id, &failed_token, auth.registration_epoch)
            .await
        {
            Ok(refreshed) => {
                if !failed_token.is_empty()
                    && refreshed.access_token_for_request(headers) == failed_token
                    && refreshed.is_access_token_rejected_for_request(headers)
                {
                    None
                } else {
                    Some(refreshed)
                }
            }
            Err(ManagerError::NotActive(_)) => {
                let manager = self.clone();
                let failed = auth.clone();
                let headers = headers.clone();
                // Adoption is read-only and must not require an Open write gate.
                if let Some(operation) = crate::gate::Operation::current() {
                    operation
                        .run_blocking(move || manager.adopt_from_store(&failed, &headers))
                        .await
                        .ok()
                        .flatten()
                } else {
                    // No operation exists to carry, and reads must work with a closed gate.
                    #[allow(clippy::disallowed_methods)]
                    tokio::task::spawn_blocking(move || manager.adopt_from_store(&failed, &headers))
                        .await
                        .ok()
                        .flatten()
                }
            }
            Err(_) => {
                tracing::debug!(auth_id = %auth.id, "credential refresh before fallback failed");
                None
            }
        }
    }

    /// Not upstream's (D3): after a 401 on a worker whose gate isn't open,
    /// reads the credential's record from the store again (a read, never
    /// gated). When the token there differs from the one that failed, the
    /// active process refreshed it: the credential takes it in memory only,
    /// nothing is saved, and the call tries again. Otherwise `None`. Never
    /// refreshes, so a rotated refresh token is never spent twice.
    ///
    /// It adopts only while memory still holds the token that failed, and
    /// applies the disk copy as one step under the state lock, and only when
    /// nothing of the credential is in flight: no refresh holds its refresh
    /// lock (a forced or request-path refresh, which has no job), no refresh
    /// job runs, no published snapshot of it waits to be saved, no save of it
    /// is owed (a refused save left a change the file lacks), none was
    /// published (so none can have been saved) between the read and the swap
    /// (its save-publication sequence is as it was before the read), and its
    /// registration is the one that was read. A credential whose token moved
    /// on (a refresh of this process published a newer one) is returned as
    /// it is. With nothing in flight every later snapshot is built on the
    /// adopted state, so adopting never advances the persist watermark.
    ///
    /// The refresh lock is taken inside the save mutex, with `try_lock` since
    /// this isn't async (so taking it after the save mutex, the other way
    /// round from a pass, cannot deadlock), and held through the disk read
    /// and the swap; the state lock is taken only inside it. It is released
    /// before the save mutex. Check (vi) runs under that mutex, so no save is
    /// refused because an adoption holds the lock: a pass flushes what it
    /// refused once it lets go, and an adoption would not.
    fn adopt_from_store(&self, failed: &Arc<Auth>, headers: &http::HeaderMap) -> Option<Arc<Auth>> {
        let store = self.shared.store.as_ref()?;
        let failed_token = failed.access_token_for_request(headers).to_owned();
        let current = self.get(&failed.id)?;
        if current.access_token_for_request(headers) != failed_token {
            return Some(current);
        }
        let id_lock = lock(&self.shared.refresh_locks)
            .entry(failed.id.clone())
            .or_default()
            .clone();
        let permit = self.mutation_gate();
        // Refuse known in-flight work before waiting for its save mutex.
        // The same checks run again inside the transaction after any wait.
        {
            let state = self.lock();
            if state
                .refresh_jobs
                .get(&failed.id)
                .is_some_and(|job| job.running)
                || state.pending_saves.contains_key(&failed.id)
                || state
                    .auths
                    .get(&failed.id)
                    .is_some_and(|entry| entry.owed_save.is_some())
            {
                return None;
            }
        }
        #[cfg(test)]
        {
            let hook = lock(&self.shared.adopt_unheld_hook).clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        let result = self.with_adoption(&failed.id, &permit, |_guard| {
            // Dropped as this closure returns, before the save mutex.
            let Ok(_refreshing) = id_lock.try_lock() else {
                // A refresh is under way; it may publish a newer token, and
                // adopting now would drop it.
                return None;
            };
            // Before it reads: nothing of the credential may be in flight, and
            // what a save of it could have changed is noted, to be seen again at
            // the swap.
            let (live, epoch, sequence) = {
                let guard = self.lock();
                if guard
                    .refresh_jobs
                    .get(&failed.id)
                    .is_some_and(|job| job.running)
                    || guard.pending_saves.contains_key(&failed.id)
                {
                    return None;
                }
                let entry = guard.auths.get(&failed.id)?;
                if entry.auth.access_token_for_request(headers) != failed_token {
                    return Some(entry.auth.clone());
                }
                if entry.auth.registration_epoch != failed.registration_epoch {
                    // Removed and registered again since the request's snapshot.
                    return None;
                }
                if entry.owed_save.is_some() {
                    // The file lacks a change of this process's; its record
                    // would drop it.
                    return None;
                }
                (
                    entry.auth.clone(),
                    entry.auth.registration_epoch,
                    guard.save_sequence.get(&failed.id).copied(),
                )
            };
            // Read the file of the registration just checked, not the failed
            // snapshot's: the path comes from the `Auth` it is given.
            // Sampled before the read: the path it resolves uses the directory
            // bound then, and a rebind since changes where `live` points with no
            // change to the credential. Compared at the swap.
            let binding = store.binding_generation();
            let read = store.read_for_adoption(&live);
            #[cfg(test)]
            {
                let hook = lock(&self.shared.adopt_hook).clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            let disk = match read {
                Ok(crate::auth::AdoptionRead::Record(disk)) => disk,
                Ok(crate::auth::AdoptionRead::Unsupported) => match store.read_back(&live) {
                    Ok(Some(disk)) => disk,
                    _ => return None,
                },
                Ok(crate::auth::AdoptionRead::Missing | crate::auth::AdoptionRead::Invalid) => {
                    return None;
                }
                Err(err) => {
                    let _ = err;
                    tracing::debug!(auth_id = %failed.id, "reading the credential again failed");
                    return None;
                }
            };
            let same_origin = match store.same_record(&live, &disk) {
                Ok(same) => same,
                Err(_) => {
                    tracing::debug!(auth_id = %failed.id, "adoption origin proof missed");
                    return None;
                }
            };
            if !same_origin
                || disk.id != live.id
                || !equal_fold(disk.provider.trim(), live.provider.trim())
            {
                tracing::debug!(auth_id = %failed.id, "adoption origin proof missed");
                return None;
            }
            let mut disk = disk;
            normalize_credential_metadata(&mut disk.metadata);
            let now = self.now();
            let adopted = {
                let mut guard = self.lock();
                let state = &mut *guard;
                if state
                    .refresh_jobs
                    .get(&failed.id)
                    .is_some_and(|job| job.running)
                    || state.pending_saves.contains_key(&failed.id)
                    || state.save_sequence.get(&failed.id).copied() != sequence
                {
                    // A save of the credential began, and may have finished,
                    // since the record was read: the record may be older than
                    // what it saved.
                    return None;
                }
                let entry = state.auths.get_mut(&failed.id)?;
                if entry.auth.registration_epoch != epoch
                    || !equal_fold(entry.auth.provider.trim(), live.provider.trim())
                {
                    return None;
                }
                // The store was rebound since the read: the directory a relative
                // `file_name` resolves against may have changed.
                if store.binding_generation() != binding {
                    return None;
                }
                // The record was read from where `live` pointed. An edit or
                // update may have moved the registration to another file since,
                // with no save, so the epoch and the save sequence are unmoved.
                // Compared are the inputs of `resolve_auth_path` (the `path`
                // attribute and `file_name`), not the whole generation: request
                // outcomes publish new snapshots all the time.
                if entry.auth.trimmed_attribute(ATTRIBUTE_PATH)
                    != live.trimmed_attribute(ATTRIBUTE_PATH)
                    || entry.auth.file_name.trim() != live.file_name.trim()
                {
                    return None;
                }
                if entry.auth.access_token_for_request(headers) != failed_token {
                    return Some(entry.auth.clone());
                }
                let failed_partition = failed.credential_partition_for_request(headers);
                let protected = entry.protected();
                if protected.contains(failed_partition) {
                    return None;
                }
                super::lifecycle::overlay_undurable(&mut disk, &entry.auth, protected);
                disk.rebase_pending_refreshes_with_origin(
                    &entry.auth,
                    Some(crate::auth::ResolvedOriginProof(same_origin)),
                );
                // Rebase can retain the failed rotated token even when raw disk
                // contains a different, consumed token. That is NOT an adoption.
                let effective = disk.access_token_for_request(headers);
                if effective.is_empty() || effective == failed_token {
                    return None;
                }
                let pending = super::lifecycle::pending_for_edit(
                    &entry.auth,
                    &disk.metadata,
                    self.permanently_ineligible(&entry.auth),
                );
                let auth = Arc::make_mut(&mut entry.auth);
                let previous = auth.clone();
                auth.metadata = disk.metadata;
                if !super::lifecycle::same_observation_policy(&previous, auth) {
                    auth.policy_generation = auth.generation.saturating_add(1);
                }
                auth.codex_pending_refresh = pending;
                auth.codex_persist_intent = Default::default();
                auth.preserve_rejected_tokens_from(&previous, false);
                auth.credential_version = auth.credential_version.saturating_add(1);
                auth.generation = auth.generation.saturating_add(1);
                auth.updated_at = Some(now);
                let snapshot = entry.auth.clone();
                self.drop_unresolved_if_ineligible(entry);
                state.sync_scheduler(self.models(), &failed.id, now);
                snapshot
            };
            self.queue_refresh_reschedule(&failed.id);
            Some(adopted)
        });
        #[cfg(test)]
        {
            let hook = lock(&self.shared.adopt_unheld_hook).clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        result
    }

    /// Refreshes a credential now, whether or not it is due (upstream's
    /// `ForceRefreshAuth`). It refreshes a credential taken out of rotation
    /// for rejected tokens or turned off after `invalid_grant` too, which
    /// nothing else refreshes; a failure leaves such a credential out.
    pub async fn force_refresh(&self, id: &str) -> Result<Arc<Auth>, ManagerError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Other("auth id is empty".into()));
        }
        self.refresh_with(id, "", 0, true, false).await
    }

    /// Refreshes every enabled credential that has a refresh token,
    /// `refresh_workers` at a time, and reports each outcome in credential
    /// order (upstream's `ForceRefreshAll`).
    pub async fn force_refresh_all(&self) -> Vec<ForceRefreshResult> {
        let ids: Vec<String> = self
            .lock()
            .auths
            .iter()
            .filter(|(_, entry)| !entry.auth.disabled && has_refresh_credential(&entry.auth))
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            return Vec::new();
        }
        let workers = self.refresh_workers().clamp(1, ids.len());
        futures_util::stream::iter(ids)
            .map(|id| async move {
                let result = self.force_refresh(&id).await;
                ForceRefreshResult {
                    success: result.is_ok(),
                    not_active: matches!(result, Err(ManagerError::NotActive(_))),
                    error: result.err().map(|err| err.to_string()),
                    id,
                }
            })
            .buffered(workers)
            .collect()
            .await
    }

    /// Refreshes a credential through its executor and folds the result in
    /// (upstream's `refreshAuthForRequestAtEpoch`). With a `failed_token`,
    /// a credential whose token already changed is returned as it is; with
    /// a non-zero `epoch`, the credential must still be that registration.
    pub(crate) async fn refresh_at_epoch(
        &self,
        id: &str,
        failed_token: &str,
        epoch: u64,
    ) -> Result<Arc<Auth>, ManagerError> {
        self.refresh_with(id, failed_token, epoch, false, false)
            .await
    }

    /// [`refresh_at_epoch`](Self::refresh_at_epoch); with `force`, as a
    /// forced refresh (upstream's `withForceRefresh` context).
    ///
    /// Not upstream's: the refresh is one task that owns an
    /// [`Operation`](crate::gate::Operation) of the manager's gate. It keeps
    /// the `refresh_locks` lock through the provider call, the save and the
    /// publication, and runs on when the caller is dropped, so a token the
    /// provider rotated is never lost to a hand-over. A gate that isn't open
    /// refuses with [`ManagerError::NotActive`] before any provider call.
    ///
    /// `resolution_only` is true only for a scheduler job queued solely for
    /// the entry's unresolved origin.
    async fn refresh_with(
        &self,
        id: &str,
        failed_token: &str,
        epoch: u64,
        force: bool,
        resolution_only: bool,
    ) -> Result<Arc<Auth>, ManagerError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Other("auth id is empty".into()));
        }
        let gate = self.gate();
        let operation = gate.begin_operation().map_err(ManagerError::NotActive)?;
        let manager = self.clone();
        let id = id.to_owned();
        let failed_token = failed_token.to_owned();
        let work = async move {
            let result = manager
                .refresh_owned(&id, &failed_token, epoch, force, resolution_only)
                .await;
            // Whatever the pass returned, a save it refused while it held the
            // refresh lock is owed, and is saved again now that it is free.
            // With nothing owed the flush would return at once, so the pass
            // skips its trip to a blocking thread.
            if let Some(epoch) = manager.locked(|s| {
                s.auths
                    .get(&id)
                    .filter(|entry| entry.owed_save.is_some())
                    .map(|entry| entry.auth.registration_epoch)
            }) {
                let (flusher, flush_id) = (manager.clone(), id.clone());
                if run_leased(move || flusher.flush_owed_save(&flush_id, epoch))
                    .await
                    .is_err()
                {
                    tracing::warn!(auth_id = %id, "owed save did not run");
                }
            }
            result
        };
        if tokio::runtime::Handle::try_current().is_err() {
            // No runtime to own the task: run it here, still as the
            // operation. Keep this rare await boxed: embedding the refresh
            // transaction here also inflates every dispatch and idle worker
            // future, even when a runtime always takes the spawned path.
            return Box::pin(operation.scope(work)).await;
        }
        let task = gate
            .spawn(&operation, work)
            .map_err(ManagerError::NotActive)?;
        drop(operation);
        task.await
            .map_err(|err| ManagerError::Other(format!("refresh task failed: {err}")))?
    }

    /// The body of [`refresh_with`](Self::refresh_with), running as its
    /// operation.
    async fn refresh_owned(
        &self,
        id: &str,
        failed_token: &str,
        epoch: u64,
        force: bool,
        resolution_only: bool,
    ) -> Result<Arc<Auth>, ManagerError> {
        if !failed_token.is_empty() {
            if epoch == 0 {
                self.mark_rejected_access_token(id, failed_token);
            } else {
                self.mark_rejected_access_token_at_epoch(id, failed_token, epoch);
            }
        }
        let id_lock = lock(&self.shared.refresh_locks)
            .entry(id.to_owned())
            .or_default()
            .clone();
        let _serialized = id_lock.lock().await;

        let (auth, base_epoch, executor, landing) = {
            let state = self.lock();
            let Some(entry) = state.auths.get(id) else {
                return Err(ManagerError::Other("auth or executor not found".into()));
            };
            let Some(executor) = Self::executor_for(&state, &entry.auth) else {
                return Err(ManagerError::Other("auth or executor not found".into()));
            };
            (
                entry.auth.clone(),
                entry.auth.registration_epoch,
                executor,
                entry.landing_unconfirmed,
            )
        };
        if epoch != 0 && base_epoch != epoch {
            return Err(ManagerError::Other(
                "auth registration changed before refresh".into(),
            ));
        }
        if has_disabled_invalid_grant_failure(&auth) && !force {
            return Err(ManagerError::Other(
                "auth is disabled with invalid grant".into(),
            ));
        }
        if has_unauthorized_auth_failure(&auth) && !force {
            return Err(ManagerError::Other("auth is unauthorized".into()));
        }
        // Another call may have refreshed the credential already.
        if !failed_token.is_empty() {
            let current = access_token(&auth);
            if !auth.matches_access_token_partition(failed_token)
                && (!current.is_empty() || !auth.outer_access_token().is_empty())
            {
                return Ok(auth);
            }
        }

        // Only a Codex record under a store takes the credential lock. For any
        // other, a landing hold still defers the pass, forced or not.
        let lease_store = self.shared.store.clone().filter(|_| is_codex_auth(&auth));
        if lease_store.is_none() && (landing.outer || landing.siwc) {
            return Err(self.defer_refresh(
                id,
                base_epoch,
                Deferral::LandingUnconfirmed,
                io::Error::other("refresh deferred: landing unconfirmed"),
            ));
        }

        // Steps 0b to 6b of the transaction: a Codex record under a store
        // takes the credential lock for the whole pass. Anything else, and a
        // record the lease cannot serve, refreshes as it always did.
        let mut leased: Option<LeasedPass> = None;
        let mut options = crate::executor::RefreshOptions::forced(force);
        let mut auth = auth;
        if let Some(store) = lease_store.as_ref() {
            'leased: {
                let Some(prep) =
                    self.locked(|s| self.prepare_leased_refresh_locked(s, id, base_epoch))
                else {
                    return Err(changed_before_refresh());
                };
                if prep.landing_unconfirmed.outer || prep.landing_unconfirmed.siwc {
                    return Err(self.defer_refresh(
                        id,
                        base_epoch,
                        Deferral::LandingUnconfirmed,
                        io::Error::other("refresh deferred: landing unconfirmed"),
                    ));
                }
                if prep.ineligible {
                    break 'leased;
                }
                let (lease_store, lease_auth) = (Arc::clone(store), Arc::clone(&auth));
                let lease = run_leased(move || lease_store.codex_refresh_lease(&lease_auth))
                    .await
                    .unwrap_or_else(|error| Err(lease_task_error(error)));
                let mut token = match lease {
                    // lease: held
                    Ok(CodexLease::Held(token)) => token,
                    Ok(CodexLease::Unsupported) => {
                        if prep.unresolved.is_some() {
                            return Err(self.defer_refresh(
                                id,
                                base_epoch,
                                Deferral::OriginUnresolved,
                                io::Error::other("origin unresolved: no credential lock"),
                            ));
                        }
                        break 'leased;
                    }
                    Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                        return Err(self.defer_refresh(id, base_epoch, Deferral::LockBusy, error));
                    }
                    Err(error) => {
                        return Err(self.defer_refresh(
                            id,
                            base_epoch,
                            Deferral::LeaseError,
                            error,
                        ));
                    }
                };

                // 3: what the file holds now.
                let (read_store, read_token) = (Arc::clone(store), token);
                let read = run_leased(move || {
                    let read = read_store.codex_lease_read(&read_token);
                    (read_token, read)
                })
                .await;
                let (back, read) = match read {
                    Ok(done) => done,
                    Err(error) => {
                        return Err(self.defer_refresh(
                            id,
                            base_epoch,
                            Deferral::LeaseError,
                            lease_task_error(error),
                        ));
                    }
                };
                token = back;
                let file = match read {
                    Err(error) => {
                        drop(token);
                        return Err(self.defer_refresh(
                            id,
                            base_epoch,
                            Deferral::LeaseError,
                            error,
                        ));
                    }
                    Ok(None) => {
                        drop(token);
                        return Err(self.defer_refresh(
                            id,
                            base_epoch,
                            Deferral::MissingFile,
                            io::Error::new(io::ErrorKind::NotFound, "credential file is missing"),
                        ));
                    }
                    Ok(Some(file)) => file,
                };
                if !file
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("codex"))
                {
                    drop(token);
                    return Err(self.defer_refresh(
                        id,
                        base_epoch,
                        Deferral::NotCodexFile,
                        io::Error::other("credential file is not a Codex file"),
                    ));
                }

                // 4: the gate may have closed during the wait.
                let state = self.gate().state();
                if state != GateState::Open {
                    drop(token);
                    return Err(ManagerError::NotActive(NotActive { state }));
                }

                // 4b: settle the unresolved origin against the file just locked.
                let mut resolved = false;
                let mut settled: Option<Protection> = None;
                if let Some(held) = prep.unresolved.as_ref() {
                    let (pinned, here) = (held.origin.clone(), token.origin().clone());
                    let proof = run_leased(move || pinned.prove(&here))
                        .await
                        .unwrap_or(OriginProof::Unproven);
                    match self.locked(|s| {
                        self.resolve_unresolved_locked(s, id, base_epoch, token.origin(), proof)
                    }) {
                        Resolution::Same => resolved = true,
                        Resolution::Different(protection) => {
                            resolved = true;
                            settled = Some(protection);
                        }
                        Resolution::Unproven => {
                            drop(token);
                            return Err(self.defer_refresh(
                                id,
                                base_epoch,
                                Deferral::OriginUnresolved,
                                io::Error::other("origin unresolved"),
                            ));
                        }
                        Resolution::RegistrationChanged => {
                            drop(token);
                            return Err(changed_before_refresh());
                        }
                    }
                }

                // 4c: settle the read-back marks against the same file.
                if !prep.read_back.is_empty() {
                    let mut proofs = Vec::with_capacity(prep.read_back.len());
                    for (partition, origin) in &prep.read_back {
                        let proof = if origin == token.origin() {
                            OriginProof::Same
                        } else {
                            let (pinned, here) = (origin.clone(), token.origin().clone());
                            run_leased(move || pinned.prove(&here))
                                .await
                                .unwrap_or(OriginProof::Unproven)
                        };
                        proofs.push((*partition, proof));
                    }
                    match self.locked(|s| {
                        self.settle_read_back_locked(
                            s,
                            id,
                            base_epoch,
                            token.origin(),
                            &proofs,
                            &file,
                        )
                    }) {
                        ReadBackSettlement::Hold => {
                            drop(token);
                            return Err(self.defer_refresh(
                                id,
                                base_epoch,
                                Deferral::LandingUnconfirmed,
                                io::Error::other("landing unconfirmed"),
                            ));
                        }
                        ReadBackSettlement::Unproven => {
                            drop(token);
                            return Err(self.defer_refresh(
                                id,
                                base_epoch,
                                Deferral::OriginUnresolved,
                                io::Error::other("origin unresolved"),
                            ));
                        }
                        ReadBackSettlement::RegistrationChanged => {
                            drop(token);
                            return Err(changed_before_refresh());
                        }
                        ReadBackSettlement::Settled(protection) => settled = Some(protection),
                    }
                }

                // 5: commit the work the entry still owes the file.
                let mut prot = settled.unwrap_or_else(|| Protection {
                    undurable: prep.undurable,
                    undurable_baselines: prep.undurable_baselines.clone(),
                });
                let mut disk = file;
                if !prep.ops.is_empty() {
                    let admission = self
                        .leased_origin_check(id, token.origin(), true, |s, check| {
                            self.admit_leased_locked(s, id, &prep, token.origin(), check, &prep.ops)
                        })
                        .await;
                    if let Some(error) = self.refuse_admission(id, base_epoch, admission) {
                        drop(token);
                        return Err(error);
                    }
                    let composed = CredentialRefreshPatch::default()
                        .with_pending_operations(&prep.ops)
                        .with_durable_baselines(
                            &prep.undurable_baselines.outer,
                            &prep.undurable_baselines.siwc,
                        );
                    let mut auth_no_pending = prep.live.clone();
                    auth_no_pending.codex_pending_refresh.clear();
                    auth_no_pending.codex_persist_intent = Default::default();
                    let (commit_store, commit_token) = (Arc::clone(store), token);
                    let committed = run_leased(move || {
                        let result = commit_store.codex_lease_commit(
                            &commit_token,
                            &auth_no_pending,
                            &composed,
                        );
                        (commit_token, result)
                    })
                    .await;
                    let (back, result) = match committed {
                        Ok(done) => done,
                        Err(error) => {
                            return Err(self.defer_refresh(
                                id,
                                base_epoch,
                                Deferral::PendingWriteFailed,
                                lease_task_error(error),
                            ));
                        }
                    };
                    token = back;
                    let persisted = match result {
                        Ok(persisted) => persisted,
                        Err(error) => {
                            drop(token);
                            return Err(self.defer_refresh(
                                id,
                                base_epoch,
                                Deferral::PendingWriteFailed,
                                error,
                            ));
                        }
                    };
                    match self
                        .leased_origin_check(id, token.origin(), true, |s, check| {
                            self.apply_pending_outcomes_locked(
                                s,
                                id,
                                &prep,
                                token.origin(),
                                check,
                                &persisted,
                            )
                        })
                        .await
                    {
                        PendingOutcome::Applied(protection) => {
                            prot = protection;
                            disk = persisted.auth.metadata;
                        }
                        PendingOutcome::Refused(admission) => {
                            drop(token);
                            return Err(self
                                .refuse_admission(id, base_epoch, admission)
                                .unwrap_or_else(changed_before_refresh));
                        }
                    }
                }

                // 6: adopt what a peer wrote, never a protected predecessor.
                let mut merged = Auth {
                    metadata: disk,
                    ..prep.live.clone()
                };
                overlay_undurable(&mut merged, &prep.live, prot.undurable);
                let still: Vec<CredentialRefreshPatch> = prep
                    .ops
                    .iter()
                    .filter(|patch| !patch.is_written())
                    .cloned()
                    .collect();
                if !still.is_empty() {
                    let composed =
                        CredentialRefreshPatch::default().with_pending_operations(&still);
                    let (rotated, applied) = composed.apply(&merged.metadata);
                    if applied.outer == Some(true) {
                        for key in OUTER_KEYS {
                            match rotated.get(*key) {
                                Some(value) => {
                                    merged.metadata.insert((*key).to_owned(), value.clone());
                                }
                                None => {
                                    merged.metadata.remove(*key);
                                }
                            }
                        }
                    }
                    if applied.siwc == Some(true) {
                        match rotated.get("siwc") {
                            Some(value) => {
                                merged.metadata.insert("siwc".to_owned(), value.clone());
                            }
                            None => {
                                merged.metadata.remove("siwc");
                            }
                        }
                    }
                }
                let disk_effective = merged.metadata;
                let adopted = disk_rotated_partitions(&auth.metadata, &disk_effective);
                let was_due = self.due_ignoring_hold(&auth, &executor);
                if adopted.any() {
                    let published = self
                        .leased_origin_check(id, token.origin(), true, |s, check| {
                            self.publish_adoption_locked(
                                s,
                                id,
                                &prep,
                                token.origin(),
                                check,
                                &disk_effective,
                                adopted,
                            )
                        })
                        .await;
                    match published {
                        Ok((snapshot, generation)) => {
                            self.publish_projections(&snapshot, generation, self.now(), true);
                            self.queue_refresh_reschedule(id);
                            auth = snapshot;
                        }
                        Err(admission) => {
                            drop(token);
                            return Err(self
                                .refuse_admission(id, base_epoch, admission)
                                .unwrap_or_else(changed_before_refresh));
                        }
                    }
                    if !failed_token.is_empty() {
                        let current = access_token(&auth);
                        if !auth.matches_access_token_partition(failed_token)
                            && (!current.is_empty() || !auth.outer_access_token().is_empty())
                        {
                            drop(token);
                            return Ok(auth);
                        }
                    }
                }

                // 6c: a pass that only settled the origin sends nothing that is not due.
                if (resolved || resolution_only)
                    && !force
                    && failed_token.is_empty()
                    && !self.due_ignoring_hold(&auth, &executor)
                {
                    drop(token);
                    return Ok(auth);
                }

                // 6b: the record still names this file, whatever happened meanwhile.
                let admission = self
                    .leased_origin_check(id, token.origin(), true, |s, check| {
                        self.admit_leased_locked(s, id, &prep, token.origin(), check, &still)
                    })
                    .await;
                if let Some(error) = self.refuse_admission(id, base_epoch, admission) {
                    drop(token);
                    return Err(error);
                }

                options = crate::executor::RefreshOptions {
                    force,
                    adopted: Some(AdoptedPartitions {
                        outer: adopted.outer,
                        siwc: adopted.siwc,
                        was_due,
                    }),
                };
                leased = Some(LeasedPass {
                    token,
                    prep,
                    prot,
                    disk_effective,
                });
            }
        }

        // 0c: a job queued only for the unresolved origin must not rotate a
        // token that is not due, whatever deferral holds its next check back.
        // Only today's path gets here without the lease: a record under no
        // store, one the lease cannot serve, and one whose store has no
        // credential lock and no unresolved origin. A leased pass has its own
        // exit at 6c, after it resolved the origin.
        if leased.is_none() && resolution_only && !force && failed_token.is_empty() {
            let mut probe = Auth::clone(&auth);
            probe.next_refresh_after = None;
            if !should_refresh(&probe, executor.refresh_lead(), self.now()) {
                return Ok(auth);
            }
        }

        // 7: the one send. Every way out but a send drops the lease first.
        let sent = match self
            .send_refresh(id, base_epoch, &executor, &auth, options)
            .await
        {
            Ok(SendOutcome::Sent(sent)) => *sent,
            Ok(SendOutcome::Deferred(kind)) => {
                drop(leased.take());
                return Err(self.defer_refresh(
                    id,
                    base_epoch,
                    kind,
                    io::Error::other("refresh deferred before the send"),
                ));
            }
            Ok(SendOutcome::Superseded(live)) => {
                drop(leased.take());
                // A token command replaced a refresh token since the
                // snapshot, so the snapshot's tokens must not be sent. With
                // a failed token, the live credential answers the caller
                // only when its access token is a different, non-empty one.
                if failed_token.is_empty() {
                    return Ok(live);
                }
                let current = access_token(&live);
                if !live.matches_access_token_partition(failed_token)
                    && (!current.is_empty() || !live.outer_access_token().is_empty())
                {
                    return Ok(live);
                }
                return Err(ManagerError::Other("auth changed before refresh".into()));
            }
            Err(error) => {
                drop(leased.take());
                return Err(error);
            }
        };
        let outcome = sent.map_err(|mut error| {
            let mut secrets = crate::observe::redact::Secrets::new();
            secrets.add_auth(&auth);
            error.message = secrets.text(error.message, crate::observe::redact::Policy::Disk);
            if let Some(cause) = error.cause.take() {
                error.cause = Some(secrets.text(cause, crate::observe::redact::Policy::Disk));
            }
            error
        });
        if outcome.is_err() {
            drop(leased.take());
        }
        let now = self.now();
        let mut updated = match outcome {
            Ok(updated) => updated,
            Err(err) if err.kind == ErrorKind::Canceled => {
                tracing::debug!(auth_id = %id, "refresh canceled");
                self.retry_after_cancel(id, now);
                return Err(ManagerError::Refresh(err));
            }
            Err(err) => {
                tracing::debug!(auth_id = %id, status = err.status, "refresh failed");
                let attempt = FailedRefresh {
                    base_epoch,
                    failed_token,
                    force,
                    partition: None,
                };
                self.record_refresh_failure(id, &attempt, &err, now);
                return Err(ManagerError::Refresh(err));
            }
        };
        // Not upstream's: the provider may have killed the old refresh token.
        #[cfg(feature = "blue-green-test")]
        let mut save_failure = crate::test_hooks::barrier("refresh.after_rotate")
            .await
            .err();
        let partial_failure = updated.codex_refresh_failure.clone();
        let partial_baseline = partial_failure.as_ref().map(|failure| {
            crate::auth::codex_refresh::CredentialRefreshPatch::new(
                &updated.metadata,
                &updated.metadata,
                failure.partition == crate::auth::CodexCredentialPartition::Outer,
                failure.partition == crate::auth::CodexCredentialPartition::Siwc,
            )
        });
        let mut partial_secrets = crate::observe::redact::Secrets::new();
        if partial_failure.is_some() {
            partial_secrets.add_auth(&auth);
            partial_secrets.add_auth(&updated);
        }
        if crate::auth::codex_refresh::is_codex_auth(&updated)
            && !matches!(
                updated.codex_persist_intent,
                crate::auth::codex_refresh::CodexPersistIntent::Refresh(_)
            )
        {
            updated.codex_persist_intent = crate::auth::codex_refresh::CodexPersistIntent::Refresh(
                crate::auth::codex_refresh::CredentialRefreshPatch::new(
                    &auth.metadata,
                    &updated.metadata,
                    crate::auth::codex_refresh::OUTER_KEYS
                        .iter()
                        .any(|key| auth.metadata.get(*key) != updated.metadata.get(*key)),
                    auth.metadata.get("siwc") != updated.metadata.get("siwc")
                        && updated.metadata.contains_key("siwc"),
                ),
            );
        }
        updated.last_refreshed_at = Some(now);
        updated.next_refresh_after = None;
        updated.preserve_rejected_tokens_from(&auth, true);
        if !updated.is_active_access_token_rejected() {
            updated.last_error = None;
            updated.status_message.clear();
            updated.unavailable = false;
            if matches!(updated.status, Status::Error | Status::Unknown) {
                updated.status = Status::Active;
            }
            clear_unauthorized_model_states(&mut updated, now);
        }
        updated.updated_at = Some(now);
        if should_refresh(&updated, executor.refresh_lead(), now) {
            updated.next_refresh_after = Some(add(now, REFRESH_INEFFECTIVE_BACKOFF));
        }

        // 8 to 11: commit the rotation to the leased file, publish it, and
        // release the lease before any ordinary save.
        let was_leased = leased.is_some();
        let mut tail: Option<LeasedTail> = None;
        if let Some(pass) = leased.take() {
            let rotated = updated
                .codex_persist_intent
                .refresh_patch()
                .map(CredentialRefreshPatch::requested)
                .unwrap_or_default();
            if rotated.any() {
                let LeasedPass {
                    token,
                    prep,
                    prot,
                    disk_effective,
                } = pass;
                let origin = token.origin().clone();
                let final_patch = CredentialRefreshPatch::with_disk_baseline(
                    &auth.metadata,
                    Some(&disk_effective),
                    &updated.metadata,
                    rotated.outer,
                    rotated.siwc,
                )
                .with_durable_baselines(
                    &prot.undurable_baselines.outer,
                    &prot.undurable_baselines.siwc,
                );
                updated.codex_persist_intent =
                    crate::auth::codex_refresh::CodexPersistIntent::Refresh(final_patch.clone());
                let (token, committed) = self
                    .commit_rotation_with_retry(id, &prep, token, &final_patch)
                    .await;
                let publication = self
                    // The commit loses its token with a lost blocking task, and
                    // the lock with it.
                    .leased_origin_check(id, &origin, token.is_some(), |s, check| {
                        self.publish_rotation_locked(
                            s,
                            id,
                            &prep,
                            &origin,
                            check,
                            &auth,
                            &mut updated,
                            &committed,
                            now,
                        )
                    })
                    .await;
                match publication {
                    Publication::Published {
                        snapshot,
                        generation,
                        tail: published,
                    } => {
                        self.publish_projections(&snapshot, generation, now, true);
                        self.queue_refresh_reschedule(id);
                        drop(token);
                        // lease: released
                        #[cfg(test)]
                        {
                            let hook = lock(&self.shared.lease_tail_hook).clone();
                            if let Some(hook) = hook {
                                hook(snapshot.as_ref());
                            }
                        }
                        tail = Some(*published);
                    }
                    Publication::Refused => {
                        drop(token);
                        return Err(changed_during_refresh(id));
                    }
                }
            } else {
                // Nothing was rotated, so there is nothing to commit; the
                // ordinary save below runs without a leased tail.
                drop(pass);
            }
        }

        // 12: the ordinary save, with the tail the publication left.
        #[cfg(feature = "blue-green-test")]
        let save_result = {
            if save_failure.is_none() {
                save_failure = crate::test_hooks::barrier("refresh.before_save")
                    .await
                    .err();
            }
            if save_failure.is_some() {
                // The common update path logs one generic injected save
                // failure; the barrier-specific cause is not carried into it.
                self.update_refreshed_after_save_failure(&auth, base_epoch, updated, tail)
            } else {
                self.save_refreshed(&auth, base_epoch, updated, tail).await
            }
        };
        #[cfg(not(feature = "blue-green-test"))]
        let save_result = self.save_refreshed(&auth, base_epoch, updated, tail).await;
        // 13: a state the publication or a fence recorded holds the next pass back.
        if was_leased {
            self.schedule_unresolved(id, base_epoch);
        }
        let (saved, generation) = match save_result {
            Ok(Some(saved)) => saved,
            Ok(None) => return Err(ManagerError::Other(format!("auth {id} not found"))),
            Err(err) => {
                tracing::warn!(
                    auth_id = %id,
                    provider = %auth.provider,
                    "persist refreshed auth failed: {err}"
                );
                return Err(err);
            }
        };
        // Not upstream's: an abort between an offloaded save and publication
        // can leave the saved credential without these projections. The owned
        // refresh task keeps running when its external caller is cancelled.
        self.publish_projections(&saved, generation, now, true);
        if let Some(failure) = partial_failure {
            let mut secrets = partial_secrets;
            secrets.add_auth(&saved);
            let partial = PartialFailure {
                failure,
                baseline: partial_baseline,
                secrets,
            };
            #[cfg(test)]
            partial_failure_seam::fire(&self.shared).await;
            let attempt = FailedRefresh {
                base_epoch,
                failed_token,
                force,
                partition: Some(partial.failure.partition),
            };
            return self.record_partial_failure(id, partial, &attempt, now);
        }
        Ok(saved)
    }

    /// Schedules the retry a canceled refresh asks for, as the loop would have.
    fn retry_after_cancel(&self, id: &str, now: Timestamp) {
        {
            let mut state = self.lock();
            if let Some(entry) = state.auths.get_mut(id) {
                let next = entry.auth.next_refresh_after;
                if is_zero(next) || next.is_some_and(|t| t < now) {
                    Arc::make_mut(&mut entry.auth).next_refresh_after =
                        Some(add(now, REFRESH_CANCELED_RETRY));
                }
                state.sync_scheduler(self.models(), id, now);
            }
        }
        self.queue_refresh_reschedule(id);
    }

    /// Whether `auth` is due for a refresh, whatever hold its next check has.
    fn due_ignoring_hold(&self, auth: &Auth, executor: &Arc<dyn ProviderExecutor>) -> bool {
        let mut probe = auth.clone();
        probe.next_refresh_after = None;
        should_refresh(&probe, executor.refresh_lead(), self.now())
    }

    /// What a refused admission makes of the pass: the error to hand back, or
    /// `None` when the admission holds. The caller drops the lease first.
    fn refuse_admission(
        &self,
        id: &str,
        base_epoch: u64,
        admission: LeaseAdmission,
    ) -> Option<ManagerError> {
        match admission {
            LeaseAdmission::Admitted => None,
            LeaseAdmission::RegistrationChanged | LeaseAdmission::OriginLost => {
                Some(changed_before_refresh())
            }
            LeaseAdmission::OriginUnproven => Some(self.defer_refresh(
                id,
                base_epoch,
                Deferral::OriginUnresolved,
                io::Error::other("origin unresolved"),
            )),
            LeaseAdmission::PolicyRefused => Some(self.defer_refresh(
                id,
                base_epoch,
                Deferral::PolicyChanged,
                io::Error::other("refresh deferred: policy changed"),
            )),
        }
    }

    /// Persists a refreshed credential and folds it in, off the runtime
    /// worker when the save can wait for a credential lock (Codex, or any
    /// save while the test barriers are installed). An operation retains its
    /// admission in run_blocking; without one, spawn_blocking owns the save.
    /// Without a runtime it runs inline.
    async fn save_refreshed(
        &self,
        base: &Arc<Auth>,
        base_epoch: u64,
        updated: Auth,
        lease: Option<LeasedTail>,
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        let blocking =
            self.shared.store.is_some() && crate::auth::codex_refresh::is_codex_auth(&updated);
        #[cfg(feature = "blue-green-test")]
        let blocking = blocking || crate::test_hooks::is_installed();
        let save = move |manager: &Self, base: &Auth, updated: Auth| match lease {
            Some(tail) => manager.update_refreshed_leased(base, base_epoch, updated, tail),
            None => manager.update_refreshed(base, base_epoch, updated),
        };
        if !blocking || tokio::runtime::Handle::try_current().is_err() {
            return save(self, base, updated);
        }
        let manager = self.clone();
        let base = base.clone();
        let saved = if let Some(operation) = crate::gate::Operation::current() {
            // run_blocking owns admission even if the caller drops its await.
            operation
                .run_blocking(move || save(&manager, &base, updated))
                .await
        } else {
            // No operation is in scope; the owned closure retains manager save admission.
            #[allow(clippy::disallowed_methods)]
            tokio::task::spawn_blocking(move || save(&manager, &base, updated)).await
        };
        saved.map_err(|error| {
            ManagerError::Store(std::io::Error::other(format!(
                "refresh save task failed: {error}"
            )))
        })?
    }

    /// Records the failed partition of a Codex refresh whose other partition
    /// rotated and was saved (L7, L33). The check against the live
    /// credential and the recording happen under one state lock: a failure
    /// the live credential has superseded (re-registered, or the failed
    /// partition's tokens replaced) is discarded and the current credential
    /// returned. A failure of the partition the live switch selects is
    /// recorded like a failed full refresh and returned as an error; the
    /// other partition's gets a bounded retry and leaves the status alone.
    fn record_partial_failure(
        &self,
        id: &str,
        partial: PartialFailure,
        attempt: &FailedRefresh<'_>,
        now: Timestamp,
    ) -> Result<Arc<Auth>, ManagerError> {
        let PartialFailure {
            failure,
            baseline,
            secrets,
        } = partial;
        let mut error = failure.error;
        error.message = secrets.text(error.message, crate::observe::redact::Policy::Disk);
        if let Some(cause) = error.cause.take() {
            error.cause = Some(secrets.text(cause, crate::observe::redact::Policy::Disk));
        }
        let mut state = self.lock();
        let Some(entry) = state.auths.get(id) else {
            return Err(ManagerError::Other("auth removed after refresh".into()));
        };
        if entry.auth.registration_epoch != attempt.base_epoch
            || baseline
                .as_ref()
                .is_some_and(|patch| !patch.can_apply_to(&entry.auth.metadata))
        {
            // A re-registration, bind or login replaced the failed tokens
            // since the refresh began: that failure is obsolete and the
            // current credential's tokens aren't marked.
            return Ok(entry.auth.clone());
        }
        if failure.partition.is_active(&entry.auth) {
            let after = self.record_refresh_failure_locked(&mut state, id, attempt, &error, now);
            drop(state);
            self.apply_after_failure(id, after);
            return Err(ManagerError::Refresh(error));
        }
        // The partition the switch doesn't use: retry it after a backoff
        // without touching the credential's status.
        let Some(entry) = state.auths.get_mut(id) else {
            return Err(ManagerError::Other("auth removed after refresh".into()));
        };
        let live = Arc::make_mut(&mut entry.auth);
        live.next_refresh_after = Some(add(now, REFRESH_FAILURE_BACKOFF));
        live.last_error = Some(refresh_error_from_error(ErrView::Exec(&error)));
        live.codex_refresh_failure = Some(crate::auth::CodexRefreshFailure {
            error,
            partition: failure.partition,
        });
        live.generation = live.generation.saturating_add(1);
        let snapshot = entry.auth.clone();
        state.sync_scheduler(self.models(), id, now);
        drop(state);
        self.queue_refresh_reschedule(id);
        Ok(snapshot)
    }

    /// Marks `failed_token` as refused while it is still the active or nested
    /// bearer, regardless of nominal expiry, so the
    /// credential reads as expired, and isn't picked, until a refresh works
    /// or its tokens change (upstream's `markRejectedAccessToken`).
    pub(crate) fn mark_rejected_access_token(&self, id: &str, failed_token: &str) {
        self.mark_rejected_access_token_at_epoch(id, failed_token, 0);
    }

    fn mark_rejected_access_token_at_epoch(&self, id: &str, failed_token: &str, epoch: u64) {
        let now = self.now();
        let mut state = self.lock();
        let Some(entry) = state.auths.get_mut(id) else {
            return;
        };
        if epoch > 0 && epoch != entry.auth.registration_epoch {
            return;
        }
        if !entry.auth.needs_rejected_token(failed_token) {
            return;
        }
        let auth = Arc::make_mut(&mut entry.auth);
        auth.record_rejected_token(failed_token);
        auth.generation = auth.generation.saturating_add(1);
        auth.updated_at = Some(now);
        state.sync_scheduler(self.models(), id, now);
    }

    /// Records a failed refresh on the live credential and sets when to try
    /// again (the failure branch of upstream's
    /// `refreshAuthForRequestAtEpoch`). Nothing changes when the credential
    /// was registered again meanwhile, or, unless the refresh was forced,
    /// when it is out of rotation for rejected tokens.
    ///
    /// When the refresh token is refused (`invalid_grant`) after the
    /// provider rejected the access token, neither can recover without a new
    /// login: the credential goes out of rotation and off the refresh queue
    /// until its tokens change, however long its access token claims to
    /// last. A failed forced refresh of such a credential leaves it out.
    fn record_refresh_failure(
        &self,
        id: &str,
        attempt: &FailedRefresh<'_>,
        err: &ExecError,
        now: Timestamp,
    ) {
        let after = {
            let mut state = self.lock();
            self.record_refresh_failure_locked(&mut state, id, attempt, err, now)
        };
        self.apply_after_failure(id, after);
    }

    fn apply_after_failure(&self, id: &str, after: Option<AfterFailure>) {
        match after {
            Some(AfterFailure::Reschedule) => self.queue_refresh_reschedule(id),
            Some(AfterFailure::Unschedule) => self.queue_refresh_unschedule(id),
            Some(AfterFailure::Nothing) | None => {}
        }
    }

    /// [`record_refresh_failure`](Self::record_refresh_failure) on the state
    /// the caller already holds, so a caller can check the live credential
    /// and record in one step. `None` when nothing was recorded; otherwise
    /// what to do with the loop's queue once the lock is released.
    fn record_refresh_failure_locked(
        &self,
        state: &mut State,
        id: &str,
        attempt: &FailedRefresh<'_>,
        err: &ExecError,
        now: Timestamp,
    ) -> Option<AfterFailure> {
        let unauthorized = is_unauthorized_error(ErrView::Exec(err));
        let invalid_grant = is_invalid_grant_error(ErrView::Exec(err));
        let entry = state.auths.get_mut(id)?;
        if entry.auth.registration_epoch != attempt.base_epoch {
            return None;
        }
        let was_terminal_unauthorized = has_unauthorized_auth_failure(&entry.auth);
        if was_terminal_unauthorized && !attempt.force {
            return None;
        }
        let mut failures = entry.refresh_failures;
        let auth = Arc::make_mut(&mut entry.auth);
        auth.generation = auth.generation.saturating_add(1);
        auth.updated_at = Some(now);
        if let Some(partition) = attempt.partition {
            auth.codex_refresh_failure = Some(crate::auth::CodexRefreshFailure {
                error: err.clone(),
                partition,
            });
        }
        if was_terminal_unauthorized {
            auth.unavailable = true;
            auth.status = Status::Error;
            auth.next_refresh_after = None;
            auth.next_retry_after = None;
            if unauthorized || invalid_grant {
                auth.last_error = Some(terminal_unauthorized_error(err));
                auth.status_message = TERMINAL_UNAUTHORIZED_MESSAGE.into();
            }
            state.sync_scheduler(self.models(), id, now);
            return Some(AfterFailure::Unschedule);
        }
        auth.last_error = Some(refresh_error_from_error(ErrView::Exec(err)));
        let disabled = is_disabled(auth);
        // The provider rejected this very access token, so its expiry no
        // longer says it works.
        let access_token_rejected = auth.matches_access_token_partition(attempt.failed_token);
        let after = if disabled && invalid_grant {
            auth.unavailable = true;
            auth.status = Status::Disabled;
            auth.next_refresh_after = None;
            failures = 0;
            auth.status_message = "disabled (invalid grant)".into();
            AfterFailure::Unschedule
        } else if disabled {
            auth.unavailable = true;
            auth.status = Status::Disabled;
            auth.next_refresh_after = Some(add(now, REFRESH_FAILURE_BACKOFF));
            if auth.status_message.is_empty() {
                auth.status_message = "disabled".into();
            }
            AfterFailure::Reschedule
        } else if access_token_rejected && invalid_grant {
            auth.unavailable = true;
            auth.status = Status::Error;
            auth.next_refresh_after = None;
            auth.next_retry_after = None;
            failures = 0;
            auth.last_error = Some(terminal_unauthorized_error(err));
            auth.status_message = TERMINAL_UNAUTHORIZED_MESSAGE.into();
            AfterFailure::Unschedule
        } else if !auth.has_valid_access_token(now) || access_token_rejected {
            auth.unavailable = true;
            auth.status = Status::Error;
            if unauthorized {
                auth.next_refresh_after = None;
                auth.next_retry_after = None;
                failures = 0;
                auth.status_message = "unauthorized".into();
                AfterFailure::Nothing
            } else if invalid_grant {
                failures = failures.saturating_add(1);
                auth.next_refresh_after = Some(add(now, invalid_grant_backoff(failures)));
                auth.status_message = "invalid grant (retrying)".into();
                AfterFailure::Reschedule
            } else {
                failures = 0;
                auth.next_refresh_after = Some(add(now, REFRESH_FAILURE_BACKOFF));
                auth.status_message = "token expired".into();
                AfterFailure::Reschedule
            }
        } else {
            // The access token still works: keep the credential's
            // status, and only set when to try again.
            let mut next_retry = add(now, REFRESH_FAILURE_BACKOFF);
            if invalid_grant {
                failures = failures.saturating_add(1);
                next_retry = add(now, invalid_grant_backoff(failures));
            } else {
                failures = 0;
            }
            if let Some(expiry) = non_zero(auth.access_token_expiration_time())
                && next_retry > expiry
            {
                next_retry = expiry;
            }
            auth.next_refresh_after = Some(next_retry);
            if !auth.unavailable {
                tracing::warn!(
                    auth_id = %id,
                    provider = %auth.provider,
                    status = err.status,
                    "credential refresh failed; keeping the credential as its access token is unexpired"
                );
            }
            AfterFailure::Reschedule
        };
        entry.refresh_failures = failures;
        state.sync_scheduler(self.models(), id, now);
        Some(after)
    }
}

/// The longest wait before an unresolved origin's pass is tried again once it
/// has deferred `ORIGIN_UNRESOLVED_SURFACE_AFTER` times, and before a landing
/// hold's next check.
const ORIGIN_UNRESOLVED_BACKOFF_CAP: Duration = Duration::from_secs(30 * 60);
/// The shortest wait after an unsettled read-back mark deferred a refresh.
const READ_BACK_UNSETTLED_FLOOR: Duration = Duration::from_secs(60);
/// The longest wait after an unsettled read-back mark deferred a refresh.
const READ_BACK_UNSETTLED_CAP: Duration = Duration::from_secs(30 * 60);

/// What a leased pass reads from the entry before it takes any I/O step.
/// Owned: the state lock is released before the lease is taken.
struct LeasePrep {
    base_epoch: u64,
    discard_epoch: u64,
    undurable: UndurablePartitions,
    undurable_baselines: UndurableBaselines,
    /// `permanently_ineligible(&entry.auth)`.
    ineligible: bool,
    live: Auth,
    /// The unwritten operations the pass must commit first.
    ops: Vec<CredentialRefreshPatch>,
    /// The entry's unresolved state, copied. Only the `*_locked` functions
    /// that resolve it change the live state.
    unresolved: Option<UnresolvedOrigin>,
    /// The partitions whose mark has a `ReadBack` cause, with that origin.
    read_back: Vec<(CodexCredentialPartition, CodexOrigin)>,
    landing_unconfirmed: UndurablePartitions,
}

/// Whether the leased file is still the live registration's, and may be
/// written for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseAdmission {
    Admitted,
    /// The entry is absent or at another registration epoch.
    RegistrationChanged,
    /// Same epoch, and the live record is proven to name another file.
    OriginLost,
    /// Same epoch, and the live record's file identity is not established.
    OriginUnproven,
    PolicyRefused,
}

/// The marks a transaction carries forward.
#[derive(Clone)]
struct Protection {
    undurable: UndurablePartitions,
    undurable_baselines: UndurableBaselines,
}

enum PendingOutcome {
    Applied(Protection),
    /// Registration or origin refused it; nothing was marked or cleared.
    Refused(LeaseAdmission),
}

enum Resolution {
    Same,
    Different(Protection),
    Unproven,
    RegistrationChanged,
}

enum ReadBackSettlement {
    Settled(Protection),
    /// A landing hold was set; nothing may be sent.
    Hold,
    Unproven,
    RegistrationChanged,
}

/// Why a refresh pass sent nothing and waits (see [`Manager::defer_refresh`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Deferral {
    LockBusy,
    MissingFile,
    NotCodexFile,
    LeaseError,
    PendingWriteFailed,
    PolicyChanged,
    OriginUnresolved,
    LandingUnconfirmed,
    ReadBackUnsettled,
}

/// What a pass that holds the lease carries from the read to the release.
struct LeasedPass {
    token: CodexLeaseToken,
    prep: LeasePrep,
    prot: Protection,
    /// The file's content with the unwritten work and the marks overlaid.
    disk_effective: Map<String, Value>,
}

/// The error of a pass whose entry is gone or at another epoch.
fn changed_before_refresh() -> ManagerError {
    ManagerError::Other("auth registration changed before refresh".into())
}

/// The error of a pass whose entry moved while it held the lease.
fn changed_during_refresh(id: &str) -> ManagerError {
    ManagerError::Other(format!("auth {id} registration changed during refresh"))
}

/// The io error of a lease task that failed outright.
fn lease_task_error(error: ManagerError) -> io::Error {
    match error {
        ManagerError::Store(error) => error,
        other => io::Error::other(other.to_string()),
    }
}

/// How many times the rotation commit is tried before it confirms the file.
const LEASE_COMMIT_ATTEMPTS: u32 = 6;
/// The wait before each retry of the rotation commit.
const LEASE_COMMIT_BACKOFF: [Duration; 5] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
    Duration::from_millis(3200),
];
/// No retry of the rotation commit starts after this long from the first
/// failure.
const LEASE_COMMIT_RETRY_WINDOW: Duration = Duration::from_secs(10);

/// Whether a retry may start: there is no deadline yet, or it has not passed.
fn may_start_retry(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_none_or(|deadline| now < deadline)
}

/// How the rotation commit ended.
enum Outcome {
    Committed(Box<CodexPersisted>),
    /// Every attempt failed. `confirmed` is the requested partitions a
    /// confirmation read found landed; `read` is whether that read worked, and
    /// `map` the file's content when it did.
    Exhausted {
        confirmed: CodexPartitions,
        read: bool,
        map: Option<Map<String, Value>>,
    },
    /// The blocking task failed; what is on disk is unknown.
    LeaseLost,
    /// Admission refused the commit by policy; nothing was written.
    PolicyRefused,
}

/// What publishing a rotation did.
enum Publication {
    Published {
        snapshot: Arc<Auth>,
        generation: u64,
        tail: Box<LeasedTail>,
    },
    /// The registration changed: the live record is untouched.
    Refused,
}

/// The decision of the send fence, made under one hold of the state lock.
enum SendFence {
    Send,
    /// The entry is gone or at another registration epoch.
    Changed,
    Defer(Deferral),
    /// A token command replaced a refresh token the pass would send; the live
    /// record is handed back.
    Superseded(Arc<Auth>),
}

/// What [`Manager::send_refresh`] did.
enum SendOutcome {
    Sent(Box<Result<Auth, ExecError>>),
    Deferred(Deferral),
    Superseded(Arc<Auth>),
}

/// The partitions of `auth` a refresh would send: those holding a refresh
/// token (SIWC also needs its client id).
fn refreshable_partitions(auth: &Auth) -> UndurablePartitions {
    UndurablePartitions {
        outer: !outer_refresh_token(auth).is_empty(),
        siwc: !auth.siwc_token_field("refresh_token").is_empty()
            && !auth.siwc_token_field("client_id").is_empty(),
    }
}

/// The outer partition's refresh token, trimmed; empty when absent.
fn outer_refresh_token(auth: &Auth) -> &str {
    auth.metadata_str("refresh_token")
        .map_or("", |token| token.trim())
}

/// Settles the availability a refused rotation leaves on `merged`, the merge
/// of the response with `current`. The refreshed credential is not published,
/// but a 401 the provider's refresh disproves is cleared and a quota hold that
/// `current` carries is kept.
fn settle_refused_availability(current: &Auth, merged: &mut Auth, now: Timestamp) {
    if is_disabled(current) || is_disabled(merged) {
        return;
    }
    let unauthorized = has_unauthorized_auth_failure(current)
        || merged
            .last_error
            .as_ref()
            .is_some_and(|error| is_unauthorized_error(ErrView::Auth(error)));
    if unauthorized {
        merged.unavailable = false;
        merged.last_error = None;
        merged.status_message.clear();
        merged.status = Status::Active;
    }
    clear_unauthorized_model_states(merged, now);
    if is_credential_quota_active(&current.quota, now) {
        merged.unavailable = current.unavailable;
        merged.next_retry_after = current.next_retry_after;
        merged.quota = current.quota.clone();
        if merged.status == Status::Active {
            merged.status = current.status;
        }
    }
}

/// When to look again: `max` from `now`, but no later than a future `expiry`,
/// and never sooner than `floor`. An expiry already past does not shorten the
/// wait, so a record with an expired token cannot loop at once.
fn bounded_retry_delay(
    now: Timestamp,
    expiry: Option<Timestamp>,
    max: Duration,
    floor: Duration,
) -> Timestamp {
    let mut delay = max;
    if let Some(expiry) = expiry
        && expiry > now
    {
        delay = delay.min((expiry - now).to_std().unwrap_or(Duration::ZERO));
    }
    add(now, delay.max(floor))
}

/// The maximum and the floor of a deferral's wait. `unproven` is the
/// unresolved state's count after this deferral, when the state is present.
fn deferral_backoff(kind: Deferral, unproven: Option<u32>) -> (Duration, Duration) {
    match kind {
        Deferral::LockBusy | Deferral::PendingWriteFailed | Deferral::PolicyChanged => {
            (REFRESH_INEFFECTIVE_BACKOFF, REFRESH_CHECK_INTERVAL)
        }
        Deferral::OriginUnresolved => match unproven {
            Some(n) if n >= ORIGIN_UNRESOLVED_SURFACE_AFTER => {
                // The cap is reached at n = 16; the shift is bounded so it cannot overflow.
                let shift = (n - ORIGIN_UNRESOLVED_SURFACE_AFTER).min(10);
                (
                    REFRESH_INEFFECTIVE_BACKOFF
                        .saturating_mul(1u32 << shift)
                        .min(ORIGIN_UNRESOLVED_BACKOFF_CAP),
                    REFRESH_CHECK_INTERVAL,
                )
            }
            _ => (REFRESH_INEFFECTIVE_BACKOFF, REFRESH_CHECK_INTERVAL),
        },
        Deferral::MissingFile | Deferral::NotCodexFile | Deferral::LeaseError => {
            (REFRESH_FAILURE_BACKOFF, REFRESH_PENDING_BACKOFF)
        }
        Deferral::LandingUnconfirmed => (ORIGIN_UNRESOLVED_BACKOFF_CAP, REFRESH_PENDING_BACKOFF),
        Deferral::ReadBackUnsettled => (READ_BACK_UNSETTLED_CAP, READ_BACK_UNSETTLED_FLOOR),
    }
}

/// Runs blocking lease work off the async thread, under the current gate
/// operation when there is one. A failed or panicked task is an error, never
/// a panic here. Outside a runtime the work runs inline.
async fn run_leased<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ManagerError> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(work());
    }
    let joined = match crate::gate::Operation::current() {
        Some(operation) => operation.run_blocking(work).await,
        None =>
        {
            #[allow(clippy::disallowed_methods)]
            tokio::task::spawn_blocking(work).await
        }
    };
    joined.map_err(|error| {
        ManagerError::Store(io::Error::other(format!("lease task failed: {error}")))
    })
}

/// One partition's token fields: the top level for outer, the nested `siwc`
/// object for SIWC.
fn partition_map(
    metadata: &Map<String, Value>,
    partition: CodexCredentialPartition,
) -> Map<String, Value> {
    match partition {
        CodexCredentialPartition::Outer => metadata.clone(),
        CodexCredentialPartition::Siwc => metadata
            .get("siwc")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    }
}

/// Whether `metadata` holds any token field of the partition.
fn has_partition(metadata: &Map<String, Value>, partition: CodexCredentialPartition) -> bool {
    match partition {
        CodexCredentialPartition::Outer => OUTER_KEYS.iter().any(|key| metadata.contains_key(*key)),
        CodexCredentialPartition::Siwc => metadata
            .get("siwc")
            .and_then(Value::as_object)
            .is_some_and(|nested| SIWC_KEYS.iter().any(|key| nested.contains_key(*key))),
    }
}

fn partition_name(partition: CodexCredentialPartition) -> &'static str {
    match partition {
        CodexCredentialPartition::Outer => "outer",
        CodexCredentialPartition::Siwc => "siwc",
    }
}

impl Manager {
    /// Runs `f` under the state lock. The closure calls exactly one `*_locked`
    /// function, or reads one entry, and returns an owned value. It never
    /// awaits, takes the lock again, does I/O or calls `publish_projections`
    /// or `queue_refresh_reschedule`.
    fn locked<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.lock();
        f(&mut state)
    }

    /// Runs `decide` under the state lock against a fresh proof that the live
    /// record's file is `origin`'s. The proof is blocking I/O, so it is made
    /// outside the lock; `decide` returns `Err(OriginMoved)` before it changes
    /// anything when the record moved since, and is then run again after a new
    /// proof, at most `ORIGIN_CHECK_ATTEMPTS` times in all.
    ///
    /// `lock_held` says whether the caller still holds the credential lock.
    /// A proof made without it can only be `Unproven`, never `Different`
    /// (`CodexOrigin::prove`), so a `Different` result is used as `Unproven`.
    async fn leased_origin_check<T>(
        &self,
        id: &str,
        origin: &CodexOrigin,
        lock_held: bool,
        mut decide: impl FnMut(&mut State, &OriginCheck) -> Result<T, OriginMoved>,
    ) -> T {
        let store = self.shared.store.as_deref();
        let mut record = self.locked(|s| s.auths.get(id).map(|e| e.auth.clone()));
        let mut attempt = 1;
        loop {
            let current = OriginCheck::resolve(store, record.as_deref());
            let proof = match &current {
                None => OriginProof::Unproven,
                Some(c) if c == origin => OriginProof::Same,
                Some(c) => {
                    let (pinned, other) = (origin.clone(), c.clone());
                    match run_leased(move || pinned.prove(&other)).await {
                        Ok(OriginProof::Different) if !lock_held => OriginProof::Unproven,
                        Ok(proof) => proof,
                        Err(_) => OriginProof::Unproven,
                    }
                }
            };
            let check = OriginCheck::proved(current, proof, attempt >= ORIGIN_CHECK_ATTEMPTS);
            #[cfg(test)]
            {
                let hook = lock(&self.shared.origin_check_hook).clone();
                if let Some(hook) = hook {
                    hook(attempt);
                }
            }
            match self.locked(|s| match decide(s, &check) {
                Ok(value) => Ok(value),
                Err(OriginMoved) => Err(s.auths.get(id).map(|e| e.auth.clone())),
            }) {
                Ok(value) => return value,
                Err(next) => {
                    record = next;
                    attempt += 1;
                }
            }
        }
    }

    /// The entry's state for a leased pass, or `None` when the entry is gone
    /// or no longer at `base_epoch`.
    fn prepare_leased_refresh_locked(
        &self,
        state: &State,
        id: &str,
        base_epoch: u64,
    ) -> Option<LeasePrep> {
        let entry = state.auths.get(id)?;
        if entry.auth.registration_epoch != base_epoch {
            return None;
        }
        let mut ops: Vec<CredentialRefreshPatch> = entry
            .auth
            .codex_pending_refresh
            .iter()
            .filter(|patch| !patch.is_written())
            .cloned()
            .collect();
        if let Some(intent) = entry.auth.codex_persist_intent.refresh_patch()
            && !intent.is_written()
            && !ops.iter().any(|patch| patch.same_operation(intent))
        {
            ops.push(intent.clone());
        }
        let mut read_back = Vec::new();
        for (partition, cause) in [
            (
                CodexCredentialPartition::Outer,
                &entry.undurable_causes.outer,
            ),
            (CodexCredentialPartition::Siwc, &entry.undurable_causes.siwc),
        ] {
            if let Some(MarkCause::ReadBack { origin }) = cause {
                read_back.push((partition, origin.clone()));
            }
        }
        Some(LeasePrep {
            base_epoch,
            discard_epoch: entry.discard_epoch,
            undurable: entry.undurable,
            undurable_baselines: entry.undurable_baselines.clone(),
            ineligible: self.permanently_ineligible(&entry.auth),
            live: (*entry.auth).clone(),
            ops,
            unresolved: entry.unresolved_origin.clone(),
            read_back,
            landing_unconfirmed: entry.landing_unconfirmed,
        })
    }

    /// Rules 1 and 2 of admission: the registration is unchanged, and the live
    /// record still names the leased file. `Admitted` here means both hold.
    fn admit_registration_and_origin(
        &self,
        state: &State,
        id: &str,
        base_epoch: u64,
        origin: &CodexOrigin,
        check: &OriginCheck,
    ) -> Result<LeaseAdmission, OriginMoved> {
        let Some(entry) = state
            .auths
            .get(id)
            .filter(|entry| entry.auth.registration_epoch == base_epoch)
        else {
            return Ok(LeaseAdmission::RegistrationChanged);
        };
        match check.verdict(self.shared.store.as_deref(), origin, &entry.auth)? {
            OriginProof::Same => Ok(LeaseAdmission::Admitted),
            OriginProof::Different => Ok(LeaseAdmission::OriginLost),
            OriginProof::Unproven => Ok(LeaseAdmission::OriginUnproven),
        }
    }

    /// The one admission predicate. `submitted` is the operations the stage
    /// would write; the discard rule is keyed on its unwritten ones.
    fn admit_leased_locked(
        &self,
        state: &State,
        id: &str,
        prep: &LeasePrep,
        origin: &CodexOrigin,
        check: &OriginCheck,
        submitted: &[CredentialRefreshPatch],
    ) -> Result<LeaseAdmission, OriginMoved> {
        let admission =
            self.admit_registration_and_origin(state, id, prep.base_epoch, origin, check)?;
        if admission != LeaseAdmission::Admitted {
            return Ok(admission);
        }
        let Some(entry) = state.auths.get(id) else {
            return Ok(LeaseAdmission::RegistrationChanged);
        };
        if (entry.discard_epoch != prep.discard_epoch
            && submitted.iter().any(|patch| !patch.is_written()))
            || self.permanently_ineligible(&entry.auth)
        {
            return Ok(LeaseAdmission::PolicyRefused);
        }
        Ok(LeaseAdmission::Admitted)
    }

    /// Records what the pending commit wrote: marks each fully landed
    /// operation written, and clears the marks of the landed partitions the
    /// disk verifies. Checks registration and origin only.
    fn apply_pending_outcomes_locked(
        &self,
        state: &mut State,
        id: &str,
        prep: &LeasePrep,
        origin: &CodexOrigin,
        check: &OriginCheck,
        persisted: &CodexPersisted,
    ) -> Result<PendingOutcome, OriginMoved> {
        let admission =
            self.admit_registration_and_origin(state, id, prep.base_epoch, origin, check)?;
        if admission != LeaseAdmission::Admitted {
            return Ok(PendingOutcome::Refused(admission));
        }
        let Some(entry) = state.auths.get_mut(id) else {
            return Ok(PendingOutcome::Refused(LeaseAdmission::RegistrationChanged));
        };
        let applied = persisted.applied;
        for op in &prep.ops {
            let requested = op.requested();
            if requested.any()
                && (!requested.outer || applied.outer == Some(true))
                && (!requested.siwc || applied.siwc == Some(true))
            {
                op.mark_written();
            }
        }
        let landed = UndurablePartitions {
            outer: applied.outer == Some(true),
            siwc: applied.siwc == Some(true),
        };
        clear_verified_partitions(
            &mut entry.undurable,
            &mut entry.undurable_baselines,
            &mut entry.undurable_causes,
            &mut entry.read_back_warned,
            landed,
            &entry.auth,
            &persisted.auth,
        );
        Ok(PendingOutcome::Applied(Protection {
            undurable: entry.undurable,
            undurable_baselines: entry.undurable_baselines.clone(),
        }))
    }

    /// Publishes the partitions adopted from `disk_effective` into the live
    /// record, after admission of the still-unwritten operations. The outer
    /// `Err` is a moved origin (nothing changed); the inner `Err` is a
    /// refusal. No watermark moves; the caller publishes projections and
    /// reschedules after the lock drops.
    #[allow(clippy::too_many_arguments)]
    fn publish_adoption_locked(
        &self,
        state: &mut State,
        id: &str,
        prep: &LeasePrep,
        origin: &CodexOrigin,
        check: &OriginCheck,
        disk_effective: &Map<String, Value>,
        adopted: crate::auth::codex_refresh::CodexPartitions,
    ) -> Result<Result<(Arc<Auth>, u64), LeaseAdmission>, OriginMoved> {
        let still: Vec<CredentialRefreshPatch> = prep
            .ops
            .iter()
            .filter(|patch| !patch.is_written())
            .cloned()
            .collect();
        let admission = self.admit_leased_locked(state, id, prep, origin, check, &still)?;
        if admission != LeaseAdmission::Admitted {
            return Ok(Err(admission));
        }
        let now = self.now();
        let no_pending_save = state.pending_saves.get(id).copied().unwrap_or(0) == 0;
        let Some(entry) = state.auths.get_mut(id) else {
            return Ok(Err(LeaseAdmission::RegistrationChanged));
        };
        let mut metadata = entry.auth.metadata.clone();
        if adopted.outer {
            for key in OUTER_KEYS {
                match disk_effective.get(*key) {
                    Some(value) => {
                        metadata.insert((*key).to_owned(), value.clone());
                    }
                    None => {
                        metadata.remove(*key);
                    }
                }
            }
        }
        if adopted.siwc {
            let disk_siwc = disk_effective.get("siwc").and_then(Value::as_object);
            let mut nested = metadata
                .get("siwc")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for key in SIWC_KEYS {
                match disk_siwc.and_then(|map| map.get(*key)) {
                    Some(value) => {
                        nested.insert((*key).to_owned(), value.clone());
                    }
                    None => {
                        nested.remove(*key);
                    }
                }
            }
            metadata.insert("siwc".to_owned(), Value::Object(nested));
        }
        // A change check (vi) deferred is owed, not in flight, and its flush
        // writes live's switch: the file's must not replace it first.
        if no_pending_save && entry.owed_save.is_none() {
            match disk_effective.get("use_siwc") {
                Some(value) => {
                    metadata.insert("use_siwc".to_owned(), value.clone());
                }
                None => {
                    metadata.remove("use_siwc");
                }
            }
        }
        let pending = pending_for_edit(&entry.auth, &metadata, false);
        let previous = entry.auth.clone();
        let auth = Arc::make_mut(&mut entry.auth);
        auth.metadata = metadata;
        auth.codex_pending_refresh = pending;
        auth.codex_persist_intent = Default::default();
        auth.preserve_rejected_tokens_from(&previous, false);
        if auth
            .codex_refresh_failure
            .as_ref()
            .is_some_and(|failure| match failure.partition {
                CodexCredentialPartition::Outer => adopted.outer,
                CodexCredentialPartition::Siwc => adopted.siwc,
            })
        {
            auth.codex_refresh_failure = None;
        }
        auth.credential_version = auth.credential_version.saturating_add(1);
        auth.generation = auth.generation.saturating_add(1);
        auth.updated_at = Some(now);
        let generation = auth.generation;
        let snapshot = entry.auth.clone();
        state.sync_scheduler(self.models(), id, now);
        Ok(Ok((snapshot, generation)))
    }

    /// Settles the entry's unresolved state against the proof just made under
    /// the lease. A proof is about `token_origin` and decides only while the
    /// live record still names it.
    fn resolve_unresolved_locked(
        &self,
        state: &mut State,
        id: &str,
        base_epoch: u64,
        token_origin: &CodexOrigin,
        proof: OriginProof,
    ) -> Resolution {
        let Some(entry) = state.auths.get_mut(id) else {
            return Resolution::RegistrationChanged;
        };
        if entry.auth.registration_epoch != base_epoch {
            return Resolution::RegistrationChanged;
        }
        if entry.unresolved_origin.is_none() {
            // A token command cleared it.
            return Resolution::Same;
        }
        if OriginCheck::resolve(self.shared.store.as_deref(), Some(entry.auth.as_ref())).as_ref()
            != Some(token_origin)
        {
            return Resolution::Unproven;
        }
        match proof {
            OriginProof::Same => {
                entry.clear_unresolved();
                Resolution::Same
            }
            OriginProof::Different => {
                let Some(held) = entry.unresolved_origin.clone() else {
                    return Resolution::Same;
                };
                entry.clear_unresolved();
                let except = UndurablePartitions {
                    outer: !held.partitions.outer,
                    siwc: !held.partitions.siwc,
                };
                mark_undurable_except(
                    &held.rotation,
                    &mut entry.undurable,
                    &mut entry.undurable_baselines,
                    &mut entry.undurable_causes,
                    &MarkCause::Different,
                    except,
                );
                Resolution::Different(Protection {
                    undurable: entry.undurable,
                    undurable_baselines: entry.undurable_baselines.clone(),
                })
            }
            OriginProof::Unproven => Resolution::Unproven,
        }
    }

    /// After a pass that ends with the unresolved state present: holds the
    /// next refresh back as an `OriginUnresolved` deferral does, without its
    /// count, and wakes the loop. Records nothing when the state is absent.
    fn schedule_unresolved(&self, id: &str, base_epoch: u64) {
        let now = self.now();
        {
            let mut guard = self.lock();
            let state = &mut *guard;
            let present = state.auths.get(id).is_some_and(|entry| {
                entry.auth.registration_epoch == base_epoch && entry.unresolved_origin.is_some()
            });
            if !present {
                return;
            }
            let (max, floor) = deferral_backoff(Deferral::OriginUnresolved, None);
            self.hold_next_refresh_locked(state, id, now, max, floor);
        }
        self.queue_refresh_reschedule(id);
        tracing::warn!(auth_id = %id, "origin unresolved: a received rotation is held until the credential file is confirmed");
    }

    /// Steps 2 and 3 of a deferral: the next refresh waits `max`, bounded by
    /// the token's expiry and `floor`; the record's generation moves and the
    /// scheduler is told. Changes no status, availability or error field.
    fn hold_next_refresh_locked(
        &self,
        state: &mut State,
        id: &str,
        now: Timestamp,
        max: Duration,
        floor: Duration,
    ) {
        let Some(entry) = state.auths.get_mut(id) else {
            return;
        };
        let auth = Arc::make_mut(&mut entry.auth);
        let expiry = non_zero(auth.access_token_expiration_time());
        auth.next_refresh_after = Some(bounded_retry_delay(now, expiry, max, floor));
        auth.generation = auth.generation.saturating_add(1);
        auth.updated_at = Some(now);
        state.sync_scheduler(self.models(), id, now);
    }

    /// Step 4c: settles each read-back mark against the leased file's
    /// contents `d`, given one proof per marked partition.
    fn settle_read_back_locked(
        &self,
        state: &mut State,
        id: &str,
        base_epoch: u64,
        token_origin: &CodexOrigin,
        proofs: &[(CodexCredentialPartition, OriginProof)],
        d: &Map<String, Value>,
    ) -> ReadBackSettlement {
        let Some(entry) = state.auths.get_mut(id) else {
            return ReadBackSettlement::RegistrationChanged;
        };
        if entry.auth.registration_epoch != base_epoch {
            return ReadBackSettlement::RegistrationChanged;
        }
        // The contents are the leased file's; they say nothing about a record
        // that now resolves elsewhere.
        if OriginCheck::resolve(self.shared.store.as_deref(), Some(entry.auth.as_ref())).as_ref()
            != Some(token_origin)
        {
            return ReadBackSettlement::Unproven;
        }
        let mut any_landing = false;
        let mut any_unproven = false;
        for partition in [
            CodexCredentialPartition::Outer,
            CodexCredentialPartition::Siwc,
        ] {
            let Some(proof) = proofs
                .iter()
                .find(|(p, _)| *p == partition)
                .map(|(_, proof)| *proof)
            else {
                continue;
            };
            let rotated = disk_rotated_partitions(&entry.auth.metadata, d);
            let rotated = match partition {
                CodexCredentialPartition::Outer => rotated.outer,
                CodexCredentialPartition::Siwc => rotated.siwc,
            };
            let only = UndurablePartitions {
                outer: partition == CodexCredentialPartition::Outer,
                siwc: partition == CodexCredentialPartition::Siwc,
            };
            let verified = verified_partitions(only, &entry.auth.metadata, d);
            let verified = match partition {
                CodexCredentialPartition::Outer => verified.outer,
                CodexCredentialPartition::Siwc => verified.siwc,
            };
            let (marked, baselines, cause, warned, landing) = match partition {
                CodexCredentialPartition::Outer => (
                    &mut entry.undurable.outer,
                    &mut entry.undurable_baselines.outer,
                    &mut entry.undurable_causes.outer,
                    &mut entry.read_back_warned.outer,
                    &mut entry.landing_unconfirmed.outer,
                ),
                CodexCredentialPartition::Siwc => (
                    &mut entry.undurable.siwc,
                    &mut entry.undurable_baselines.siwc,
                    &mut entry.undurable_causes.siwc,
                    &mut entry.read_back_warned.siwc,
                    &mut entry.landing_unconfirmed.siwc,
                ),
            };
            if !*marked || !matches!(cause, Some(MarkCause::ReadBack { .. })) {
                continue;
            }
            let hold = match proof {
                OriginProof::Same => {
                    let disk = partition_map(d, partition);
                    if has_partition(d, partition) && !rotated {
                        // The write landed when every token field matches.
                        // With equal identities but another field (the expiry,
                        // the scopes), nothing changes: the mark and its cause
                        // stay, so the send fence refuses with
                        // `ReadBackUnsettled` and sends nothing.
                        if verified {
                            *marked = false;
                            baselines.clear();
                            *cause = None;
                            *warned = false;
                        }
                        false
                    } else if has_partition(d, partition)
                        && baselines.iter().any(|known| same_version(&disk, known))
                    {
                        // The write did not land: the file holds another value.
                        *cause = Some(MarkCause::Different);
                        *warned = false;
                        false
                    } else {
                        true
                    }
                }
                OriginProof::Different => true,
                OriginProof::Unproven => {
                    any_unproven = true;
                    false
                }
            };
            if hold {
                if !*landing {
                    *landing = true;
                    tracing::warn!(
                        auth_id = %id,
                        partition = partition_name(partition),
                        "landing unconfirmed: refresh paused until the user signs in again"
                    );
                }
                any_landing = true;
            }
        }
        if any_landing {
            ReadBackSettlement::Hold
        } else if any_unproven {
            ReadBackSettlement::Unproven
        } else {
            ReadBackSettlement::Settled(Protection {
                undurable: entry.undurable,
                undurable_baselines: entry.undurable_baselines.clone(),
            })
        }
    }

    /// Records that a pass sent nothing and waits, and returns the error to
    /// hand back. Changes no status, availability, error or failure-count
    /// field, so routing is unaffected. Logs ids, partition names and the io
    /// kind only.
    fn defer_refresh(
        &self,
        id: &str,
        base_epoch: u64,
        kind: Deferral,
        error: io::Error,
    ) -> ManagerError {
        let now = self.now();
        let mut recorded = false;
        let mut unproven = None;
        let mut first_missing = false;
        let mut newly_warned: Vec<&'static str> = Vec::new();
        {
            let mut guard = self.lock();
            let state = &mut *guard;
            if let Some(entry) = state.auths.get_mut(id)
                && entry.auth.registration_epoch == base_epoch
            {
                recorded = true;
                if kind == Deferral::OriginUnresolved
                    && let Some(origin) = entry.unresolved_origin.as_mut()
                {
                    origin.unproven = origin.unproven.saturating_add(1);
                    unproven = Some(origin.unproven);
                }
                if kind == Deferral::ReadBackUnsettled {
                    let held = entry.read_back_partitions();
                    for (partition, is_held, warned) in [
                        (
                            CodexCredentialPartition::Outer,
                            held.outer,
                            &mut entry.read_back_warned.outer,
                        ),
                        (
                            CodexCredentialPartition::Siwc,
                            held.siwc,
                            &mut entry.read_back_warned.siwc,
                        ),
                    ] {
                        if is_held && !*warned {
                            *warned = true;
                            newly_warned.push(partition_name(partition));
                        }
                    }
                }
                if kind == Deferral::MissingFile {
                    let epoch = entry.auth.registration_epoch;
                    first_missing = state.missing_file_deferrals.get(id) != Some(&epoch);
                    state.missing_file_deferrals.insert(id.to_owned(), epoch);
                }
            }
            if recorded {
                let (max, floor) = deferral_backoff(kind, unproven);
                self.hold_next_refresh_locked(state, id, now, max, floor);
            }
        }
        self.queue_refresh_reschedule(id);
        if recorded {
            let io_kind = error.kind();
            match kind {
                Deferral::LockBusy => {
                    tracing::debug!(auth_id = %id, ?io_kind, "credential lock busy; refresh deferred");
                }
                Deferral::NotCodexFile
                | Deferral::LeaseError
                | Deferral::PendingWriteFailed
                | Deferral::PolicyChanged => {
                    tracing::warn!(auth_id = %id, ?kind, ?io_kind, "refresh deferred");
                }
                Deferral::OriginUnresolved => {
                    let held = unproven.is_some();
                    match unproven {
                        Some(n) if n > ORIGIN_UNRESOLVED_SURFACE_AFTER => {
                            tracing::debug!(auth_id = %id, rotation_held = held, "origin unresolved");
                        }
                        Some(n) if n == ORIGIN_UNRESOLVED_SURFACE_AFTER => {
                            tracing::warn!(
                                auth_id = %id,
                                rotation_held = held,
                                "origin unresolved: refresh paused until the credential file is confirmed or the user signs in again"
                            );
                        }
                        _ => {
                            tracing::warn!(auth_id = %id, rotation_held = held, "origin unresolved");
                        }
                    }
                }
                Deferral::LandingUnconfirmed => {
                    tracing::debug!(auth_id = %id, "landing unconfirmed; refresh deferred");
                }
                Deferral::ReadBackUnsettled => {
                    if newly_warned.is_empty() {
                        tracing::debug!(auth_id = %id, "refresh paused until the credential file's last write is confirmed");
                    }
                    for partition in newly_warned {
                        tracing::warn!(
                            auth_id = %id,
                            partition,
                            "refresh paused until the credential file's last write is confirmed"
                        );
                    }
                }
                Deferral::MissingFile => {
                    if first_missing {
                        tracing::warn!(auth_id = %id, ?io_kind, "credential file is missing; refresh deferred");
                    } else {
                        tracing::debug!(auth_id = %id, ?io_kind, "credential file is missing; refresh deferred");
                    }
                }
            }
        }
        ManagerError::Store(error)
    }

    /// The one place a refresh is sent. Right before the call, one hold of the
    /// state lock decides whether the pass may send (see
    /// [`fence_send_locked`](Self::fence_send_locked)). The result is handed
    /// back as it is; the caller redacts it.
    async fn send_refresh(
        &self,
        id: &str,
        base_epoch: u64,
        executor: &Arc<dyn ProviderExecutor>,
        auth: &Arc<Auth>,
        options: crate::executor::RefreshOptions,
    ) -> Result<SendOutcome, ManagerError> {
        // A waiter that got the lock after the switch began declines before
        // it calls the provider.
        let state = self.gate().state();
        if state != GateState::Open {
            return Err(ManagerError::NotActive(NotActive { state }));
        }
        #[cfg(test)]
        {
            let pause = lock(&self.shared.send_fence_pause).clone();
            if let Some(pause) = pause {
                pause.0.notify_one();
                pause.1.notified().await;
            }
        }
        match self.locked(|s| self.fence_send_locked(s, id, base_epoch, auth)) {
            SendFence::Send => {}
            SendFence::Changed => {
                return Err(ManagerError::Other(
                    "auth registration changed before refresh".into(),
                ));
            }
            SendFence::Defer(kind) => return Ok(SendOutcome::Deferred(kind)),
            SendFence::Superseded(live) => return Ok(SendOutcome::Superseded(live)),
        }
        Ok(SendOutcome::Sent(Box::new(
            executor
                .refresh_with_options(Arc::clone(auth), options)
                .await,
        )))
    }

    /// Whether the pass whose snapshot is `auth` may send now. A refresh token
    /// is spent once sent, so nothing is sent that the live entry no longer
    /// holds, or that a mark says may already have been spent. Decided on the
    /// partitions the pass would send.
    fn fence_send_locked(
        &self,
        state: &State,
        id: &str,
        base_epoch: u64,
        auth: &Auth,
    ) -> SendFence {
        if !is_codex_auth(auth) {
            return SendFence::Send;
        }
        let Some(entry) = state
            .auths
            .get(id)
            .filter(|entry| entry.auth.registration_epoch == base_epoch)
        else {
            return SendFence::Changed;
        };
        let to_send = refreshable_partitions(auth);
        let hit = |held: UndurablePartitions| {
            (held.outer && to_send.outer) || (held.siwc && to_send.siwc)
        };
        if hit(entry.landing_unconfirmed) {
            return SendFence::Defer(Deferral::LandingUnconfirmed);
        }
        if hit(entry.read_back_partitions()) {
            return SendFence::Defer(Deferral::ReadBackUnsettled);
        }
        if hit(entry.unresolved_partitions()) {
            return SendFence::Defer(Deferral::OriginUnresolved);
        }
        let live = &entry.auth;
        if (to_send.outer && outer_refresh_token(auth) != outer_refresh_token(live))
            || (to_send.siwc
                && auth.siwc_token_field("refresh_token") != live.siwc_token_field("refresh_token"))
        {
            return SendFence::Superseded(Arc::clone(live));
        }
        SendFence::Send
    }

    /// Commits the pass's final rotation to the leased file. Each attempt is
    /// admitted again first, since the live record can move between them. A
    /// failed attempt is retried after a backoff while the retry window is
    /// open; when the attempts or the window run out, one read of the file
    /// says which partitions landed. The token comes back with the outcome
    /// unless the blocking task was lost, which leaves `None`.
    async fn commit_rotation_with_retry(
        &self,
        id: &str,
        prep: &LeasePrep,
        token: CodexLeaseToken,
        final_patch: &CredentialRefreshPatch,
    ) -> (Option<CodexLeaseToken>, Outcome) {
        let Some(store) = self.shared.store.clone() else {
            return (None, Outcome::LeaseLost);
        };
        #[cfg(test)]
        let window = lock(&self.shared.lease_retry_window).unwrap_or(LEASE_COMMIT_RETRY_WINDOW);
        #[cfg(not(test))]
        let window = LEASE_COMMIT_RETRY_WINDOW;
        let mut auth_no_pending = prep.live.clone();
        auth_no_pending.codex_pending_refresh.clear();
        auth_no_pending.codex_persist_intent = Default::default();

        let mut token = token;
        let mut deadline: Option<Instant> = None;
        let mut last: Option<io::Error> = None;
        let mut attempt = 0usize;
        loop {
            if let Some(error) = last.take()
                && !may_start_retry(deadline, Instant::now())
            {
                return confirm_commit(id, store, token, final_patch, error).await;
            }
            let admission = self
                .leased_origin_check(id, token.origin(), true, |s, check| {
                    self.admit_leased_locked(
                        s,
                        id,
                        prep,
                        token.origin(),
                        check,
                        std::slice::from_ref(final_patch),
                    )
                })
                .await;
            match admission {
                LeaseAdmission::PolicyRefused => return (Some(token), Outcome::PolicyRefused),
                // A moved or lost record does not stop the commit: the token
                // is a lease on the file the rotation was read from, and the
                // rotation is spent either way.
                LeaseAdmission::Admitted
                | LeaseAdmission::RegistrationChanged
                | LeaseAdmission::OriginLost
                | LeaseAdmission::OriginUnproven => {}
            }
            let (commit_store, commit_auth, commit_patch) = (
                Arc::clone(&store),
                auth_no_pending.clone(),
                final_patch.clone(),
            );
            let committed = run_leased(move || {
                let result = commit_store.codex_lease_commit(&token, &commit_auth, &commit_patch);
                (token, result)
            })
            .await;
            let (returned, result) = match committed {
                Ok(done) => done,
                Err(_) => return (None, Outcome::LeaseLost),
            };
            token = returned;
            let error = match result {
                Ok(persisted) => return (Some(token), Outcome::Committed(Box::new(persisted))),
                Err(error) => error,
            };
            let now = Instant::now();
            let until = *deadline.get_or_insert_with(|| now + window);
            attempt += 1;
            if attempt >= LEASE_COMMIT_ATTEMPTS as usize || !may_start_retry(Some(until), now) {
                return confirm_commit(id, store, token, final_patch, error).await;
            }
            let wait = LEASE_COMMIT_BACKOFF[attempt - 1];
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::time::sleep(wait).await;
            } else {
                std::thread::sleep(wait);
            }
            last = Some(error);
        }
    }

    /// Publishes the pass's rotation into the live record, after admission of
    /// the final operation. `Err(OriginMoved)` changes nothing. What the
    /// commit `outcome` says landed decides the marks, the pending operation
    /// and whether the rotation counts as written. The caller publishes
    /// projections and reschedules after the lock drops.
    #[allow(clippy::too_many_arguments)]
    fn publish_rotation_locked(
        &self,
        state: &mut State,
        id: &str,
        prep: &LeasePrep,
        origin: &CodexOrigin,
        check: &OriginCheck,
        base: &Auth,
        updated: &mut Auth,
        outcome: &Outcome,
        now: Timestamp,
    ) -> Result<Publication, OriginMoved> {
        // A Codex pass always carries its rotation as the intent.
        let Some(final_patch) = updated.codex_persist_intent.refresh_patch().cloned() else {
            return Ok(Publication::Refused);
        };
        let admission = self.admit_leased_locked(
            state,
            id,
            prep,
            origin,
            check,
            std::slice::from_ref(&final_patch),
        )?;
        // The commit's refusal is carried through the pass, as a refusal after
        // release is: an edit that lifts the policy since then must not let the
        // ordinary save write a rotation the commit refused.
        let admission =
            if matches!(outcome, Outcome::PolicyRefused) && admission == LeaseAdmission::Admitted {
                LeaseAdmission::PolicyRefused
            } else {
                admission
            };
        let landed = match outcome {
            Outcome::Committed(persisted) => CodexPartitions {
                outer: persisted.applied.outer == Some(true),
                siwc: persisted.applied.siwc == Some(true),
            },
            Outcome::Exhausted { confirmed, .. } => *confirmed,
            Outcome::LeaseLost | Outcome::PolicyRefused => CodexPartitions::default(),
        };
        if admission == LeaseAdmission::RegistrationChanged {
            if landed.any() {
                tracing::debug!(
                    auth_id = %id,
                    "refreshed Codex credential landed in the file; registration changed during refresh"
                );
            } else {
                tracing::error!(
                    auth_id = %id,
                    "refreshed Codex credential lost: registration changed during refresh and the rotation was not committed"
                );
            }
            return Ok(Publication::Refused);
        }
        let Some(entry) = state.auths.get_mut(id) else {
            return Ok(Publication::Refused);
        };
        let before = Arc::clone(&entry.auth);
        let requested = final_patch.requested();
        let all_landed = (!requested.outer || landed.outer) && (!requested.siwc || landed.siwc);
        let landed_marks = UndurablePartitions {
            outer: landed.outer,
            siwc: landed.siwc,
        };
        let cause = match outcome {
            Outcome::Exhausted { read: false, .. } | Outcome::LeaseLost => MarkCause::ReadBack {
                origin: origin.clone(),
            },
            _ => MarkCause::Different,
        };
        let disk_from_map;
        let disk: Option<&Auth> = match outcome {
            Outcome::Committed(persisted) => Some(&persisted.auth),
            Outcome::Exhausted { map: Some(map), .. } => {
                disk_from_map = Auth {
                    metadata: map.clone(),
                    ..Default::default()
                };
                Some(&disk_from_map)
            }
            _ => None,
        };

        let mut disposition = LeasedDisposition::Persist;
        let mut refused = false;
        match admission {
            LeaseAdmission::RegistrationChanged => return Ok(Publication::Refused),
            LeaseAdmission::OriginLost => {
                let live = Arc::make_mut(&mut entry.auth);
                live.metadata = final_patch.apply(&live.metadata).0;
                mark_undurable(
                    &final_patch,
                    &mut entry.undurable,
                    &mut entry.undurable_baselines,
                    &mut entry.undurable_causes,
                    &MarkCause::Different,
                );
            }
            LeaseAdmission::PolicyRefused => {
                refused = true;
                disposition = LeasedDisposition::PolicyRefused;
                let mut merged = merge_refreshed_auth_with(
                    base,
                    &entry.auth,
                    updated,
                    now,
                    ResponseCredentials::Refused,
                );
                normalize_credential_metadata(&mut merged.metadata);
                settle_refused_availability(&entry.auth, &mut merged, now);
                *Arc::make_mut(&mut entry.auth) = merged;
                let live = Arc::make_mut(&mut entry.auth);
                live.metadata = final_patch.apply(&live.metadata).0;
                live.codex_persist_intent = Default::default();
                mark_undurable(
                    &final_patch,
                    &mut entry.undurable,
                    &mut entry.undurable_baselines,
                    &mut entry.undurable_causes,
                    &MarkCause::Different,
                );
                updated.codex_persist_intent = Default::default();
                updated.codex_pending_refresh.clear();
            }
            LeaseAdmission::Admitted | LeaseAdmission::OriginUnproven => {
                let live = Arc::make_mut(&mut entry.auth);
                live.metadata = final_patch.apply(&live.metadata).0;
                if !all_landed {
                    mark_undurable(
                        &final_patch,
                        &mut entry.undurable,
                        &mut entry.undurable_baselines,
                        &mut entry.undurable_causes,
                        &cause,
                    );
                }
                if landed.any()
                    && let Some(disk) = disk
                {
                    clear_verified_partitions(
                        &mut entry.undurable,
                        &mut entry.undurable_baselines,
                        &mut entry.undurable_causes,
                        &mut entry.read_back_warned,
                        landed_marks,
                        &entry.auth,
                        disk,
                    );
                }
                if admission == LeaseAdmission::OriginUnproven {
                    entry.record_unresolved(origin, &final_patch);
                    self.drop_unresolved_if_ineligible(entry);
                    disposition = LeasedDisposition::OriginUnresolved;
                }
            }
        }

        if !refused {
            let reported = match outcome {
                Outcome::Committed(_) => true,
                Outcome::Exhausted { .. } => all_landed,
                Outcome::LeaseLost | Outcome::PolicyRefused => false,
            };
            let live = Arc::make_mut(&mut entry.auth);
            if reported {
                final_patch.mark_written();
            } else if !live
                .codex_pending_refresh
                .iter()
                .any(|patch| patch.same_operation(&final_patch))
            {
                live.codex_pending_refresh.push(final_patch.clone());
            }
            live.codex_persist_intent =
                crate::auth::codex_refresh::CodexPersistIntent::Refresh(final_patch.clone());
        }

        let live = Arc::make_mut(&mut entry.auth);
        if credentials_changed(&before, live) {
            live.credential_version = live.credential_version.saturating_add(1);
        }
        live.generation = live.generation.saturating_add(1);
        let refreshed_active = !refused
            && !updated
                .codex_refresh_failure
                .as_ref()
                .is_some_and(|failure| failure.partition.is_active(live));
        live.preserve_rejected_tokens_from(&before, refreshed_active);
        live.updated_at = Some(now);
        let generation = live.generation;
        let snapshot = Arc::clone(&entry.auth);
        state.sync_scheduler(self.models(), id, now);
        Ok(Publication::Published {
            snapshot,
            generation,
            tail: Box::new(LeasedTail {
                origin: origin.clone(),
                rotation: final_patch,
                disposition,
            }),
        })
    }
}

/// Reads the leased file once after the commit gave up, to learn which
/// requested partitions landed. A failed read leaves them unknown.
async fn confirm_commit(
    id: &str,
    store: Arc<dyn crate::auth::AuthStore>,
    token: CodexLeaseToken,
    final_patch: &CredentialRefreshPatch,
    err: io::Error,
) -> (Option<CodexLeaseToken>, Outcome) {
    let read = run_leased(move || {
        let result = store.codex_lease_read(&token);
        (token, result)
    })
    .await;
    tracing::warn!(
        auth_id = %id,
        kind = ?err.kind(),
        "codex lease commit failed after every attempt"
    );
    match read {
        Err(_) => (None, Outcome::LeaseLost),
        Ok((token, Ok(Some(map)))) => {
            let confirmed = final_patch.landed_in(&map);
            (
                Some(token),
                Outcome::Exhausted {
                    confirmed,
                    read: true,
                    map: Some(map),
                },
            )
        }
        Ok((token, _)) => (
            Some(token),
            Outcome::Exhausted {
                confirmed: CodexPartitions::default(),
                read: false,
                map: None,
            },
        ),
    }
}

/// Test seam: awaited after a Codex refresh was saved and published, before
/// its partial failure is checked against the live credential. Keyed by the
/// manager's shared state, so parallel tests don't see each other's hooks.
#[cfg(test)]
pub(super) mod partial_failure_seam {
    use super::Shared;
    use futures_core::future::BoxFuture;
    use std::sync::{Arc, Mutex};

    pub(crate) type Hook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
    static HOOKS: Mutex<Vec<(usize, Hook)>> = Mutex::new(Vec::new());

    fn key(shared: &Arc<Shared>) -> usize {
        Arc::as_ptr(shared) as usize
    }

    /// Installs `hook` until the returned guard is dropped.
    pub(crate) fn install(shared: &Arc<Shared>, hook: Hook) -> Guard {
        let key = key(shared);
        super::lock(&HOOKS).push((key, hook));
        Guard(key)
    }

    pub(crate) struct Guard(usize);

    impl Drop for Guard {
        fn drop(&mut self) {
            super::lock(&HOOKS).retain(|(key, _)| *key != self.0);
        }
    }

    pub(super) async fn fire(shared: &Arc<Shared>) {
        let key = key(shared);
        let hook = super::lock(&HOOKS)
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, hook)| hook.clone());
        if let Some(hook) = hook {
            hook().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use chrono::{TimeDelta, Utc};

    use serde_json::json;

    use super::super::lifecycle::{OriginCheck, OriginMoved};
    use super::super::merge::{ResponseCredentials, merge_refreshed_auth_with};
    use super::super::{Entry, Manager, MarkCause, Settings, State, UndurablePartitions};
    use super::{
        Deferral, LeasePrep, ReadBackSettlement, SendFence, add, bounded_retry_delay,
        deferral_backoff, has_unauthorized_auth_failure, may_start_retry,
        settle_refused_availability,
    };
    use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
    use crate::auth::{
        Auth, AuthError, AuthStore, CodexCredentialPartition, CodexLeaseToken, CodexOrigin,
        FileStore, ModelState, OriginProof, Status, Timestamp,
    };
    use crate::executor::ProviderExecutor;

    fn require_send<T: Send>(_: T) {}

    /// B-8: the deferral wait is `max` from now, shortened by a future token
    /// expiry, and never shorter than the floor.
    #[test]
    fn bounded_retry_delay_follows_expiry_between_floor_and_max() {
        let now = Utc::now();
        let secs = Duration::from_secs;
        let rows = [
            (Some(now - TimeDelta::seconds(1)), 300, 5, 300),
            (Some(now + TimeDelta::seconds(2)), 300, 5, 5),
            (Some(now + TimeDelta::seconds(60)), 300, 5, 60),
            (None, 300, 5, 300),
            (Some(now + TimeDelta::seconds(60)), 30, 5, 30),
        ];
        for (expiry, max, floor, expected) in rows {
            assert_eq!(
                bounded_retry_delay(now, expiry, secs(max), secs(floor)),
                add(now, secs(expected)),
                "expiry {expiry:?} max {max} floor {floor}"
            );
        }
    }

    /// B-22: a retry may start with no deadline, or before it; at the
    /// deadline or after it, it may not.
    #[test]
    fn a_retry_may_start_only_before_the_deadline() {
        // Headroom so the subtraction below cannot underflow.
        let now = Instant::now() + Duration::from_secs(1);
        let ms = Duration::from_millis(1);
        let rows = [
            (None, true),
            (Some(now + ms), true),
            (Some(now), false),
            (Some(now - ms), false),
        ];
        for (index, (deadline, expected)) in rows.into_iter().enumerate() {
            assert_eq!(may_start_retry(deadline, now), expected, "row {index}");
        }
    }

    #[test]
    fn unresolved_backoff_grows_after_the_surface_count_and_is_capped() {
        let wait = |n| deferral_backoff(Deferral::OriginUnresolved, Some(n)).0;
        assert_eq!(wait(1), Duration::from_secs(30));
        assert_eq!(wait(9), Duration::from_secs(30));
        assert_eq!(wait(10), Duration::from_secs(30));
        assert_eq!(wait(11), Duration::from_secs(60));
        assert_eq!(wait(16), Duration::from_secs(1800));
        assert_eq!(wait(u32::MAX), Duration::from_secs(1800));
    }

    /// The future of `leased_origin_check` is `Send`. It is built and
    /// never polled, so no lease is needed.
    #[test]
    fn leased_origin_check_future_is_send() {
        let manager = Manager::new(
            Settings::default(),
            Arc::new(crate::registry::ModelRegistry::new()),
            None,
        );
        let exe = std::env::current_exe().expect("current exe");
        let origin = CodexOrigin::of(&exe).expect("origin of the test binary");
        let future = manager.leased_origin_check(
            "id",
            &origin,
            true,
            |_: &mut State, _: &OriginCheck| -> Result<(), OriginMoved> { Ok(()) },
        );
        require_send(future);
    }

    /// A registered Codex credential over a real file store and the origin of
    /// its file, with the ids the read-back settlement tests use.
    fn read_back_fixture() -> (tempfile::TempDir, Manager, CodexOrigin) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("codex.json"), b"{}").expect("credential file");
        let store = Arc::new(FileStore::new(dir.path()));
        let manager = Manager::new(
            Settings::default(),
            Arc::new(crate::registry::ModelRegistry::new()),
            Some(store.clone()),
        );
        let mut auth = codex(json!({
            "type": "codex",
            "access_token": "t-live",
            "refresh_token": "r-live",
            "id_token": "i-live",
            "account_id": "acct",
            "expired": "2099-01-01T00:00:00Z",
            "siwc": {
                "access_token": "st-live",
                "refresh_token": "sr-live",
                "id_token": "si-live",
                "client_id": "client",
                "device_id": "device",
                "expired": "2099-01-01T00:00:00Z",
                "scope": "openid",
                "scopes": ["openid"],
            },
        }));
        auth.file_name = "codex.json".into();
        let registered = manager.register_unsaved(auth).expect("register");
        let origin = store
            .codex_origin(&registered)
            .expect("origin")
            .expect("a file store names an origin");
        (dir, manager, origin)
    }

    /// Marks `partition` of the fixture with a `ReadBack` cause on `origin`,
    /// one baseline, and the warned flag.
    fn mark_read_back(
        manager: &Manager,
        partition: CodexCredentialPartition,
        origin: &CodexOrigin,
    ) {
        let mut guard = manager.lock();
        let entry = guard.auths.get_mut("codex-1").expect("entry");
        let baseline = serde_json::Map::from_iter([("refresh_token".to_owned(), json!("r-old"))]);
        let (marked, baselines, cause, warned) = match partition {
            CodexCredentialPartition::Outer => (
                &mut entry.undurable.outer,
                &mut entry.undurable_baselines.outer,
                &mut entry.undurable_causes.outer,
                &mut entry.read_back_warned.outer,
            ),
            CodexCredentialPartition::Siwc => (
                &mut entry.undurable.siwc,
                &mut entry.undurable_baselines.siwc,
                &mut entry.undurable_causes.siwc,
                &mut entry.read_back_warned.siwc,
            ),
        };
        *marked = true;
        baselines.push(baseline);
        *cause = Some(MarkCause::ReadBack {
            origin: origin.clone(),
        });
        *warned = true;
    }

    /// The partition's mark, baseline count, whether the cause is `ReadBack`,
    /// the warned flag and the landing hold.
    fn read_back_marks(
        manager: &Manager,
        partition: CodexCredentialPartition,
    ) -> (bool, usize, bool, bool, bool) {
        let guard = manager.lock();
        let entry = guard.auths.get("codex-1").expect("entry");
        match partition {
            CodexCredentialPartition::Outer => (
                entry.undurable.outer,
                entry.undurable_baselines.outer.len(),
                matches!(
                    entry.undurable_causes.outer,
                    Some(MarkCause::ReadBack { .. })
                ),
                entry.read_back_warned.outer,
                entry.landing_unconfirmed.outer,
            ),
            CodexCredentialPartition::Siwc => (
                entry.undurable.siwc,
                entry.undurable_baselines.siwc.len(),
                matches!(
                    entry.undurable_causes.siwc,
                    Some(MarkCause::ReadBack { .. })
                ),
                entry.read_back_warned.siwc,
                entry.landing_unconfirmed.siwc,
            ),
        }
    }

    /// Settles the fixture's `partition` against `d` with a `Same` proof.
    fn settle_same(
        manager: &Manager,
        partition: CodexCredentialPartition,
        origin: &CodexOrigin,
        d: &serde_json::Map<String, serde_json::Value>,
    ) -> ReadBackSettlement {
        let mut guard = manager.lock();
        let state = &mut *guard;
        let epoch = state.auths["codex-1"].auth.registration_epoch;
        manager.settle_read_back_locked(
            state,
            "codex-1",
            epoch,
            origin,
            &[(partition, OriginProof::Same)],
            d,
        )
    }

    /// The live metadata with the partition's `expired` changed, which leaves
    /// every identity key as it was.
    fn disk_with_other_expiry(
        manager: &Manager,
        partition: CodexCredentialPartition,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut d = manager.lock().auths["codex-1"].auth.metadata.clone();
        match partition {
            CodexCredentialPartition::Outer => {
                d.insert("expired".into(), json!("2099-02-02T00:00:00Z"));
            }
            CodexCredentialPartition::Siwc => {
                d.get_mut("siwc")
                    .and_then(serde_json::Value::as_object_mut)
                    .expect("siwc")
                    .insert("expired".into(), json!("2099-02-02T00:00:00Z"));
            }
        }
        d
    }

    const BOTH_PARTITIONS: [CodexCredentialPartition; 2] = [
        CodexCredentialPartition::Outer,
        CodexCredentialPartition::Siwc,
    ];

    /// A read-back mark settles when the file equals live in every token field.
    #[test]
    fn read_back_mark_clears_when_every_token_field_matches() {
        for partition in BOTH_PARTITIONS {
            let (_dir, manager, origin) = read_back_fixture();
            mark_read_back(&manager, partition, &origin);
            let d = manager.lock().auths["codex-1"].auth.metadata.clone();
            let settled = settle_same(&manager, partition, &origin, &d);
            let ReadBackSettlement::Settled(protection) = settled else {
                panic!("{partition:?}: expected Settled");
            };
            let (outer, siwc) = (protection.undurable.outer, protection.undurable.siwc);
            assert!(!outer && !siwc, "{partition:?}: the mark leaves Protection");
            let baselines = &protection.undurable_baselines;
            assert!(
                baselines.outer.is_empty() && baselines.siwc.is_empty(),
                "{partition:?}: the baselines leave Protection"
            );
            assert_eq!(
                read_back_marks(&manager, partition),
                (false, 0, false, false, false),
                "{partition:?}"
            );
        }
    }

    /// Equal identities with another `expired` do not verify: the mark, its
    /// baselines, its cause and the warned flag stay, with no landing hold.
    #[test]
    fn read_back_mark_stays_when_only_the_expiry_differs() {
        for partition in BOTH_PARTITIONS {
            let (_dir, manager, origin) = read_back_fixture();
            mark_read_back(&manager, partition, &origin);
            let d = disk_with_other_expiry(&manager, partition);
            let settled = settle_same(&manager, partition, &origin, &d);
            let ReadBackSettlement::Settled(protection) = settled else {
                panic!("{partition:?}: expected Settled");
            };
            let held = match partition {
                CodexCredentialPartition::Outer => protection.undurable.outer,
                CodexCredentialPartition::Siwc => protection.undurable.siwc,
            };
            assert!(held, "{partition:?}: the mark stays in Protection");
            let kept = match partition {
                CodexCredentialPartition::Outer => protection.undurable_baselines.outer.len(),
                CodexCredentialPartition::Siwc => protection.undurable_baselines.siwc.len(),
            };
            assert_eq!(kept, 1, "{partition:?}: the baseline stays in Protection");
            assert_eq!(
                read_back_marks(&manager, partition),
                (true, 1, true, true, false),
                "{partition:?}"
            );
        }
    }

    /// The futures of `send_refresh` and `commit_rotation_with_retry` are
    /// `Send`. The closure is type-checked and never called.
    #[test]
    fn send_refresh_and_commit_futures_are_send() {
        let _ = |manager: &Manager,
                 executor: &Arc<dyn ProviderExecutor>,
                 auth: &Arc<Auth>,
                 prep: &LeasePrep,
                 token: CodexLeaseToken,
                 patch: &CredentialRefreshPatch| {
            require_send(manager.send_refresh(
                "id",
                1,
                executor,
                auth,
                crate::executor::RefreshOptions::forced(false),
            ));
            require_send(manager.commit_rotation_with_retry("id", prep, token, patch));
        };
    }

    fn codex(metadata: serde_json::Value) -> Auth {
        Auth {
            id: "codex-1".into(),
            provider: "codex".into(),
            status: Status::Active,
            metadata: metadata.as_object().cloned().unwrap_or_default(),
            ..Auth::default()
        }
    }

    fn unauthorized() -> Option<AuthError> {
        Some(AuthError {
            code: "unauthorized".into(),
            message: "unauthorized".into(),
            http_status: 401,
            ..AuthError::default()
        })
    }

    fn model_401(retry: Option<Timestamp>) -> ModelState {
        ModelState {
            status: Status::Error,
            status_message: "unauthorized".into(),
            unavailable: true,
            next_retry_after: retry,
            last_error: unauthorized(),
            ..ModelState::default()
        }
    }

    /// `current` merged with a refused refresh shaped as `refresh_owned`
    /// leaves `updated`: the state step 1b of a refused publication starts
    /// from.
    fn refused_merge(current: &Auth, now: Timestamp) -> Auth {
        let base = codex(json!({"access_token": "t-base", "refresh_token": "r-base"}));
        let mut updated = codex(json!({"access_token": "t-upd", "refresh_token": "r-upd"}));
        updated.last_refreshed_at = Some(now);
        updated.next_refresh_after = Some(add(now, Duration::from_secs(300)));
        updated.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &base.metadata,
            &updated.metadata,
            true,
            false,
        ));
        merge_refreshed_auth_with(&base, current, &updated, now, ResponseCredentials::Refused)
    }

    fn settled(current: &Auth, now: Timestamp) -> (Auth, Auth) {
        let before = refused_merge(current, now);
        let mut after = before.clone();
        settle_refused_availability(current, &mut after, now);
        (before, after)
    }

    /// B-U3: a kept non-401 error stays, and a model 401 that is the only
    /// model state is reset with it.
    #[test]
    fn refused_availability_keeps_a_non_401_error_and_resets_a_model_401() {
        let now = Utc::now();
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        current.status = Status::Error;
        current.unavailable = true;
        current.status_message = "held".into();
        current.last_error = Some(AuthError {
            message: "boom".into(),
            http_status: 500,
            ..AuthError::default()
        });
        current.model_states.insert(
            "m".into(),
            model_401(Some(add(now, Duration::from_secs(60)))),
        );
        let (before, after) = settled(&current, now);
        assert_eq!(
            before.last_error.as_ref().map(|e| e.message.as_str()),
            Some("boom")
        );
        assert_eq!(
            after.last_error.as_ref().map(|e| e.message.as_str()),
            Some("boom")
        );
        assert_eq!(after.status, Status::Error);
        let state = &after.model_states["m"];
        assert_eq!(state.status, Status::Active);
        assert!(!state.unavailable);
        assert!(state.last_error.is_none());
        assert!(state.status_message.is_empty());
    }

    /// B-U3, the oracle's unit case: the merge carries the hold a model's 401
    /// derived, and step 1b releases it.
    #[test]
    fn refused_availability_releases_the_hold_a_model_401_derived() {
        let now = Utc::now();
        let retry = add(now, Duration::from_secs(1800));
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        current
            .model_states
            .insert("m".into(), model_401(Some(retry)));
        current.unavailable = true;
        current.next_retry_after = Some(retry);
        let (before, after) = settled(&current, now);
        assert!(before.unavailable, "the merge carries the derived hold");
        assert_eq!(before.next_retry_after, Some(retry));
        assert!(!after.unavailable);
        assert!(after.next_retry_after.is_none());
        assert_eq!(after.model_states["m"].status, Status::Active);
    }

    /// B-U3: a concurrent cooldown with no unauthorized model state is left
    /// as the merge made it.
    #[test]
    fn refused_availability_leaves_a_cooldown_without_a_model_401() {
        let now = Utc::now();
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        current.status = Status::Error;
        current.unavailable = true;
        current.status_message = "held".into();
        current.next_retry_after = Some(add(now, Duration::from_secs(60)));
        current.last_error = Some(AuthError {
            message: "rate limited".into(),
            http_status: 429,
            ..AuthError::default()
        });
        let (before, after) = settled(&current, now);
        assert_eq!(after.unavailable, before.unavailable);
        assert_eq!(after.next_retry_after, before.next_retry_after);
        assert_eq!(after.status, before.status);
        assert_eq!(after.status_message, before.status_message);
        assert_eq!(
            after.last_error.as_ref().map(|e| e.message.as_str()),
            before.last_error.as_ref().map(|e| e.message.as_str())
        );
        assert!(after.model_states.is_empty());
    }

    /// B-U3: a model 401 under an active model quota loses only its error.
    #[test]
    fn refused_availability_keeps_a_model_quota_under_a_model_401() {
        let now = Utc::now();
        let recover = add(now, Duration::from_secs(600));
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        let mut state = model_401(Some(recover));
        state.quota.exceeded = true;
        state.quota.reason = "quota".into();
        state.quota.next_recover_at = Some(recover);
        current.model_states.insert("m".into(), state);
        let (_, after) = settled(&current, now);
        let state = &after.model_states["m"];
        assert!(state.last_error.is_none());
        assert_eq!(state.status_message, "quota");
        assert_eq!(state.updated_at, Some(now));
        assert!(state.quota.exceeded);
        assert_eq!(state.quota.next_recover_at, Some(recover));
        assert!(state.unavailable);
    }

    /// B-U3: a top-level 401 on `current` is cleared.
    #[test]
    fn refused_availability_clears_a_top_level_401() {
        let now = Utc::now();
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        current.status = Status::Error;
        current.unavailable = true;
        current.status_message = "unauthorized".into();
        current.last_error = unauthorized();
        assert!(has_unauthorized_auth_failure(&current));
        let (_, after) = settled(&current, now);
        assert_eq!(after.status, Status::Active);
        assert!(!after.unavailable);
        assert!(after.last_error.is_none());
        assert!(after.status_message.is_empty());
    }

    /// B-U3: an active credential quota on `current` is restored over the
    /// availability the model-state reset derived.
    #[test]
    fn refused_availability_restores_an_active_credential_quota() {
        let now = Utc::now();
        let recover = add(now, Duration::from_secs(600));
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        current.status = Status::Error;
        current.unavailable = true;
        current.next_retry_after = Some(recover);
        current.quota.exceeded = true;
        current.quota.reason = "credential_quota".into();
        current.quota.next_recover_at = Some(recover);
        current.model_states.insert("m".into(), model_401(None));
        // The top-level 401 makes step 1b's unauthorized reset set the merged
        // record Active and available. `status` pins the quota restore block:
        // only that block brings `current`'s Error status back. `unavailable`
        // returns anyway, through `clear_unauthorized_model_states` ->
        // `update_aggregated_availability`, so it doesn't pin the block.
        current.last_error = unauthorized();
        let (before, after) = settled(&current, now);
        assert_eq!(before.status, Status::Error, "the merge before step 1b");
        assert!(before.unavailable, "the merge before step 1b");
        assert_eq!(before.next_retry_after, Some(recover));
        assert!(after.last_error.is_none(), "the reset ran");
        assert_eq!(after.status, Status::Error);
        assert!(after.unavailable);
        assert_eq!(after.next_retry_after, Some(recover));
        assert_eq!(after.quota, current.quota);
        assert_eq!(after.model_states["m"].status, Status::Active);
    }

    /// B-U3: a disabled `current`, or a disabled merge, is left alone.
    #[test]
    fn refused_availability_leaves_a_disabled_credential() {
        let now = Utc::now();
        let mut current = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        current.disabled = true;
        current.status = Status::Disabled;
        current.model_states.insert("m".into(), model_401(None));
        current.last_error = unauthorized();
        let before = refused_merge(&current, now);
        let mut after = before.clone();
        settle_refused_availability(&current, &mut after, now);
        assert_eq!(after.model_states, before.model_states);
        assert_eq!(after.status, before.status);
        assert_eq!(after.last_error.is_some(), before.last_error.is_some());

        let mut enabled = codex(json!({"access_token": "t-cur", "refresh_token": "r-cur"}));
        enabled.model_states.insert("m".into(), model_401(None));
        let mut merged = refused_merge(&enabled, now);
        merged.disabled = true;
        let before = merged.clone();
        settle_refused_availability(&enabled, &mut merged, now);
        assert_eq!(merged.model_states, before.model_states);
        assert_eq!(merged.status, before.status);

        // A disabled `current` with an enabled merge that holds a model 401:
        // only the `current` guard keeps step 1b from resetting it.
        let mut merged = refused_merge(&current, now);
        merged.disabled = false;
        merged.status = Status::Active;
        merged.model_states.insert("m".into(), model_401(None));
        let before = merged.clone();
        settle_refused_availability(&current, &mut merged, now);
        assert_eq!(merged.model_states, before.model_states);
        assert_eq!(merged.status, before.status);
        assert_eq!(merged.unavailable, before.unavailable);
        assert_eq!(merged.next_retry_after, before.next_retry_after);
    }

    fn fence_manager() -> Manager {
        Manager::new(
            Settings::default(),
            Arc::new(crate::registry::ModelRegistry::new()),
            None,
        )
    }

    /// Registers a Codex record with the given outer and SIWC refresh tokens
    /// and returns its registered snapshot.
    fn register_codex(manager: &Manager, outer: &str, siwc: &str) -> Arc<Auth> {
        let auth = codex(json!({
            "type": "codex",
            "access_token": "t",
            "refresh_token": outer,
            "siwc": {"refresh_token": siwc, "client_id": "client"},
        }));
        manager.register_unsaved(auth).expect("register")
    }

    fn fence(manager: &Manager, snapshot: &Auth) -> SendFence {
        manager.locked(|s| {
            manager.fence_send_locked(s, "codex-1", snapshot.registration_epoch, snapshot)
        })
    }

    fn mutate(manager: &Manager, f: impl FnOnce(&mut Entry)) {
        let mut state = manager.lock();
        f(state.auths.get_mut("codex-1").expect("registered"));
    }

    #[test]
    fn fence_sends_a_non_codex_record_without_looking() {
        let manager = fence_manager();
        let other = Auth {
            id: "other".into(),
            provider: "claude".into(),
            ..Auth::default()
        };
        let decision = manager.locked(|s| manager.fence_send_locked(s, "other", 9, &other));
        assert!(matches!(decision, SendFence::Send));
    }

    #[test]
    fn fence_reports_a_changed_registration() {
        let manager = fence_manager();
        let snapshot = register_codex(&manager, "r1", "s1");
        assert!(matches!(fence(&manager, &snapshot), SendFence::Send));
        let epoch = snapshot.registration_epoch;
        let decision =
            manager.locked(|s| manager.fence_send_locked(s, "codex-1", epoch + 1, &snapshot));
        assert!(matches!(decision, SendFence::Changed));
        let decision = manager.locked(|s| manager.fence_send_locked(s, "gone", epoch, &snapshot));
        assert!(matches!(decision, SendFence::Changed));
    }

    #[test]
    fn fence_defers_on_a_mark_of_a_partition_it_would_send() {
        let manager = fence_manager();
        let snapshot = register_codex(&manager, "r1", "s1");
        let outer = UndurablePartitions {
            outer: true,
            siwc: false,
        };
        mutate(&manager, |e| e.landing_unconfirmed = outer);
        assert!(matches!(
            fence(&manager, &snapshot),
            SendFence::Defer(Deferral::LandingUnconfirmed)
        ));
        mutate(&manager, |e| {
            e.landing_unconfirmed = UndurablePartitions::default();
            e.undurable_causes.siwc = Some(MarkCause::ReadBack {
                origin: CodexOrigin::of(&std::env::current_exe().expect("exe")).expect("origin"),
            });
        });
        assert!(matches!(
            fence(&manager, &snapshot),
            SendFence::Defer(Deferral::ReadBackUnsettled)
        ));
    }

    #[test]
    fn fence_ignores_a_mark_of_a_partition_it_would_not_send() {
        let manager = fence_manager();
        // The snapshot has no SIWC client id, so only outer would be sent.
        let auth = codex(json!({
            "type": "codex",
            "refresh_token": "r1",
            "siwc": {"refresh_token": "s1"},
        }));
        let snapshot = manager.register_unsaved(auth).expect("register");
        mutate(&manager, |e| {
            e.landing_unconfirmed = UndurablePartitions {
                outer: false,
                siwc: true,
            }
        });
        assert!(matches!(fence(&manager, &snapshot), SendFence::Send));
    }

    /// Records an unresolved-origin state for the partitions in `held` on the
    /// registered entry.
    fn hold_unresolved(manager: &Manager, held: UndurablePartitions) {
        let origin = CodexOrigin::of(&std::env::current_exe().expect("exe")).expect("origin");
        let before = json!({"refresh_token": "r1"});
        let after = json!({"refresh_token": "r-next"});
        let rotation = CredentialRefreshPatch::new(
            before.as_object().expect("object"),
            after.as_object().expect("object"),
            held.outer,
            held.siwc,
        );
        mutate(manager, |e| e.record_unresolved(&origin, &rotation));
    }

    #[test]
    fn fence_defers_on_an_unresolved_origin_of_a_partition_it_would_send() {
        let manager = fence_manager();
        let snapshot = register_codex(&manager, "r1", "s1");
        hold_unresolved(
            &manager,
            UndurablePartitions {
                outer: true,
                siwc: false,
            },
        );
        assert!(matches!(
            fence(&manager, &snapshot),
            SendFence::Defer(Deferral::OriginUnresolved)
        ));
    }

    #[test]
    fn fence_ignores_an_unresolved_origin_of_a_partition_it_would_not_send() {
        let manager = fence_manager();
        // The snapshot has no SIWC client id, so only outer would be sent.
        let auth = codex(json!({
            "type": "codex",
            "refresh_token": "r1",
            "siwc": {"refresh_token": "s1"},
        }));
        let snapshot = manager.register_unsaved(auth).expect("register");
        hold_unresolved(
            &manager,
            UndurablePartitions {
                outer: false,
                siwc: true,
            },
        );
        assert!(matches!(fence(&manager, &snapshot), SendFence::Send));
    }

    #[test]
    fn fence_returns_the_live_record_when_a_refresh_token_was_replaced() {
        let manager = fence_manager();
        let snapshot = register_codex(&manager, "r1", "s1");
        mutate(&manager, |e| {
            Arc::make_mut(&mut e.auth)
                .metadata
                .insert("refresh_token".into(), json!("r2"));
        });
        match fence(&manager, &snapshot) {
            SendFence::Superseded(live) => {
                assert_eq!(live.metadata_str("refresh_token"), Some("r2"))
            }
            _ => panic!("expected the outer replacement to supersede the pass"),
        }
        mutate(&manager, |e| {
            let live = Arc::make_mut(&mut e.auth);
            live.metadata.insert("refresh_token".into(), json!("r1"));
            live.metadata.insert(
                "siwc".into(),
                json!({"refresh_token": "s2", "client_id": "client"}),
            );
        });
        assert!(matches!(
            fence(&manager, &snapshot),
            SendFence::Superseded(_)
        ));
    }
}
