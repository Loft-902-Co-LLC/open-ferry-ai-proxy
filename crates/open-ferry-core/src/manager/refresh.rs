// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_refresh.go and
// sdk/cliproxy/auth/auto_refresh_loop.go (v8.0.20, MIT).
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc, watch};

use super::classify::{
    ErrView, has_disabled_invalid_grant_failure, has_unauthorized_auth_failure,
    is_invalid_grant_error, is_request_scoped_error, is_unauthorized_error,
    refresh_error_from_error,
};
use super::cooldown::{add, reset_model_state, update_aggregated_availability};
use super::credential::{
    KIND_API_KEY, access_token, auth_kind, has_refresh_credential, is_zero, last_refresh_timestamp,
    preferred_interval, refresh_token,
};
use super::merge::normalize_credential_metadata;
use super::text::{equal_fold, go_lower};
use super::{Manager, ManagerError, Shared, State, lock};
use crate::auth::classification::ATTRIBUTE_PATH;
use crate::auth::{Auth, AuthError, Status, Timestamp};
use crate::exec::{ErrorKind, ExecError};
use crate::gate::{GateState, NotActive};

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

/// Whether a credential should refresh now (upstream's `shouldRefresh`).
/// `lead` is how long before expiry its executor refreshes, if it does.
pub(crate) fn should_refresh(auth: &Auth, lead: Option<Duration>, now: Timestamp) -> bool {
    if has_unauthorized_auth_failure(auth) || has_disabled_invalid_grant_failure(auth) {
        return false;
    }
    if refresh_held(auth, now) {
        return false;
    }
    if auth.siwc_refresh_due_at(now).is_some() {
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
    if auth.siwc_refresh_due_at(now).is_some() {
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

impl Auth {
    /// Due time for the refreshable outer Codex partition. The outer metadata
    /// refresh timestamp wins over runtime time so SIWC rotations cannot
    /// postpone the outer interval. `None` lead uses Codex's 24-hour default.
    pub fn outer_refresh_due_at(
        &self,
        now: Timestamp,
        lead: Option<Duration>,
    ) -> Option<Timestamp> {
        if !crate::auth::codex_refresh::is_codex_auth(self) || refresh_token(self).is_empty() {
            return None;
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
            let lead = lead.unwrap_or(Duration::from_secs(24 * 3600));
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
            && let Err(ManagerError::NotActive(_)) =
                manager.refresh_at_epoch(&job.id, "", job.epoch).await
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
        next_refresh_check_at(
            now,
            &entry.auth,
            Self::refresh_lead_for(&state, &entry.auth),
        )
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
                    next_refresh_check_at(now, &entry.auth, lead).map(|next| (id.clone(), next))
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
        let (next, due, has_executor, epoch) = {
            let state = self.lock();
            let Some(entry) = state.auths.get(id) else {
                return;
            };
            let executor = Self::executor_for(&state, &entry.auth);
            let lead = executor
                .as_ref()
                .and_then(|executor| executor.refresh_lead());
            (
                next_refresh_check_at(now, &entry.auth, lead),
                should_refresh(&entry.auth, lead, now),
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
        let Some(job) = self.mark_refresh_pending(shared.id, id, epoch, now) else {
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
    pub(crate) fn mark_refresh_pending(
        &self,
        loop_id: u64,
        id: &str,
        epoch: u64,
        now: Timestamp,
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
    pub(crate) async fn try_refresh_after_unauthorized(
        &self,
        auth: &Arc<Auth>,
        err: &ExecError,
        already_tried: bool,
    ) -> Option<Arc<Auth>> {
        if already_tried {
            return None;
        }
        // A request-scoped failure is about this request, not the token.
        if is_request_scoped_error(ErrView::Exec(err)) {
            return None;
        }
        if !is_unauthorized_error(ErrView::Exec(err)) || !has_refresh_credential(auth) {
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
            .refresh_at_epoch(&auth.id, &access_token(auth), 0)
            .await
        {
            Ok(refreshed) => Some(refreshed),
            Err(ManagerError::NotActive(_)) => {
                let manager = self.clone();
                let failed = auth.clone();
                // Adoption is read-only and must not require an Open write gate.
                if let Some(operation) = crate::gate::Operation::current() {
                    operation
                        .run_blocking(move || manager.adopt_from_store(&failed))
                        .await
                        .ok()
                        .flatten()
                } else {
                    // No operation exists to carry, and reads must work with a closed gate.
                    #[allow(clippy::disallowed_methods)]
                    tokio::task::spawn_blocking(move || manager.adopt_from_store(&failed))
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
    /// job runs, no published snapshot of it waits to be saved, none was
    /// published (so none can have been saved) between the read and the swap
    /// (its save-publication sequence is as it was before the read), and its
    /// registration is the one that was read. A credential whose token moved
    /// on (a refresh of this process published a newer one) is returned as
    /// it is. With nothing in flight every later snapshot is built on the
    /// adopted state, so adopting never advances the persist watermark.
    ///
    /// The refresh lock is taken first, as `refresh_owned` takes it, with
    /// `try_lock` since this isn't async, and held through the disk read and
    /// the swap; the state lock is taken only inside it.
    fn adopt_from_store(&self, failed: &Arc<Auth>) -> Option<Arc<Auth>> {
        let store = self.shared.store.as_ref()?;
        let failed_token = access_token(failed);
        let current = self.get(&failed.id)?;
        if access_token(&current) != failed_token {
            return Some(current);
        }
        let id_lock = lock(&self.shared.refresh_locks)
            .entry(failed.id.clone())
            .or_default()
            .clone();
        let Ok(_refreshing) = id_lock.try_lock() else {
            // A refresh is under way; it may publish a newer token, and
            // adopting now would drop it.
            return None;
        };
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
            {
                return None;
            }
        }
        self.with_adoption(&failed.id, &permit, |_guard| {
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
                if access_token(&entry.auth) != failed_token {
                    return Some(entry.auth.clone());
                }
                if entry.auth.registration_epoch != failed.registration_epoch {
                    // Removed and registered again since the request's snapshot.
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
                if access_token(&entry.auth) != failed_token {
                    return Some(entry.auth.clone());
                }
                let failed_partition = if failed.use_siwc() {
                    crate::auth::CodexCredentialPartition::Siwc
                } else {
                    crate::auth::CodexCredentialPartition::Outer
                };
                if entry.undurable.contains(failed_partition) {
                    return None;
                }
                super::lifecycle::overlay_undurable(&mut disk, &entry.auth, entry.undurable);
                disk.rebase_pending_refreshes_with_origin(
                    &entry.auth,
                    Some(crate::auth::ResolvedOriginProof(same_origin)),
                );
                // Rebase can retain the failed rotated token even when raw disk
                // contains a different, consumed token. That is NOT an adoption.
                let effective = access_token(&disk);
                if effective.is_empty() || effective == failed_token {
                    return None;
                }
                let pending = super::lifecycle::pending_for_edit(
                    &entry.auth,
                    &disk.metadata,
                    self.permanently_ineligible(&entry.auth),
                );
                let auth = Arc::make_mut(&mut entry.auth);
                auth.metadata = disk.metadata;
                auth.codex_pending_refresh = pending;
                auth.codex_persist_intent = Default::default();
                auth.rejected_access_token.clear();
                auth.credential_version = auth.credential_version.saturating_add(1);
                auth.generation = auth.generation.saturating_add(1);
                auth.updated_at = Some(now);
                let snapshot = entry.auth.clone();
                state.sync_scheduler(self.models(), &failed.id, now);
                snapshot
            };
            self.queue_refresh_reschedule(&failed.id);
            Some(adopted)
        })
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
        self.refresh_with(id, "", 0, true).await
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
        self.refresh_with(id, failed_token, epoch, false).await
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
    async fn refresh_with(
        &self,
        id: &str,
        failed_token: &str,
        epoch: u64,
        force: bool,
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
            manager
                .refresh_owned(&id, &failed_token, epoch, force)
                .await
        };
        if tokio::runtime::Handle::try_current().is_err() {
            // No runtime to own the task: run it here, still as the
            // operation.
            return operation.scope(work).await;
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
    ) -> Result<Arc<Auth>, ManagerError> {
        if !failed_token.is_empty() {
            self.mark_rejected_access_token(id, failed_token);
        }
        let id_lock = lock(&self.shared.refresh_locks)
            .entry(id.to_owned())
            .or_default()
            .clone();
        let _serialized = id_lock.lock().await;

        let (auth, base_epoch, executor) = {
            let state = self.lock();
            let Some(entry) = state.auths.get(id) else {
                return Err(ManagerError::Other("auth or executor not found".into()));
            };
            let Some(executor) = Self::executor_for(&state, &entry.auth) else {
                return Err(ManagerError::Other("auth or executor not found".into()));
            };
            (entry.auth.clone(), entry.auth.registration_epoch, executor)
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
            if !current.is_empty() && current != failed_token {
                return Ok(auth);
            }
        }

        // A waiter that got the lock after the switch began declines before
        // it calls the provider.
        let state = self.gate().state();
        if state != GateState::Open {
            return Err(ManagerError::NotActive(NotActive { state }));
        }
        let outcome = executor
            .refresh_with_options(auth.clone(), crate::executor::RefreshOptions::forced(force))
            .await
            .map_err(|mut error| {
                let mut secrets = crate::observe::redact::Secrets::new();
                secrets.add_auth(&auth);
                error.message = secrets.text(error.message, crate::observe::redact::Policy::Disk);
                if let Some(cause) = error.cause.take() {
                    error.cause = Some(secrets.text(cause, crate::observe::redact::Policy::Disk));
                }
                error
            });
        let now = self.now();
        let mut updated = match outcome {
            Ok(updated) => updated,
            Err(err) if err.kind == ErrorKind::Canceled => {
                tracing::debug!(auth_id = %id, "refresh canceled");
                // Try again shortly, as the loop would have.
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
        if partial_failure
            .as_ref()
            .is_some_and(|failure| failure.partition.is_active(&auth))
        {
            updated
                .rejected_access_token
                .clone_from(&auth.rejected_access_token);
        } else {
            updated.rejected_access_token.clear();
        }
        updated.last_error = None;
        updated.status_message.clear();
        updated.unavailable = false;
        if matches!(updated.status, Status::Error | Status::Unknown) {
            updated.status = Status::Active;
        }
        updated.updated_at = Some(now);
        clear_unauthorized_model_states(&mut updated, now);
        if should_refresh(&updated, executor.refresh_lead(), now) {
            updated.next_refresh_after = Some(add(now, REFRESH_INEFFECTIVE_BACKOFF));
        }
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
                self.update_refreshed_after_save_failure(&auth, base_epoch, updated)
            } else {
                self.save_refreshed(&auth, base_epoch, updated).await
            }
        };
        #[cfg(not(feature = "blue-green-test"))]
        let save_result = self.save_refreshed(&auth, base_epoch, updated).await;
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
    ) -> Result<Option<(Arc<Auth>, u64)>, ManagerError> {
        let blocking =
            self.shared.store.is_some() && crate::auth::codex_refresh::is_codex_auth(&updated);
        #[cfg(feature = "blue-green-test")]
        let blocking = blocking || crate::test_hooks::is_installed();
        if !blocking || tokio::runtime::Handle::try_current().is_err() {
            return self.update_refreshed(base, base_epoch, updated);
        }
        let manager = self.clone();
        let base = base.clone();
        let saved = if let Some(operation) = crate::gate::Operation::current() {
            // run_blocking owns admission even if the caller drops its await.
            operation
                .run_blocking(move || manager.update_refreshed(&base, base_epoch, updated))
                .await
        } else {
            // No operation is in scope; the owned closure retains manager save admission.
            #[allow(clippy::disallowed_methods)]
            tokio::task::spawn_blocking(move || {
                manager.update_refreshed(&base, base_epoch, updated)
            })
            .await
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

    /// Marks `failed_token` as refused by the provider while it is still the
    /// credential's access token and carries no expiry of its own, so the
    /// credential reads as expired, and isn't picked, until a refresh works
    /// or its tokens change (upstream's `markRejectedAccessToken`).
    pub(crate) fn mark_rejected_access_token(&self, id: &str, failed_token: &str) {
        let now = self.now();
        let mut state = self.lock();
        let Some(entry) = state.auths.get_mut(id) else {
            return;
        };
        if access_token(&entry.auth) != failed_token
            || entry.auth.access_token_expiration_time().is_some()
        {
            return;
        }
        let auth = Arc::make_mut(&mut entry.auth);
        auth.generation = auth.generation.saturating_add(1);
        auth.updated_at = Some(now);
        auth.rejected_access_token = failed_token.to_owned();
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
        let access_token_rejected =
            !attempt.failed_token.is_empty() && access_token(auth) == attempt.failed_token;
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
