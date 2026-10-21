// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_responses_interrupt_test.go
// and openai_responses_late_interrupt_test.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use futures_util::future::BoxFuture;
use open_ferry_core::exec::{Dispatcher, Options, ProviderId, Request, Response, StreamResponse};

const CREATE: &str = r#"{"type":"response.create","model":"test-model","input":[]}"#;

#[derive(Clone, Default)]
struct Gate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Gate {
    async fn wait(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

struct Controlled {
    fake: Arc<FakeDispatcher>,
    dial: Option<Gate>,
    lookup: Option<Gate>,
    streams: Mutex<std::collections::VecDeque<open_ferry_core::exec::ChunkStream>>,
}

impl Dispatcher for Controlled {
    fn execute<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        self.fake.execute(providers, request, options)
    }

    fn count_tokens<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        self.fake.count_tokens(providers, request, options)
    }

    fn execute_stream<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        Box::pin(async move {
            if let Some(gate) = &self.dial {
                gate.wait().await;
            }
            let stream = self.streams.lock().unwrap().pop_front();
            if let Some(chunks) = stream {
                Ok(StreamResponse {
                    headers: HeaderMap::new(),
                    chunks,
                })
            } else {
                self.fake.execute_stream(providers, request, options).await
            }
        })
    }

    fn interrupt_execution_session<'a>(
        &'a self,
        _: &'a str,
        _: &'a [u8],
    ) -> BoxFuture<'a, Result<bool, ExecError>> {
        Box::pin(async move {
            if let Some(gate) = &self.lookup {
                gate.wait().await;
            }
            Ok(false)
        })
    }
}

async fn serve_controlled(dispatcher: Controlled) -> String {
    let state = AppState::new(
        ServerConfig {
            api_keys: vec!["sk-test".into()],
            ..Default::default()
        },
        Arc::new(dispatcher),
        Arc::new(test_catalog()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, crate::router(state)).await.unwrap() });
    url
}

fn channel_stream() -> (
    tokio::sync::mpsc::UnboundedSender<Result<Bytes, ExecError>>,
    open_ferry_core::exec::ChunkStream,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    (tx, Box::pin(stream))
}

async fn entered(gate: &Gate) {
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn interrupt_lookup_crossing_a_turn_boundary_cannot_cancel_the_next_turn() {
    let gate = Gate::default();
    let (a, stream_a) = channel_stream();
    let (b, stream_b) = channel_stream();
    let url = serve_controlled(Controlled {
        fake: FakeDispatcher::new(Vec::new()),
        dial: None,
        lookup: Some(gate.clone()),
        streams: Mutex::new([stream_a, stream_b].into()),
    })
    .await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, CREATE).await;
    a.send(Ok(Bytes::from(sse(
        r#"{"type":"response.created","response":{"id":"a"}}"#,
    ))))
    .unwrap();
    assert_eq!(recv(&mut ws).await["response"]["id"], "a");
    // Queue B before pausing the only socket reader in A's lookup.
    send(&mut ws, CREATE).await;
    send(
        &mut ws,
        r#"{"type":"response.interrupt","response_id":"a"}"#,
    )
    .await;
    entered(&gate).await;
    a.send(Ok(Bytes::from(sse(
        r#"{"type":"response.completed","response":{"id":"a","output":[]}}"#,
    ))))
    .unwrap();
    drop(a);
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    b.send(Ok(Bytes::from(sse(
        r#"{"type":"response.created","response":{"id":"b"}}"#,
    ))))
    .unwrap();
    assert_eq!(recv(&mut ws).await["response"]["id"], "b");
    gate.release.notify_one();
    assert_eq!(recv(&mut ws).await["status"], 400);
    b.send(Ok(Bytes::from(sse(
        r#"{"type":"response.completed","response":{"id":"b","output":[]}}"#,
    ))))
    .unwrap();
    drop(b);
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
}

#[tokio::test]
async fn interrupt_during_dial_is_rejected_and_the_turn_proceeds() {
    let gate = Gate::default();
    let url = serve_controlled(Controlled {
        fake: FakeDispatcher::new(vec![completes("dialed", "[]")]),
        dial: Some(gate.clone()),
        lookup: None,
        streams: Mutex::default(),
    })
    .await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, CREATE).await;
    entered(&gate).await;
    send(
        &mut ws,
        r#"{"type":"response.interrupt","response_id":"dialed"}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["status"], 400);
    gate.release.notify_one();
    assert_eq!(recv(&mut ws).await["response"]["id"], "dialed");
}

#[tokio::test]
async fn interrupt_after_the_stream_ended_is_rejected() {
    let (url, _) = serve(test_catalog(), vec![completes("ended", "[]")]).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, CREATE).await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    // A local warmup is a barrier: it is processed only after forwarding
    // and post-forward bookkeeping of the preceding stream have finished.
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[],"generate":false}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.created");
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    send(
        &mut ws,
        r#"{"type":"response.interrupt","response_id":"ended"}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["status"], 400);
}

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
    let partial = r#"{"type":"response.incomplete","response":{"id":"first","output":[{"type":"function_call","call_id":"call","arguments":"partial"}],"incomplete_details":{"reason":"interrupted"}}}"#;
    let (url, dispatcher) = serve(test_catalog(), vec![
        Outcome::chunks(&[
            &sse(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call","name":"tool","arguments":"complete"}}"#),
            &sse(partial),
        ]),
        completes("next", "[]"),
    ]).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, CREATE).await;
    assert_eq!(recv(&mut ws).await["type"], "response.output_item.done");
    assert_eq!(
        recv(&mut ws).await,
        serde_json::from_str::<Value>(partial).unwrap()
    );
    send(&mut ws, CREATE).await;
    assert_eq!(recv(&mut ws).await["response"]["id"], "next");
    assert_eq!(dispatcher.calls().len(), 2);
}

struct InterruptError(Arc<FakeDispatcher>, ExecError);

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
        Box::pin(async { Err(self.1.clone()) })
    }
}

#[tokio::test]
async fn interrupt_validation_and_executor_errors_preserve_upstream_texts() {
    for failure in [
        ExecError::upstream(400, "upstream executor does not support response.interrupt"),
        ExecError::upstream(400, "websocket credential is no longer enabled"),
        ExecError::upstream(500, "write failed"),
        ExecError::new(ErrorKind::Upstream, "no status"),
    ] {
        let message = failure.message.clone();
        let dispatcher = Arc::new(InterruptError(
            FakeDispatcher::new(vec![completes("next", "[]")]),
            failure,
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
        let error = recv(&mut ws).await;
        assert_eq!(error["error"]["message"], message);
        assert_eq!(error["status"], 400);
        send(&mut ws, CREATE).await;
        assert_eq!(recv(&mut ws).await["response"]["id"], "next");
    }
}
