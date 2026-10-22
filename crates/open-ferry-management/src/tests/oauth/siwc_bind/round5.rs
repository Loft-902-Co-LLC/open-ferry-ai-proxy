use super::*;

const SETTLE: Duration = Duration::from_secs(12);

async fn idle(api: &Api) {
    tokio::time::timeout(SETTLE, async {
        while api.state.oauth_sessions().bind_workers.counts() != (0, 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn paused_collector(api: &Api) -> (String, Arc<(tokio::sync::Notify, tokio::sync::Notify)>) {
    api.sync.stop();
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    api.state.oauth_sessions().overrides().bind_before_complete = Some(pause.clone());
    let session = start_login(api, START).await;
    deliver(api, &session, "codex", issued());
    tokio::time::timeout(SETTLE, pause.0.notified())
        .await
        .unwrap();
    tokio::time::timeout(SETTLE, async {
        while api.state.oauth_sessions().bind_workers.counts().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(api.state.oauth_sessions().bind_workers.counts(), (0, 1));
    (session, pause)
}

#[tokio::test]
async fn round5_closed_sync_shutdown_waits_for_collector_finalization() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let (session, pause) = paused_collector(&api).await;
    // This runtime is current-thread. One explicit poll reaches idle before
    // any owner-abort/join suspension can disguise a premature idle return.
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    api.state.oauth_sessions().overrides().bind_shutdown_events = Some(events.clone());
    let shutdown = api.state.shutdown();
    tokio::pin!(shutdown);
    let polled =
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx))).await;
    assert!(polled.is_pending());
    let before_release = events.lock().unwrap().clone();
    pause.1.notify_one();
    tokio::time::timeout(SETTLE, shutdown).await.unwrap();
    idle(&api).await;
    assert!(
        before_release.is_empty(),
        "shutdown passed idle with the collector paused"
    );
    assert_eq!(
        *events.lock().unwrap(),
        ["collector finalized", "shutdown passed idle"]
    );
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .get(&session)
            .unwrap()
            .completed
    );
}

#[tokio::test]
async fn round5_collector_only_grace_warning_names_finalization() {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().overrides().bind_shutdown_grace = Some(Duration::from_millis(80));
    let (_, pause) = paused_collector(&api).await;
    tokio::time::timeout(SETTLE, api.state.shutdown())
        .await
        .unwrap();
    let text = logs.text();
    pause.1.notify_one();
    idle(&api).await;
    assert!(text.contains("sessions are still finalizing"));
    assert!(!text.contains("workers may still be writing"));
}

async fn ordinary_closed_sync(with_grace: bool) {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::start(|_, request| {
        if request.contains("/api/accounts/oauth/token") {
            tokens()
        } else {
            http_response(
                "200 OK",
                &[("Content-Type", "application/json")],
                CLAUDE_TOKENS.as_bytes(),
            )
        }
    })
    .await;
    let api = login_api(&dir, &upstream.url);
    let ordinary = start_login(&api, CLAUDE_AUTH_URL).await;
    api.sync.stop();
    let held = if with_grace {
        Some(paused_collector(&api).await)
    } else {
        None
    };
    let shutdown = if with_grace {
        let state = api.state.clone();
        let shutdown = tokio::spawn(async move { state.shutdown().await });
        tokio::time::timeout(SETTLE, async {
            while !api.state.oauth_sessions().is_shut_down() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Some(shutdown)
    } else {
        None
    };
    deliver(&api, &ordinary, "anthropic", code("round5-ordinary-test"));
    let result = settled(&api, &ordinary).await.unwrap();
    let during_grace = shutdown
        .as_ref()
        .is_none_or(|shutdown| !shutdown.is_finished());
    if let Some((_, pause)) = held {
        pause.1.notify_one();
    }
    if let Some(shutdown) = shutdown {
        tokio::time::timeout(SETTLE, shutdown)
            .await
            .unwrap()
            .unwrap();
    }
    idle(&api).await;
    assert!(
        result.completed,
        "durable ordinary save misreported: {}",
        result.status
    );
    assert!(during_grace);
    let name = saved_file(&dir, "claude-");
    let saved = dir.read_json(&name);
    assert_eq!(saved["access_token"], "access-claude");
    let text = logs.text();
    let masked = open_ferry_core::observe::mask::mask_emails(&name);
    assert!(
        text.contains(&format!("authentication saved as {masked}")),
        "masked saved filename missing: {text}"
    );
    assert!(!text.contains("claude-user@example.test"));
    assert!(text.contains("will load on restart"));
    assert!(!text.contains("Failed to save authentication tokens"));
    assert!(!logs.levels().contains(&tracing::Level::ERROR));
}

#[tokio::test]
async fn round5_ordinary_login_with_closed_sync_is_saved() {
    ordinary_closed_sync(false).await;
}

#[tokio::test]
async fn round5_ordinary_login_finishes_during_bind_shutdown_grace() {
    ordinary_closed_sync(true).await;
}

#[tokio::test]
async fn round5_shutdown_cancels_pre_fence_bind_at_info() {
    let (logs, _capture) = Logs::capture();
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
    let waiting = Arc::new(tokio::sync::Notify::new());
    api.state.oauth_sessions().overrides().bind_lock_wait = Some(waiting.clone());
    deliver(&api, &session, "codex", issued());
    tokio::time::timeout(SETTLE, waiting.notified())
        .await
        .unwrap();
    tokio::time::timeout(SETTLE, api.state.shutdown())
        .await
        .unwrap();
    drop(guard);
    let result = api.state.oauth_sessions().store().get(&session).unwrap();
    assert!(!result.completed);
    assert_eq!(result.status, "server shutting down");
    assert!(!logs.levels().contains(&tracing::Level::ERROR));
    assert!(
        logs.text()
            .contains("SIWC bind cancelled: server shutting down")
    );
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}

#[tokio::test]
async fn round5_busy_advice_refuses_before_exchange() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().bind_workers.limit_for_test(0);
    let session = start_login(&api, START).await;
    deliver(&api, &session, "codex", issued());
    assert_eq!(
        failure(&api, &session).await,
        "SIWC bind workers are busy; start a new binding"
    );
    assert_eq!(
        upstream.requests().len(),
        0,
        "capacity refusal consumed a single-use authorization"
    );
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}

#[tokio::test]
async fn round5_worker_would_block_is_a_save_failure() {
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().overrides().bind_worker_error = Some(std::io::ErrorKind::WouldBlock);
    let session = start_login(&api, START).await;
    deliver(&api, &session, "codex", issued());
    assert_eq!(
        failure(&api, &session).await,
        "Failed to save authentication tokens"
    );
    assert_eq!(upstream.requests().len(), 1);
    assert!(logs.levels().contains(&tracing::Level::ERROR));
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}
