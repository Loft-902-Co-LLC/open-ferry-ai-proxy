// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor.go
// (auth removal and ID-state acquisition regression tests) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

#[tokio::test]
async fn xai_recreation_waits_for_auth_metadata_cleanup() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = Arc::new(executor());
    let a = auth_as("xai-a", API_KEY, &server.url);
    streamed(&executor, &a, HELLO, ws_options("cleanup")).await;
    let old = executor.websockets.ids.get("cleanup").unwrap();
    let (entered, removed) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let cleanup = tokio::task::spawn_blocking({
        let executor = executor.clone();
        move || {
            let mut entered = Some(entered);
            executor
                .websockets
                .store
                .close_auth_sessions_with(&a.id, |id| {
                    entered.take().unwrap().send(()).unwrap();
                    gate.recv_timeout(WAIT).unwrap();
                    executor.websockets.ids.delete(id);
                    executor.websockets.targets().remove(id);
                });
        }
    });
    tokio::time::timeout(WAIT, removed).await.unwrap().unwrap();
    let (started, recreating) = tokio::sync::oneshot::channel();
    let recreate = tokio::task::spawn_blocking({
        let executor = executor.clone();
        move || {
            started.send(()).unwrap();
            let session = executor.websockets.store.get_or_create("cleanup").unwrap();
            let state = executor.websockets.ids.get("cleanup").unwrap();
            state.replace_transcript(vec![json!({"type":"message", "content":"B"})]);
            executor.websockets.record_target(
                "cleanup",
                &Target {
                    auth_id: "xai-b".into(),
                    url: "ws://127.0.0.1/responses".into(),
                    proxy: "direct".into(),
                },
            );
            (session, state)
        }
    });
    tokio::time::timeout(WAIT, recreating)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(WAIT, cleanup).await.unwrap().unwrap();
    let (session, state) = tokio::time::timeout(WAIT, recreate).await.unwrap().unwrap();
    assert!(!Arc::ptr_eq(&old, &state));
    assert!(Arc::ptr_eq(
        &session,
        &executor.websockets.store.get("cleanup").unwrap()
    ));
    assert!(Arc::ptr_eq(
        &state,
        &executor.websockets.ids.get("cleanup").unwrap()
    ));
    assert_eq!(state.transcript()[0]["content"], "B");
    assert_eq!(
        executor
            .websockets
            .targets()
            .get("cleanup")
            .unwrap()
            .auth_id,
        "xai-b"
    );
    executor.close_execution_session("cleanup");
}

#[tokio::test]
async fn xai_waiting_request_takes_id_state_after_the_session_lock() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = executor();
    let a = auth_as("xai-a", API_KEY, &server.url);
    let b = auth_as("xai-b", API_KEY, &server.url);
    streamed(&executor, &a, HELLO, ws_options("state-order")).await;
    let state = executor.websockets.ids.get("state-order").unwrap();
    state.map_downstream_to_upstream("client-previous", "removed-upstream");
    let session = executor.websockets.store.get("state-order").unwrap();
    let guard = session.lock_requests().await;
    let owners = Arc::strong_count(&state);
    let (release, gate) = tokio::sync::oneshot::channel();
    *executor.websockets.id_state_gate.lock().unwrap() = Some(gate);
    let body = json!({"model":"grok-4.3", "previous_response_id":"client-previous", "input":[]})
        .to_string();
    let mut call =
        Box::pin(executor.execute_stream(b.clone(), request(&body), ws_options("state-order")));
    // Poll to the request-lock gate: no transcript snapshot may be held yet.
    assert!(futures_util::poll!(&mut call).is_pending());
    assert_eq!(Arc::strong_count(&state), owners);
    drop(guard);
    // The call now owns reqMu and is gated immediately before ID acquisition.
    assert!(futures_util::poll!(&mut call).is_pending());
    assert_eq!(Arc::strong_count(&state), owners);
    executor.close_auth_execution_sessions(&a.id);
    release.send(()).unwrap();
    assert!(tokio::time::timeout(WAIT, call).await.unwrap().is_err());
    streamed(&executor, &b, &body, ws_options("state-order")).await;
    assert_ne!(
        json(&server.record().messages[1])["previous_response_id"],
        "removed-upstream"
    );
    executor.close_execution_session("state-order");
}

#[tokio::test]
async fn xai_waiting_compaction_takes_id_state_after_the_session_lock() {
    let server = Server::start(|n| {
        if n == 0 {
            Answer::accept(|mut peer| async move {
                peer.recv().await.unwrap();
                peer.send(COMPLETED).await;
                peer.hold().await;
            })
        } else {
            Answer::Refuse { status: 200, headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"id":"compact","output":[{"type":"compaction","encrypted_content":"ZW5jcnlwdGVk"}]}"#.into() }
        }
    }).await;
    let executor = executor();
    let a = auth_as("xai-a", API_KEY, &server.url);
    let b = auth_as("xai-b", API_KEY, &server.url);
    streamed(&executor, &a, HELLO, ws_options("compact-order")).await;
    let state = executor.websockets.ids.get("compact-order").unwrap();
    state.replace_transcript(Vec::new());
    state.map_downstream_to_upstream("client-previous", "removed-upstream");
    let session = executor.websockets.store.get("compact-order").unwrap();
    let guard = session.lock_requests().await;
    let owners = Arc::strong_count(&state);
    let (release, gate) = tokio::sync::oneshot::channel();
    *executor.websockets.id_state_gate.lock().unwrap() = Some(gate);
    let body = r#"{"model":"grok-4.3","previous_response_id":"client-previous","input":[{"type":"compaction_trigger"}]}"#;
    let mut call = Box::pin(executor.execute_stream(b, request(body), ws_options("compact-order")));
    assert!(futures_util::poll!(&mut call).is_pending());
    assert_eq!(Arc::strong_count(&state), owners);
    drop(guard);
    assert!(futures_util::poll!(&mut call).is_pending());
    assert_eq!(Arc::strong_count(&state), owners);
    executor.close_auth_execution_sessions(&a.id);
    release.send(()).unwrap();
    let response = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
    let _: Vec<_> = response.chunks.collect().await;
    assert_eq!(
        json(&server.record().bodies[0])["previous_response_id"],
        "client-previous"
    );
    assert!(!Arc::ptr_eq(
        &state,
        &executor.websockets.ids.get("compact-order").unwrap()
    ));
    executor.close_execution_session("compact-order");
}

async fn failed_replacement_restores_owner(cancel: bool) {
    let (release, gate) = watch::channel(false);
    let server = Server::start(move |n| {
        if n == 0 {
            Answer::accept(|mut peer| async move {
                peer.recv().await.unwrap();
                peer.send(COMPLETED).await;
                peer.hold().await;
            })
        } else {
            Answer::Held(gate.clone(), Box::new(Answer::refuse(503, "dial failed")))
        }
    })
    .await;
    let executor = Arc::new(executor());
    let b = auth_as("xai-b", API_KEY, &server.url);
    let c = auth_as("xai-c", API_KEY, &server.url);
    streamed(&executor, &b, HELLO, ws_options("failed-owner")).await;
    let session = executor.websockets.store.get("failed-owner").unwrap();
    let state = executor.websockets.ids.get("failed-owner").unwrap();
    let task = tokio::spawn({
        let executor = executor.clone();
        async move {
            executor
                .execute_stream(c, request(HELLO), ws_options("failed-owner"))
                .await
        }
    });
    server
        .wait_for("C's pending dial", |r| r.handshakes.len() == 2)
        .await;
    // The last successful owner must not remove C's pending claim.
    executor.close_auth_execution_sessions(&b.id);
    assert!(Arc::ptr_eq(
        &session,
        &executor.websockets.store.get("failed-owner").unwrap()
    ));
    if cancel {
        task.abort();
        assert!(
            tokio::time::timeout(WAIT, task)
                .await
                .unwrap()
                .err()
                .unwrap()
                .is_cancelled()
        );
    } else {
        release.send_replace(true);
        assert!(
            tokio::time::timeout(WAIT, task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    executor.close_auth_execution_sessions(&b.id);
    assert!(executor.websockets.store.get("failed-owner").is_none());
    assert!(!Arc::ptr_eq(
        &state,
        &executor.websockets.ids.get("failed-owner").unwrap()
    ));
    assert!(!executor.websockets.targets().contains_key("failed-owner"));
    let error = session
        .ensure_conn(
            ConnTarget::new(&b.id, &server.url, "direct", API_KEY),
            || async { panic!("a removed session must stay closed") },
        )
        .await;
    assert!(error.is_err());
    release.send_replace(true);
    assert_eq!(server.record().messages.len(), 1);
}

#[tokio::test]
async fn failed_xai_replacement_dial_restores_the_successful_owner() {
    failed_replacement_restores_owner(false).await;
}

#[tokio::test]
async fn cancelled_xai_replacement_dial_restores_the_successful_owner() {
    failed_replacement_restores_owner(true).await;
}

#[tokio::test]
async fn dropping_an_earlier_xai_dial_preserves_a_later_ownership_claim() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = executor();
    let b = auth_as("xai-b", API_KEY, &server.url);
    streamed(&executor, &b, HELLO, ws_options("claims")).await;
    let session = executor.websockets.store.get("claims").unwrap();
    let mut first = Box::pin(session.ensure_conn(
        ConnTarget::new("xai-c", &server.url, "direct", API_KEY),
        std::future::pending,
    ));
    assert!(futures_util::poll!(&mut first).is_pending());
    let mut later = Box::pin(session.ensure_conn(
        ConnTarget::new("xai-d", &server.url, "direct", API_KEY),
        std::future::pending,
    ));
    assert!(futures_util::poll!(&mut later).is_pending());
    drop(first);
    executor.close_auth_execution_sessions(&b.id);
    assert!(Arc::ptr_eq(
        &session,
        &executor.websockets.store.get("claims").unwrap()
    ));
    executor.close_auth_execution_sessions("xai-d");
    assert!(executor.websockets.store.get("claims").is_none());
    assert!(tokio::time::timeout(WAIT, later).await.unwrap().is_err());
    assert!(!executor.websockets.targets().contains_key("claims"));
}
