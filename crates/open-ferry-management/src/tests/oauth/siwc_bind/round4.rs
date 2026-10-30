use super::*;

// Setup/cleanup may include the ten-second shutdown grace. These waits do not
// measure signal withdrawal or the drain deadline.
const SETTLE: Duration = Duration::from_secs(12);

pub(super) fn pause_write(
    api: &Api,
    before_file: bool,
) -> (crate::oauth::BindWritePause, std::sync::mpsc::Sender<()>) {
    let (release, released) = std::sync::mpsc::channel();
    let pause = Arc::new((tokio::sync::Notify::new(), std::sync::Mutex::new(released)));
    if before_file {
        api.state.oauth_sessions().overrides().bind_before_file = Some(pause.clone());
    } else {
        api.state.oauth_sessions().overrides().bind_before_write = Some(pause.clone());
    }
    (pause, release)
}

pub(super) async fn entered(pause: &crate::oauth::BindWritePause) {
    tokio::time::timeout(SETTLE, pause.0.notified())
        .await
        .unwrap();
}

pub(super) async fn settled(api: &Api) {
    tokio::time::timeout(SETTLE, async {
        while api.state.oauth_sessions().bind_workers.counts() != (0, 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn real_shutdown(stopped: bool) {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let sync_pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    if !stopped {
        *api.sync.completed_pause.lock().unwrap() = Some(sync_pause.clone());
    }
    let session = start_login(&api, START).await;
    let (pause, release) = pause_write(&api, false);
    deliver(&api, &session, "codex", issued());
    entered(&pause).await;
    if stopped {
        api.sync.stop();
    }
    let state = api.state.clone();
    let shutdown = tokio::spawn(async move { state.shutdown().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let waited = !shutdown.is_finished();
    release.send(()).unwrap();
    let mut waited_for_sync = true;
    if !stopped {
        tokio::time::timeout(SETTLE, sync_pause.0.notified())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        waited_for_sync = !shutdown.is_finished();
        sync_pause.1.notify_one();
    }
    tokio::time::timeout(SETTLE, shutdown)
        .await
        .unwrap()
        .unwrap();
    let at_return = api.state.oauth_sessions().store().get(&session);
    let counts_at_return = api.state.oauth_sessions().bind_workers.counts();
    settled(&api).await;
    assert!(waited, "shutdown returned with a live post-fence worker");
    assert!(
        waited_for_sync,
        "shutdown waited for the worker but aborted before synchronization"
    );
    assert_eq!(dir.read_json("codex-bind.json")["use_siwc"], true);
    let outcome = at_return.expect("durable session retained before shutdown returned");
    assert!(
        outcome.completed,
        "durable bind became save failure: {}",
        outcome.status
    );
    assert!(outcome.status.is_empty());
    assert_eq!(api.sync.completed_commands.load(Ordering::SeqCst), 1);
    assert_eq!(
        api.sync.adopted_commands.load(Ordering::SeqCst),
        usize::from(!stopped)
    );
    assert_eq!(api.sync.calls().len(), usize::from(!stopped));
    assert_eq!(api.state.oauth_sessions().bind_workers.counts(), (0, 0));
    assert_eq!(counts_at_return, (0, 0));
    if stopped {
        assert!(
            logs.text()
                .contains("SIWC binding saved, runtime sync unavailable")
        );
        assert!(!logs.text().contains("Failed to save authentication tokens"));
    } else {
        assert_eq!(api.manager.list()[0].metadata["use_siwc"], true);
    }
}

#[tokio::test]
async fn round4_shutdown_refuses_late_worker_admission() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let starts = Arc::new(AtomicUsize::new(0));
    api.state.oauth_sessions().overrides().bind_worker_starts = Some(starts.clone());
    api.state.shutdown().await;
    assert!(
        crate::oauth::siwc_bind::save_for_test(&api.state, "late-worker")
            .await
            .is_err()
    );
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(api.state.oauth_sessions().bind_workers.counts(), (0, 0));
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}

#[tokio::test]
async fn round4_shutdown_preserves_real_login_with_stopped_sync() {
    real_shutdown(true).await;
}

#[tokio::test]
// Rule test: services FakeSync during shutdown. Production closes runtime sync
// first; round5's closed-sync collector test covers that ordering.
async fn round4_shutdown_waits_for_worker_and_session_sync() {
    real_shutdown(false).await;
}

#[tokio::test]
async fn round4_sync_closed_after_fence_reports_saved() {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, START).await;
    let (pause, release) = pause_write(&api, false);
    deliver(&api, &session, "codex", issued());
    entered(&pause).await;
    api.sync.stop();
    release.send(()).unwrap();
    wait_for_bind_task(&api).await;
    let outcome = api.state.oauth_sessions().store().get(&session).unwrap();
    assert!(
        outcome.completed,
        "a stopped receiver converted a durable write to SAVE_FAILED"
    );
    assert_eq!(dir.read_json("codex-bind.json")["use_siwc"], true);
    assert_eq!(api.sync.completed_commands.load(Ordering::SeqCst), 1);
    assert_eq!(api.sync.adopted_commands.load(Ordering::SeqCst), 0);
    assert!(
        logs.text()
            .contains("SIWC binding saved, runtime sync unavailable")
    );
}

#[tokio::test]
async fn round4_shutdown_bound_warns_and_keeps_committed_collector() {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().overrides().bind_shutdown_grace = Some(Duration::from_millis(80));
    let session = start_login(&api, START).await;
    let (pause, release) = pause_write(&api, false);
    deliver(&api, &session, "codex", issued());
    entered(&pause).await;
    api.sync.stop();
    tokio::time::timeout(SETTLE, api.state.shutdown())
        .await
        .unwrap();
    let live_at_return = api.state.oauth_sessions().bind_workers.counts();
    let warned = logs.text().contains("SIWC bind shutdown grace expired");
    release.send(()).unwrap();
    settled(&api).await;
    assert_eq!(dir.read_json("codex-bind.json")["use_siwc"], true);
    let outcome = api
        .state
        .oauth_sessions()
        .store()
        .get(&session)
        .expect("committed collector retained its session");
    assert!(
        outcome.completed,
        "post-grace durable bind was not finalized"
    );
    assert_eq!(live_at_return, (1, 1));
    assert!(warned);
    assert_eq!(api.sync.completed_commands.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn round4_worker_cap_survives_cancelled_session_churn() {
    let dir = AuthDir::new();
    for name in ["codex-bind.json", "second.json"] {
        dir.write(name, OUTER);
    }
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().bind_workers.limit_for_test(1);
    let starts = Arc::new(AtomicUsize::new(0));
    api.state.oauth_sessions().overrides().bind_worker_starts = Some(starts.clone());
    let first = start_login(&api, START).await;
    let second_route = START.replace("codex-bind.json", "second.json");
    let (pause, release) = pause_write(&api, true);
    deliver(&api, &first, "codex", issued());
    entered(&pause).await;
    assert!(api.state.oauth_sessions().store().cancel(&first));
    api.state.oauth_sessions().overrides().bind_before_file = None;
    let second = start_login(&api, &second_route).await;
    deliver(&api, &second, "codex", issued());
    let refused = tokio::time::timeout(SETTLE, async {
        loop {
            if api
                .state
                .oauth_sessions()
                .store()
                .get(&second)
                .is_some_and(|s| !s.status.is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let spawned_while_paused = starts.load(Ordering::SeqCst);
    let outcome = api.state.oauth_sessions().store().get(&second).unwrap();
    release.send(()).unwrap();
    wait_for_bind_task(&api).await;
    settled(&api).await;
    assert_eq!(
        spawned_while_paused, 1,
        "a cancelled session freed a live thread's permit"
    );
    refused.unwrap();
    assert_eq!(
        outcome.status,
        "SIWC bind workers are busy; start a new binding"
    );
    assert_eq!(api.state.oauth_sessions().bind_workers.counts(), (0, 0));
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
    let third = start_login(&api, &second_route).await;
    deliver(&api, &third, "codex", issued());
    completed(&api, &third).await;
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(dir.read_json("second.json")["use_siwc"], true);
}

#[tokio::test]
async fn round4_busy_file_allows_another_credential_to_bind() {
    let dir = AuthDir::new();
    for name in ["codex-bind.json", "second.json"] {
        dir.write(name, OUTER);
    }
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let starts = Arc::new(AtomicUsize::new(0));
    api.state.oauth_sessions().overrides().bind_worker_starts = Some(starts.clone());
    let first = start_login(&api, START).await;
    let second = start_login(&api, &START.replace("codex-bind.json", "second.json")).await;
    let guard = open_ferry_core::auth::credential_lock::acquire_credential_lock(
        &dir.path().join("codex-bind.json"),
    )
    .await
    .unwrap();
    let missed = Arc::new(tokio::sync::Notify::new());
    api.state.oauth_sessions().overrides().bind_lock_wait = Some(missed.clone());
    // Queue B while A owns its first reservation, before A's F miss. Once
    // A defers, B stays ahead even if this process is descheduled past backoff.
    let (before_file, release_file) = pause_write(&api, true);
    deliver(&api, &first, "codex", issued());
    entered(&before_file).await;
    let queued = Arc::new(tokio::sync::Notify::new());
    api.state.oauth_sessions().overrides().bind_turn_started = Some(queued.clone());
    deliver(&api, &second, "codex", issued());
    tokio::time::timeout(SETTLE, queued.notified())
        .await
        .unwrap();
    api.state.oauth_sessions().overrides().bind_before_file = None;
    // Keep A retrying while its file stays locked. Parking A after its miss
    // can strand B behind it if B also defers a transient lock-registry miss.
    // Observe B's write admission directly, before runtime synchronization;
    // A's held file lock makes B the only worker that can reach this fence.
    let (other_write, release_other) = pause_write(&api, false);
    release_file.send(()).unwrap();
    tokio::time::timeout(SETTLE, missed.notified())
        .await
        .unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    let other_admitted = tokio::time::timeout(SETTLE, other_write.0.notified())
        .await
        .is_ok();
    api.state.oauth_sessions().overrides().bind_before_write = None;
    // Release the write pause and held file lock before awaiting completion,
    // including on timeout, so an assertion failure cannot strand a worker.
    let _ = release_other.send(());
    drop(guard);
    completed(&api, &first).await;
    completed(&api, &second).await;
    assert!(
        other_admitted,
        "a busy credential retained the global FIFO head"
    );
}

#[tokio::test]
async fn round4_failed_file_tries_back_off_to_the_ceiling() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, START).await;
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    api.state.oauth_sessions().overrides().bind_admission_times = Some(times.clone());
    let guard = open_ferry_core::auth::credential_lock::acquire_credential_lock(
        &dir.path().join("codex-bind.json"),
    )
    .await
    .unwrap();
    deliver(&api, &session, "codex", issued());
    let observed = tokio::time::timeout(Duration::from_secs(30), async {
        while times.lock().unwrap().len() < 9 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    api.state.oauth_sessions().store().cancel(&session);
    drop(guard);
    wait_for_bind_task(&api).await;
    observed.unwrap();
    let times = times.lock().unwrap();
    for (pair, minimum) in times
        .windows(2)
        .zip([20, 40, 80, 160, 320, 640, 1000, 1000])
    {
        assert!(
            pair[1].duration_since(pair[0]) >= Duration::from_millis(minimum),
            "failed post-drain try reset backoff"
        );
    }
}
