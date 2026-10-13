// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_responses_interrupt_test.go
// and openai_responses_late_interrupt_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use futures_util::future::BoxFuture;
use open_ferry_core::exec::{Dispatcher, Options, ProviderId, Request, Response, StreamResponse};

const CREATE: &str = r#"{"type":"response.create","model":"test-model","input":[]}"#;

#[tokio::test]
async fn idle_interrupt_has_no_upstream_and_does_not_start_a_turn() {
    let (url, dispatcher) = serve(test_catalog(), Vec::new()).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.interrupt","response_id":"idle"}"#,
    )
    .await;
    let error = recv(&mut ws).await;
    assert_eq!(error["status"], 400);
    assert_eq!(
        error["error"]["message"],
        "no active upstream websocket for response.interrupt"
    );
    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn interrupt_cancels_http_turn_and_accepts_the_next_turn() {
    let created = sse(r#"{"type":"response.created","response":{"id":"http-response"}}"#);
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![
            Outcome::Hang(HeaderMap::new(), vec![Ok(Bytes::from(created))]),
            completes("next", "[]"),
        ],
    )
    .await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, CREATE).await;
    assert_eq!(recv(&mut ws).await["type"], "response.created");
    // Queued creates must not block the interrupt's direct reader path.
    send(&mut ws, CREATE).await;
    send(
        &mut ws,
        r#"{"type":"response.interrupt","response_id":"http-response"}"#,
    )
    .await;
    let incomplete = recv(&mut ws).await;
    assert_eq!(incomplete["type"], "response.incomplete");
    assert_eq!(incomplete["response"]["id"], "http-response");
    assert_eq!(
        incomplete["response"]["incomplete_details"]["reason"],
        "interrupted"
    );
    assert_eq!(recv(&mut ws).await["response"]["id"], "next");
    assert_eq!(dispatcher.calls().len(), 2);
}

#[tokio::test]
async fn incomplete_is_a_terminal_turn_and_the_socket_stays_open() {
    let (url, dispatcher) = serve(test_catalog(), vec![
        Outcome::chunks(&[&sse(r#"{"type":"response.incomplete","response":{"id":"first","output":[],"incomplete_details":{"reason":"interrupted"}}}"#)]),
        completes("next", "[]"),
    ]).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, CREATE).await;
    assert_eq!(recv(&mut ws).await["type"], "response.incomplete");
    send(&mut ws, CREATE).await;
    assert_eq!(recv(&mut ws).await["response"]["id"], "next");
    assert_eq!(dispatcher.calls().len(), 2);
}

struct InterruptError(Arc<FakeDispatcher>, &'static str);

impl Dispatcher for InterruptError {
    fn execute<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        self.0.execute(providers, request, options)
    }
    fn count_tokens<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        self.0.count_tokens(providers, request, options)
    }
    fn execute_stream<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        self.0.execute_stream(providers, request, options)
    }
    fn interrupt_execution_session<'a>(
        &'a self,
        _: &'a str,
        _: &'a [u8],
    ) -> BoxFuture<'a, Result<bool, ExecError>> {
        Box::pin(async { Err(ExecError::upstream(400, self.1)) })
    }
}

#[tokio::test]
async fn interrupt_validation_and_executor_errors_preserve_upstream_texts() {
    for message in [
        "upstream executor does not support response.interrupt",
        "websocket credential is no longer enabled",
    ] {
        let dispatcher = Arc::new(InterruptError(
            FakeDispatcher::new(vec![completes("next", "[]")]),
            message,
        ));
        let state = AppState::new(
            ServerConfig {
                api_keys: vec!["sk-test".into()],
                ..Default::default()
            },
            dispatcher,
            Arc::new(test_catalog()),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, crate::router(state)).await.unwrap() });
        let mut ws = connect(&url, &[]).await;
        for id in ["null", "0", "\" \""] {
            send(
                &mut ws,
                &format!(r#"{{"type":"response.interrupt","response_id":{id}}}"#),
            )
            .await;
            let error = recv(&mut ws).await;
            assert_eq!(
                error["error"]["message"],
                "response.interrupt requires response_id"
            );
            assert_eq!(error["status"], 400);
        }
        send(
            &mut ws,
            r#"{"type":"response.interrupt","response_id":"response"}"#,
        )
        .await;
        assert_eq!(recv(&mut ws).await["error"]["message"], message);
        send(&mut ws, CREATE).await;
        assert_eq!(recv(&mut ws).await["response"]["id"], "next");
    }
}
