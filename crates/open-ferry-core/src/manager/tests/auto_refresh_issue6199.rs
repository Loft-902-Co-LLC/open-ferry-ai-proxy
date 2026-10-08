// Ported from CLIProxyAPI sdk/cliproxy/auth/auto_refresh_issue6199_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The refresh loop's job queue (upstream issue 6199): a credential queued
//! or refreshing isn't queued again, a full queue doesn't hold the loop, and
//! a stopped or replaced loop lets its jobs go.
//!
//! Each test drives the loop's steps by hand, as upstream does, through a
//! [`TestLoop`] installed as the manager's loop.
//!
//! Deviations from upstream:
//! - A refresh blocked on a channel is a refresh with a Tokio delay
//!   ([`FakeExecutor::set_refresh_delay`]); moving Tokio time releases it.
//!   The delay is read when a refresh starts, so setting it back to zero
//!   blocks only the refreshes already running.
//! - Cancelling the loop's context is stopping the loop. A refresh already
//!   running finishes (refresh.rs), where upstream's blocked refresh in
//!   `RunningAuthDoesNotStarveHealthyAuth` returns the context's error; the
//!   test checks the same thing: the workers exit once it is released.
//! - The loop hands jobs out with `try_send`, so `FullQueueDoesNotBlockScheduler`
//!   can't block by construction; the test still checks the queue and the
//!   overflow's retry.
//! - Upstream reads the loop's heap index for a scheduled credential; here
//!   the credential is found by popping the queue past its retry time.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::support::*;
use crate::auth::{Auth, Status, Timestamp};
use crate::manager::refresh::{LoopShared, RefreshJob, RefreshLoopHandle, run_loop, run_worker};
use crate::manager::{Manager, Settings, lock};

const PROVIDER: &str = "issue6199-refresh";
/// Upstream's `refreshCheckInterval`.
const INTERVAL: Duration = Duration::from_secs(5);
/// Upstream's `refreshPendingBackoff`.
const PENDING_BACKOFF: TimeDelta = TimeDelta::minutes(1);
/// The job buffer of a loop with two workers: four per worker, at least
/// 64 (upstream's `newAuthAutoRefreshLoop`).
const JOB_BUFFER: usize = 64;
/// Long enough that a blocked refresh never ends on its own in a test.
const BLOCKED: Duration = Duration::from_secs(3600);

/// One refresh loop driven by hand (upstream's `authAutoRefreshLoop` as the
/// tests use it).
struct TestLoop {
    shared: Arc<LoopShared>,
    jobs: mpsc::Sender<RefreshJob>,
    queue: Arc<tokio::sync::Mutex<mpsc::Receiver<RefreshJob>>>,
    stop_tx: watch::Sender<bool>,
    stop: watch::Receiver<bool>,
}

impl TestLoop {
    /// A loop, installed as the manager's so reschedules reach it
    /// (upstream's `newAuthAutoRefreshLoop` then `manager.refreshLoop =
    /// loop`).
    fn install(manager: &Manager) -> Self {
        let shared = Arc::new(LoopShared::new(INTERVAL));
        let (jobs, queue) = mpsc::channel(JOB_BUFFER);
        let (stop_tx, stop) = watch::channel(false);
        // The manager's handle only stops loops it started; this loop has
        // its own stop channel.
        let (handle_stop, _) = watch::channel(false);
        *lock(&manager.shared.refresh_loop) = Some(RefreshLoopHandle {
            shared: shared.clone(),
            stop: handle_stop,
        });
        Self {
            shared,
            jobs,
            queue: Arc::new(tokio::sync::Mutex::new(queue)),
            stop_tx,
            stop,
        }
    }

    fn rebuild(&self, manager: &Manager, now: Timestamp) {
        manager.rebuild_refresh_queue(&self.shared, now);
    }

    fn apply_dirty(&self, manager: &Manager, now: Timestamp) {
        manager.apply_dirty(&self.shared, now);
    }

    fn handle_due(&self, manager: &Manager, now: Timestamp) {
        manager.handle_due(&self.shared, &self.jobs, &self.stop, now);
    }

    /// How many jobs wait in the queue (upstream's `len(loop.jobs)`).
    fn queued(&self) -> usize {
        self.jobs.max_capacity() - self.jobs.capacity()
    }

    /// Takes every waiting job off the queue without running it.
    async fn drain(&self) {
        let mut queue = self.queue.lock().await;
        while queue.try_recv().is_ok() {}
    }

    /// Starts a worker (upstream's `go loop.worker(ctx)`).
    fn worker(&self, manager: &Manager) -> JoinHandle<()> {
        tokio::spawn(run_worker(
            Arc::downgrade(&manager.shared),
            self.queue.clone(),
            self.stop.clone(),
        ))
    }

    /// Stops the loop and its workers (upstream's `cancelCtx()`).
    fn cancel(&self) {
        self.stop_tx.send_replace(true);
    }

    /// Runs the loop until it stops (upstream's `loop.run(ctx)`), with two
    /// workers.
    async fn run(&self, manager: &Manager) {
        let _workers = [self.worker(manager), self.worker(manager)];
        run_loop(
            Arc::downgrade(&manager.shared),
            self.shared.clone(),
            self.stop.clone(),
            self.jobs.clone(),
        )
        .await;
    }
}

fn rfc3339(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Upstream's `newIssue6199RefreshLoop`: a manager with the executor, and a
/// loop with two workers.
fn new_refresh_loop() -> (Harness, Arc<FakeExecutor>, TestLoop) {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new(PROVIDER);
    // Upstream's countingRefreshExecutor.
    executor.set_refresh(|auth: &Auth| {
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("refreshed-token"));
        Ok(auth)
    });
    h.executor(&executor);
    let test_loop = TestLoop::install(&h.manager);
    (h, executor, test_loop)
}

/// Upstream's `registerIssue6199ExpiredAuth`: a credential whose token
/// expired an hour before `now`, refreshing every second.
fn register_expired_auth(h: &Harness, id: &str, now: Timestamp) {
    let hour_ago = now - TimeDelta::hours(1);
    let mut auth = auth_with_metadata(
        id,
        PROVIDER,
        json!({
            "access_token": "expired-access",
            "refresh_token": format!("refresh-{id}"),
            "expires_at": rfc3339(hour_ago),
            "refresh_interval_seconds": 1,
        }),
    );
    auth.status = Status::Active;
    auth.last_refreshed_at = Some(hour_ago);
    h.add(auth, &[]);
}

/// Whether `id` is on the loop's queue (upstream's `loop.index[id]`). Pops
/// the queue.
fn scheduled(test_loop: &TestLoop, id: &str, by: Timestamp) -> bool {
    test_loop.shared.pop_due(by).iter().any(|due| due == id)
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_does_not_requeue_queued_auth() {
    let (h, _executor, test_loop) = new_refresh_loop();
    let mut now = h.now();
    register_expired_auth(&h, "queued", now);
    test_loop.rebuild(&h.manager, now);
    test_loop.handle_due(&h.manager, now);
    assert_eq!(test_loop.queued(), 1, "initial queued jobs");

    // Move past the pending window three times, with no worker to take the
    // job.
    for _ in 0..3 {
        test_loop.apply_dirty(&h.manager, now);
        now += PENDING_BACKOFF;
        test_loop.handle_due(&h.manager, now);
    }
    assert_eq!(
        test_loop.queued(),
        1,
        "queued auth has jobs across pending windows, want exactly 1"
    );
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_running_auth_does_not_starve_healthy_auth() {
    let (h, executor, test_loop) = new_refresh_loop();
    let completed = Arc::new(Mutex::new(Vec::<String>::new()));
    let done = completed.clone();
    executor.set_refresh(move |auth: &Auth| {
        lock(&done).push(auth.id.clone());
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("refreshed-token"));
        Ok(auth)
    });
    // Only the refresh that starts now blocks.
    executor.set_refresh_delay(BLOCKED);

    let mut now = h.now();
    register_expired_auth(&h, "blocked", now);
    test_loop.rebuild(&h.manager, now);
    test_loop.handle_due(&h.manager, now);
    let first = test_loop.worker(&h.manager);
    settle().await;
    let started: Vec<String> = executor.refreshes().iter().map(|a| a.id.clone()).collect();
    assert_eq!(
        started,
        ["blocked"],
        "initial refresh did not enter the executor"
    );
    executor.set_refresh_delay(Duration::ZERO);

    // The refresh stays blocked while the loop sees three pending windows.
    for _ in 0..3 {
        test_loop.apply_dirty(&h.manager, now);
        now += PENDING_BACKOFF;
        test_loop.handle_due(&h.manager, now);
    }
    assert_eq!(test_loop.queued(), 0, "running auth queued duplicate jobs");

    register_expired_auth(&h, "healthy", now);
    test_loop.apply_dirty(&h.manager, now);
    test_loop.handle_due(&h.manager, now);
    // The spare worker starts only once the queue is arranged.
    let spare = test_loop.worker(&h.manager);
    settle().await;
    assert_eq!(
        *lock(&completed),
        ["healthy"],
        "healthy auth could not refresh while another auth was blocked"
    );

    // Stop the loop and release the blocked refresh: the workers exit.
    test_loop.cancel();
    tokio::time::advance(BLOCKED).await;
    let workers = async {
        first.await.expect("first worker");
        spare.await.expect("spare worker");
    };
    tokio::time::timeout(Duration::from_secs(3), workers)
        .await
        .expect("refresh workers did not exit after releasing the blocked refresh");
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_full_queue_does_not_block_scheduler() {
    let (h, _executor, test_loop) = new_refresh_loop();
    let mut now = h.now();
    let mut overflow_id = String::new();
    for i in 0..=JOB_BUFFER {
        let id = format!("full-queue-{i:02}");
        register_expired_auth(&h, &id, now);
        // Distinct times make the last credential the one that overflows.
        test_loop
            .shared
            .upsert(&id, now + TimeDelta::nanoseconds(i as i64));
        overflow_id = id;
    }
    now += TimeDelta::nanoseconds(JOB_BUFFER as i64);
    // handle_due never waits on the queue: it returns with no consumer.
    test_loop.handle_due(&h.manager, now);

    assert_eq!(test_loop.queued(), JOB_BUFFER, "queued jobs, want capacity");
    assert_eq!(test_loop.jobs.max_capacity(), JOB_BUFFER);
    assert!(
        scheduled(&test_loop, &overflow_id, now + TimeDelta::hours(1)),
        "overflow auth {overflow_id} lost its retry schedule"
    );
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_full_queue_retries_after_capacity_returns() {
    let (h, _executor, test_loop) = new_refresh_loop();
    let mut now = h.now();
    for i in 0..=JOB_BUFFER {
        let id = format!("capacity-{i:02}");
        register_expired_auth(&h, &id, now);
        test_loop
            .shared
            .upsert(&id, now + TimeDelta::nanoseconds(i as i64));
    }
    now += TimeDelta::nanoseconds(JOB_BUFFER as i64);
    test_loop.handle_due(&h.manager, now);
    test_loop.drain().await;

    now += PENDING_BACKOFF + TimeDelta::from_std(INTERVAL).expect("interval");
    test_loop.handle_due(&h.manager, now);
    assert_eq!(
        test_loop.queued(),
        1,
        "overflow retry jobs after capacity returned"
    );
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_restart_does_not_duplicate_running_job() {
    let (h, executor, old_loop) = new_refresh_loop();
    // Every refresh blocks until released, whether or not the loop stopped.
    let release = Duration::from_secs(1);
    executor.set_refresh_delay(release);

    let mut now = h.now();
    register_expired_auth(&h, "restart-running", now);
    old_loop.rebuild(&h.manager, now);
    old_loop.handle_due(&h.manager, now);
    let _worker = old_loop.worker(&h.manager);
    settle().await;
    assert_eq!(executor.refresh_count(), 1, "refresh did not start");
    old_loop.cancel();

    let new_loop = TestLoop::install(&h.manager);
    now += PENDING_BACKOFF * 2;
    new_loop.rebuild(&h.manager, now);
    new_loop.handle_due(&h.manager, now);
    assert_eq!(
        new_loop.queued(),
        0,
        "restarted loop queued duplicate jobs while the old refresh still ran"
    );

    tokio::time::advance(release).await;
    settle().await;
    new_loop.apply_dirty(&h.manager, now);
    new_loop.handle_due(&h.manager, now);
    assert_eq!(
        new_loop.queued(),
        1,
        "restarted loop jobs after old refresh completed"
    );
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_canceled_loop_releases_queued_job() {
    let (h, executor, old_loop) = new_refresh_loop();
    let mut now = h.now();
    register_expired_auth(&h, "restart-queued", now);
    old_loop.rebuild(&h.manager, now);
    old_loop.handle_due(&h.manager, now);
    old_loop.cancel();
    old_loop.run(&h.manager).await;
    settle().await;
    assert_eq!(
        executor.refresh_count(),
        0,
        "canceled loop executed queued refreshes"
    );

    let new_loop = TestLoop::install(&h.manager);
    now += TimeDelta::from_std(INTERVAL).expect("interval");
    new_loop.rebuild(&h.manager, now);
    new_loop.handle_due(&h.manager, now);
    assert_eq!(
        new_loop.queued(),
        1,
        "jobs after queued refresh cancellation and restart"
    );
}

#[tokio::test(start_paused = true)]
async fn issue6199_auto_refresh_queued_job_does_not_refresh_new_registration() {
    let (h, executor, test_loop) = new_refresh_loop();
    let now = h.now();
    register_expired_auth(&h, "replacement", now);
    test_loop.rebuild(&h.manager, now);
    test_loop.handle_due(&h.manager, now);
    h.manager.remove("replacement");
    register_expired_auth(&h, "replacement", now);
    let _worker = test_loop.worker(&h.manager);
    settle().await;
    assert_eq!(
        executor.refresh_count(),
        0,
        "stale queued job refreshed replacement registration"
    );

    test_loop.apply_dirty(&h.manager, now);
    test_loop.handle_due(&h.manager, now);
    settle().await;
    assert_eq!(
        executor.refresh_count(),
        1,
        "replacement registration refresh calls"
    );
}
