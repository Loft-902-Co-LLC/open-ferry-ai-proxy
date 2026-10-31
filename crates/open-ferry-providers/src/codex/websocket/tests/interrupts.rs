// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_interrupt_test.go,
// codex_websockets_late_interrupt_test.go, codex_websockets_executor_store_test.go
// and codex_stream_bootstrap_buffering_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

const INTERRUPT: &str = r#"{ "type":"response.interrupt", "response_id":"resp-1", "mode":"immediate", "extension":{"keep":true} }"#;

#[tokio::test]
async fn interrupt_forwards_the_original_payload_and_late_interrupt_is_handled() {
    let (tasks, mut task_rx) = mpsc::channel(1);
    let server = Server::start(move |_| {
        let tasks = tasks.clone();
        Answer::accept(move |mut peer| {
            let tasks = tasks.clone();
            async move {
                let task = tokio::spawn(async move {
                    peer.recv().await.unwrap();
                    peer.send(r#"{"type":"response.created","response":{"id":"resp-1"}}"#).await;
                    assert_eq!(peer.recv().await.as_deref(), Some(INTERRUPT));
                    peer.send(r#"{"type":"response.incomplete","response":{"id":"resp-1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[]}}"#).await;
                    let next = peer.recv().await.unwrap();
                    assert_eq!(json(&next)["type"], "response.create");
                    peer.send(r#"{"type":"response.created","response":{"id":"resp-2"}}"#).await;
                    // No late interrupt may reach this retained connection.
                    assert!(peer.recv().await.is_none());
                });
                tasks.send(task).await.unwrap();
            }
        })
    }).await;
    let executor = executor();
    let mut response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("interrupt"),
        )
        .await
        .unwrap();
    let server_task = within("the server task", task_rx.recv()).await.unwrap();
    assert!(response.chunks.next().await.unwrap().is_ok());
    assert!(
        executor
            .interrupt_execution_session(
                "interrupt",
                INTERRUPT.as_bytes(),
                Arc::new(|_| Some(Arc::new(Auth::default())))
            )
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
            .interrupt_execution_session("interrupt", INTERRUPT.as_bytes(), Arc::new(|_| None))
            .await
            .unwrap()
    );
    assert!(
        !executor
            .interrupt_execution_session(
                "interrupt",
                br#"{"type":"response.interrupt","response_id":"other"}"#,
                Arc::new(|_| Some(Arc::new(Auth::default())))
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
            .interrupt_execution_session(
                "interrupt",
                INTERRUPT.as_bytes(),
                Arc::new(|_| Some(Arc::new(Auth::default())))
            )
            .await
            .unwrap()
    );
    drop(next);
    within("the server assertions", server_task).await.unwrap();
    server
        .wait_for("three recorded messages", |r| r.messages.len() == 3)
        .await;
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
        .interrupt_execution_session("disabled", INTERRUPT.as_bytes(), Arc::new(|_| None))
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
    assert_eq!(session.global_proxy().as_deref(), Some("direct"));
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
async fn provider_stream_yields_the_queued_usage_limit_error_after_disconnect() {
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
        within("the upstream disconnect", conn.wait_closed()).await;
        // This checks provider delivery only. Cancellation priority and
        // credential cooldown must be tested through the manager.
        let error = response.chunks.next().await.unwrap().unwrap_err();
        assert_eq!(error.status, 429);
        assert!(error.retry_after.is_some(), "{error:?}");
        executor.close_auth_execution_sessions("codex-test");
        assert_eq!(executor.websockets().len(), 0);
    }
}

#[tokio::test]
async fn removal_during_a_pending_codex_handshake_sends_no_request() {
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
    let credential = Arc::new(auth(&server.url));
    let task = tokio::spawn({
        let executor = executor.clone();
        let credential = credential.clone();
        async move {
            executor
                .execute_stream(
                    credential,
                    request("gpt-5-codex", HELLO),
                    ws_options("pending"),
                )
                .await
        }
    });
    server
        .wait_for("pending handshake", |r| r.handshakes.len() == 1)
        .await;
    executor.close_auth_execution_sessions(&credential.id);
    release.send_replace(true);
    assert!(within("the removed dial", task).await.unwrap().is_err());
    server.wait_closed(1).await;
    assert!(server.record().messages.is_empty());
    assert_eq!(executor.websockets().len(), 0);
}

#[tokio::test]
async fn interrupt_revalidates_a_credential_after_waiting_for_the_send_lock() {
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
            ws_options("queued-interrupt"),
        )
        .await
        .unwrap();
    response.chunks.next().await.unwrap().unwrap();
    let conn = executor
        .websockets()
        .get("queued-interrupt")
        .unwrap()
        .conn()
        .unwrap();
    let permit = conn.lock_send().await;
    let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let check: open_ferry_core::exec::AuthCheck = {
        let enabled = enabled.clone();
        Arc::new(move |_| {
            enabled
                .load(Ordering::SeqCst)
                .then(|| Arc::new(Auth::default()))
        })
    };
    let interrupt =
        executor.interrupt_execution_session("queued-interrupt", INTERRUPT.as_bytes(), check);
    tokio::pin!(interrupt);
    // Poll to the held permit, deterministically, before disabling.
    assert!(futures_util::poll!(&mut interrupt).is_pending());
    enabled.store(false, Ordering::SeqCst);
    drop(permit);
    let error = interrupt.await.unwrap_err();
    assert_eq!(error.message, "websocket credential is no longer enabled");
    drop(response);
    server.wait_closed(1).await;
    assert_eq!(server.record().messages.len(), 1);
}

#[tokio::test]
async fn clearing_a_codex_credential_proxy_redials_through_the_current_global_proxy() {
    let server = Server::turns(&[COMPLETED]).await;
    let proxy = super::super::mock::Proxy::start().await;
    let previous = CodexExecutor::new(proxy.url.clone());
    let mut credential = auth(&server.url);
    credential.proxy_url = "direct".into();
    let response = previous
        .execute_stream(
            Arc::new(credential.clone()),
            request("gpt-5-codex", HELLO),
            ws_options("proxy-clear"),
        )
        .await
        .unwrap();
    assert!(collect(response).await.1.is_none());
    assert!(proxy.connects().is_empty());
    let next = executor().with_execution_sessions_from(&previous);
    credential.proxy_url.clear();
    let response = next
        .execute_stream(
            Arc::new(credential),
            request("gpt-5-codex", HELLO),
            ws_options("proxy-clear"),
        )
        .await
        .unwrap();
    assert!(collect(response).await.1.is_none());
    assert_eq!(server.record().handshakes.len(), 2);
    assert!(proxy.connects().is_empty());
    next.close_execution_session("proxy-clear");
}

// Deviation from upstream: completion is checked again after the send permit.
#[tokio::test]
async fn interrupt_waiting_for_the_send_lock_drops_a_completed_response() {
    let (complete, gate) = watch::channel(false);
    let server = Server::start(move |_| {
        let gate = gate.clone();
        Answer::accept(move |mut peer| {
            let mut gate = gate.clone();
            async move {
                peer.recv().await.unwrap();
                peer.send(r#"{"type":"response.created","response":{"id":"resp-1"}}"#)
                    .await;
                gate.wait_for(|ready| *ready).await.unwrap();
                peer.send(
                    r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#,
                )
                .await;
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = executor();
    let mut response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("completion-gate"),
        )
        .await
        .unwrap();
    response.chunks.next().await.unwrap().unwrap();
    let session = executor.websockets().get("completion-gate").unwrap();
    let conn = session.conn().unwrap();
    let permit = conn.lock_send().await;
    let interrupt = executor.interrupt_execution_session(
        "completion-gate",
        INTERRUPT.as_bytes(),
        Arc::new(|_| Some(Arc::new(Auth::default()))),
    );
    tokio::pin!(interrupt);
    assert!(futures_util::poll!(&mut interrupt).is_pending());
    complete.send_replace(true);
    // Consuming the completion proves the reader updated terminal state.
    assert!(collect(response).await.1.is_none());
    drop(permit);
    assert!(interrupt.await.unwrap());
    executor.close_execution_session("completion-gate");
    server.wait_closed(1).await;
    assert_eq!(server.record().messages.len(), 1);
}

#[tokio::test]
async fn codex_replacement_after_reload_dials_the_new_global_proxy() {
    let server = Server::turns(&[COMPLETED]).await;
    let p1 = super::super::mock::Proxy::start().await;
    let p2 = super::super::mock::Proxy::start().await;
    let previous = CodexExecutor::new(p1.url.clone());
    let mut credential = auth(&server.url);
    credential.proxy_url = "direct".into();
    let first = previous
        .execute_stream(
            Arc::new(credential.clone()),
            request("gpt-5-codex", HELLO),
            ws_options("proxy-reload"),
        )
        .await
        .unwrap();
    assert!(collect(first).await.1.is_none());
    let next = CodexExecutor::new(p2.url.clone()).with_execution_sessions_from(&previous);
    // A reload alone retains the existing connection and its route.
    let second = next
        .execute_stream(
            Arc::new(credential.clone()),
            request("gpt-5-codex", HELLO),
            ws_options("proxy-reload"),
        )
        .await
        .unwrap();
    assert!(collect(second).await.1.is_none());
    assert_eq!(server.record().handshakes.len(), 1);
    credential.proxy_url.clear();
    let third = next
        .execute_stream(
            Arc::new(credential.clone()),
            request("gpt-5-codex", HELLO),
            ws_options("proxy-reload"),
        )
        .await
        .unwrap();
    assert!(collect(third).await.1.is_none());
    assert!(p1.connects().is_empty());
    assert_eq!(p2.connects().len(), 1);
    assert_eq!(
        next.websockets()
            .get("proxy-reload")
            .unwrap()
            .global_proxy(),
        Some(p2.url.clone())
    );
    let fourth = next
        .execute_stream(
            Arc::new(credential),
            request("gpt-5-codex", HELLO),
            ws_options("proxy-reload"),
        )
        .await
        .unwrap();
    assert!(collect(fourth).await.1.is_none());
    assert_eq!(server.record().handshakes.len(), 2);
    assert_eq!(p2.connects().len(), 1);
    next.close_execution_session("proxy-reload");
}

#[tokio::test]
async fn p47_interrupt_rejects_unknown_and_terminal_identity_before_teardown() {
    for frame in [
        r#"{"type":"response.output_text.delta","delta":"live"}"#,
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        r#"{"type":"response.failed","response":{"id":"r1","error":{"message":"failed"}}}"#,
        r#"{"type":"error","response_id":"r1","status":400,"error":{"message":"failed"}}"#,
        r#"{"type":"error","status":400,"error":{"message":"failed"}}"#,
    ] {
        let server = Server::start(move |_| {
            Answer::accept(move |mut peer| async move {
                peer.recv().await.unwrap();
                peer.send(r#"{"type":"response.created","response":{"id":"r1"}}"#)
                    .await;
                peer.send(frame).await;
                peer.hold().await;
            })
        })
        .await;
        let executor = executor();
        let mut response = executor
            .execute_stream(
                Arc::new(auth(&server.url)),
                request("gpt-5-codex", HELLO),
                ws_options("identity"),
            )
            .await
            .unwrap();
        response.chunks.next().await.unwrap().unwrap();
        let conn = executor
            .websockets()
            .get("identity")
            .unwrap()
            .conn()
            .unwrap();
        let kind = json(frame)["type"].as_str().unwrap().to_owned();
        within("reader identity update", async {
            while conn.last_event() != kind {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let error = executor
            .interrupt_execution_session(
                "identity",
                br#"{"type":"response.interrupt","response_id":"unknown"}"#,
                Arc::new(|_| Some(Arc::new(Auth::default()))),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.message, "response.interrupt does not target the active upstream response",
            "{frame}"
        );
        assert_eq!(
            server.record().messages.len(),
            1,
            "unknown interrupt reached upstream"
        );
        drop(response);
    }
}

#[tokio::test]
async fn p47_interrupt_queued_failure_does_not_end_running_response() {
    let server = Server::start(|_| Answer::accept(|mut peer| async move {
        peer.recv().await.unwrap();
        peer.send(r#"{"type":"response.created","response":{"id":"r1"}}"#).await;
        peer.send(r#"{"type":"response.failed","response":{"id":"queued","error":{"message":"rejected"}}}"#).await;
        assert_eq!(json(&peer.recv().await.unwrap())["response_id"], "r1");
        peer.send(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#).await;
        peer.hold().await;
    })).await;
    let executor = executor();
    let mut response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("queued-error"),
        )
        .await
        .unwrap();
    response.chunks.next().await.unwrap().unwrap();
    let conn = executor
        .websockets()
        .get("queued-error")
        .unwrap()
        .conn()
        .unwrap();
    within("queued rejection read", async {
        while conn.last_event() != "response.failed" {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        executor
            .interrupt_execution_session(
                "queued-error",
                br#"{"type":"response.interrupt","response_id":"r1"}"#,
                Arc::new(|_| Some(Arc::new(Auth::default())))
            )
            .await
            .unwrap()
    );
    server
        .wait_for("interrupt forwarded", |record| record.messages.len() == 2)
        .await;
    drop(response);
}
