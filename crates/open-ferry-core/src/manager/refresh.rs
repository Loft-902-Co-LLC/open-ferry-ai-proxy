// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_refresh.go and
// sdk/cliproxy/auth/auto_refresh_loop.go (v8.0.10, MIT).
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
    preferred_interval,
};
use super::text::{equal_fold, go_lower};
use super::{Manager, ManagerError, Shared, State, lock};
use crate::auth::{Auth, Status, Timestamp};
use crate::exec::{ErrorKind, ExecError};

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
    if auth_kind(auth) == KIND_API_KEY {
        return None;
    }
    if let Some(held) = auth.next_refresh_after
        && now < held
    {
        return Some(held);
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
/// models (upstream's `clearUnauthorizedModelStates`).
pub(crate) fn clear_unauthorized_model_states(auth: &mut Auth, now: Timestamp) -> Vec<String> {
    let mut resumed = Vec::new();
    for (model, state) in &mut auth.model_states {
        let unauthorized = state.last_error.as_ref().is_some_and(|err| {
            err.http_status == 401
                || equal_fold(&err.code, "unauthorized")
                || is_unauthorized_error(ErrView::Auth(err))
        }) || go_lower(&state.status_message).contains("unauthorized");
        if !unauthorized {
            continue;
        }
        reset_model_state(state, now);
        resumed.push(model.clone());
    }
    if !resumed.is_empty() {
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
        let wait = {
            let Some(manager) = upgrade(&weak) else {
                return;
            };
            shared.next_wait(manager.now())
        };
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
        if manager.begin_refresh_job(&job, &stop) {
            let _ = manager.refresh_at_epoch(&job.id, "", job.epoch).await;
        }
        manager.finish_refresh_job(&job, None, false);
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
        for _ in 0..workers {
            runtime.spawn(run_worker(
                Arc::downgrade(&self.shared),
                jobs_rx.clone(),
                stop_rx.clone(),
            ));
        }
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
    fn mark_refresh_pending(
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
            Err(_) => {
                tracing::debug!(auth_id = %auth.id, "credential refresh before fallback failed");
                None
            }
        }
    }

    /// Refreshes a credential now, whether or not it is due (upstream's
    /// `ForceRefreshAuth`).
    pub async fn force_refresh(&self, id: &str) -> Result<Arc<Auth>, ManagerError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Other("auth id is empty".into()));
        }
        self.refresh_at_epoch(id, "", 0).await
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
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Other("auth id is empty".into()));
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
        if has_disabled_invalid_grant_failure(&auth) {
            return Err(ManagerError::Other(
                "auth is disabled with invalid grant".into(),
            ));
        }
        // Another call may have refreshed the credential already.
        if !failed_token.is_empty() {
            let current = access_token(&auth);
            if !current.is_empty() && current != failed_token {
                return Ok(auth);
            }
        }

        let outcome = executor.refresh(auth.clone()).await;
        let now = self.now();
        let mut updated = match outcome {
            Ok(updated) => updated,
            Err(err) if err.kind == ErrorKind::Canceled => {
                tracing::debug!(auth_id = %id, "refresh canceled");
                return Err(ManagerError::Refresh(err));
            }
            Err(err) => {
                tracing::debug!(auth_id = %id, status = err.status, "refresh failed");
                self.record_refresh_failure(id, base_epoch, &err, now);
                return Err(ManagerError::Refresh(err));
            }
        };
        updated.last_refreshed_at = Some(now);
        updated.next_refresh_after = None;
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
        let (saved, generation) = match self.update_refreshed(&auth, base_epoch, updated) {
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
        self.publish_projections(&saved, generation, now, true);
        Ok(saved)
    }

    /// Records a failed refresh on the live credential and sets when to try
    /// again (the failure branch of upstream's
    /// `refreshAuthForRequestAtEpoch`). Nothing changes when the credential
    /// was registered again meanwhile.
    fn record_refresh_failure(&self, id: &str, base_epoch: u64, err: &ExecError, now: Timestamp) {
        let unauthorized = is_unauthorized_error(ErrView::Exec(err));
        let invalid_grant = is_invalid_grant_error(ErrView::Exec(err));
        let after = {
            let mut state = self.lock();
            let Some(entry) = state.auths.get_mut(id) else {
                return;
            };
            if entry.auth.registration_epoch != base_epoch {
                return;
            }
            let mut failures = entry.refresh_failures;
            let auth = Arc::make_mut(&mut entry.auth);
            auth.generation = auth.generation.saturating_add(1);
            auth.updated_at = Some(now);
            auth.last_error = Some(refresh_error_from_error(ErrView::Exec(err)));
            let disabled = is_disabled(auth);
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
            } else if !auth.has_valid_access_token(now) {
                auth.unavailable = true;
                auth.status = Status::Error;
                if unauthorized {
                    auth.next_refresh_after = None;
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
            after
        };
        match after {
            AfterFailure::Reschedule => self.queue_refresh_reschedule(id),
            AfterFailure::Unschedule => self.queue_refresh_unschedule(id),
            AfterFailure::Nothing => {}
        }
    }
}
