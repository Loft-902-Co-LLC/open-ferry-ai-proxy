use super::round4::{entered, pause_write, settled};
use super::*;

const SETTLE: Duration = Duration::from_secs(12);

#[tokio::test]
async fn round6_target_changed_keeps_its_cause_when_shutdown_overlaps() {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, START).await;
    let (pause, release) = pause_write(&api, true);
    let observed = Arc::new(tokio::sync::Notify::new());
    api.state.oauth_sessions().overrides().bind_cancel_observed = Some(observed.clone());
    deliver(&api, &session, "codex", issued());
    entered(&pause).await;
    let target =
        crate::oauth::mutation_path::BindMutationPath::capture(dir.path().join("codex-bind.json"));
    api.state
        .oauth_sessions()
        .store()
        .invalidate_bind_target(&target);
    tokio::time::timeout(SETTLE, observed.notified())
        .await
        .unwrap();
    let state = api.state.clone();
    let shutdown = state.shutdown();
    tokio::pin!(shutdown);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(api.state.oauth_sessions().is_shut_down());
    release.send(()).unwrap();
    tokio::time::timeout(SETTLE, shutdown).await.unwrap();
    settled(&api).await;
    assert_eq!(
        api.state
            .oauth_sessions()
            .store()
            .get(&session)
            .unwrap()
            .status,
        "SIWC binding target changed; start a new binding"
    );
    assert!(
        !logs
            .text()
            .contains("SIWC bind cancelled: server shutting down")
    );
    assert!(logs.text().contains("SIWC binding failed (Other, OS None)"));
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}

#[tokio::test]
async fn round6_unreserved_loop_retry_keeps_the_base_delay_after_growth() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, START).await;
    let guard = open_ferry_core::auth::credential_lock::acquire_credential_lock(
        &dir.path().join("codex-bind.json"),
    )
    .await
    .unwrap();
    let (before_file, release_file) = pause_write(&api, true);
    let (release_miss, miss_released) = std::sync::mpsc::channel();
    let miss_pause = Arc::new((
        tokio::sync::Notify::new(),
        std::sync::Mutex::new(miss_released),
    ));
    api.state.oauth_sessions().overrides().bind_after_file_miss = Some(miss_pause.clone());
    let records = Arc::new((
        tokio::sync::Notify::new(),
        std::sync::Mutex::new(Vec::new()),
    ));
    api.state.oauth_sessions().overrides().bind_retry_records = Some(records.clone());
    deliver(&api, &session, "codex", issued());
    // Grow the real loop's failure count through three reserved F misses.
    for _ in 0..2 {
        entered(&before_file).await;
        release_file.send(()).unwrap();
        entered(&miss_pause).await;
        release_miss.send(()).unwrap();
    }
    entered(&before_file).await;
    // Queue another credential while this attempt still owns R. After the
    // next F miss defers A, B is first regardless of wall-clock backoff expiry.
    let manager = api.manager.clone();
    let queued = Arc::new(tokio::sync::Notify::new());
    let reserved = Arc::new(tokio::sync::Notify::new());
    let (start, started) = std::sync::mpsc::channel();
    let (release_other, other_released) = std::sync::mpsc::channel();
    let queued_worker = queued.clone();
    let reserved_worker = reserved.clone();
    let other = std::thread::spawn(move || {
        let turn = manager.siwc_bind_turn("another-credential");
        queued_worker.notify_one();
        started.recv_timeout(Duration::from_secs(30)).unwrap();
        let reservation = turn.reserve_siwc_bind().expect("B is ahead of deferred A");
        reserved_worker.notify_one();
        other_released
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        drop(reservation);
    });
    tokio::time::timeout(SETTLE, queued.notified())
        .await
        .unwrap();
    release_file.send(()).unwrap();
    entered(&miss_pause).await;
    start.send(()).unwrap();
    tokio::time::timeout(SETTLE, reserved.notified())
        .await
        .unwrap();
    api.state.oauth_sessions().overrides().bind_before_file = None;
    api.state.oauth_sessions().overrides().bind_after_file_miss = None;
    release_miss.send(()).unwrap();
    let observed = tokio::time::timeout(SETTLE, async {
        loop {
            if let Some(record) = records.1.lock().unwrap().iter().find(|r| !r.0).copied() {
                break record;
            }
            records.0.notified().await;
        }
    })
    .await;
    api.state.oauth_sessions().store().cancel(&session);
    release_other.send(()).unwrap();
    other.join().unwrap();
    drop(guard);
    settled(&api).await;
    let records = records.1.lock().unwrap();
    assert_eq!(
        &records[..3],
        &[
            (true, 1, Duration::from_millis(20)),
            (true, 2, Duration::from_millis(40)),
            (true, 3, Duration::from_millis(80)),
        ]
    );
    assert_eq!(observed.unwrap(), (false, 3, Duration::from_millis(20)));
}
