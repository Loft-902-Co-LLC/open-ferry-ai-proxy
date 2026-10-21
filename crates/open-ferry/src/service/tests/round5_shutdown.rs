use super::*;
use axum::body::{Body, to_bytes};
use tower_service::Service as _;

async fn request(
    state: &open_ferry_management::ManagementState,
    method: &str,
    path: &str,
) -> serde_json::Value {
    let mut app = open_ferry_management::api_router(state.clone());
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", "Bearer round5-test-key")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.call(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    serde_json::from_slice(&body).unwrap()
}

async fn server_end(queued: bool) {
    use open_ferry_management::test_support;
    let dir = tempfile::tempdir().unwrap();
    let path = codex_file(
        dir.path(),
        "round5.json",
        r#", "refresh_token":"outer-refresh-test""#,
    );
    let mut service = service(
        dir.path(),
        "proxy-url: direct\nremote-management:\n  secret-key: round5-test-key\n",
    );
    service.handle_and_drain(added(&path), Path::new("")).await;
    service.management = service
        .management
        .clone()
        .with_local_password("round5-test-key");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    assert_ne!(address.port(), 8317);
    let tokens = axum::Router::new().route("/api/accounts/oauth/token", axum::routing::post(|| async {
        axum::Json(serde_json::json!({"access_token":"round5-test", "refresh_token":"round5-refresh-test", "scope":"openid chatgpt.tokens.use.direct", "id_token":"unsigned-test", "expires_in":3600}))
    }));
    let upstream = tokio::spawn(async move {
        axum::serve(listener, tokens).await.unwrap();
    });
    let mut pause = test_support::prepare_bind(&service.management, &format!("http://{address}"));
    let answer = request(
        &service.management,
        "GET",
        "/v8/management/oauth/auth-url?provider=codex&mode=siwc-bind&name=round5.json",
    )
    .await;
    let session = answer["state"].as_str().unwrap().to_owned();
    request(
        &service.management,
        "GET",
        &format!(
            "/v8/management/oauth/callback?state={session}&code=round5-test&client_id=round5-client"
        ),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(12), pause.entered())
        .await
        .unwrap();
    if queued {
        pause.release();
        tokio::time::timeout(Duration::from_secs(12), async {
            while service.sync_requests.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!test_support::finalized(&service.management, &session));
    }
    let state = service.management.clone();
    let result = tokio::spawn(async { Ok::<(), io::Error>(()) }).await;
    // Calls the exact production select arm, with no service.work consumer.
    let ended = server_ended(&mut service, result);
    let release = async {
        if !queued {
            while !test_support::shutting_down(&state) {
                tokio::task::yield_now().await;
            }
            pause.release();
        }
    };
    let (ended, ()) = tokio::join!(tokio::time::timeout(Duration::from_secs(5), ended), release);
    upstream.abort();
    assert!(
        ended.is_ok(),
        "server-ended shutdown stranded the saved bind for the grace"
    );
    assert!(
        test_support::finalized(&state, &session),
        "saved session not finalized as shutdown returned"
    );
    let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(disk["use_siwc"], true);
    // No runtime adoption: real SyncSender returned Stopped, the saved file loads on restart.
    assert!(service.manager.list()[0].metadata.get("use_siwc").is_none());
    assert!(service.sync_requests.is_closed());
    assert!(service.sync_requests.is_empty());
    assert!(service.reload_requests.is_closed());
}

#[tokio::test]
async fn round5_server_ended_finishes_post_fence_bind() {
    server_end(false).await;
}

#[tokio::test]
async fn round5_server_ended_refuses_already_queued_bind_sync() {
    server_end(true).await;
}
