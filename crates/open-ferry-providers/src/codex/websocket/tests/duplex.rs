// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_duplex_test.go,
// codex_websockets_duplex_bootstrap_input_test.go, codex_websockets_duplex_credential_failure_test.go,
// codex_websockets_duplex_health_test.go, codex_websockets_duplex_initial_failure_test.go,
// codex_websockets_duplex_rejection_test.go and codex_websockets_duplex_successor_metadata_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Response steering: a streaming call that keeps its connection for the
//! client's socket, against a mock Codex on 127.0.0.1.
//!
//! Deviations from upstream:
//! - The client's socket ends when its sender is dropped, where upstream's
//!   tests cancel the call's context; a test that drops the stream instead
//!   cancels the call.
//! - The mock's script can't fail a test, so what Codex read is checked
//!   from the server's record once the call ends.
//! - The input channel is buffered, where some of upstream's tests use an
//!   unbuffered one to know the duplex took a frame; the duplex reads
//!   Codex's messages before the client's, so the orders those tests rely
//!   on hold either way.
//! - Each test has its own model registry, and the reasoning replay
//!   sessions are the test's own rather than the cache being cleared.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use chrono::{TimeDelta, Utc};
use futures_util::StreamExt as _;
use open_ferry_core::auth::{Auth, Status};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    ChunkStream, Dispatcher, ErrorKind, ExecError, Format, InputFrame, Options, TransportFault,
    WebsocketInput,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::super::mock::{Answer, Peer, Server};
use super::{auth, executor_with, json, lock_free, request, within, ws_options};
use crate::codex::CodexExecutor;
use crate::codex::replay_cache::ReplayCache;
use crate::codex::replay_cache::tests::valid_encrypted_content;
use crate::json::{exists, str_at};

const MODEL: &str = "gpt-6-astra";

/// An executor with `codex.response-steering` on.
fn steering() -> CodexExecutor {
    let mut config = Config::default();
    config.codex.response_steering = true;
    executor_with(config)
}

/// A mock Codex whose every connection runs `script`.
async fn serve<F, Fut>(script: F) -> Server
where
    F: Fn(Peer) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Server::start(move |_| Answer::accept(script.clone())).await
}

/// The client's end of its Responses WebSocket: what it sends next.
struct Client {
    tx: Option<mpsc::Sender<InputFrame>>,
}

impl Client {
    /// A client that can queue `capacity` frames, and the call's end.
    fn new(capacity: usize) -> (Self, WebsocketInput) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx: Some(tx) }, WebsocketInput::new(rx))
    }

    /// Sends a frame.
    fn send(&self, text: &str) {
        self.frame(InputFrame::Payload(Bytes::from(text.to_owned())));
    }

    fn frame(&self, frame: InputFrame) {
        let tx = self.tx.as_ref().expect("the client went away");
        tx.try_send(frame).expect("the client's frames are full");
    }

    /// Goes away.
    fn close(&mut self) {
        self.tx = None;
    }
}

/// Options of a Codex client on the Responses WebSocket in `session`, its
/// frames read from `input`.
fn duplex_options(session: &str, input: WebsocketInput) -> Options {
    let mut options = ws_options(session);
    options.source_format = Format::CODEX;
    options.websocket_input = Some(input);
    options
}

/// Starts a call for `auth_id` at `url` with `payload`.
async fn start(
    executor: &CodexExecutor,
    url: &str,
    auth_id: &str,
    payload: &str,
    options: Options,
) -> ChunkStream {
    let mut credential = auth(url);
    credential.id = auth_id.to_owned();
    within(
        "the call to start",
        executor.execute_stream(Arc::new(credential), request(MODEL, payload), options),
    )
    .await
    .unwrap_or_else(|error| panic!("the call failed: {error:?}"))
    .chunks
}

/// Reads the stream to its end, handing each event and its text to `on`:
/// the events' texts, and the error that ended the stream.
async fn run(
    mut chunks: ChunkStream,
    mut on: impl FnMut(&Value, &str),
) -> (Vec<String>, Option<ExecError>) {
    within("the stream to end", async {
        let mut events = Vec::new();
        let mut error = None;
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) => {
                    let text = String::from_utf8_lossy(&chunk).into_owned();
                    on(&json(&text), &text);
                    events.push(text);
                }
                Err(failure) => {
                    assert!(error.is_none(), "a second error: {failure:?}");
                    error = Some(failure);
                }
            }
        }
        (events, error)
    })
    .await
}

/// An event's type and response ID.
fn kind(event: &Value) -> (String, String) {
    (str_at(event, "type"), str_at(event, "response.id"))
}

/// A response event.
fn response(kind: &str, id: &str) -> String {
    format!(r#"{{"type":"{kind}","response":{{"id":"{id}","output":[]}}}}"#)
}

/// The messages Codex read, as JSON.
fn messages(server: &Server) -> Vec<Value> {
    server
        .record()
        .messages
        .iter()
        .map(|text| json(text))
        .collect()
}

/// The API keys of the connections Codex accepted, in order.
fn keys(server: &Server) -> Vec<String> {
    server
        .record()
        .handshakes
        .iter()
        .map(|handshake| {
            handshake
                .header("authorization")
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_owned()
        })
        .collect()
}

/// A manager with the steering executor and `credentials` (ID, API key,
/// priority) for `url`, each serving `model`; the higher priority is
/// picked first.
fn manager(url: &str, model: &str, credentials: &[(&str, &str, &str)]) -> Manager {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, None);
    manager.register_executor(Arc::new(steering()));
    let models = [ModelInfo {
        id: model.to_owned(),
        ..ModelInfo::default()
    }];
    for (id, key, priority) in credentials {
        registry.register_client(id, "codex", &models);
        let mut credential = Auth {
            id: (*id).to_owned(),
            provider: "codex".into(),
            status: Status::Active,
            ..Auth::default()
        };
        for (name, value) in [
            ("api_key", *key),
            ("base_url", url),
            ("websockets", "true"),
            ("priority", *priority),
        ] {
            credential.attributes.insert(name.into(), value.into());
        }
        credential
            .metadata
            .insert("disable_cooling".into(), Value::Bool(false));
        manager.register(credential).unwrap();
    }
    manager
}

/// Starts a call for `model` through `manager`.
async fn start_managed(
    manager: &Manager,
    model: &str,
    session: &str,
    input: WebsocketInput,
) -> ChunkStream {
    let request = request(model, &format!(r#"{{"model":"{model}","input":[]}}"#));
    within(
        "the call to start",
        manager.execute_stream(
            &["codex".to_owned()],
            request,
            duplex_options(session, input),
        ),
    )
    .await
    .unwrap_or_else(|error| panic!("the call failed: {error:?}"))
    .chunks
}

const LIFECYCLE_STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":[{"role":"user","content":[{"type":"input_text","text":"STEER_OK"}]}]}"#;
const LIFECYCLE_ACCEPTED: &str = r#"{"type":"response.steer.accepted","sequence_number":2,"steer":{"id":"s1","previous_response_id":"r1"}}"#;
const LIFECYCLE_PENDING: &str = r#"{"type":"response.steer.pending","sequence_number":8,"steer":{"id":"s2","previous_response_id":"r2"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1","name":"lookup"}]}"#;
const LIFECYCLE_FAILED: &str = r#"{"type":"response.steer.failed","sequence_number":14,"steer":{"id":"s3","previous_response_id":"missing","input":"recover me"},"error":{"code":"response_not_found","message":"missing response"}}"#;

// TestCodexDuplexSteeringLifecycle: Codex can't end the first response
// until it reads the steering, so a call that only reads, then writes,
// would stall. The steering, Codex's answers to it, a tool continuation
// with its own settings and an idle steering message all use the one
// connection.
#[tokio::test]
async fn steering_lifecycle() {
    for boundary in ["response.incomplete", "response.completed"] {
        let server = serve(move |mut peer| async move {
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(&response("response.created", "r1")).await;
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(LIFECYCLE_ACCEPTED).await;
            peer.send(&format!(
                r#"{{"type":"{boundary}","response":{{"id":"r1","output":[],"incomplete_details":{{"reason":"steered"}}}}}}"#
            ))
            .await;
            peer.send(&response("response.created", "r2")).await;
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"r2"}}"#).await;
            peer.send(r#"{"type":"response.completed","response":{"id":"r2","output":[{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"}]}}"#).await;
            peer.send(LIFECYCLE_PENDING).await;
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(&response("response.created", "r3")).await;
            peer.send(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"STEER_OK"}]}}"#).await;
            peer.send(&response("response.completed", "r3")).await;
            // Steering while nothing runs still goes to this connection.
            if peer.recv().await.is_none() {
                return;
            }
            peer.send(LIFECYCLE_FAILED).await;
            peer.hold().await;
        })
        .await;
        let executor = steering();
        let (mut client, input) = Client::new(8);
        let chunks = start(
            &executor,
            &server.url,
            "A",
            r#"{"model":"gpt-6-astra","input":[],"instructions":"Initial settings"}"#,
            duplex_options("duplex-lifecycle", input),
        )
        .await;
        let mut seen = HashSet::new();
        let (_, error) = run(chunks, |event, text| match kind(event) {
            (event, id) if event == "response.created" && id == "r1" => {
                client.send(LIFECYCLE_STEER);
            }
            (event, _) if event == "response.steer.accepted" => {
                if str_at(&json(text), "steer.id") == "s1" {
                    assert_eq!(text, LIFECYCLE_ACCEPTED, "the accepted event changed");
                    seen.insert("accepted");
                }
            }
            (event, id) if event == boundary && id == "r1" => {
                seen.insert("boundary");
            }
            (event, id) if event == "response.created" && id == "r2" => {
                client.send(r#"{"type":"response.steer","previous_response_id":"r2","input":"Use tool result"}"#);
            }
            (event, _) if event == "response.steer.pending" => {
                assert_eq!(text, LIFECYCLE_PENDING, "the pending event changed");
                seen.insert("pending");
                client.send(r#"{"type":"response.create","previous_response_id":"r2","instructions":"New settings","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#);
            }
            (event, id) if event == "response.completed" && id == "r3" => {
                assert_eq!(
                    str_at(&json(text), "response.output.0.content.0.text"),
                    "STEER_OK",
                    "the successor's output is missing or another response's: {text}"
                );
                seen.insert("successor");
                client.send(r#"{"type":"response.steer","previous_response_id":"missing","input":"recover me"}"#);
            }
            (event, _) if event == "response.steer.failed" => {
                assert_eq!(text, LIFECYCLE_FAILED, "the failed event changed");
                seen.insert("failed");
                client.close();
            }
            _ => {}
        })
        .await;
        assert!(error.is_none(), "{boundary}: {error:?}");
        for key in ["accepted", "boundary", "pending", "successor", "failed"] {
            assert!(seen.contains(key), "{boundary}: missing {key}");
        }
        let record = server.wait_closed(1).await;
        assert_eq!(record.handshakes.len(), 1, "{boundary}: one connection");
        assert_eq!(
            record.messages.get(1).map(String::as_str),
            Some(LIFECYCLE_STEER)
        );
        let sent = messages(&server);
        assert_eq!(str_at(&sent[0], "type"), "response.create", "{boundary}");
        assert_eq!(str_at(&sent[0], "instructions"), "Initial settings");
        assert_eq!(str_at(&sent[2], "type"), "response.steer", "{boundary}");
        let create = &sent[3];
        assert_eq!(str_at(create, "type"), "response.create", "{create}");
        assert_eq!(str_at(create, "previous_response_id"), "r2", "{create}");
        assert_eq!(str_at(create, "input.0.call_id"), "c1", "{create}");
        assert_eq!(str_at(create, "instructions"), "New settings", "{create}");
        assert_eq!(str_at(&sent[4], "previous_response_id"), "missing");
        assert!(lock_free(&executor, "duplex-lifecycle").await, "{boundary}");
    }
}

// TestCodexDuplexAppendInheritsContextAndInstructions: an append without
// `previous_response_id`, `model` or `instructions` continues the last
// response with the first request's model and instructions.
#[tokio::test]
async fn append_inherits_context_and_instructions() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "resp-1")).await;
        peer.send(&response("response.completed", "resp-1")).await;
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "resp-2")).await;
        peer.send(&response("response.completed", "resp-2")).await;
        peer.hold().await;
    })
    .await;
    let (mut client, input) = Client::new(4);
    let chunks = start(
        &steering(),
        &server.url,
        "append-test-auth",
        r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"turn 1"}],"instructions":"Initial system instructions"}"#,
        duplex_options("append-test", input),
    )
    .await;
    let (_, error) = run(chunks, |event, _| match kind(event) {
        (event, id) if event == "response.completed" && id == "resp-1" => {
            client
                .send(r#"{"type":"response.append","input":[{"role":"user","content":"turn 2"}]}"#);
        }
        (event, id) if event == "response.completed" && id == "resp-2" => client.close(),
        _ => {}
    })
    .await;
    assert!(error.is_none(), "{error:?}");
    server.wait_closed(1).await;
    let sent = messages(&server);
    assert_eq!(
        str_at(&sent[0], "instructions"),
        "Initial system instructions"
    );
    let second = &sent[1];
    assert_eq!(str_at(second, "type"), "response.create", "{second}");
    assert_eq!(str_at(second, "previous_response_id"), "resp-1", "{second}");
    assert_eq!(str_at(second, "model"), MODEL, "{second}");
    assert_eq!(
        str_at(second, "instructions"),
        "Initial system instructions",
        "{second}"
    );
}

// TestCodexDuplexStandaloneCreateDoesNotInheritParentID: a create without
// `previous_response_id` replaces the history and gains none.
#[tokio::test]
async fn standalone_create_does_not_inherit_parent_id() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "resp-1")).await;
        peer.send(&response("response.completed", "resp-1")).await;
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "resp-2")).await;
        peer.send(&response("response.completed", "resp-2")).await;
        peer.hold().await;
    })
    .await;
    let (mut client, input) = Client::new(4);
    let chunks = start(
        &steering(),
        &server.url,
        "standalone-create-test",
        r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"turn 1"}]}"#,
        duplex_options("standalone-test", input),
    )
    .await;
    let (_, error) = run(chunks, |event, _| match kind(event) {
        (event, id) if event == "response.completed" && id == "resp-1" => {
            client.send(r#"{"type":"response.create","model":"gpt-6-astra","input":[{"role":"user","content":"turn 2 standalone"}]}"#);
        }
        (event, id) if event == "response.completed" && id == "resp-2" => client.close(),
        _ => {}
    })
    .await;
    assert!(error.is_none(), "{error:?}");
    server.wait_closed(1).await;
    let second = &messages(&server)[1];
    assert_eq!(str_at(second, "type"), "response.create", "{second}");
    assert!(!exists(second, "previous_response_id"), "{second}");
}

// TestCodexDuplexQueuedCreateDoesNotBlockSubsequentSteer: a create waiting
// for an automatic successor to end doesn't hold back the steering sent
// after it.
#[tokio::test]
async fn queued_create_does_not_block_subsequent_steer() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "r1")).await;
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#).await;
        peer.send(r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"incomplete_details":{"reason":"steered"}}}"#).await;
        peer.send(r#"{"type":"response.created","response":{"id":"auto-1","previous_response_id":"r1","output":[]}}"#).await;
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"auto-1"}}"#).await;
        peer.send(&response("response.completed", "auto-1")).await;
        peer.send(r#"{"type":"response.created","response":{"id":"auto-2","previous_response_id":"auto-1","output":[]}}"#).await;
        peer.send(&response("response.completed", "auto-2")).await;
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "r2")).await;
        peer.send(&response("response.completed", "r2")).await;
        peer.hold().await;
    })
    .await;
    let (mut client, input) = Client::new(8);
    let chunks = start(
        &steering(),
        &server.url,
        "steer-blocking-test",
        r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"start"}]}"#,
        duplex_options("steer-block-test", input),
    )
    .await;
    let (_, error) = run(chunks, |event, _| match kind(event) {
        (event, id) if event == "response.created" && id == "r1" => {
            client.send(r#"{"type":"response.steer","previous_response_id":"r1","input":"steer 1"}"#);
        }
        (event, id) if event == "response.created" && id == "auto-1" => {
            client.send(r#"{"type":"response.create","model":"gpt-6-astra","previous_response_id":"auto-1","input":[{"role":"user","content":"queued next"}]}"#);
            client.send(r#"{"type":"response.steer","previous_response_id":"auto-1","input":"steer 2 in flight"}"#);
        }
        (event, id) if event == "response.completed" && id == "r2" => client.close(),
        _ => {}
    })
    .await;
    assert!(error.is_none(), "{error:?}");
    server.wait_closed(1).await;
    let sent = messages(&server);
    let types: Vec<String> = sent.iter().map(|message| str_at(message, "type")).collect();
    assert_eq!(
        types,
        [
            "response.create",
            "response.steer",
            "response.steer",
            "response.create"
        ],
        "the steering must go before the waiting create"
    );
    assert_eq!(str_at(&sent[2], "input"), "steer 2 in flight");
}

// TestCodexDuplexBootstrapPreservesFollowup: the client's next frame,
// already waiting when the first credential is refused, isn't read on the
// refused connection; the manager's next credential sends it.
#[tokio::test]
async fn bootstrap_preserves_followup() {
    const BOOTSTRAP_MODEL: &str = "duplex-bootstrap-followup-model";
    for frame_type in ["response.steer", "response.create"] {
        let refused_followups = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&refused_followups);
        let server = serve(move |mut peer| {
            let counter = Arc::clone(&counter);
            async move {
                let bad = peer.handshake().header("authorization") == Some("Bearer bad-key");
                if peer.recv().await.is_none() {
                    return;
                }
                if bad {
                    peer.send(r#"{"type":"error","status":401,"error":{"type":"authentication_error","message":"expired credential"}}"#).await;
                    // The call must close this connection, sending nothing more.
                    while peer.recv().await.is_some() {
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                    return;
                }
                peer.send(&response("response.created", "healthy")).await;
                if peer.recv().await.is_none() {
                    return;
                }
                peer.send(&response("response.completed", "healthy")).await;
                peer.hold().await;
            }
        })
        .await;
        let manager = manager(
            &server.url,
            BOOTSTRAP_MODEL,
            &[
                ("followup-bad", "bad-key", "4"),
                ("followup-good", "good-key", "3"),
            ],
        );
        let (mut client, input) = Client::new(1);
        client.send(&format!(
            r#"{{"type":"{frame_type}","input":[{{"role":"user","content":[{{"type":"input_text","text":"PRESERVE_ME"}}]}}]}}"#
        ));
        let chunks = start_managed(&manager, BOOTSTRAP_MODEL, "bootstrap-followup", input).await;
        let mut completed = false;
        let (_, error) = run(chunks, |event, _| {
            if kind(event).0 == "response.completed" {
                completed = true;
                client.close();
            }
        })
        .await;
        assert!(error.is_none(), "{frame_type}: {error:?}");
        assert!(
            completed,
            "{frame_type}: the healthy credential never completed"
        );
        server.wait_closed(1).await;
        assert_eq!(keys(&server), ["bad-key", "good-key"], "{frame_type}");
        assert_eq!(refused_followups.load(Ordering::SeqCst), 0, "{frame_type}");
        let sent = messages(&server);
        assert_eq!(sent.len(), 3, "{frame_type}: {sent:?}");
        let followup = &sent[2];
        assert_eq!(str_at(followup, "type"), frame_type, "{followup}");
        assert_eq!(
            str_at(followup, "input.0.content.0.text"),
            "PRESERVE_ME",
            "{followup}"
        );
    }
}

// TestCodexDuplexBootstrapCancellationPreservesInput: a call dropped
// before `response.created` reads none of the client's frames and closes
// its connection.
#[tokio::test]
async fn bootstrap_cancellation_preserves_input() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_some() {
            // No `response.created`: dropping the call must end this.
            peer.hold().await;
        }
    })
    .await;
    let (client, input) = Client::new(1);
    client.send(r#"{"type":"response.steer","input":[]}"#);
    let chunks = start(
        &steering(),
        &server.url,
        "bootstrap-cancel",
        r#"{"model":"bootstrap-cancel-model","input":[]}"#,
        duplex_options("bootstrap-cancel", input.clone()),
    )
    .await;
    server
        .wait_for("the first request", |record| record.messages.len() == 1)
        .await;
    drop(chunks);
    server.wait_closed(1).await;
    match within("the client's frame", input.recv()).await {
        Some(InputFrame::Payload(payload)) => {
            assert_eq!(payload, r#"{"type":"response.steer","input":[]}"#);
        }
        _ => panic!("the follow-up was read before response.created"),
    }
}

// TestCodexDuplexLaterCredentialFailure: Codex refusing the credential
// after the first response started is passed on, then ends the stream
// with an error the credential answers for; nothing is sent again.
#[tokio::test]
async fn later_credential_failure() {
    const LATER_MODEL: &str = "later-credential-model";
    for event_type in ["error", "response.failed"] {
        for status in [401_u16, 403, 429] {
            for queued in [false, true] {
                let row = format!("{event_type}/{status}/queued={queued}");
                let server = serve(move |mut peer| async move {
                    if peer.recv().await.is_none() {
                        return;
                    }
                    peer.send(&response("response.created", "started")).await;
                    if queued {
                        if peer.recv().await.is_none() {
                            return;
                        }
                    } else {
                        peer.send(&response("response.completed", "started")).await;
                    }
                    let error_type = match status {
                        403 => "permission_error",
                        429 => "usage_limit_reached",
                        _ => "authentication_error",
                    };
                    let body = format!(
                        r#"{{"type":"{error_type}","status":{status},"message":"credential rejected","resets_in_seconds":3600}}"#
                    );
                    if event_type == "error" {
                        peer.send(&format!(r#"{{"type":"error","status":{status},"headers":{{"X-Request-Id":"later-rejection"}},"error":{body}}}"#)).await;
                    } else {
                        peer.send(&format!(r#"{{"type":"response.failed","response":{{"id":"started","error":{body}}}}}"#)).await;
                    }
                    // The call must close this connection itself.
                    peer.hold().await;
                })
                .await;
                let manager = manager(
                    &server.url,
                    LATER_MODEL,
                    &[("later-bad", "test", "4"), ("later-good", "unused", "3")],
                );
                let (client, input) = Client::new(1);
                let chunks = start_managed(&manager, LATER_MODEL, "later-credential", input).await;
                let before = Utc::now();
                let mut forwarded = false;
                let (_, error) = run(chunks, |event, _| {
                    let (kind, _) = kind(event);
                    if kind == "response.created" && queued {
                        client.send(r#"{"type":"response.create","input":[]}"#);
                    }
                    if kind == event_type {
                        forwarded = true;
                    }
                })
                .await;
                assert!(forwarded, "{row}: the failure wasn't passed on");
                let error = error.unwrap_or_else(|| panic!("{row}: no terminal error"));
                assert_eq!(error.status, status, "{row}: {error:?}");
                assert!(!error.request_scoped, "{row}: {error:?}");

                let bad = manager.get("later-bad").unwrap();
                let state = bad.model_states.get(LATER_MODEL);
                assert!(
                    state.is_some_and(|state| state.unavailable
                        && state
                            .last_error
                            .as_ref()
                            .is_some_and(|error| error.http_status == status)),
                    "{row}: the failure wasn't recorded: {state:?}"
                );
                if status == 429 {
                    assert_eq!(bad.quota.reason, "credential_quota", "{row}");
                    let recover = bad.quota.next_recover_at.expect("no recovery time");
                    assert!(
                        recover >= before + TimeDelta::hours(1) - TimeDelta::seconds(5),
                        "{row}: {recover}"
                    );
                }
                let good = manager.get("later-good").unwrap();
                assert!(!good.unavailable && good.last_error.is_none(), "{row}");
                server.wait_closed(1).await;
                assert_eq!(keys(&server), ["test"], "{row}: the stream was replayed");
            }
        }
    }
}

// TestCodexDuplexConnectionTimeoutDoesNotCoolHealthyAccount: the client's
// socket failing after a response ends the stream with an error that
// leaves the credential alone.
#[tokio::test]
async fn connection_timeout_does_not_cool_healthy_account() {
    const HEALTH_MODEL: &str = "duplex-health-model";
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(r#"{"type":"response.created","response":{"id":"r1"}}"#)
            .await;
        peer.send(&response("response.completed", "r1")).await;
        peer.hold().await;
    })
    .await;
    let manager = manager(
        &server.url,
        HEALTH_MODEL,
        &[("duplex-health-account", "test-key", "0")],
    );
    let (client, input) = Client::new(1);
    let chunks = start_managed(&manager, HEALTH_MODEL, "duplex-health-session", input).await;
    let mut completed = false;
    let (_, error) = run(chunks, |event, _| {
        if kind(event).0 == "response.completed" {
            completed = true;
            let mut timeout = ExecError::new(ErrorKind::Upstream, "read tcp: i/o timeout");
            timeout.transport = Some(TransportFault::Transient);
            client.frame(InputFrame::Err(timeout));
        }
    })
    .await;
    assert!(completed);
    let error = error.expect("no error");
    assert!(error.message.contains("i/o timeout"), "{error:?}");
    assert!(error.request_scoped, "{error:?}");
    let current = manager.get("duplex-health-account").unwrap();
    assert!(!current.unavailable, "the timeout cooled the credential");
    assert!(
        current
            .model_states
            .get(HEALTH_MODEL)
            .is_none_or(|state| !state.unavailable),
        "the timeout cooled the model"
    );
    server.wait_closed(1).await;
}

/// An initial refusal of upstream's `TestCodexDuplexInitialFailure`.
struct InitialFailure {
    name: &'static str,
    payload: &'static str,
    status: u16,
    quota: bool,
    headers: bool,
}

const INITIAL_FAILURES: [InitialFailure; 4] = [
    InitialFailure {
        name: "response_failed_auth",
        payload: r#"{"type":"response.failed","response":{"error":{"type":"authentication_error","message":"expired credential"}}}"#,
        status: 401,
        quota: false,
        headers: false,
    },
    InitialFailure {
        name: "response_failed_quota",
        payload: r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}}"#,
        status: 429,
        quota: true,
        headers: false,
    },
    // The top-level status counts even when the error type is generic.
    InitialFailure {
        name: "error_auth",
        payload: r#"{"type":"error","status":401,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"server_error","message":"expired credential"}}"#,
        status: 401,
        quota: false,
        headers: true,
    },
    InitialFailure {
        name: "error_quota",
        payload: r#"{"type":"error","status_code":429,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}"#,
        status: 429,
        quota: true,
        headers: true,
    },
];

/// A mock Codex refusing the API key `bad-key` with `payload` and holding
/// that connection open, and serving any other.
async fn refusing_bad_key(payload: &'static str) -> Server {
    serve(move |mut peer| async move {
        let bad = peer.handshake().header("authorization") == Some("Bearer bad-key");
        if peer.recv().await.is_none() {
            return;
        }
        if bad {
            peer.send(payload).await;
        } else {
            peer.send(&response("response.created", "healthy-response"))
                .await;
            peer.send(&response("response.completed", "healthy-response"))
                .await;
        }
        // The call must close the refused connection itself.
        peer.hold().await;
    })
    .await
}

// TestCodexDuplexInitialFailure, without failover: Codex refusing the
// first request ends the stream with that error, as the credential's, its
// status, quota, wait and headers kept.
#[tokio::test]
async fn initial_failure() {
    for case in INITIAL_FAILURES {
        let name = case.name;
        let server = refusing_bad_key(case.payload).await;
        let (_client, input) = Client::new(1);
        let mut credential = auth(&server.url);
        credential.id = "duplex-initial-bad".into();
        credential
            .attributes
            .insert("api_key".into(), "bad-key".into());
        let response = within(
            "the call to start",
            steering().execute_stream(
                Arc::new(credential),
                request(
                    "duplex-initial-failure-model",
                    r#"{"model":"duplex-initial-failure-model","input":[]}"#,
                ),
                duplex_options("duplex-initial-failure", input),
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let mut chunks = response.chunks;
        let error = match within("the first chunk", chunks.next()).await {
            Some(Err(error)) => error,
            other => panic!("{name}: the refusal must be an error chunk: {other:?}"),
        };
        assert_eq!(error.status, case.status, "{name}: {error:?}");
        assert!(!error.request_scoped, "{name}: {error:?}");
        if case.quota {
            assert!(error.credential_scoped, "{name}: {error:?}");
            assert_eq!(error.retry_after, Some(Duration::from_secs(3600)), "{name}");
        }
        if case.headers {
            assert_eq!(
                error
                    .headers
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok()),
                Some("initial-rejection"),
                "{name}: {error:?}"
            );
        }
        assert!(
            within("the stream to end", chunks.next()).await.is_none(),
            "{name}: the refused stream stayed open"
        );
        server.wait_closed(1).await;
        assert_eq!(keys(&server), ["bad-key"], "{name}");
    }
}

// TestCodexDuplexInitialFailure, with failover: the manager records the
// refusal against the first credential and the next one serves the call.
#[tokio::test]
async fn initial_failure_fails_over() {
    const INITIAL_MODEL: &str = "duplex-initial-failure-model";
    for case in INITIAL_FAILURES {
        let name = case.name;
        let server = refusing_bad_key(case.payload).await;
        let manager = manager(
            &server.url,
            INITIAL_MODEL,
            &[
                ("duplex-initial-bad", "bad-key", "4"),
                ("duplex-initial-good", "good-key", "3"),
            ],
        );
        let (mut client, input) = Client::new(1);
        let before = Utc::now();
        let chunks = start_managed(&manager, INITIAL_MODEL, "duplex-initial-failover", input).await;
        let mut completed = false;
        let (_, error) = run(chunks, |event, text| match kind(event) {
            (event, id) if event == "response.completed" => {
                completed = id == "healthy-response";
                client.close();
            }
            (event, _) if event == "response.created" => {}
            _ => panic!("{name}: the refused credential's payload got out: {text}"),
        })
        .await;
        assert!(error.is_none(), "{name}: {error:?}");
        assert!(completed, "{name}: the healthy credential never completed");
        let bad = manager.get("duplex-initial-bad").unwrap();
        let state = bad.model_states.get(INITIAL_MODEL);
        assert!(
            state.is_some_and(|state| state.unavailable
                && state
                    .last_error
                    .as_ref()
                    .is_some_and(|error| error.http_status == case.status)),
            "{name}: the refusal wasn't recorded: {state:?}"
        );
        if case.quota {
            assert_eq!(bad.quota.reason, "credential_quota", "{name}");
            let recover = bad.quota.next_recover_at.expect("no recovery time");
            assert!(
                recover >= before + TimeDelta::hours(1) - TimeDelta::seconds(5),
                "{name}: {recover}"
            );
        }
        server.wait_closed(2).await;
        assert_eq!(keys(&server), ["bad-key", "good-key"], "{name}");
    }
}

// TestCodexDuplexRejectedCreateMetadata: a failure is told to the request
// it belongs to, whether a queued create was refused or the running
// response failed; one that can't be told apart ends the stream. Codex
// answers only once it read every create.
#[tokio::test]
async fn rejected_create_metadata() {
    for (name, active_failure, ambiguous) in [
        ("rejected_create", false, false),
        ("active_failure", true, false),
        ("ambiguous_failure", true, true),
    ] {
        let server = serve(move |mut peer| async move {
            let event = |kind: &str, id: &str, key: &str| {
                format!(
                    r#"{{"type":"{kind}","response":{{"id":"{id}","prompt_cache_key":"{key}","output":[],"error":{{"type":"invalid_request_error","message":"rejected"}}}}}}"#
                )
            };
            let Some(text) = peer.recv().await else {
                return;
            };
            let first_key = str_at(&json(&text), "prompt_cache_key");
            peer.send(&event("response.created", "first", &first_key))
                .await;
            let (mut rejected_key, mut rejected_id) = (first_key.clone(), "first");
            if !active_failure {
                peer.send(&event("response.completed", "first", &first_key))
                    .await;
                let Some(text) = peer.recv().await else {
                    return;
                };
                rejected_key = str_at(&json(&text), "prompt_cache_key");
                rejected_id = "rejected";
            }
            let Some(text) = peer.recv().await else {
                return;
            };
            let good_key = str_at(&json(&text), "prompt_cache_key");
            if ambiguous {
                peer.send(&event("response.failed", "", &rejected_key)).await;
                peer.hold().await;
                return;
            }
            peer.send(&event("response.failed", rejected_id, &rejected_key))
                .await;
            peer.send(&event("response.created", "good", &good_key)).await;
            peer.send(&event("response.completed", "good", &good_key))
                .await;
            peer.hold().await;
        })
        .await;
        let create = |key: &str| {
            format!(
                r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","input":[]}}"#
            )
        };
        let (mut client, input) = Client::new(2);
        let chunks = start(
            &steering(),
            &server.url,
            "metadata-account",
            &create("first-key"),
            duplex_options("rejected-create-metadata", input),
        )
        .await;
        let (mut sent, mut failed, mut completed) = (false, false, false);
        let (_, error) = run(chunks, |event, text| {
            let (kind, id) = kind(event);
            if !sent
                && id == "first"
                && ((active_failure && kind == "response.created")
                    || (!active_failure && kind == "response.completed"))
            {
                sent = true;
                if !active_failure {
                    client.send(&create("rejected-key"));
                }
                client.send(&create("good-key"));
            }
            if kind == "response.failed" {
                failed = true;
                if !ambiguous {
                    let want = if active_failure {
                        "first-key"
                    } else {
                        "rejected-key"
                    };
                    assert_eq!(
                        str_at(event, "response.prompt_cache_key"),
                        want,
                        "{name}: {text}"
                    );
                }
            }
            if kind == "response.completed" && id == "good" {
                completed = true;
                assert_eq!(str_at(event, "response.prompt_cache_key"), "good-key");
                client.close();
            }
        })
        .await;
        assert!(failed, "{name}");
        if ambiguous {
            let error = error.unwrap_or_else(|| panic!("{name}: no error"));
            assert!(error.request_scoped, "{name}: {error:?}");
            assert!(!completed, "{name}");
        } else {
            assert!(error.is_none(), "{name}: {error:?}");
            assert!(completed, "{name}");
        }
        server.wait_closed(1).await;
    }
}

// TestCodexDuplexLaterInvalidSignatureClearsReplay: a later create Codex
// refuses for its reasoning signature clears that create's own replay,
// started or not, and leaves the other sessions' alone.
#[tokio::test]
async fn later_invalid_signature_clears_replay() {
    let encrypted = valid_encrypted_content(51);
    for failure in ["response.failed", "error", "error_without_status"] {
        for started in [false, true] {
            let row = format!("{failure}/started={started}");
            let server = serve(move |mut peer| async move {
                if peer.recv().await.is_none() {
                    return;
                }
                peer.send(&response("response.created", "first")).await;
                peer.send(&response("response.completed", "first")).await;
                if peer.recv().await.is_none() {
                    return;
                }
                if started {
                    peer.send(&response("response.created", "rejected")).await;
                }
                peer.send(match failure {
                    "response.failed" => r#"{"type":"response.failed","response":{"id":"rejected","error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}}"#,
                    "error" => r#"{"type":"error","status":400,"body":{"error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}}"#,
                    _ => r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}"#,
                })
                .await;
                if peer.recv().await.is_none() {
                    return;
                }
                peer.send(&response("response.created", "corrected")).await;
                peer.send(&response("response.completed", "corrected")).await;
                peer.hold().await;
            })
            .await;
            // The sessions are this row's own: the cache is the process's.
            let session = |name: &str| format!("duplex-{failure}-{started}-{name}");
            let scope = |name: &str| format!("claude:{}:agent:main", session(name));
            let item = json!({"type": "reasoning", "summary": [], "encrypted_content": encrypted});
            for name in ["first", "rejected", "unrelated"] {
                assert!(ReplayCache::global().store(MODEL, &scope(name), &[&item]));
            }
            let create = |name: &str| {
                let user = json!({"session_id": session(name)}).to_string();
                json!({
                    "type": "response.create",
                    "model": MODEL,
                    "metadata": {"user_id": user},
                    "messages": [{"role": "user", "content": "continue"}],
                })
                .to_string()
            };
            let (mut client, input) = Client::new(1);
            let mut options = duplex_options("later-invalid-signature", input);
            options.source_format = Format::CLAUDE;
            let chunks = start(
                &steering(),
                &server.url,
                "replay-account",
                &create("first"),
                options,
            )
            .await;
            let (mut failed, mut completed) = (false, false);
            let (_, error) = run(chunks, |event, _| {
                let (kind, id) = kind(event);
                if kind == "response.completed" && id == "first" {
                    client.send(&create("rejected"));
                }
                if kind == "response.failed" || kind == "error" {
                    failed = true;
                    assert_eq!(
                        ReplayCache::global().get_item(MODEL, &scope("rejected")),
                        None,
                        "{row}: the refused session kept its reasoning"
                    );
                    for name in ["first", "unrelated"] {
                        assert!(
                            ReplayCache::global()
                                .get_item(MODEL, &scope(name))
                                .is_some(),
                            "{row}: cleared {name}"
                        );
                    }
                    client.send(&create("rejected"));
                }
                if kind == "response.completed" && id == "corrected" {
                    completed = true;
                    client.close();
                }
            })
            .await;
            assert!(error.is_none(), "{row}: {error:?}");
            assert!(
                failed && completed,
                "{row}: failed={failed} completed={completed}"
            );
            server.wait_closed(1).await;
            let record = server.record();
            let corrected = record.messages.get(2).expect("no corrected create");
            assert!(
                !corrected.contains(&encrypted),
                "{row}: the corrected create sent the refused reasoning again"
            );
        }
    }
}

// TestCodexDuplexAutomaticSuccessorMetadata: an automatic successor runs
// with the steered response's settings, and the explicit create that
// waited for it with its own.
#[tokio::test]
async fn automatic_successor_metadata() {
    for scenario in [
        "completed",
        "incomplete",
        "early_tool_result",
        "failed_steering",
        "multiple_steers",
        "create_before_steer",
    ] {
        let (queued_tx, queued_rx) = watch::channel(false);
        let (before_steer_tx, before_steer_rx) = watch::channel(false);
        let server = serve(move |mut peer| {
            let (mut queued, mut before_steer) = (queued_rx.clone(), before_steer_rx.clone());
            async move {
                let event = |kind: &str, id: &str, key: &str| {
                    let parent = if id == "automatic" { "first" } else { "" };
                    format!(
                        r#"{{"type":"{kind}","response":{{"id":"{id}","previous_response_id":"{parent}","prompt_cache_key":"{key}","output":[],"incomplete_details":{{"reason":"steered"}}}}}}"#
                    )
                };
                let Some(first) = peer.recv().await else {
                    return;
                };
                let first_key = str_at(&json(&first), "prompt_cache_key");
                peer.send(&event("response.created", "first", &first_key)).await;
                if scenario == "create_before_steer" {
                    let Some(middle) = peer.recv().await else {
                        return;
                    };
                    let middle = json(&middle);
                    if str_at(&middle, "type") != "response.create" {
                        return;
                    }
                    let _ = before_steer.wait_for(|ready| *ready).await;
                    let middle_key = str_at(&middle, "prompt_cache_key");
                    peer.send(&event("response.completed", "first", &first_key)).await;
                    peer.send(&event("response.created", "middle", &middle_key)).await;
                    peer.send(&event("response.completed", "middle", &middle_key)).await;
                }
                if peer.recv().await.is_none() {
                    return;
                }
                peer.send(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"first"}}"#).await;
                if scenario == "multiple_steers" {
                    if peer.recv().await.is_none() {
                        return;
                    }
                    peer.send(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"first"}}"#).await;
                    peer.send(r#"{"type":"response.steer.failed","steer":{"id":"s2","previous_response_id":"first","input":"second"},"error":{"code":"invalid_input","message":"second failed"}}"#).await;
                }
                let _ = queued.wait_for(|ready| *ready).await;
                if scenario != "create_before_steer" {
                    let boundary = if scenario == "incomplete" {
                        "response.incomplete"
                    } else {
                        "response.completed"
                    };
                    peer.send(&event(boundary, "first", &first_key)).await;
                }
                match scenario {
                    "early_tool_result" => peer.send(r#"{"type":"response.steer.pending","steer":{"id":"s1","previous_response_id":"first"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1"}]}"#).await,
                    "failed_steering" => peer.send(r#"{"type":"response.steer.failed","steer":{"id":"s1","previous_response_id":"first","input":"first"},"error":{"code":"successor_creation_failed","message":"failed"}}"#).await,
                    _ => {
                        peer.send(&event("response.created", "automatic", &first_key)).await;
                        peer.send(&event("response.completed", "automatic", &first_key)).await;
                    }
                }
                let Some(next) = peer.recv().await else {
                    return;
                };
                let next_key = str_at(&json(&next), "prompt_cache_key");
                peer.send(&event("response.created", "explicit", &next_key)).await;
                peer.send(&event("response.completed", "explicit", &next_key)).await;
                peer.hold().await;
            }
        })
        .await;
        let create = |key: &str| {
            format!(
                r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","previous_response_id":"first","input":[]}}"#
            )
        };
        let (mut client, input) = Client::new(4);
        let chunks = start(
            &steering(),
            &server.url,
            scenario,
            &create("first-key"),
            duplex_options(&format!("successor-{scenario}"), input),
        )
        .await;
        let (mut queued_create, mut automatic, mut explicit) = (false, false, false);
        let (_, error) = run(chunks, |event, text| {
            let (kind, id) = kind(event);
            if kind == "response.created" && id == "first" {
                if scenario == "create_before_steer" {
                    client.send(&create("middle-key"));
                }
                client.send(r#"{"type":"response.steer","previous_response_id":"first","input":"first"}"#);
                if scenario == "create_before_steer" {
                    before_steer_tx.send_replace(true);
                }
            }
            if kind == "response.steer.accepted" {
                if scenario == "multiple_steers" && str_at(event, "steer.id") == "s1" {
                    client.send(r#"{"type":"response.steer","previous_response_id":"first","input":"second"}"#);
                } else if !queued_create {
                    queued_create = true;
                    client.send(&create("explicit-key"));
                    queued_tx.send_replace(true);
                }
            }
            if kind == "response.created" || kind == "response.completed" {
                let want = match id.as_str() {
                    "explicit" => "explicit-key",
                    "middle" => "middle-key",
                    _ => "first-key",
                };
                assert_eq!(
                    str_at(event, "response.prompt_cache_key"),
                    want,
                    "{scenario}: {text}"
                );
                if id == "automatic" && kind == "response.completed" {
                    automatic = true;
                }
                if id == "explicit" && kind == "response.completed" {
                    explicit = true;
                    client.close();
                }
            }
        })
        .await;
        assert!(error.is_none(), "{scenario}: {error:?}");
        let want_automatic = !matches!(scenario, "early_tool_result" | "failed_steering");
        assert!(explicit, "{scenario}: the explicit create never completed");
        assert_eq!(automatic, want_automatic, "{scenario}");
        server.wait_closed(1).await;
    }
}

// Added: a frame that isn't JSON, or isn't a request the duplex takes,
// gets a local 400 `error` event, and the socket goes on.
#[tokio::test]
async fn local_errors_leave_the_socket_open() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "r1")).await;
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.completed", "r1")).await;
        peer.hold().await;
    })
    .await;
    let (mut client, input) = Client::new(4);
    let chunks = start(
        &steering(),
        &server.url,
        "local-errors",
        r#"{"model":"gpt-6-astra","input":[]}"#,
        duplex_options("local-errors", input),
    )
    .await;
    let mut errors = Vec::new();
    let (_, error) = run(chunks, |event, text| match kind(event) {
        (event, _) if event == "response.created" => {
            client.send("not json");
            client.send(r#"{"type":"response.cancel"}"#);
            client.send(r#"{"type":"response.steer","previous_response_id":"r1","input":"go on"}"#);
        }
        (event, _) if event == "error" => errors.push(text.to_owned()),
        (event, _) if event == "response.completed" => client.close(),
        _ => {}
    })
    .await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        errors,
        [
            r#"{"error":{"message":"invalid websocket request JSON","type":"invalid_request_error"},"status":400,"type":"error"}"#,
            r#"{"error":{"message":"unsupported websocket request type: response.cancel","type":"invalid_request_error"},"status":400,"type":"error"}"#,
        ]
    );
    server.wait_closed(1).await;
    assert_eq!(server.record().messages.len(), 2);
}

// Added: the steering waiting on Codex is bounded (upstream's isn't); one
// more ends the stream with an error that leaves the credential usable.
#[tokio::test]
async fn outstanding_steers_are_bounded() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "r1")).await;
        // Never answers the steering.
        peer.hold().await;
    })
    .await;
    let limit = super::super::duplex::MAX_OUTSTANDING_STEERS;
    let (client, input) = Client::new(limit + 1);
    let chunks = start(
        &steering(),
        &server.url,
        "bounded-steers",
        r#"{"model":"gpt-6-astra","input":[]}"#,
        duplex_options("bounded-steers", input),
    )
    .await;
    let (_, error) = run(chunks, |event, _| {
        if kind(event).0 == "response.created" {
            for _ in 0..=limit {
                client.send(
                    r#"{"type":"response.steer","previous_response_id":"r1","input":"more"}"#,
                );
            }
        }
    })
    .await;
    let error = error.expect("no error");
    assert!(error.request_scoped, "{error:?}");
    assert!(error.message.contains("response.steer"), "{error:?}");
    server.wait_closed(1).await;
    assert_eq!(server.record().messages.len(), limit + 1);
}

// Added: a create for another model can't go on this connection; the
// stream ends asking for the request to be sent again over HTTP.
#[tokio::test]
async fn another_model_requires_replay() {
    let server = serve(|mut peer| async move {
        if peer.recv().await.is_none() {
            return;
        }
        peer.send(&response("response.created", "r1")).await;
        peer.send(&response("response.completed", "r1")).await;
        peer.hold().await;
    })
    .await;
    let (client, input) = Client::new(1);
    let chunks = start(
        &steering(),
        &server.url,
        "another-model",
        r#"{"model":"gpt-6-astra","input":[]}"#,
        duplex_options("another-model", input),
    )
    .await;
    let (_, error) = run(chunks, |event, _| {
        if kind(event).0 == "response.completed" {
            client.send(r#"{"type":"response.create","model":"gpt-5.6-sol","input":[]}"#);
        }
    })
    .await;
    let error = error.expect("no error");
    assert_eq!(error.status, 426, "{error:?}");
    assert!(error.request_scoped, "{error:?}");
    server.wait_closed(1).await;
    assert_eq!(server.record().messages.len(), 1);
}
