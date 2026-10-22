use super::*;

#[tokio::test]
async fn round3_slow_holder_retries_have_growing_gaps() {
    let (dir, pause) = admission::fixture();
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, START).await;
    let active = admission::shared_holder(&api, &pause).await;
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    api.state.oauth_sessions().overrides().bind_admission_times = Some(times.clone());
    deliver(&api, &session, "codex", issued());
    let observed = tokio::time::timeout(Duration::from_secs(3), async {
        while times.lock().unwrap().len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    api.state.oauth_sessions().store().cancel(&session);
    wait_for_bind_task(&api).await;
    pause.resume();
    tokio::task::spawn_blocking(move || active.join().unwrap())
        .await
        .unwrap();
    observed.unwrap();
    let times = times.lock().unwrap();
    assert!(
        times[3].duration_since(times[2]) >= Duration::from_millis(170),
        "drain timeouts retained the fixed 20-40 ms retry interval"
    );
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}

#[tokio::test]
async fn round3_shutdown_during_write_reports_the_durable_outcome() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let (release, released) = std::sync::mpsc::channel();
    let pause = Arc::new((tokio::sync::Notify::new(), std::sync::Mutex::new(released)));
    api.state.oauth_sessions().overrides().bind_before_write = Some(pause.clone());
    let state = api.state.clone();
    let saving = tokio::spawn(async move {
        crate::oauth::siwc_bind::save_for_test(&state, "round3-write").await
    });
    tokio::time::timeout(Duration::from_secs(2), pause.0.notified())
        .await
        .unwrap();
    let state = api.state.clone();
    let shutdown = tokio::spawn(async move { state.shutdown().await });
    tokio::time::sleep(Duration::from_millis(60)).await;
    release.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(12), saving)
        .await
        .unwrap()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    while dir.read_json("codex-bind.json").get("use_siwc") != Some(&json!(true)) {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        result.is_ok(),
        "durable commit was reported as failed at shutdown"
    );
    assert_eq!(api.sync.calls().len(), 1);
    shutdown.await.unwrap();
}
