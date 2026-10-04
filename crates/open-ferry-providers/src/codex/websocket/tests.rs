// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_executor_test.go,
// codex_websockets_executor_store_test.go, websocket_proxy_reuse_test.go,
// websocket_session_target_test.go, websocket_lifecycle_bind_test.go and
// codex_websockets_spawn_agent_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses WebSocket upstream against a mock Codex on 127.0.0.1 (see
//! [`super::mock`]), ported from upstream's WebSocket tests where they test
//! what is ported. The calls are in this file, the sessions in
//! [`sessions`], connecting in [`proxy`], and keeping the credential's
//! secret out of failures in [`secrets`].
//!
//! Upstream's tests call its WebSocket executor, which takes any client;
//! here [`CodexExecutor`] only takes the WebSocket route for a client on the
//! Responses WebSocket, so a test of a client that isn't calls
//! [`super::execute`] or [`super::execute_stream`] directly.
//!
//! Dropped, with why:
//! - The execution lifecycle tests (`websocket_lifecycle_bind_test.go`'s
//!   `TestCodexWebsocketSessionBindsSameLifecycleAndConnectionOnce`;
//!   `TestWebsocketSessionCloseEndsRetainedLifecycleOnce`,
//!   `TestSessionlessWebsocketSelectionEndAndDirectCloseRaceClosesOnce`,
//!   `TestWebsocketDrainDuringBindClosesOwnedConnectionOnce`,
//!   `TestWebsocketTargetReplacementPhysicallyClosesOwnedConnectionOnce`,
//!   `TestWebsocketLifecycleEndThenInvalidateAndCloseAllPhysicallyClosesOnce`,
//!   `TestAuditAccountedCodexXAIReconnectReuseAndTargetChange`,
//!   `TestHomeSelectionRegistryDrainClosesRealWebsocketSessions`,
//!   `TestAuditHomeCodex426WebsocketToHTTPFreshSelection` and
//!   `TestWebsocketRegistryDrainClosesAndEndsRetainedSession`;
//!   `TestCodexWebsocketUpgradeRequiredDoesNotFallbackToHTTPWithLifecycle`,
//!   `TestCodexWebsocketTerminalFailureInvalidatesRetainedLifecycle`,
//!   `TestCodexWebsocketNonstreamLifecycleBindFailureDetachesConnection` and
//!   `TestCodexWebsocketLifecycleBindFailureReleasesSessionRequestLock`):
//!   the execution lifecycle, its registry and Home aren't ported.
//!   `TestWebsocketRetryBindFailureClearsActiveSessionState` becomes
//!   `send_on_a_stale_connection_is_tried_once_more`, without the
//!   lifecycle.
//! - `TestCodexAutoExecutorRequiredUpstreamWebsocketRejectsHTTPFallback`,
//!   `TestCodexWebsocketUpgradeFallbackLocalErrorDoesNotMarkUpstreamAttempt`
//!   and `TestCodexWebsocketMissingRequiredSessionDoesNotMarkUpstreamAttempt`:
//!   `RequiredUpstreamWebsocket` and upstream attempt markers aren't ported.
//! - `TestCodexWebsocketsUpstreamDisconnectChanSignalsOnInvalidate`:
//!   `UpstreamDisconnectChan` isn't ported.
//! - `TestApplyCodexWebsocketHeadersDefaultsToCodexCloaking`,
//!   `...NativeSessionCombinations`, `...UsesConfigDefaultsForOAuth`,
//!   `...PrefersExistingHeadersOverClientAndConfig`,
//!   `...ConfigUserAgentOverridesClientHeader`,
//!   `...IgnoresConfigForAPIKeyAuth`, the five
//!   `TestApplyCodexPromptCacheHeaders*`, the five HTTP
//!   `TestApplyCodexHeaders*` and the two `TestApplyModelHeaderOverrides*`:
//!   the headers they test are made up (cloaking, `codex-header-defaults`,
//!   prompt cache sessions, model header overrides), and none is sent; the
//!   HTTP route's own headers are tested in `codex/executor/tests.rs`.
//! - `TestCodexWebsocketsExecuteObservesWebSocketResponseEvents` and
//!   `TestCodexWebsocketsExecuteStreamObservesWebSocketResponseEvents`:
//!   they test `Options.WebSocketResponseObserver`, which upstream wires
//!   only for the plugin host; plugins aren't ported. Usage records of a
//!   WebSocket call are made from the call's own traffic, and are tested
//!   in `open-ferry-core`'s `observe::usage`.
//! - `TestCodexWebsocketsExecuteResponsesLiteDoesNotInjectImageGenerationTool`
//!   keeps its Responses Lite checks; the image generation tool isn't
//!   ported.
//! - `TestCodexWebsockets_PingHandlerDoesNotBlockOnWriteMu`, the three
//!   `TestCodexWebsockets_KeepalivePingDuringUpload_*` and
//!   `TestCodexWebsockets_ChunkedWriteAllowsPongInterleaving`: the
//!   WebSocket library answers pings as the reader reads, and a message is
//!   sent in one write; `answers_pings_during_a_call` checks a ping is
//!   answered.
//! - `TestCodexWebsockets_PingLoggingRedacted` and
//!   `TestCodexWebsockets_SendErrorLogsSessionObject`: logging.
//! - `TestCodexWebsocketsExecutorOptimizeMultiAgentV2`: the body rewrite
//!   is the HTTP route's, tested in `codex/compat/tests.rs`.
//! - `codex_websockets_routing_hint_test.go` (the routing hint is made up)
//!   and the `codex_websockets_duplex_*_test.go` files (response steering
//!   isn't ported).
//! - The xAI subtests: xAI isn't ported.
//! - `BenchmarkBuildCodexWebsocketRequestBodyLargePayload`: a benchmark.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::{HeaderMap, HeaderValue};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Request, Response, StreamResponse, TransportFault};
use open_ferry_core::executor::{CLOSE_ALL_EXECUTION_SESSIONS, ProviderExecutor};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, watch};

use super::errors::{self, Failure};
use super::mock::{Answer, Server};
use super::request::{self, BETA_HEADER_VALUE};
use super::session::{Conn, Read, Target, is_terminal_event, send_terminal};
use super::*;
use crate::codex::CodexExecutor;
use crate::codex::client::USER_AGENT;
use crate::codex::replay_cache::ReplayCache;
use crate::codex::replay_cache::tests::valid_encrypted_content;
use crate::codex::request::CONTROL_CHARACTER;
use crate::json::{exists, get, str_at};

mod observe;
mod proxy;
mod secrets;
mod sessions;

/// How long a test waits for a call.
const WAIT: Duration = Duration::from_secs(10);

const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp-1","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}"#;

const DELTA: &str = r#"{"type":"response.output_text.delta","delta":"hello"}"#;

const HELLO: &str =
    r#"{"model":"gpt-5-codex","input":[{"type":"message","role":"user","content":"hello"}]}"#;

/// An executor that doesn't use the environment's proxy.
fn executor() -> CodexExecutor {
    CodexExecutor::new("direct")
}

/// An executor with `config`.
fn executor_with(config: Config) -> CodexExecutor {
    executor().with_config(Arc::new(config))
}

/// An API key credential for `base_url`, with websockets on.
fn auth(base_url: &str) -> Auth {
    auth_with(base_url, &[])
}

/// [`auth`] with `attributes` as well.
fn auth_with(base_url: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut auth = Auth {
        id: "codex-test".into(),
        provider: "codex".into(),
        ..Auth::default()
    };
    for (name, value) in [
        ("base_url", base_url),
        ("api_key", "sk-test"),
        ("websockets", "true"),
    ]
    .iter()
    .chain(attributes)
    {
        auth.attributes.insert((*name).into(), (*value).into());
    }
    auth
}

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

/// Options of a client that isn't on the Responses WebSocket.
fn options(format: &str) -> Options {
    Options::new(Format::from(format.to_owned()))
}

/// Options of a client on the Responses WebSocket in the session
/// `session` (none when it is empty).
fn ws_options(session: &str) -> Options {
    let mut options = Options {
        stream: true,
        downstream_websocket: true,
        ..options("openai-response")
    };
    if !session.is_empty() {
        options.metadata.execution_session_id = Some(session.into());
    }
    options
}

fn with_header(mut options: Options, name: &'static str, value: &str) -> Options {
    options
        .headers
        .append(name, HeaderValue::from_str(value).unwrap());
    options
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}

/// Waits for `future`, failing the test after a while.
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Reads a stream to its end: its chunks and its error.
async fn collect(response: StreamResponse) -> (Vec<String>, Option<ExecError>) {
    let mut chunks = response.chunks;
    let mut out = Vec::new();
    let mut error = None;
    within("the stream to end", async {
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) => out.push(String::from_utf8_lossy(&chunk).into_owned()),
                Err(failure) => {
                    assert!(error.is_none(), "a second error: {failure:?}");
                    error = Some(failure);
                }
            }
        }
    })
    .await;
    (out, error)
}

/// The error of a call that didn't start.
fn refused<T>(result: Result<T, ExecError>) -> ExecError {
    match result {
        Ok(_) => panic!("the call went through"),
        Err(error) => error,
    }
}

/// The `n`th message Codex read, as JSON.
fn message(server: &Server, n: usize) -> Value {
    let record = server.record();
    json(
        record
            .messages
            .get(n)
            .unwrap_or_else(|| panic!("no message {n}: {record:?}")),
    )
}

/// Whether the session's request lock can be taken within a second.
async fn lock_free(executor: &CodexExecutor, session: &str) -> bool {
    let session = executor.websockets().get_or_create(session).unwrap();
    tokio::time::timeout(Duration::from_secs(1), session.lock_requests())
        .await
        .is_ok()
}

// TestBuildCodexWebsocketRequestBodyPreservesPreviousResponseID
#[test]
fn message_keeps_previous_response_id() {
    let message = json(&request::message(&json(
        r#"{"model":"gpt-5-codex","previous_response_id":"resp-1","input":[{"type":"message","id":"msg-1"}]}"#,
    )));
    assert_eq!(str_at(&message, "type"), "response.create");
    assert_eq!(str_at(&message, "previous_response_id"), "resp-1");
    assert_eq!(str_at(&message, "input.0.id"), "msg-1");
}

// TestBuildCodexWebsocketRequestBodySanitizesOverlongInputItemIDs
#[test]
fn message_sanitizes_overlong_input_item_ids() {
    let long_reasoning = format!("rs_{}", "a".repeat(64));
    let long_call = "grok-call-item-".repeat(6);
    let long_output = "grok-output-item-".repeat(6);
    let body = json(&format!(
        r#"{{"model":"gpt-5-codex","input":[{{"type":"reasoning","id":"{long_reasoning}","encrypted_content":"gAAAA-encrypted","summary":[]}},{{"type":"function_call","id":"{long_call}","call_id":"call-1","name":"lookup"}},{{"type":"function_call_output","id":"{long_output}","call_id":"call-1","output":"ok"}},{{"type":"message","id":"item_74ec40c883248ebb4885ec84"}}]}}"#
    ));
    let first = json(&request::message(&body));
    let second = json(&request::message(&body));

    let input = get(&first, "input").and_then(Value::as_array).unwrap();
    assert_eq!(input.len(), 3, "{first}");
    assert_eq!(str_at(&first, "input.0.type"), "function_call");
    let short_call = str_at(&first, "input.0.id");
    let short_output = str_at(&first, "input.1.id");
    assert!(short_call.chars().count() <= 64 && short_call != long_call);
    assert!(short_output.chars().count() <= 64 && short_output != long_output);
    assert_ne!(short_call, short_output);
    assert_eq!(str_at(&second, "input.0.id"), short_call);
    assert_eq!(str_at(&first, "input.0.call_id"), "call-1");
    assert_eq!(str_at(&first, "input.1.call_id"), "call-1");
    assert_eq!(
        str_at(&first, "input.2.id"),
        "msg_item_74ec40c883248ebb4885ec84"
    );
}

// TestCodexWebsocketsExecuteRestoresClaudeAgentReasoningReplay. The cache
// is the process's, so the session is this test's own; the reasoning is
// stored directly rather than from a completed response.
#[tokio::test]
async fn execute_restores_claude_agent_reasoning_replay() {
    let encrypted = valid_encrypted_content(31);
    let item = json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": encrypted});
    assert!(ReplayCache::global().store(
        "gpt-5.4",
        "claude:ws-replay-session:agent:agent-a",
        &[&item]
    ));
    let server = Server::once(&[
        r#"{"type":"response.completed","response":{"id":"resp-ws-replay","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"next answer"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let options = with_header(
        with_header(
            options("claude"),
            "X-Claude-Code-Session-Id",
            "ws-replay-session",
        ),
        "X-Claude-Code-Agent-Id",
        "agent-a",
    );
    let request = request(
        "gpt-5.4",
        r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"first"},{"role":"assistant","content":"previous answer"},{"role":"user","content":"next"}]}"#,
    );
    within(
        "the call",
        super::execute(&executor(), &auth(&server.url), &request, &options),
    )
    .await
    .unwrap();

    let sent = message(&server, 0);
    let input = get(&sent, "input").and_then(Value::as_array).unwrap();
    assert_eq!(input.len(), 4, "{sent}");
    assert_eq!(str_at(&sent, "input.1.type"), "reasoning", "{sent}");
    assert_eq!(str_at(&sent, "input.1.encrypted_content"), encrypted);
    assert_eq!(str_at(&sent, "input.2.role"), "assistant", "{sent}");
}

// TestClearCodexReasoningReplayOnWebsocketInvalidSignature, through a call:
// Codex's error event clears the session's replay.
#[tokio::test]
async fn invalid_signature_error_event_clears_replay() {
    let session = "claude:ws-invalid:agent:main";
    let item = json!({"type": "reasoning", "summary": [], "content": null, "encrypted_content": valid_encrypted_content(32)});
    assert!(ReplayCache::global().store("gpt-5.4", session, &[&item]));
    let server = Server::once(&[
        r#"{"type":"error","status":400,"body":{"error":{"message":"Invalid signature in thinking block","type":"invalid_request_error","code":"invalid_request_error"}}}"#,
    ])
    .await;
    let options = with_header(options("claude"), "X-Claude-Code-Session-Id", "ws-invalid");
    let request = request(
        "gpt-5.4",
        r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"next"}]}"#,
    );
    let error = refused(
        within(
            "the call",
            super::execute(&executor(), &auth(&server.url), &request, &options),
        )
        .await,
    );
    assert_eq!(error.status, 400);
    assert_eq!(ReplayCache::global().get_item("gpt-5.4", session), None);
}

const LITE_PAYLOAD: &str = r#"{"model":"MODEL","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"custom","name":"exec"}]},{"role":"user","content":"hello"}],"parallel_tool_calls":true,"client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":"true"}}"#;

// TestCodexWebsocketsExecuteResponsesLiteDoesNotInjectImageGenerationTool
#[tokio::test]
async fn execute_responses_lite_turns_parallel_tool_calls_off() {
    let server = Server::once(&[COMPLETED]).await;
    let request = request("gpt-5.6-sol", &LITE_PAYLOAD.replace("MODEL", "gpt-5.6-sol"));
    let auth = auth_with(&server.url, &[("plan_type", "pro")]);
    within(
        "the call",
        super::execute(&executor(), &auth, &request, &options("codex")),
    )
    .await
    .unwrap();

    let sent = message(&server, 0);
    assert!(!exists(&sent, "instructions"), "{sent}");
    assert!(!exists(&sent, "tools"), "{sent}");
    assert_eq!(str_at(&sent, "input.0.type"), "additional_tools");
    assert_eq!(
        str_at(
            &sent,
            "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite"
        ),
        "true"
    );
    assert_eq!(get(&sent, "parallel_tool_calls"), Some(&json!(false)));
}

// TestCodexWebsocketsExecuteStreamResponsesLiteForcesParallelToolCallsFalse
#[tokio::test]
async fn stream_responses_lite_turns_parallel_tool_calls_off() {
    let server = Server::once(&[COMPLETED]).await;
    let request = request(
        "gpt-5.6-luna",
        &LITE_PAYLOAD.replace("MODEL", "gpt-5.6-luna"),
    );
    let auth = auth_with(&server.url, &[("plan_type", "pro")]);
    let options = Options {
        stream: true,
        ..options("codex")
    };
    let response = super::execute_stream(&executor(), &auth, request, options)
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        get(&message(&server, 0), "parallel_tool_calls"),
        Some(&json!(false))
    );
}

// TestCodexWebsocketsExecutePreservesPreviousResponseIDUpstream
#[tokio::test]
async fn execute_sends_response_create_to_responses() {
    let server = Server::once(&[COMPLETED]).await;
    let request = request(
        "gpt-5-codex",
        r#"{"model":"gpt-5-codex","previous_response_id":"resp-1","input":[{"type":"message","id":"msg-1"}]}"#,
    );
    within(
        "the call",
        super::execute(&executor(), &auth(&server.url), &request, &options("codex")),
    )
    .await
    .unwrap();

    let record = server.record();
    assert_eq!(record.handshakes.len(), 1);
    assert_eq!(record.handshakes[0].method, "GET");
    assert_eq!(record.handshakes[0].path, "/responses");
    assert_eq!(record.handshakes[0].header("upgrade"), Some("websocket"));
    let sent = message(&server, 0);
    assert_eq!(str_at(&sent, "type"), "response.create");
    assert_eq!(str_at(&sent, "previous_response_id"), "resp-1");
}

// TestCodexWebsocketsExecuteStreamUpgradeRequiredReturnsWithoutLockingSession
#[tokio::test]
async fn upgrade_required_returns_without_locking_session() {
    let server = Server::refusing(426, r#"{"error":{"message":"websocket unavailable"}}"#).await;
    let executor = executor();
    let auth = Arc::new(auth(&server.url));
    for payload in [
        r#"{"model":"gpt-5.4","generate":false,"input":[]}"#,
        r#"{"model":"gpt-5.4","previous_response_id":"resp-1","input":[{"type":"message","id":"msg-2"}]}"#,
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            executor.execute_stream(
                Arc::clone(&auth),
                request("gpt-5.4", payload),
                ws_options("ws-upgrade-required-session"),
            ),
        )
        .await
        .expect("the session is still locked");
        assert_eq!(refused(result).status, 426);
    }
    let record = server.record();
    assert_eq!(record.handshakes.len(), 2);
    assert!(
        record
            .handshakes
            .iter()
            .all(|handshake| handshake.header("upgrade") == Some("websocket")),
        "a client on the WebSocket went over HTTP: {record:?}"
    );
}

// TestCodexWebsocketsExecuteStreamHandshakeErrorReturnsWithoutLockingSession
#[tokio::test]
async fn handshake_error_returns_without_locking_session() {
    let server = Server::refusing(401, r#"{"error":{"message":"unauthorized"}}"#).await;
    let executor = executor();
    let auth = auth(&server.url);
    let mut options = Options {
        stream: true,
        ..options("openai-response")
    };
    options.metadata.execution_session_id = Some("ws-handshake-error-session".into());
    for attempt in 1..=2 {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            super::execute_stream(
                &executor,
                &auth,
                request(
                    "gpt-5.4",
                    r#"{"model":"gpt-5.4","input":[{"type":"message","id":"msg-1"}]}"#,
                ),
                options.clone(),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("attempt {attempt}: the session is still locked"));
        assert_eq!(refused(result).status, 401, "attempt {attempt}");
    }
    assert_eq!(server.record().handshakes.len(), 2);
}

// TestCodexWebsocketsExecuteStreamPassesThroughUpstreamWebsocketPayloadForDownstreamWebsocket
#[tokio::test]
async fn stream_passes_codex_events_through() {
    let server = Server::once(&[DELTA, COMPLETED]).await;
    let response = executor()
        .execute_stream(
            Arc::new(auth(&server.url)),
            request(
                "gpt-5-codex",
                r#"{"model":"prolite/gpt-5-codex","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"custom","name":"exec"}]},{"type":"message","role":"user","content":"hello"}],"parallel_tool_calls":true}"#,
            ),
            ws_options(""),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(chunks.first().map(|chunk| chunk.trim()), Some(DELTA));
    assert_eq!(chunks.len(), 2, "{chunks:?}");

    let sent = message(&server, 0);
    assert_eq!(str_at(&sent, "model"), "gpt-5-codex");
    assert_eq!(get(&sent, "parallel_tool_calls"), Some(&json!(true)));
}

// TestCodexWebsocketsExecuteStreamPropagatesUpstreamErrorForDownstreamWebsocket
#[tokio::test]
async fn stream_propagates_error_event() {
    let server = Server::once(&[
        r#"{"type":"error","status":429,"error":{"code":"websocket_connection_limit_reached","message":"too many websockets"}}"#,
    ])
    .await;
    let response = executor()
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options(""),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(chunks.is_empty(), "{chunks:?}");
    let error = error.expect("no error");
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::ZERO));
}

// TestSendTerminalWebsocketReadInvalidatesBeforeWaitingForCapacity
#[tokio::test]
async fn send_terminal_invalidates_before_waiting_for_room() {
    let terminal = || {
        Read::new(
            1,
            Err(Failure::Close {
                code: 1009,
                reason: String::new(),
            }),
        )
    };
    let is_terminal = |read: Option<Read>| {
        matches!(
            read.map(|read| read.result),
            Some(Err(Failure::Close { code: 1009, .. }))
        )
    };

    // available channel keeps fast path ordering
    let (tx, mut rx) = mpsc::channel(1);
    let (_done, done_rx) = watch::channel(());
    let mut calls = 0;
    let invalidated = send_terminal(&tx, done_rx, terminal(), || calls += 1).await;
    assert!(!invalidated);
    assert_eq!(calls, 0);
    assert!(is_terminal(rx.recv().await));

    // full channel invalidates before waiting
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(Read::new(1, Ok("queued".into()))).unwrap();
    let (done, done_rx) = watch::channel(());
    let (called, invalidate_called) = oneshot::channel();
    let sender = tokio::spawn(async move {
        send_terminal(&tx, done_rx, terminal(), move || {
            let _ = called.send(());
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), invalidate_called)
        .await
        .expect("no invalidation before waiting for room")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!sender.is_finished(), "returned before there was room");
    assert!(matches!(rx.recv().await.map(|read| read.result), Some(Ok(text)) if text == "queued"));
    assert!(is_terminal(rx.recv().await));
    assert!(within("the sender", sender).await.unwrap());
    drop(done);

    // full channel stops when invalidation cancels active read
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(Read::new(1, Ok("queued".into()))).unwrap();
    let (done, done_rx) = watch::channel(());
    let invalidated = send_terminal(&tx, done_rx, terminal(), move || drop(done)).await;
    assert!(invalidated);
    assert_eq!(rx.len(), 1);
}

// TestMapCodexWebsocketWriteErrorStopsRetryForMessageTooBig. Upstream's
// `ErrCloseSent` is a send on a closed connection here.
#[test]
fn write_error_after_message_too_big_stops_the_retry() {
    let target = Target::new("auth", "ws://example.test/responses", "", "token");
    let broken_pipe = Failure::Other {
        message: "write: broken pipe".into(),
        transient: true,
    };
    for (name, code, failure, too_big) in [
        (
            "close sent after message too big",
            1009,
            Failure::closed(),
            true,
        ),
        (
            "network write error after message too big",
            1009,
            broken_pipe,
            true,
        ),
        ("other close", 1000, Failure::closed(), false),
    ] {
        let conn = Conn::detached(target.clone());
        conn.set_disconnect(code);
        let error = errors::write_error(conn.disconnect_code(), &failure);
        assert_eq!(errors::should_retry(&error), !too_big, "{name}");
        if too_big {
            assert_eq!(error.status, 413, "{name}");
            assert!(error.request_scoped, "{name}");
        } else {
            assert_eq!(error.message, failure.text(), "{name}");
        }
    }
}

// TestMapCodexWebsocketWriteErrorDoesNotReusePriorConnectionClose
#[test]
fn write_error_reads_its_own_connections_close() {
    let target = Target::new("auth", "ws://example.test/responses", "", "token");
    let prior = Conn::detached(target.clone());
    prior.set_disconnect(1009);
    let error = errors::write_error(prior.disconnect_code(), &Failure::closed());
    assert!(!errors::should_retry(&error));

    let replacement = Conn::detached(target);
    prior.set_disconnect(1009);
    replacement.set_disconnect(1000);
    let error = errors::write_error(replacement.disconnect_code(), &Failure::closed());
    assert_eq!(error.message, Failure::closed().text());
    assert!(errors::should_retry(&error));
}

// TestCodexWebsocketsExecuteStreamMapsMessageTooBigClose
#[tokio::test]
async fn stream_maps_message_too_big_close() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.close(1009, "message too big").await;
                peer.hold().await;
            }
        })
    })
    .await;
    let response = executor()
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options(""),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("no error");
    assert_eq!(error.status, 413);
    assert_eq!(
        str_at(&json(&error.message), "error.code"),
        "message_too_big"
    );
    assert!(error.request_scoped);
}

// TestApplyCodexWebsocketHeadersDefaultsToCurrentResponsesBeta. Upstream
// makes up Codex's `User-Agent` and `Originator`; here the request says
// it's open-ferry and has no `Originator`.
#[test]
fn headers_default_to_the_responses_beta_and_own_identity() {
    let headers = request::build_headers(&Auth::default(), &HeaderMap::new(), false).unwrap();
    assert_eq!(headers["openai-beta"], BETA_HEADER_VALUE);
    assert_eq!(headers["user-agent"], USER_AGENT);
    assert!(USER_AGENT.starts_with("open-ferry/"));
    for name in [
        "originator",
        "version",
        "x-codex-beta-features",
        "x-codex-turn-metadata",
        "x-client-request-id",
        "session_id",
        "conversation_id",
        "authorization",
        "content-type",
        "accept",
    ] {
        assert!(headers.get(name).is_none(), "{name} was sent");
    }
}

// Not upstream's: the client's own `OpenAI-Beta` is kept when it names the
// Responses WebSocket, and replaced when it doesn't.
#[test]
fn headers_keep_the_clients_websocket_beta() {
    let mut client = HeaderMap::new();
    client.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2099-01-01"),
    );
    let headers = request::build_headers(&Auth::default(), &client, false).unwrap();
    assert_eq!(headers["openai-beta"], "responses_websockets=2099-01-01");

    client.insert("openai-beta", HeaderValue::from_static("assistants=v2"));
    let headers = request::build_headers(&Auth::default(), &client, false).unwrap();
    assert_eq!(headers["openai-beta"], BETA_HEADER_VALUE);
}

// TestApplyCodexWebsocketHeadersPassesThroughClientIdentityHeadersWhenCloakingDisabled.
// The client's headers on the plan's list go on as they are, and its
// session ID as `session_id` (upstream copies it as `Session-Id` with
// cloaking off). `Thread-Id`, `X-Codex-Routing-Hint` and `X-Codex-Window-Id`
// aren't on the list, so they don't.
#[test]
fn headers_pass_the_clients_own_identity_through() {
    let mut client = HeaderMap::new();
    for (name, value) in [
        ("originator", "Codex Desktop"),
        ("user-agent", "codex_cli_rs/0.1.0"),
        ("version", "0.115.0-alpha.27"),
        ("x-codex-turn-metadata", r#"{"turn_id":"turn-1"}"#),
        (
            "x-client-request-id",
            "019d2233-e240-7162-992d-38df0a2a0e0d",
        ),
        ("x-codex-beta-features", "feature-a"),
        ("x-codex-turn-state", "state-1"),
        ("x-responsesapi-include-timing-metrics", "true"),
        ("session-id", "legacy-session"),
        ("thread-id", "thread-1"),
        ("x-codex-routing-hint", "route-1"),
        ("x-codex-window-id", "window-1"),
        ("conversation_id", "conversation-1"),
    ] {
        client.insert(name, HeaderValue::from_static(value));
    }
    let auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    let headers = request::build_headers(&auth, &client, true).unwrap();
    for (name, value) in [
        ("originator", "Codex Desktop"),
        ("user-agent", "codex_cli_rs/0.1.0"),
        ("version", "0.115.0-alpha.27"),
        ("x-codex-turn-metadata", r#"{"turn_id":"turn-1"}"#),
        (
            "x-client-request-id",
            "019d2233-e240-7162-992d-38df0a2a0e0d",
        ),
        ("x-codex-beta-features", "feature-a"),
        ("x-codex-turn-state", "state-1"),
        ("x-responsesapi-include-timing-metrics", "true"),
        ("session_id", "legacy-session"),
    ] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            Some(value),
            "{name}"
        );
    }
    for name in [
        "session-id",
        "thread-id",
        "x-codex-routing-hint",
        "x-codex-window-id",
        "conversation_id",
    ] {
        assert!(headers.get(name).is_none(), "{name} was sent");
    }
}

// TestApplyCodexWebsocketHeadersCanonicalizesLegacyUnderscoreSessionHeader
#[test]
fn headers_keep_a_legacy_underscore_session_id() {
    let mut client = HeaderMap::new();
    client.insert("originator", HeaderValue::from_static("Codex Desktop"));
    client.insert("user-agent", HeaderValue::from_static("codex_cli_rs/0.1.0"));
    client.insert(
        "session_id",
        HeaderValue::from_static("legacy-underscore-session"),
    );
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.metadata
        .insert("email".into(), "user@example.com".into());
    let headers = request::build_headers(&auth, &client, false).unwrap();
    let values: Vec<_> = headers.get_all("session_id").iter().collect();
    assert_eq!(values, ["legacy-underscore-session"]);
    assert!(headers.get("session-id").is_none());
}

// TestApplyCodexWebsocketHeadersPreservesExplicitAPIKeyUserAgent
#[test]
fn headers_keep_an_api_key_clients_user_agent() {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("api_key".into(), "sk-test".into());
    let mut client = HeaderMap::new();
    client.insert("user-agent", HeaderValue::from_static("api-key-client/1.0"));
    client.insert("originator", HeaderValue::from_static("explicit-origin"));
    let headers = request::build_headers(&auth, &client, false).unwrap();
    assert_eq!(headers["user-agent"], "api-key-client/1.0");
    assert_eq!(headers["originator"], "explicit-origin");
    assert_eq!(headers["authorization"], "Bearer sk-test");
    assert!(headers["authorization"].is_sensitive());
}

// TestApplyCodexWebsocketHeadersUsesCanonicalAccountHeader
#[test]
fn headers_send_the_credentials_account() {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.metadata.insert("account_id".into(), "acct-1".into());
    let headers = request::build_headers(&auth, &HeaderMap::new(), false).unwrap();
    let values: Vec<_> = headers.get_all("chatgpt-account-id").iter().collect();
    assert_eq!(values, ["acct-1"]);
}

// TestApplyCodexWebsocketHeaders_EmptyAPIKey_OmitsAuthorizationAndOAuthHeaders.
// Upstream's config defaults aren't ported, so there is no config.
#[test]
fn headers_of_an_empty_api_key_leave_out_authorization_and_account() {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("auth_kind".into(), "apikey".into());
    auth.attributes
        .insert("base_url".into(), "https://custom-codex.example.com".into());
    auth.metadata
        .insert("account_id".into(), "acc-ws-123".into());
    let headers = request::build_headers(&auth, &HeaderMap::new(), false).unwrap();
    for name in [
        "authorization",
        "chatgpt-account-id",
        "originator",
        "x-codex-beta-features",
    ] {
        assert!(headers.get(name).is_none(), "{name} was sent");
    }
    assert_eq!(headers["user-agent"], USER_AGENT);
}

// Not upstream's: the credential's `header:` attributes can't give the
// handshake a conversation, thread or window ID; upstream sends whatever
// is configured. Other custom headers still go through.
#[tokio::test]
async fn custom_headers_cannot_set_a_conversation() {
    let server = Server::once(&[COMPLETED]).await;
    let auth = auth_with(
        &server.url,
        &[
            ("header:Conversation_id", "invented-conversation"),
            ("header:conversation-id", "invented-conversation"),
            ("header:Thread-Id", "invented-thread"),
            ("header:thread_id", "invented-thread"),
            ("header:X-Codex-Window-Id", "invented-window"),
            ("header:Session_id", "invented-session"),
            ("header:X-Team", "blue"),
        ],
    );
    let response = executor()
        .execute_stream(
            Arc::new(auth),
            request("gpt-5-codex", HELLO),
            ws_options(""),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let record = server.record();
    let handshake = &record.handshakes[0];
    for name in [
        "conversation_id",
        "conversation-id",
        "thread-id",
        "thread_id",
        "x-codex-window-id",
        "session_id",
    ] {
        assert!(handshake.header(name).is_none(), "{name} was sent");
    }
    assert_eq!(handshake.header("x-team"), Some("blue"));
}

// TestBuildCodexResponsesWebsocketURLRequiresHTTPURL
#[test]
fn websocket_url_needs_an_http_url() {
    assert_eq!(
        request::websocket_url("https://example.com/backend/responses").unwrap(),
        "wss://example.com/backend/responses"
    );
    assert_eq!(
        request::websocket_url("http://127.0.0.1:8080/responses").unwrap(),
        "ws://127.0.0.1:8080/responses"
    );
    assert!(request::websocket_url("ftp://example.com/responses").is_err());
    assert!(request::websocket_url("https:///responses").is_err());
}

// Not upstream's: as Go's url.Parse in buildCodexResponsesWebsocketURL
// (Go 1.26.4), a URL with an ASCII control character fails before anything
// is sent, here without quoting the URL. The URL is trimmed first, and one
// in the fragment is escaped.
#[tokio::test]
async fn a_url_with_a_control_character_is_refused_unsent() {
    for url in [
        "http://127.0.0.1:9/v1\t/responses",
        "http://127.0.0.1:9/v1\n/responses",
        "http://127.0.0.1:9/v1\x7f/responses",
    ] {
        let error = request::websocket_url(url).unwrap_err();
        assert_eq!(error.message, CONTROL_CHARACTER, "{url:?}");
        assert_eq!(error.status, 0);
    }
    assert_eq!(
        request::websocket_url("\thttp://127.0.0.1:9/v1/responses\n").unwrap(),
        "ws://127.0.0.1:9/v1/responses"
    );
    assert!(request::websocket_url("http://127.0.0.1:9/v1#a\tb").is_ok());

    let server = Server::once(&[COMPLETED]).await;
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth(&format!("{}/v1\t", server.url))),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert_eq!(error.message, CONTROL_CHARACTER);
    assert_eq!(error.status, 0);
    assert!(server.record().handshakes.is_empty());
}

// Not upstream's: pins a deviation. Gorilla sends `/a/%2e%2e/v1/responses`
// and `/a/../v1/responses` as written, and a `\` as `%5C` (Go 1.26.4); the
// WHATWG parser resolves the dot segments and reads `\` as `/`.
#[tokio::test]
async fn dot_segments_are_resolved() {
    for (base, path) in [
        ("/a/%2e%2e/v1", "/v1/responses"),
        ("/a/../v1", "/v1/responses"),
        (r"/a\v1", "/a/v1/responses"),
    ] {
        let server = Server::once(&[COMPLETED]).await;
        let response = executor()
            .execute_stream(
                Arc::new(auth(&format!("{}{base}", server.url))),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            )
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(server.record().handshakes[0].path, path, "{base}");
    }
}

// TestParseCodexWebsocketErrorMarksConnectionLimitRetryable
#[test]
fn connection_limit_error_event_may_be_retried_at_once() {
    let (error, status, _) = errors::parse_ws_error(
        &json(
            r#"{"type":"error","status":429,"error":{"code":"websocket_connection_limit_reached","message":"too many websockets"},"headers":{"retry-after":"1"}}"#,
        ),
        false,
        "",
        SystemTime::now(),
    )
    .expect("not an error event");
    assert_eq!((status, error.status), (429, 429));
    assert_eq!(error.retry_after, Some(Duration::ZERO));
    assert_eq!(error.headers["retry-after"], "1");
}

// TestParseCodexWebsocketErrorUsesUsageLimitRetryMetadata
#[test]
fn usage_limit_error_event_waits_for_the_reset() {
    let (error, _, _) = errors::parse_ws_error(
        &json(
            r#"{"type":"error","status":429,"body":{"error":{"type":"usage_limit_reached","message":"usage limit reached","resets_in_seconds":7}}}"#,
        ),
        false,
        "",
        SystemTime::now(),
    )
    .expect("not an error event");
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    assert!(error.credential_scoped);
}

// TestParseCodexWebsocketErrorPreservesWrappedBodyAndHeaders
#[test]
fn error_event_keeps_wrapped_body_and_headers() {
    let (error, _, _) = errors::parse_ws_error(
        &json(
            r#"{"type":"error","status":429,"body":{"error":{"code":"websocket_connection_limit_reached","type":"server_error","message":"too many websocket connections"}},"headers":{"x-request-id":"req-1"}}"#,
        ),
        false,
        "",
        SystemTime::now(),
    )
    .expect("not an error event");
    let body = json(&error.message);
    assert_eq!(get(&body, "status"), Some(&json!(429)));
    assert_eq!(
        str_at(&body, "body.error.code"),
        "websocket_connection_limit_reached"
    );
    assert_eq!(
        str_at(&body, "error.code"),
        "websocket_connection_limit_reached"
    );
    assert!(error.retry_after.is_some());
    assert_eq!(error.headers["x-request-id"], "req-1");
}

// Not upstream's: an event without a status isn't an error event here; the
// terminal failure check reads it.
#[test]
fn error_event_without_status_is_left_alone() {
    for event in [
        r#"{"type":"error","error":{"message":"overloaded"}}"#,
        r#"{"type":"response.failed","status":500}"#,
    ] {
        assert!(errors::parse_ws_error(&json(event), false, "", SystemTime::now()).is_none());
    }
}

// TestCodexWebsocketHandshakeFailureReleasesSessionRequestLock. A 426 for a
// client not on a WebSocket goes over HTTP, which the mock refuses too.
#[tokio::test]
async fn handshake_failure_releases_session_lock() {
    for status in [426, 502] {
        let server = Server::refusing(status, "upstream rejected websocket").await;
        let executor = executor();
        let mut options = Options {
            stream: true,
            ..options("openai-response")
        };
        options.metadata.execution_session_id = Some("failed-handshake".into());
        let result = within(
            "the call",
            super::execute_stream(
                &executor,
                &auth(&server.url),
                request("gpt-5-codex", HELLO),
                options,
            ),
        )
        .await;
        assert_eq!(refused(result).status, status);
        assert!(
            lock_free(&executor, "failed-handshake").await,
            "{status}: the handshake failure left the session locked"
        );
    }
}

const USAGE_LIMIT: &str = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":120}}"#;

fn usage_limit_answer() -> Answer {
    Answer::Refuse {
        status: 429,
        headers: vec![("Content-Type", "application/json".into())],
        body: USAGE_LIMIT.into(),
    }
}

// TestCodexWebsocketsExecuteHandshakeUsageLimitReachedSetsRetryAfter
#[tokio::test]
async fn execute_handshake_usage_limit_waits_for_the_reset() {
    let server = Server::start(|_| usage_limit_answer()).await;
    let auth = Auth {
        id: "codex-auth-quota-exhausted".into(),
        ..auth(&server.url)
    };
    let request = request(
        "gpt-5.6-luna",
        r#"{"model":"gpt-5.6-luna","input":[{"type":"message","id":"msg-1"}]}"#,
    );
    let error = refused(
        within(
            "the call",
            super::execute(&executor(), &auth, &request, &options("openai-response")),
        )
        .await,
    );
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(120)));
    assert!(!error.message.contains("sk-test"));
}

// TestCodexWebsocketsExecuteStreamHandshakeUsageLimitReachedSetsRetryAfter
#[tokio::test]
async fn stream_handshake_usage_limit_waits_for_the_reset() {
    let server = Server::start(|_| usage_limit_answer()).await;
    let auth = Auth {
        id: "codex-auth-quota-exhausted-stream".into(),
        ..auth(&server.url)
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request(
                    "gpt-5.6-luna",
                    r#"{"model":"gpt-5.6-luna","input":[{"type":"message","id":"msg-1"}]}"#,
                ),
                ws_options(""),
            ),
        )
        .await,
    );
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(120)));
}

// Not upstream's: a refused handshake's chunked body is read as one.
#[tokio::test]
async fn handshake_reads_a_chunked_refusal() {
    let (first, second) = USAGE_LIMIT.split_at(20);
    let raw = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
        first.len(),
        second.len()
    );
    let server = Server::start(move |_| Answer::Raw(raw.clone().into_bytes())).await;
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth(&server.url)),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert_eq!(error.status, 429);
    assert_eq!(json(&error.message), json(USAGE_LIMIT));
    assert_eq!(error.retry_after, Some(Duration::from_secs(120)));
}

const EMPTY_INCOMPLETE: &str = r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":10,"output_tokens":0,"total_tokens":10}}}"#;

fn buffering() -> Config {
    let mut config = Config::default();
    config.codex.stream_bootstrap_buffering = true;
    config
}

// TestCodexWebsocketZeroTokenIncompleteReleasesSessionRequestLock
#[tokio::test]
async fn zero_token_incomplete_releases_session_lock() {
    let server = Server::once(&[EMPTY_INCOMPLETE]).await;
    let executor = executor_with(buffering());
    let result = within(
        "the call",
        executor.execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("zero-token-session"),
        ),
    )
    .await;
    if let Ok(response) = result {
        let (_, error) = collect(response).await;
        assert!(error.is_some(), "an empty response.incomplete went through");
    }
    assert!(lock_free(&executor, "zero-token-session").await);
}

// TestCodexWebsockets_SessionlessBufferingImmediateTerminalClosesConnection
#[tokio::test]
async fn sessionless_call_closes_its_connection() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.send(r#"{"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[]}}"#)
                    .await;
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = executor_with(buffering());
    let response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request(
                "gpt-5.6-sol",
                r#"{"model":"gpt-5.6-sol","input":[{"type":"message","role":"user","content":"buffering close test"}]}"#,
            ),
            ws_options(""),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    server.wait_closed(1).await;
}

// TestCodexWebsockets_LastEventAndTerminalTracking. The last event is the
// connection's, so a new connection starts without one.
#[test]
fn last_event_and_terminal_tracking() {
    let target = Target::new("auth", "ws://example.test/responses", "", "token");
    let first = Conn::detached(target.clone());
    assert_eq!(first.last_event(), "");

    first.set_last_event("response.output_item.added");
    assert_eq!(first.last_event(), "response.output_item.added");
    assert!(!is_terminal_event(&first.last_event()));

    first.set_last_event("response.completed");
    assert_eq!(first.last_event(), "response.completed");
    assert!(is_terminal_event(&first.last_event()));

    let second = Conn::detached(target);
    assert_eq!(second.last_event(), "");
    for event in [
        "response.done",
        "response.incomplete",
        "response.failed",
        "error",
    ] {
        assert!(is_terminal_event(event), "{event}");
    }
}

const SPAWN_AGENT_PAYLOAD: &str = r#"{
    "model":"gpt-5.4",
    "input":[{
        "type":"additional_tools",
        "role":"developer",
        "tools":[{
            "type":"namespace",
            "name":"collaboration",
            "tools":[{
                "type":"function",
                "name":"spawn_agent",
                "description":"Available model overrides (optional; inherited parent model is preferred):\n- old-model\nSpawns an agent.",
                "parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}
            }]
        }]
    },{
        "type":"agent_message",
        "id":"amsg_1",
        "author":"/root",
        "recipient":"/root/worker",
        "content":[
            {"type":"input_text","text":"Payload:\n"},
            {"type":"encrypted_content","encrypted_content":"delegated task"}
        ],
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}
    }]
}"#;

fn assert_client_namespace(payload: &str) {
    assert!(
        !payload.contains("collaboration-optimize"),
        "optimized namespace leaked to client: {payload}"
    );
    assert!(
        payload.contains(r#""namespace":"collaboration""#),
        "restored collaboration namespace missing from client payload: {payload}"
    );
}

fn assert_incremental(sent: &Value) {
    let text = sent.to_string();
    assert!(
        !text.contains("collaboration") && !text.contains("spawn_agent"),
        "incremental upstream request contains collaboration tools: {text}"
    );
}

// TestCodexWebsocketsExecutorRestoresMultiAgentV2NamespaceAcrossIncrementalTurns.
// Upstream's spawn agent tests are on the plan's drop list, but this one
// tests the WebSocket session's bookkeeping, so it is kept. The client's
// `User-Agent` is Codex's own, as in upstream's test.
#[tokio::test]
async fn multi_agent_v2_namespace_is_restored_across_incremental_turns() {
    for stream in [false, true] {
        let turns = Arc::new(AtomicUsize::new(0));
        let server = {
            let turns = Arc::clone(&turns);
            Server::start(move |_| {
                let turns = Arc::clone(&turns);
                Answer::accept(move |mut peer| {
                    let turns = Arc::clone(&turns);
                    async move {
                        while peer.recv().await.is_some() {
                            let turn = turns.fetch_add(1, Ordering::SeqCst) + 1;
                            peer.send(&format!(
                                r#"{{"type":"response.completed","response":{{"id":"resp_{turn}","object":"response","status":"completed","output":[{{"type":"function_call","name":"spawn_agent","namespace":"collaboration-optimize","arguments":"{{}}","call_id":"call_{turn}"}}]}}}}"#
                            ))
                            .await;
                        }
                    }
                })
            })
            .await
        };
        let mut config = Config::default();
        config.client.codex.optimize_multi_agent_v2 = true;
        let executor = executor_with(config);
        let auth = Arc::new(auth(&server.url));
        let session = "multi-agent-v2-incremental";
        let options = Options {
            stream,
            ..with_header(ws_options(session), "user-agent", "codex-tui/0.154.0")
        };
        let call = |payload: &str| {
            let request = request("gpt-5.4", payload);
            let auth = Arc::clone(&auth);
            let options = options.clone();
            let executor = &executor;
            async move {
                if stream {
                    let response = executor
                        .execute_stream(auth, request, options)
                        .await
                        .unwrap();
                    let (chunks, error) = collect(response).await;
                    assert!(error.is_none(), "{error:?}");
                    chunks.concat()
                } else {
                    let Response { payload, .. } =
                        executor.execute(auth, request, options).await.unwrap();
                    String::from_utf8_lossy(&payload).into_owned()
                }
            }
        };

        let first = call(SPAWN_AGENT_PAYLOAD).await;
        assert_eq!(
            str_at(&message(&server, 0), "input.0.tools.0.name"),
            "collaboration-optimize"
        );
        assert_client_namespace(&first);

        let second = call(r#"{"model":"gpt-5.4","previous_response_id":"resp_1","input":[{"type":"function_call_output","call_id":"call_1","output":"done"}]}"#).await;
        assert_incremental(&message(&server, 1));
        assert_client_namespace(&second);

        let conflicting = call(r#"{"model":"gpt-5.4","tools":[{"type":"namespace","name":"collaboration-optimize","tools":[{"type":"function","name":"spawn_agent","description":"User-defined tool."}]}],"input":[{"type":"message","role":"user","content":"use the user-defined namespace"}]}"#).await;
        assert_eq!(
            str_at(&message(&server, 2), "tools.0.name"),
            "collaboration-optimize"
        );
        assert!(
            conflicting.contains(r#""namespace":"collaboration-optimize""#),
            "the user-defined namespace was rewritten: {conflicting}"
        );

        let fourth = call(r#"{"model":"gpt-5.4","previous_response_id":"resp_3","input":[{"type":"function_call_output","call_id":"call_3","output":"done"}]}"#).await;
        assert_incremental(&message(&server, 3));
        assert!(
            fourth.contains(r#""namespace":"collaboration-optimize""#),
            "the user-defined namespace was rewritten after the conflict: {fourth}"
        );

        let fifth = call(SPAWN_AGENT_PAYLOAD).await;
        assert_eq!(
            str_at(&message(&server, 4), "input.0.tools.0.name"),
            "collaboration-optimize"
        );
        assert_client_namespace(&fifth);

        let sixth = call(r#"{"model":"gpt-5.4","previous_response_id":"resp_5","input":[{"type":"function_call_output","call_id":"call_5","output":"done"}]}"#).await;
        assert_incremental(&message(&server, 5));
        assert_client_namespace(&sixth);

        assert_eq!(server.record().handshakes.len(), 1, "stream: {stream}");
        executor.close_execution_session(session);
    }
}

// Not upstream's: the client's turns in one session share a connection,
// and a new token (a refresh) connects again with it, as it goes only in
// the handshake.
#[tokio::test]
async fn a_new_token_connects_again() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = executor();
    for key in ["sk-one", "sk-one", "sk-two"] {
        let response = executor
            .execute_stream(
                Arc::new(auth_with(&server.url, &[("api_key", key)])),
                request("gpt-5-codex", HELLO),
                ws_options("token-change"),
            )
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "{error:?}");
    }
    let record = server.wait_closed(1).await;
    let tokens: Vec<_> = record
        .handshakes
        .iter()
        .map(|handshake| handshake.header("authorization").unwrap_or_default())
        .collect();
    assert_eq!(tokens, ["Bearer sk-one", "Bearer sk-two"]);
    assert_eq!(record.messages.len(), 3);
    executor.close_execution_session("token-change");
    server.wait_closed(2).await;
}

// Not upstream's: closing the client's session closes its connection, and
// closing them all leaves none.
#[tokio::test]
async fn closing_the_session_closes_its_connection() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = executor();
    for session in ["close-me", "close-all"] {
        let response = executor
            .execute_stream(
                Arc::new(auth(&server.url)),
                request("gpt-5-codex", HELLO),
                ws_options(session),
            )
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "{error:?}");
    }
    assert_eq!(executor.websockets().len(), 2);
    assert_eq!(server.record().client_closed, 0);

    executor.close_execution_session("close-me");
    server.wait_closed(1).await;
    assert_eq!(executor.websockets().len(), 1);

    executor.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS);
    server.wait_closed(2).await;
    assert_eq!(executor.websockets().len(), 0);
}

// Not upstream's: a ping from Codex is answered during a call.
#[tokio::test]
async fn answers_pings_during_a_call() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                let delta = if peer.ping_pong().await {
                    r#"{"type":"response.output_text.delta","delta":"pong"}"#
                } else {
                    r#"{"type":"response.output_text.delta","delta":"no pong"}"#
                };
                peer.send(delta).await;
                peer.send(COMPLETED).await;
            }
        })
    })
    .await;
    let response = executor()
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options(""),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        str_at(&json(&chunks[0]), "delta"),
        "pong",
        "the ping wasn't answered"
    );
}

// Not upstream's: a connection idle past the timeout is closed, and the
// call gets a network error.
#[tokio::test]
async fn idle_connection_times_out() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = executor().with_websocket_idle(Duration::from_millis(200));
    let response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("idle"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("no error");
    assert!(error.message.contains("i/o timeout"), "{error:?}");
    assert_eq!(error.transport, Some(TransportFault::Transient));
    server.wait_closed(1).await;
}

// Not upstream's: dropping a call before it ends (the client went away)
// closes its connection, and the session's next call connects again.
#[tokio::test]
async fn dropping_a_call_closes_its_connection() {
    let server = Server::start(|n| {
        Answer::accept(move |mut peer| async move {
            if peer.recv().await.is_some() {
                peer.send(DELTA).await;
                if n > 0 {
                    peer.send(COMPLETED).await;
                }
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = executor();
    let auth = Arc::new(auth(&server.url));
    let response = executor
        .execute_stream(
            Arc::clone(&auth),
            request("gpt-5-codex", HELLO),
            ws_options("drop-me"),
        )
        .await
        .unwrap();
    let mut chunks = response.chunks;
    let first = within("the first chunk", chunks.next()).await;
    assert!(matches!(first, Some(Ok(_))), "{first:?}");
    drop(chunks);
    server.wait_closed(1).await;

    let response = executor
        .execute_stream(auth, request("gpt-5-codex", HELLO), ws_options("drop-me"))
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert_eq!(server.record().handshakes.len(), 2);
    executor.close_execution_session("drop-me");
}

// Not upstream's: closing the session during a call ends the call with an
// error rather than leaving it waiting.
#[tokio::test]
async fn closing_the_session_ends_its_call() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.send(DELTA).await;
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = executor();
    let response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("closing"),
        )
        .await
        .unwrap();
    let mut chunks = response.chunks;
    let first = within("the first chunk", chunks.next()).await;
    assert!(matches!(first, Some(Ok(_))), "{first:?}");
    executor.close_execution_session("closing");
    let next = within("the call to end", chunks.next()).await;
    assert!(matches!(next, Some(Err(_))), "{next:?}");
    server.wait_closed(1).await;
}

// Not upstream's: closing the session closes its connection at once while
// its call's channel is full and the call reads nothing; the call's stream
// then ends after what the channel held. Upstream's reader closes the
// socket and waits for room.
#[tokio::test]
async fn closing_the_session_closes_a_backed_up_connection() {
    let frames = vec![DELTA.to_owned(); 4100];
    let server = Server::start(move |_| {
        let frames = frames.clone();
        Answer::accept(move |mut peer| {
            let frames = frames.clone();
            async move {
                if peer.recv().await.is_some() {
                    peer.send_all(&frames).await;
                    peer.hold().await;
                }
            }
        })
    })
    .await;
    let executor = executor();
    let response = executor
        .execute_stream(
            Arc::new(auth(&server.url)),
            request("gpt-5-codex", HELLO),
            ws_options("backed-up"),
        )
        .await
        .unwrap();
    let session = executor.websockets().get_or_create("backed-up").unwrap();
    within("the call's channel to fill", async {
        while !session
            .conn()
            .and_then(|conn| session.active_for(conn.id()))
            .is_some_and(|(_, tx, _)| tx.capacity() == 0)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    executor.close_execution_session("backed-up");
    server.wait_closed(1).await;
    let (chunks, error) = collect(response).await;
    assert!(error.is_some(), "the stream ended without an error");
    assert!(chunks.len() < 4100, "{} chunks", chunks.len());
}

// Not upstream's: closing the session during its handshake abandons it, so
// the closed session gets no connection, nothing is sent and nothing is
// left open. Upstream gives the closed session the connection and sends
// `response.create` on it.
#[tokio::test]
async fn closing_the_session_abandons_its_handshake() {
    let (gate, held) = watch::channel(false);
    let server = Server::start(move |_| {
        Answer::Held(held.clone(), Box::new(Answer::accept(|peer| peer.hold())))
    })
    .await;
    let executor = Arc::new(executor());
    let call = tokio::spawn({
        let executor = Arc::clone(&executor);
        let auth = Arc::new(auth(&server.url));
        async move {
            executor
                .execute_stream(
                    auth,
                    request("gpt-5-codex", HELLO),
                    ws_options("handshaking"),
                )
                .await
        }
    });
    server
        .wait_for("the handshake", |record| !record.handshakes.is_empty())
        .await;

    executor.close_execution_session("handshaking");
    gate.send_replace(true);
    let error = refused(within("the call", call).await.unwrap());
    assert_eq!(error.message, Failure::closed().text());
    let record = server.wait_closed(1).await;
    assert!(record.messages.is_empty(), "{record:?}");
    executor.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS);
    assert_eq!(executor.websockets().len(), 0);
    let record = server.record();
    assert_eq!(record.handshakes.len(), 1);
    assert_eq!(record.client_closed, 1);
    assert!(record.messages.is_empty(), "{record:?}");
}

// Not upstream's: an error event, or a close from Codex, lets the
// connection go, and the session's next call connects again.
#[tokio::test]
async fn error_event_or_close_has_the_next_call_connect_again() {
    for close in [false, true] {
        let server = Server::start(move |n| {
            Answer::accept(move |mut peer| async move {
                if peer.recv().await.is_none() {
                    return;
                }
                match (n, close) {
                    (0, false) => {
                        peer.send(r#"{"type":"error","status":500,"error":{"message":"boom"}}"#)
                            .await;
                    }
                    (0, true) => peer.close(1000, "bye").await,
                    _ => peer.send(COMPLETED).await,
                }
                peer.hold().await;
            })
        })
        .await;
        let executor = executor();
        let auth = Arc::new(auth(&server.url));
        let response = executor
            .execute_stream(
                Arc::clone(&auth),
                request("gpt-5-codex", HELLO),
                ws_options("reconnect"),
            )
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        let error = error.expect("no error");
        if close {
            assert_eq!(
                error.transport,
                Some(TransportFault::Lifecycle),
                "{error:?}"
            );
        } else {
            assert_eq!(error.status, 500);
        }

        let response = executor
            .execute_stream(auth, request("gpt-5-codex", HELLO), ws_options("reconnect"))
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "close {close}: {error:?}");
        assert_eq!(server.record().handshakes.len(), 2, "close {close}");
        executor.close_execution_session("reconnect");
    }
}

// Not upstream's: a send that fails on the session's connection is tried
// once more on a new one (upstream's
// `TestWebsocketRetryBindFailureClearsActiveSessionState` without its
// lifecycle; a connection without a socket stands in for a stale one).
#[tokio::test]
async fn send_on_a_stale_connection_is_tried_once_more() {
    let server = Server::once(&[COMPLETED]).await;
    let executor = executor();
    let auth = auth(&server.url);
    let session = executor.websockets().get_or_create("stale").unwrap();
    let url = request::websocket_url(&format!("{}/responses", server.url)).unwrap();
    let stale = Conn::detached(Target::new(
        &auth.id,
        &url,
        &executor.proxy_for(&auth),
        "sk-test",
    ));
    session.set_conn(Arc::clone(&stale));

    let response = super::execute_stream(
        &executor,
        &auth,
        request("gpt-5-codex", HELLO),
        ws_options("stale"),
    )
    .await
    .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(stale.is_closed());
    assert_eq!(server.record().handshakes.len(), 1);
    assert!(session.active_for(stale.id()).is_none());
    executor.close_execution_session("stale");
}

// Not upstream's: Codex asking for HTTP (a 426) sends a client that isn't
// on a WebSocket over HTTP, as upstream's `CodexAutoExecutor` does.
#[tokio::test]
async fn upgrade_required_goes_over_http_for_other_clients() {
    let sse = format!("data: {COMPLETED}\n\n");
    let http = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
        sse.len()
    );
    let server = Server::start(move |n| {
        if n % 2 == 0 {
            Answer::refuse(426, "use http")
        } else {
            Answer::Raw(http.clone().into_bytes())
        }
    })
    .await;
    let executor = executor();
    let auth = auth(&server.url);
    within(
        "the call",
        super::execute(
            &executor,
            &auth,
            &request("gpt-5-codex", HELLO),
            &options("openai-response"),
        ),
    )
    .await
    .unwrap();
    let response = super::execute_stream(
        &executor,
        &auth,
        request("gpt-5-codex", HELLO),
        Options {
            stream: true,
            ..options("openai-response")
        },
    )
    .await
    .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");

    let methods: Vec<_> = server
        .record()
        .handshakes
        .iter()
        .map(|handshake| handshake.method.clone())
        .collect();
    assert_eq!(methods, ["GET", "POST", "GET", "POST"]);
}

// Not upstream's (`CodexAutoExecutor` and `codexWebsocketsEnabled`): which
// calls take the WebSocket.
#[test]
fn routes_only_websocket_clients_with_websockets_on() {
    let on = auth_with("http://127.0.0.1:1", &[]);
    let off = auth_with("http://127.0.0.1:1", &[("websockets", "false")]);
    assert!(routes(&on, &ws_options("")));
    assert!(!routes(&off, &ws_options("")));
    assert!(!routes(&on, &options("openai-response")));
    let compact = Options {
        alt: COMPACT_ALT.into(),
        ..ws_options("")
    };
    assert!(!routes(&on, &compact));
}

#[test]
fn websockets_setting_reads_attribute_then_metadata() {
    let with = |attribute: Option<&str>, metadata: Option<Value>| {
        let mut auth = Auth::default();
        if let Some(attribute) = attribute {
            auth.attributes
                .insert("websockets".into(), attribute.into());
        }
        if let Some(metadata) = metadata {
            auth.metadata.insert("websockets".into(), metadata);
        }
        websockets_enabled(&auth)
    };
    assert!(!with(None, None));
    assert!(with(Some("true"), None));
    assert!(with(Some(" 1 "), None));
    assert!(!with(Some("false"), Some(json!(true))));
    assert!(with(Some("maybe"), Some(json!(true))));
    assert!(with(Some(""), Some(json!("T"))));
    assert!(!with(None, Some(json!("yes"))));
    assert!(!with(None, Some(json!(1))));
}

#[test]
fn parse_bool_is_gos() {
    for text in ["1", "t", "T", "TRUE", "true", "True"] {
        assert_eq!(parse_bool(text), Some(true), "{text}");
    }
    for text in ["0", "f", "F", "FALSE", "false", "False"] {
        assert_eq!(parse_bool(text), Some(false), "{text}");
    }
    for text in ["", "yes", "tRUE", " true"] {
        assert_eq!(parse_bool(text), None, "{text}");
    }
}
