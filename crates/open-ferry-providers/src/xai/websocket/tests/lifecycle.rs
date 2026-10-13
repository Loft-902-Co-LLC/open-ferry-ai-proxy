// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_executor_store_test.go
// (xAI shared-store and auth-removal cases) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use open_ferry_core::config::Config;

#[tokio::test]
async fn reload_keeps_xai_sessions_and_removal_closes_only_one_credential() {
    let server = Server::turns(&[COMPLETED]).await;
    let config = Arc::new(Config::parse("proxy-url: direct\n").unwrap());
    let previous = executor().with_config(config.clone());
    let a = auth_as("xai-a", API_KEY, &server.url);
    let b = auth_as("xai-b", API_KEY, &server.url);
    streamed(&previous, &a, HELLO, ws_options("a")).await;
    streamed(&previous, &b, HELLO, ws_options("b")).await;
    let next_config = Arc::new(Config::parse(format!("proxy-url: {}\n", server.url)).unwrap());
    let next = XaiExecutor::new(server.url.clone())
        .with_config(next_config.clone())
        .with_execution_sessions_from(&previous);
    assert!(previous.shares_execution_sessions_with(&next));
    streamed(&next, &b, HELLO, ws_options("b")).await;
    assert_eq!(server.record().handshakes.len(), 2);
    let session = next.websockets.store.get("b").unwrap();
    assert!(Arc::ptr_eq(
        &session.effective_config(&b.id, Some(next_config)).unwrap(),
        &config
    ));
    let state_a = next.websockets.ids.get("a").unwrap();
    let state_b = next.websockets.ids.get("b").unwrap();
    next.close_auth_execution_sessions(&a.id);
    server
        .wait_for("credential a to close", |record| record.client_closed == 1)
        .await;
    assert!(!Arc::ptr_eq(
        &state_a,
        &next.websockets.ids.get("a").unwrap()
    ));
    assert!(Arc::ptr_eq(
        &state_b,
        &next.websockets.ids.get("b").unwrap()
    ));
    streamed(&next, &b, HELLO, ws_options("b")).await;
    assert_eq!(server.record().handshakes.len(), 2);
    next.close_execution_session("b");
}

#[tokio::test]
async fn removal_during_a_pending_xai_handshake_sends_no_request() {
    let (release, gate) = watch::channel(false);
    let server = Server::start(move |_| {
        Answer::Held(
            gate.clone(),
            Box::new(Answer::accept(|peer| async move {
                peer.hold().await;
            })),
        )
    })
    .await;
    let executor = Arc::new(executor());
    let credential = auth(&server.url);
    let task = tokio::spawn({
        let executor = executor.clone();
        let credential = credential.clone();
        async move {
            executor
                .execute_stream(credential, request(HELLO), ws_options("pending"))
                .await
        }
    });
    server
        .wait_for("pending handshake", |r| r.handshakes.len() == 1)
        .await;
    executor.close_auth_execution_sessions(&credential.id);
    release.send_replace(true);
    assert!(
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server.wait_closed(1).await;
    assert!(server.record().messages.is_empty());
    assert!(executor.websockets.store.get("pending").is_none());
}

#[tokio::test]
async fn clearing_an_xai_credential_proxy_redials_and_forgets_the_previous_response() {
    let server = Server::turns(&[COMPLETED]).await;
    let proxy = crate::codex::websocket::mock::Proxy::start().await;
    let previous = XaiExecutor::new(proxy.url.clone());
    let mut credential = (*auth(&server.url)).clone();
    credential.proxy_url = "direct".into();
    let first = streamed(
        &previous,
        &Arc::new(credential.clone()),
        HELLO,
        ws_options("proxy-clear"),
    )
    .await;
    let followup = |id: &str| {
        json!({"model":"grok-4.3", "previous_response_id": id,
        "input":[{"type":"message", "role":"user", "content":"next"}]})
        .to_string()
    };
    let body = followup(
        last_event(&first, "response.completed")["response"]["id"]
            .as_str()
            .unwrap(),
    );
    // Establish that the retained target normally sends the upstream ID.
    let second = streamed(
        &previous,
        &Arc::new(credential.clone()),
        &body,
        ws_options("proxy-clear"),
    )
    .await;
    assert_eq!(
        json(&server.record().messages[1])["previous_response_id"],
        "resp-xai-1"
    );
    assert!(proxy.connects().is_empty());
    let next = executor().with_execution_sessions_from(&previous);
    credential.proxy_url.clear();
    let body = followup(
        last_event(&second, "response.completed")["response"]["id"]
            .as_str()
            .unwrap(),
    );
    streamed(
        &next,
        &Arc::new(credential),
        &body,
        ws_options("proxy-clear"),
    )
    .await;
    assert_eq!(server.record().handshakes.len(), 2);
    assert_eq!(proxy.connects().len(), 1);
    assert!(
        json(&server.record().messages[2])
            .get("previous_response_id")
            .is_none()
    );
    next.close_execution_session("proxy-clear");
}
