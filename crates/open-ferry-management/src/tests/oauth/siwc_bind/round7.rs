use super::round4::{entered, pause_write, settled};
use super::*;

const SETTLE: Duration = Duration::from_secs(12);

#[tokio::test(flavor = "current_thread")]
async fn round7_ended_prioritizes_target_changed_when_both_causes_are_ready() {
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
    // No yield between invalidation and the first shutdown poll: the owner
    // must next observe both an ended session and the shutdown cutoff.
    api.state
        .oauth_sessions()
        .store()
        .invalidate_bind_target(&target);
    let state = api.state.clone();
    let shutdown = state.shutdown();
    tokio::pin!(shutdown);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(api.state.oauth_sessions().is_shut_down());
    tokio::time::timeout(SETTLE, observed.notified())
        .await
        .unwrap();
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
