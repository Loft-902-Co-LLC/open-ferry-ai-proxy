// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_interrupt_test.go,
// codex_websockets_late_interrupt_test.go, codex_websockets_executor_store_test.go
// and codex_stream_bootstrap_buffering_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

const INTERRUPT: &str = r#"{ "type":"response.interrupt", "response_id":"resp-1", "mode":"immediate", "extension":{"keep":true} }"#;

#[tokio::test]
async fn interrupt_forwards_the_original_payload_and_late_interrupt_is_handled() {
    let server = Server::start(|_| Answer::accept(|mut peer| async move {
        peer.recv().await.unwrap();
        peer.send(r#"{"type":"response.created","response":{"id":"resp-1"}}"#).await;
        assert_eq!(peer.recv().await.as_deref(), Some(INTERRUPT));
        peer.send(r#"{"type":"response.incomplete","response":{"id":"resp-1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[]}}"#).await;
        let next = peer.recv().await.unwrap();
        assert_eq!(json(&next)["type"], "response.create");
        peer.send(r#"{"type":"response.created","response":{"id":"resp-2"}}"#).await;
        // No late interrupt may reach this retained connection.
        assert!(peer.recv().await.is_none());
    })).await;
    let executor = executor();
    let mut response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("interrupt"),
        )
        .await
        .unwrap();
    assert!(response.chunks.next().await.unwrap().is_ok());
    assert!(
        executor
            .interrupt_execution_session("interrupt", INTERRUPT.as_bytes(), Arc::new(|_| true))
            .await
            .unwrap()
    );
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        chunks
            .iter()
            .any(|chunk| json(chunk)["type"] == "response.incomplete")
    );
    assert!(
        executor
            .interrupt_execution_session("interrupt", INTERRUPT.as_bytes(), Arc::new(|_| false))
            .await
            .unwrap()
    );
    assert!(
        !executor
            .interrupt_execution_session(
                "interrupt",
                br#"{"type":"response.interrupt","response_id":"other"}"#,
                Arc::new(|_| true)
            )
            .await
            .unwrap()
    );
    let mut next = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("interrupt"),
        )
        .await
        .unwrap();
    assert!(next.chunks.next().await.unwrap().is_ok());
    assert!(
        executor
            .interrupt_execution_session("interrupt", INTERRUPT.as_bytes(), Arc::new(|_| true))
            .await
            .unwrap()
    );
    drop(next);
    assert_eq!(server.record().messages.len(), 3);
}

#[tokio::test]
async fn interrupt_rejects_a_disabled_credential_without_writing() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            peer.recv().await.unwrap();
            peer.send(DELTA).await;
            peer.hold().await;
        })
    })
    .await;
    let executor = executor();
    let mut response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("disabled"),
        )
        .await
        .unwrap();
    response.chunks.next().await.unwrap().unwrap();
    let error = executor
        .interrupt_execution_session("disabled", INTERRUPT.as_bytes(), Arc::new(|_| false))
        .await
        .unwrap_err();
    assert_eq!(error.message, "websocket credential is no longer enabled");
    assert_eq!(server.record().messages.len(), 1);
    drop(response);
}

#[tokio::test]
async fn reload_shares_sessions_and_removal_closes_only_the_removed_credential() {
    let server = Server::turns(&[COMPLETED]).await;
    let config = Arc::new(Config::parse("proxy-url: direct\n").unwrap());
    let previous = executor().with_config(config.clone());
    let a = Arc::new(auth(&server.url));
    let mut b = (*a).clone();
    b.id = "other-codex".into();
    let b = Arc::new(b);
    for (auth, session) in [(&a, "a"), (&b, "b")] {
        let response = previous
            .execute_stream(
                auth.clone(),
                request("gpt-5-codex", HELLO),
                ws_options(session),
            )
            .await
            .unwrap();
        assert!(collect(response).await.1.is_none());
    }
    let next_config = Arc::new(Config::parse(format!("proxy-url: {}\n", server.url)).unwrap());
    let next = CodexExecutor::new(server.url.clone())
        .with_config(next_config.clone())
        .with_execution_sessions_from(&previous);
    assert!(previous.shares_execution_sessions_with(&next));
    let response = next
        .execute_stream(b.clone(), request("gpt-5-codex", HELLO), ws_options("b"))
        .await
        .unwrap();
    assert!(collect(response).await.1.is_none());
    assert_eq!(server.record().handshakes.len(), 2);
    let session = next.websockets().get("b").unwrap();
    assert!(Arc::ptr_eq(
        &session.effective_config(&b.id, Some(next_config)).unwrap(),
        &config
    ));
    next.close_auth_execution_sessions(&a.id);
    server
        .wait_for("only credential a to close", |r| r.client_closed == 1)
        .await;
    assert_eq!(next.websockets().len(), 1);
    let response = next
        .execute_stream(b, request("gpt-5-codex", HELLO), ws_options("b"))
        .await
        .unwrap();
    assert!(collect(response).await.1.is_none());
    assert_eq!(server.record().handshakes.len(), 2);
    next.close_execution_session("b");
}

#[tokio::test]
async fn ready_usage_limit_error_reaches_the_stream_after_upstream_disconnect() {
    for frame in [
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":600}}"#,
        r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","status_code":429,"message":"limit","resets_in_seconds":600}}}"#,
    ] {
        let (send_error, gate) = watch::channel(false);
        let server = Server::start(move |_| {
            let gate = gate.clone();
            Answer::accept(move |mut peer| {
                let mut gate = gate.clone();
                async move {
                    peer.recv().await.unwrap();
                    gate.wait_for(|ready| *ready).await.unwrap();
                    peer.send(frame).await;
                }
            })
        })
        .await;
        let executor = executor();
        let mut response = executor
            .execute_stream(
                Arc::new(auth(&server.url)),
                request("gpt-5-codex", HELLO),
                ws_options("ready-error"),
            )
            .await
            .unwrap();
        let conn = executor
            .websockets()
            .get("ready-error")
            .unwrap()
            .conn()
            .unwrap();
        send_error.send_replace(true);
        within("the upstream disconnect", async {
            while !conn.is_closed() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        // Cancellation is ready too: the already-queued upstream error wins.
        let error = tokio::select! {
            biased;
            chunk = response.chunks.next() => chunk.unwrap().unwrap_err(),
            _ = std::future::ready(()) => panic!("ready error lost to cancellation"),
        };
        assert_eq!(error.status, 429);
        assert!(error.retry_after.is_some(), "{error:?}");
        executor.close_auth_execution_sessions("codex-test");
        assert_eq!(executor.websockets().len(), 0);
    }
}
