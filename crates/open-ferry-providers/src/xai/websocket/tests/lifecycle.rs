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
