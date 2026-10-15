// SIWC live-credential regressions for the Responses duplex connection.

use super::*;
use std::sync::RwLock;

async fn events(chunks: &mut futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>) {
    for expected in ["response.created", "response.completed"] {
        let chunk = within("live credential duplex events", chunks.next())
            .await
            .unwrap()
            .unwrap();
        let event: Value = serde_json::from_slice(&chunk).unwrap();
        assert_eq!(event["type"], expected);
    }
}

async fn change(initial_siwc: bool, next_siwc: bool, clear: bool) {
    let server = Server::turns(&[
        r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#,
        COMPLETED,
    ])
    .await;
    let mut config = Config::default();
    config.codex.response_steering = true;
    let executor = executor_with(config);
    let mut initial = credential(&format!("{}/v1", server.url));
    initial
        .metadata
        .insert("use_siwc".into(), json!(initial_siwc));
    initial.attributes.remove("header:authorization");
    let live = Arc::new(RwLock::new(Arc::new(initial.clone())));
    let (tx, rx) = mpsc::channel(4);
    let mut opts = attempted_options("live-change", false);
    let lookup = live.clone();
    opts.websocket_input = Some(WebsocketInput::new(rx).with_auth_check(Arc::new(move |id| {
        let auth = lookup.read().unwrap().clone();
        (auth.id == id && !auth.disabled && auth.status != open_ferry_core::auth::Status::Disabled)
            .then_some(auth)
    })));
    let mut chunks = executor
        .execute_stream(Arc::new(initial), request("gpt-5-codex", BODY), opts)
        .await
        .unwrap()
        .chunks;
    events(&mut chunks).await;
    if initial_siwc {
        assert_headers(&server.record().handshakes[0], TOKEN, false);
    }
    let mut next = (**live.read().unwrap()).clone();
    next.metadata.insert("use_siwc".into(), json!(next_siwc));
    next.metadata["siwc"]["access_token"] =
        json!(if clear { "" } else { "live-rotated-siwc-token" });
    *live.write().unwrap() = Arc::new(next.clone());
    let mut body: Value = serde_json::from_str(BODY).unwrap();
    body["type"] = json!("response.create");
    tx.send(InputFrame::Payload(Bytes::from(body.to_string())))
        .await
        .unwrap();
    let error = within("live credential rejection", chunks.next())
        .await
        .unwrap()
        .unwrap_err();
    if clear {
        assert_eq!(error.status, 401);
        assert!(!error.request_scoped);
        assert!(error.message.contains("siwc access_token is missing"));
    } else {
        assert_eq!(error.message, ExecError::replay_required().message);
    }
    assert!(
        within("closed live credential stream", chunks.next())
            .await
            .is_none()
    );
    assert_eq!(
        server.record().messages.len(),
        1,
        "no frame after credential change"
    );
    assert_eq!(server.record().handshakes.len(), 1);
    drop(tx);
    drop(chunks);
    if !clear {
        let response = executor
            .execute_stream(
                Arc::new(next),
                request("gpt-5-codex", BODY),
                attempted_options("live-change", false),
            )
            .await
            .unwrap();
        assert!(collect(response).await.1.is_none());
        let record = server.record();
        assert_eq!(
            record.handshakes.len(),
            2,
            "changed credential needs a new connection"
        );
        assert_eq!(
            record.messages.len(),
            2,
            "consumed create is never replayed"
        );
        if next_siwc {
            assert_headers(&record.handshakes[1], "live-rotated-siwc-token", false);
            assert_body(&record.messages[1]);
        } else {
            assert!(
                record.handshakes[1].header("authorization")
                    == Some(format!("Bearer {API_KEY}").as_str())
            );
        }
    }
    executor.close_execution_session("live-change");
}

#[tokio::test]
async fn siwc_duplex_live_rotation_rejects_the_old_connection_and_redials() {
    change(true, true, false).await;
}

#[tokio::test]
async fn siwc_duplex_live_mode_switches_reject_both_directions() {
    change(true, false, false).await;
    change(false, true, false).await;
}

#[tokio::test]
async fn siwc_duplex_live_cleared_token_fails_closed() {
    change(true, true, true).await;
}

#[tokio::test]
async fn siwc_duplex_live_ordinary_outer_refresh_keeps_the_connection() {
    let server = Server::turns(&[
        r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#,
        COMPLETED,
    ])
    .await;
    let mut config = Config::default();
    config.codex.response_steering = true;
    let executor = executor_with(config);
    let mut initial = auth(&format!("{}/v1", server.url));
    initial.attributes.remove("api_key");
    initial
        .metadata
        .insert("access_token".into(), json!("ordinary-before-refresh"));
    let live = Arc::new(RwLock::new(Arc::new(initial.clone())));
    let lookup = live.clone();
    let (tx, rx) = mpsc::channel(4);
    let mut opts = ws_options("ordinary-refresh");
    opts.websocket_input = Some(
        WebsocketInput::new(rx)
            .with_auth_check(Arc::new(move |_| Some(lookup.read().unwrap().clone()))),
    );
    let mut chunks = executor
        .execute_stream(Arc::new(initial), request("gpt-5-codex", HELLO), opts)
        .await
        .unwrap()
        .chunks;
    events(&mut chunks).await;
    let mut next = (**live.read().unwrap()).clone();
    next.metadata
        .insert("access_token".into(), json!("ordinary-after-refresh"));
    *live.write().unwrap() = Arc::new(next);
    tx.send(InputFrame::Payload(Bytes::from_static(
        br#"{"type":"response.create","input":"next"}"#,
    )))
    .await
    .unwrap();
    events(&mut chunks).await;
    let record = server.record();
    assert_eq!(record.handshakes.len(), 1);
    assert_eq!(record.messages.len(), 2);
    assert_eq!(
        record.handshakes[0].header("authorization"),
        Some("Bearer ordinary-before-refresh")
    );
    drop(tx);
    drop(chunks);
    executor.close_execution_session("ordinary-refresh");
}
