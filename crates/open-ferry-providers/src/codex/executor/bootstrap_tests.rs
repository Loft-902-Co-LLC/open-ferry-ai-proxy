// Ported from CLIProxyAPI internal/runtime/executor/codex_stream_bootstrap_buffering_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Stream bootstrap buffering against a mock Codex server on 127.0.0.1,
//! which writes its SSE body a piece at a time.
//!
//! Deviations from upstream:
//! - The `TestCodexWebsocketsExecutor_*` tests are in [`websocket`], which
//!   lists its own deviations.
//! - `TestCodexExecutor_BootstrapBuffering_CancelDuringBootstrapIsNotAnUpstreamFailure`
//!   becomes `dropping_the_call_during_bootstrap_closes_the_upstream`. A
//!   call is cancelled by dropping its future, so there is no context error
//!   to check; it checks instead that held lines keep the call waiting and
//!   that dropping the call closes Codex's connection.
//! - `TestCodexExecutor_BootstrapBuffering_ContextCancelDuringBootstrap` is
//!   dropped: a future dropped before it is polled never calls Codex.
//! - `TestCodexConfig_StreamBootstrapTimeoutDuration` drops its nil config
//!   case, as a Rust config can't be nil.
//! - Upstream swaps a package-wide clock; here the executor takes the mock
//!   clock.

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use futures_util::StreamExt as _;
use open_ferry_core::config::CodexConfig;
use tokio::sync::{Notify, mpsc, oneshot};

use super::*;
use crate::codex::terminal::{
    MAX_BOOTSTRAP_BYTES, MAX_BOOTSTRAP_FRAMES, is_bootstrap_bufferable_event,
    is_overload_bootstrap_failure,
};

mod websocket;

pub(super) const OVERLOAD_EVENT: &str = r#"{"type":"error","error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","param":null},"sequence_number":2}"#;
const CAPACITY_EVENT: &str = r#"{"type":"error","error":{"message":"Selected model is at capacity. Please try a different model."},"sequence_number":2}"#;
const INVALID_EVENT: &str = r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."},"sequence_number":2}"#;
const CREATED_EVENT: &str =
    r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6-terra"}}"#;
const IN_PROGRESS_EVENT: &str = r#"{"type":"response.in_progress","response":{"id":"resp_1"}}"#;
const OUTPUT_ADDED_EVENT: &str = r#"{"type":"response.output_item.added","item":{"id":"msg_1","type":"message","role":"assistant","content":[]},"output_index":0}"#;
const KEEPALIVE_EVENT: &str = r#"{"type":"keepalive","sequence_number":1}"#;
const OUTPUT_DELTA_EVENT: &str = r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#;
const COMPLETED_EVENT: &str = r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
const EMPTY_INCOMPLETE_EVENT: &str = r#"{"type":"response.incomplete","response":{"id":"resp_1","output":[],"usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}}"#;

/// Writes a mock response's body a piece at a time.
struct Writer(mpsc::UnboundedSender<Bytes>);

impl Writer {
    fn write(&self, text: &str) {
        // The client may have gone; what it misses doesn't matter.
        let _ = self.0.send(Bytes::from(text.to_owned()));
    }

    /// Waits until the client has gone.
    async fn closed(&self) {
        self.0.closed().await;
    }
}

/// A server on an ephemeral 127.0.0.1 port that answers every request with
/// `status` and a body that `script` writes, ending when it returns.
/// Returns the server's URL.
async fn serve<S, F>(status: u16, script: S) -> String
where
    S: Fn(Writer) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let app = Router::new().fallback(move |_body: Bytes| {
        let script = script.clone();
        async move {
            let (sender, receiver) = mpsc::unbounded_channel();
            tokio::spawn(script(Writer(sender)));
            let body = futures_util::stream::unfold(receiver, |mut receiver| async move {
                let chunk = receiver.recv().await?;
                Some((Ok::<_, io::Error>(chunk), receiver))
            });
            axum::response::Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(body))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

/// A server whose 200 response is `body` (upstream's `codexSSERawServer`).
async fn serve_raw(body: String) -> String {
    serve(200, move |writer| {
        let body = body.clone();
        async move { writer.write(&body) }
    })
    .await
}

/// A server streaming each event as an `event:` line naming its type and a
/// `data:` line, then a blank line (upstream's `codexSSEServer`).
async fn serve_events(events: &[&str]) -> String {
    let mut body = String::new();
    for event in events {
        let event_type = event
            .split_once(r#""type":""#)
            .map_or("message", |(_, rest)| {
                rest.split_once('"').map_or(rest, |(name, _)| name)
            });
        body.push_str(&format!("event: {event_type}\ndata: {event}\n\n"));
    }
    serve_raw(body).await
}

/// A config with buffering as `enabled` says and `timeout` as the time
/// limit (upstream's `codexBufferingConfigWithTimeout`).
fn config(enabled: bool, timeout: &str) -> Config {
    let mut config = Config::default();
    config.codex = CodexConfig {
        stream_bootstrap_buffering: enabled,
        stream_bootstrap_timeout: timeout.to_owned(),
        ..CodexConfig::default()
    };
    config
}

fn executor_with(config: Config) -> CodexExecutor {
    CodexExecutor::new("direct").with_config(Arc::new(config))
}

/// An executor with buffering as `enabled` says and no time limit
/// (upstream's `codexBufferingConfig`).
fn buffering(enabled: bool) -> CodexExecutor {
    executor_with(config(enabled, ""))
}

/// An executor with buffering on, limited to `timeout` on `clock`.
fn timed(timeout: &str, clock: &MockClock) -> CodexExecutor {
    executor_with(config(true, timeout)).with_bootstrap_clock(clock.clock())
}

/// An API key credential for `base_url` (upstream's `codexTestAuth`).
fn auth(base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), "test".into());
    Arc::new(auth)
}

/// Starts a streaming call from a client speaking `format`, with upstream's
/// `codexTestRequest` payload.
async fn start_as(
    executor: &CodexExecutor,
    url: &str,
    format: &str,
) -> Result<StreamResponse, ExecError> {
    let request = Request {
        model: "gpt-5.6-terra".into(),
        payload: Bytes::from_static(br#"{"model":"gpt-5.6-terra","input":"hello"}"#),
    };
    let options = Options {
        stream: true,
        ..Options::new(Format::from(format.to_owned()))
    };
    executor.execute_stream(auth(url), request, options).await
}

/// Starts upstream's `codexTestRequest`, from an OpenAI Responses client.
async fn start(executor: &CodexExecutor, url: &str) -> Result<StreamResponse, ExecError> {
    start_as(executor, url, "openai-response").await
}

/// Every chunk joined with `\n`, and the first error (upstream's
/// `drainChunks`).
async fn drain(response: StreamResponse) -> (String, Option<ExecError>) {
    let mut chunks = response.chunks;
    let mut payloads = Vec::new();
    let mut error = None;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => payloads.push(String::from_utf8_lossy(&chunk).into_owned()),
            Err(failure) => {
                error.get_or_insert(failure);
            }
        }
    }
    (payloads.join("\n"), error)
}

/// The error of a call that failed before its stream started.
fn failed_over(result: Result<StreamResponse, ExecError>) -> ExecError {
    match result {
        Ok(_) => panic!("the stream started where the call should have failed over"),
        Err(error) => error,
    }
}

/// The stream of a call that started.
fn started(result: Result<StreamResponse, ExecError>) -> StreamResponse {
    match result {
        Ok(response) => response,
        Err(error) => {
            panic!("the call failed over where its stream should have started: {error:?}")
        }
    }
}

/// A clock that stands still until moved, and tells when it is first read
/// (upstream's `mockClock` and `bootstrapStarted`).
#[derive(Clone)]
struct MockClock {
    base: Instant,
    offset: Arc<Mutex<Duration>>,
    read: Arc<AtomicBool>,
    first_read: Arc<Notify>,
}

impl MockClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Arc::default(),
            read: Arc::default(),
            first_read: Arc::default(),
        }
    }

    fn now(&self) -> Instant {
        if !self.read.swap(true, Ordering::SeqCst) {
            self.first_read.notify_one();
        }
        self.base + *self.offset.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }

    /// Waits until the executor has read the clock, which it does once the
    /// response's headers have come.
    async fn started(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.first_read.notified())
            .await
            .expect("the executor never read the bootstrap clock");
    }

    fn clock(&self) -> Clock {
        let clock = self.clone();
        Arc::new(move || clock.now())
    }
}

// An overload smuggled into an HTTP 200 stream fails the whole attempt
// before any chunk escapes, so the credential manager can retry on another
// credential, with the status Codex hid.
#[tokio::test]
async fn overload_fails_attempt_without_leaking_handshake() {
    let url = serve_events(&[CREATED_EVENT, IN_PROGRESS_EVENT, OVERLOAD_EVENT]).await;
    let error = failed_over(start(&buffering(true), &url).await);
    assert_eq!(error.status, 503, "Codex hides a 503 behind HTTP 200");
}

#[tokio::test]
async fn capacity_fails_attempt_without_leaking_handshake() {
    let url = serve_events(&[CREATED_EVENT, IN_PROGRESS_EVENT, CAPACITY_EVENT]).await;
    let error = failed_over(start(&buffering(true), &url).await);
    assert_eq!(error.status, 429);
}

// A failure that isn't an overload keeps its in-stream delivery: the held
// handshake comes first and the error after it, so the manager sees a
// stream that started and doesn't spend another credential on it.
#[tokio::test]
async fn non_overload_stays_in_stream() {
    let url = serve_events(&[CREATED_EVENT, IN_PROGRESS_EVENT, INVALID_EVENT]).await;
    let (combined, error) = drain(started(start(&buffering(true), &url).await)).await;
    let error = error.expect("the invalid request must come as an in-stream error");
    assert!(
        combined.contains(r#""type":"response.created""#),
        "the held handshake must come before the in-stream error: {combined}"
    );
    assert_eq!(error.status, 400);
}

// Past the frame budget the stream is released and overload probing stops.
#[tokio::test]
async fn buffer_limit_releases_stream() {
    let mut events: Vec<String> = (0..=MAX_BOOTSTRAP_FRAMES)
        .map(|i| format!(r#"{{"type":"response.in_progress","response":{{"id":"resp_{i}"}}}}"#))
        .collect();
    events.push(OVERLOAD_EVENT.to_owned());
    let events: Vec<&str> = events.iter().map(String::as_str).collect();
    let url = serve_events(&events).await;
    let (_, error) = drain(started(start(&buffering(true), &url).await)).await;
    assert!(
        error.is_some(),
        "the overload must come in-stream after the limit"
    );
}

// Not upstream's: a stream that ends while only a handshake is held fails
// with a 408 when upstream's translation of the held lines gave a chunk,
// even an empty one, as a Claude client's does for every `data:` line; it is
// an empty stream when it gave none, as Chat Completions does for
// `response.created`.
#[tokio::test]
async fn ending_during_bootstrap_counts_empty_chunks_as_upstream() {
    let url = serve_events(&[IN_PROGRESS_EVENT]).await;
    let error = failed_over(start_as(&buffering(true), &url, "claude").await);
    assert_eq!(error.status, 408, "{error:?}");

    let url = serve_events(&[CREATED_EVENT]).await;
    let (combined, error) = drain(started(start_as(&buffering(true), &url, "openai").await)).await;
    assert!(
        combined.is_empty() && error.is_none(),
        "{combined} {error:?}"
    );
}

// TestCodexBootstrapBudgetsStaySmall: pinned, as the framing tests scale
// with them and config.example.yaml quotes them.
#[test]
fn bootstrap_budgets_stay_small() {
    assert_eq!(MAX_BOOTSTRAP_FRAMES, 48);
    assert_eq!(MAX_BOOTSTRAP_BYTES, 1 << 20);
}

// In-stream delivery needs the held frames to render to a chunk: Chat
// Completions renders these as nothing, so only the error comes.
#[tokio::test]
async fn in_stream_delivery_needs_rendered_frames() {
    let url = serve_events(&[
        CREATED_EVENT,
        IN_PROGRESS_EVENT,
        OUTPUT_ADDED_EVENT,
        EMPTY_INCOMPLETE_EVENT,
    ])
    .await;
    let response = started(start_as(&buffering(true), &url, "openai").await);
    let (combined, error) = drain(response).await;
    assert!(
        error.is_some(),
        "the failure must come as an in-stream error"
    );
    assert_eq!(
        combined, "",
        "Chat Completions renders these frames as nothing"
    );
}

// An empty response.incomplete isn't an overload, so it keeps its
// in-stream delivery after the held frames.
#[tokio::test]
async fn non_overload_terminal_stays_in_stream() {
    let url = serve_events(&[
        CREATED_EVENT,
        IN_PROGRESS_EVENT,
        OUTPUT_ADDED_EVENT,
        EMPTY_INCOMPLETE_EVENT,
    ])
    .await;
    let (combined, error) = drain(started(start(&buffering(true), &url).await)).await;
    assert!(
        error.is_some(),
        "the failure must come as an in-stream error"
    );
    assert!(
        combined.contains(r#""type":"response.created""#),
        "held frames must come before the in-stream error: {combined}"
    );
}

// The byte cap is checked before a line is held: one that alone passes it
// starts the stream, so the rejection right after it comes in-stream.
#[tokio::test]
async fn oversized_frame_is_not_admitted() {
    let oversized = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "q".repeat(MAX_BOOTSTRAP_BYTES * 2)
    );
    let url = serve_raw(format!("data: {oversized}\ndata: {OVERLOAD_EVENT}\n\n")).await;
    drain(started(start(&buffering(true), &url).await)).await;
}

// The byte budget counts the upstream lines, not only their chunks: Chat
// Completions renders these as nothing, and ten of them stay well inside
// the frame budget.
#[tokio::test]
async fn byte_cap_counts_upstream_frames() {
    let frame = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "w".repeat(MAX_BOOTSTRAP_BYTES / 8)
    );
    let frames = format!("data: {frame}\n\n").repeat(10);
    let url = serve_raw(format!("{frames}data: {OVERLOAD_EVENT}\n\n")).await;
    drain(started(start_as(&buffering(true), &url, "openai").await)).await;
}

// Comment heartbeats cost a line each, so the budget holds exactly
// MAX_BOOTSTRAP_FRAMES of them, and the next one releases.
#[tokio::test]
async fn budget_boundary_is_exact() {
    let overload = format!("data: {OVERLOAD_EVENT}\n\n");
    for (heartbeats, fails_over) in [
        (MAX_BOOTSTRAP_FRAMES - 1, true),
        (MAX_BOOTSTRAP_FRAMES, true),
        (MAX_BOOTSTRAP_FRAMES + 1, false),
    ] {
        let url = serve_raw(format!("{}{overload}", ": keepalive\n".repeat(heartbeats))).await;
        match start(&buffering(true), &url).await {
            Ok(response) => {
                assert!(
                    !fails_over,
                    "{heartbeats} held lines are inside the budget and must still fail over"
                );
                drain(response).await;
            }
            Err(error) => assert!(
                fails_over,
                "{heartbeats} held lines exceed the budget and must release: {error:?}"
            ),
        }
    }
}

// An empty data: line is the SSE heartbeat idiom, and is held.
#[tokio::test]
async fn empty_data_frame_does_not_release_stream() {
    for frame in ["data:\n\n", "data: \n\n", "data:   \t\n\n"] {
        let url = serve_raw(format!("{frame}data: {OVERLOAD_EVENT}\n\n")).await;
        assert!(
            start(&buffering(true), &url).await.is_err(),
            "an empty data frame {frame:?} must keep the bootstrap window open"
        );
    }
}

// The bound holds whatever framing Codex picks, against Chat Completions,
// which renders these lines as nothing.
#[tokio::test]
async fn bound_holds_for_every_sse_framing() {
    let overload = format!("data: {OVERLOAD_EVENT}\n\n");
    let handshake = r#"{"type":"response.in_progress","response":{"id":"resp_1"}}"#;
    let times = MAX_BOOTSTRAP_FRAMES * 4;
    let cases = [
        (
            "comment heartbeats without blank separators",
            ": keepalive\n".repeat(times),
        ),
        (
            "data frames without blank separators",
            format!("data: {handshake}\n").repeat(times),
        ),
        (
            "three line framing",
            format!("event: response.in_progress\ndata: {handshake}\n\n").repeat(times),
        ),
        (
            "double blank separators",
            format!("data: {handshake}\n\n\n").repeat(times),
        ),
    ];
    for (name, body) in cases {
        let url = serve_raw(format!("{body}{overload}")).await;
        let response = start_as(&buffering(true), &url, "openai")
            .await
            .unwrap_or_else(|error| {
                panic!("{name}: the bound must release the stream before the overload: {error:?}")
            });
        let (_, error) = drain(response).await;
        assert!(
            error.is_some(),
            "{name}: the overload must come in-stream after the bound released"
        );
    }
}

// Lines each well inside the byte cap that add up past it release the
// stream, against a passthrough client, where held lines take memory.
#[tokio::test]
async fn byte_cap_releases_stream() {
    let third = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "y".repeat(MAX_BOOTSTRAP_BYTES / 3)
    );
    let frames = format!("data: {third}\n\n").repeat(4);
    let url = serve_raw(format!("{frames}data: {OVERLOAD_EVENT}\n\n")).await;
    drain(started(start(&buffering(true), &url).await)).await;
}

// Keepalives and items announced with no content haven't reached the
// client, so an overload after one still fails over.
#[tokio::test]
async fn non_content_frames_do_not_release_stream() {
    for (name, frame) in [
        ("keepalive", KEEPALIVE_EVENT),
        ("output item added", OUTPUT_ADDED_EVENT),
        (
            "content part added",
            r#"{"type":"response.content_part.added","part":{"type":"output_text","text":""}}"#,
        ),
        (
            "reasoning summary part added",
            r#"{"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":""}}"#,
        ),
    ] {
        let url = serve_events(&[CREATED_EVENT, IN_PROGRESS_EVENT, frame, OVERLOAD_EVENT]).await;
        let error = start(&buffering(true), &url)
            .await
            .err()
            .unwrap_or_else(|| panic!("a {name} frame must keep the bootstrap window open"));
        assert_eq!(error.status, 503, "{name}");
    }
}

// Generated output, and a server-side tool that may already be running,
// release the stream (upstream's `codexReleasingFrameCases`).
#[tokio::test]
async fn content_frame_releases_before_overload() {
    for (name, frame) in [
        ("output text delta", OUTPUT_DELTA_EVENT),
        (
            "web search call announced",
            r#"{"type":"response.output_item.added","item":{"id":"ws_1","type":"web_search_call","status":"in_progress"},"output_index":0}"#,
        ),
    ] {
        let url = serve_events(&[CREATED_EVENT, IN_PROGRESS_EVENT, frame, OVERLOAD_EVENT]).await;
        let response = start(&buffering(true), &url)
            .await
            .unwrap_or_else(|error| panic!("a {name} frame must release the stream: {error:?}"));
        let (_, error) = drain(response).await;
        assert!(
            error.is_some(),
            "the overload after a {name} frame must come in-stream"
        );
    }
}

// TestCodexExecutor_BootstrapBuffering_CancelDuringBootstrapIsNotAnUpstreamFailure,
// adapted: held lines keep the call waiting, and dropping it, which is how
// a call is cancelled, closes Codex's connection.
#[tokio::test]
async fn dropping_the_call_during_bootstrap_closes_the_upstream() {
    let (closed, gone) = oneshot::channel();
    let closed = Arc::new(Mutex::new(Some(closed)));
    let url = serve(200, move |writer| {
        let closed = Arc::clone(&closed);
        async move {
            writer.write(&format!("data: {CREATED_EVENT}\n\n"));
            writer.closed().await;
            if let Some(closed) = closed.lock().unwrap_or_else(PoisonError::into_inner).take() {
                let _ = closed.send(());
            }
        }
    })
    .await;
    let executor = buffering(true);
    let call = start(&executor, &url);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), call)
            .await
            .is_err(),
        "a held handshake must keep the call waiting"
    );
    tokio::time::timeout(Duration::from_secs(10), gone)
        .await
        .expect("dropping the call must close Codex's connection")
        .unwrap();
}

// The frame budget in heartbeats, per framing, as config.example.yaml
// quotes it. Each count is one short of the obvious division because the
// overload's own event: line is charged too.
#[tokio::test]
async fn heartbeat_arithmetic_per_framing() {
    let overload = format!("event: error\ndata: {OVERLOAD_EVENT}\n\n");
    let cases = [
        (
            "three-line event:/data:/blank",
            format!("event: keepalive\ndata: {KEEPALIVE_EVENT}\n\n"),
            15,
        ),
        (
            "two-line : keepalive comment",
            ": keepalive\n\n".to_owned(),
            23,
        ),
        (
            "one-line : keepalive comment",
            ": keepalive\n".to_owned(),
            47,
        ),
    ];
    for (name, heartbeat, protected) in cases {
        for (heartbeats, want) in [(protected, true), (protected + 1, false)] {
            let url = serve_raw(format!("{}{overload}", heartbeat.repeat(heartbeats))).await;
            let fails_over = match start(&buffering(true), &url).await {
                Ok(response) => {
                    drain(response).await;
                    false
                }
                Err(_) => true,
            };
            assert_eq!(
                fails_over, want,
                "{heartbeats} heartbeats of the {name} framing (config.example.yaml documents {protected})"
            );
        }
    }
}

// The byte budget counts the chunks as well as the lines: both padded
// lines fit the cap on their own bytes.
#[tokio::test]
async fn byte_cap_counts_translated_chunks() {
    let padded = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "p".repeat(300 << 10)
    );
    let url = serve_events(&[&padded, &padded, OVERLOAD_EVENT]).await;
    let (_, error) = drain(started(start(&buffering(true), &url).await)).await;
    assert!(
        error.is_some(),
        "the overload must come in-stream after the byte budget released"
    );
}

// TestIsCodexBootstrapBufferableEvent: anything the client has seen, any
// server-side tool already started, and anything unknown releases.
#[test]
fn bootstrap_bufferable_events() {
    let bufferable = |payload: &str| {
        let event: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        is_bootstrap_bufferable_event(&str_at(&event, "type"), payload.as_bytes(), &event)
    };
    let hold = [
        r#"{"type":"response.created"}"#,
        r#"{"type":"response.in_progress"}"#,
        r#"{"type":"codex.rate_limits"}"#,
        r#"{"type":"codex.response.metadata"}"#,
        KEEPALIVE_EVENT,
        OUTPUT_ADDED_EVENT,
        r#"{"type":"response.output_item.added","item":{"id":"rs_1","type":"reasoning"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","arguments":""}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"ct_1","type":"custom_tool_call","input":""}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"msg_2","type":"message","content":[{"type":"output_text","text":""}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"msg_3","type":"message","content":[{"type":"refusal","refusal":""}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"rs_2","type":"reasoning","summary":[{"type":"summary_text","text":""}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"rs_3","type":"reasoning","content":[{"type":"reasoning_text","text":""}]}}"#,
        r#"{"type":"response.content_part.added","part":{"type":"text","text":""}}"#,
        r#"{"type":"response.reasoning_summary_part.added","part":{"type":"reasoning_text","text":""}}"#,
        r#"{"type":"response.content_part.added","part":{"type":"output_text","text":""}}"#,
        r#"{"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":""}}"#,
        "",
        "   ",
    ];
    for payload in hold {
        assert!(bufferable(payload), "must stay bufferable: {payload}");
    }
    let release = [
        r#"{"type":"response.output_text.delta","delta":"hi"}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":"x"}"#,
        r#"{"type":"response.shell_call_command.delta","delta":"ls"}"#,
        r#"{"type":"response.shell_call_output_content.delta","delta":{"stdout":"x"}}"#,
        r#"{"type":"response.shell_call_output_content.done","output":[]}"#,
        r#"{"type":"response.output_item.done","item":{"id":"m1","type":"message"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"ws_1","type":"web_search_call","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"fs_1","type":"file_search_call"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"ig_1","type":"image_generation_call"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"x_1","type":"some_future_server_tool"}}"#,
        r#"{"type":"response.content_part.added","part":{"type":"output_text","text":"already here"}}"#,
        r#"{"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":"already here"}}"#,
        r#"{"type":"response.content_part.added","part":{"type":"output_audio","audio":"AAAA"}}"#,
        r#"{"type":"response.content_part.added","part":{"type":"refusal","refusal":"I cannot help"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"rf1","type":"message","content":[{"type":"refusal","refusal":"I cannot help"}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"m1","type":"message","content":[{"type":"output_text","text":"already generated"}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"r1","type":"reasoning","summary":[{"type":"summary_text","text":"already reasoned"}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"f1","type":"function_call","arguments":"{\"path\":\"/\"}"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"c1","type":"custom_tool_call","input":"already here"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"a1","type":"message","content":[{"type":"output_audio","audio":"AAAA"}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"i1","type":"message","content":[{"type":"output_image","image_url":"data:x"}]}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"r2","type":"reasoning","encrypted_content":"BLOB"}}"#,
        r#"{"type":"response.output_item.added","item":{"id":"r3","type":"reasoning","content":[{"type":"reasoning_text","text":"already reasoned"}]}}"#,
        r#"{"type":"response.web_search_call.searching","item_id":"ws_1"}"#,
        COMPLETED_EVENT,
        OVERLOAD_EVENT,
        r#"{"type":"response.some_future_event_we_have_never_seen"}"#,
    ];
    for payload in release {
        assert!(!bufferable(payload), "must release the stream: {payload}");
    }
}

// Held events are replayed in upstream order, once each, ahead of the
// first generated one.
#[tokio::test]
async fn flushes_in_order_on_first_output() {
    let url = serve_events(&[
        CREATED_EVENT,
        IN_PROGRESS_EVENT,
        OUTPUT_ADDED_EVENT,
        OUTPUT_DELTA_EVENT,
        COMPLETED_EVENT,
    ])
    .await;
    let (combined, error) = drain(started(start(&buffering(true), &url).await)).await;
    assert!(error.is_none(), "unexpected chunk error: {error:?}");
    let order = [
        r#""type":"response.created""#,
        r#""type":"response.in_progress""#,
        r#""type":"response.output_item.added""#,
        r#""type":"response.output_text.delta""#,
    ];
    let mut previous = None;
    for marker in order {
        let at = combined
            .find(marker)
            .unwrap_or_else(|| panic!("missing {marker}: {combined}"));
        assert!(
            previous.is_none_or(|previous| at > previous),
            "frames must be replayed in upstream order, {marker} came too early: {combined}"
        );
        previous = Some(at);
    }
    for marker in order {
        assert_eq!(
            combined.matches(marker).count(),
            1,
            "held frame {marker} must come once: {combined}"
        );
    }
}

// With buffering off an overload keeps its in-stream delivery and its 502.
#[tokio::test]
async fn default_disabled_passthrough() {
    let url = serve_events(&[CREATED_EVENT, OVERLOAD_EVENT]).await;
    let executor = executor_with(Config::default());
    let (_, error) = drain(started(start(&executor, &url).await)).await;
    let error = error.expect("an in-stream error while buffering is off");
    assert_eq!(error.status, 502);
}

// TestIsCodexOverloadBootstrapFailureRejectsRequestFaults.
#[test]
fn overload_bootstrap_failure_rejects_request_faults() {
    for body in [
        r#"{"error":{"type":"invalid_request_error","code":"invalid_value"}}"#,
        r#"{"error":{"type":"authentication_error","code":"invalid_api_key"}}"#,
        r#"{"error":{"type":"upstream_error","code":"unknown"}}"#,
        r#"{"error":{"type":"server_error","code":"server_error","message":"An internal error occurred without retry advice"}}"#,
    ] {
        assert!(
            !is_overload_bootstrap_failure(body.as_bytes()),
            "a request fault must not fail over: {body}"
        );
    }
    for body in [
        r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        r#"{"error":{"type":"server_error","code":"server_error","message":"An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists. Please include the request ID 2f5014c9-7cfe-4fb7-813e-ecb447da3edd in your message."}}"#,
        r#"{"error":{"type":"server_error","code":"server_error","message":"You can retry your request"}}"#,
        r#"{"error":{"message":"Selected model is at capacity. Please try a different model."}}"#,
        r#"{"error":{"message":"Selected Model is at capacity"}}"#,
    ] {
        assert!(
            is_overload_bootstrap_failure(body.as_bytes()),
            "must fail over: {body}"
        );
    }
}

// TestCodexConfig_StreamBootstrapTimeoutDuration.
#[test]
fn stream_bootstrap_timeout_duration() {
    for (raw, want) in [
        ("", Duration::ZERO),
        ("   ", Duration::ZERO),
        ("10s", Duration::from_secs(10)),
        ("8s", Duration::from_secs(8)),
        ("15", Duration::from_secs(15)),
        ("500ms", Duration::from_millis(500)),
        ("0", Duration::ZERO),
        ("0s", Duration::ZERO),
        ("0m", Duration::ZERO),
        ("0ms", Duration::ZERO),
        ("none", Duration::ZERO),
        ("NONE", Duration::ZERO),
        ("unlimited", Duration::ZERO),
        ("disabled", Duration::ZERO),
        ("off", Duration::ZERO),
        ("never", Duration::ZERO),
        ("invalid", Duration::ZERO),
        ("-5s", Duration::ZERO),
        ("-1", Duration::ZERO),
        ("9223372037", Duration::ZERO),
        ("18446744074", Duration::ZERO),
        ("36028797018963968", Duration::ZERO),
    ] {
        let codex = CodexConfig {
            stream_bootstrap_timeout: raw.to_owned(),
            ..CodexConfig::default()
        };
        assert_eq!(codex.stream_bootstrap_timeout_duration(), want, "{raw:?}");
    }
}

/// A server that writes an in-progress event, moves `clock` on by `by`
/// (once the executor has read it, when `wait` is set), then writes another
/// and an overload.
async fn serve_overload_after(clock: &MockClock, wait: bool, by: Duration) -> String {
    let clock = clock.clone();
    serve(200, move |writer| {
        let clock = clock.clone();
        async move {
            writer.write(&format!(
                "event: response.in_progress\ndata: {IN_PROGRESS_EVENT}\n\n"
            ));
            if wait {
                clock.started().await;
            }
            clock.advance(by);
            writer.write(&format!(
                "event: response.in_progress\ndata: {IN_PROGRESS_EVENT}\n\n"
            ));
            writer.write(&format!("event: error\ndata: {OVERLOAD_EVENT}\n\n"));
        }
    })
    .await
}

// Past the time limit the stream is released, so the overload that follows
// comes in-stream.
#[tokio::test]
async fn time_budget_releases_stream() {
    let clock = MockClock::new();
    let url = serve_overload_after(&clock, true, Duration::from_secs(11)).await;
    let response = started(start(&timed("10s", &clock), &url).await);
    let (_, error) = drain(response).await;
    assert!(
        error.is_some(),
        "the overload must come in-stream after the time budget released"
    );
}

// With the time limit off, time passing doesn't release the stream.
#[tokio::test]
async fn disabled_time_budget() {
    let clock = MockClock::new();
    let url = serve_overload_after(&clock, false, Duration::from_secs(100)).await;
    assert!(
        start(&timed("none", &clock), &url).await.is_err(),
        "the call must fail over when the time budget is off"
    );
}

// An unset time limit means no limit.
#[tokio::test]
async fn default_unset_timeout_is_unlimited() {
    let clock = MockClock::new();
    let url = serve_overload_after(&clock, false, Duration::from_secs(100)).await;
    assert!(
        start(&timed("", &clock), &url).await.is_err(),
        "the call must fail over when the time budget is unset"
    );
}

// An overload that is the first event after the time limit comes
// in-stream rather than failing over.
#[tokio::test]
async fn overload_directly_after_timeout_delivered_in_stream() {
    let clock = MockClock::new();
    let server_clock = clock.clone();
    let url = serve(200, move |writer| {
        let clock = server_clock.clone();
        async move {
            clock.started().await;
            clock.advance(Duration::from_secs(11));
            writer.write(&format!("event: error\ndata: {OVERLOAD_EVENT}\n\n"));
        }
    })
    .await;
    let response = started(start(&timed("10s", &clock), &url).await);
    let (_, error) = drain(response).await;
    assert!(error.is_some(), "the overload must come in-stream");
}

// Not upstream's: the lines held back while the stream starts have the
// secrets the request sent redacted, as the ones that follow do: a comment
// and an event that quote the token reach the client without it.
#[tokio::test]
async fn held_lines_that_echo_the_token_hide_it() {
    const TOKEN: &str = "sk-codex-echo-0123456789";
    let created = format!(
        r#"{{"type":"response.created","response":{{"id":"resp_1","note":"key {TOKEN}"}}}}"#
    );
    let body = format!(
        ": key {TOKEN}\n\nevent: response.created\ndata: {created}\n\nevent: response.in_progress\ndata: {IN_PROGRESS_EVENT}\n\nevent: response.output_text.delta\ndata: {OUTPUT_DELTA_EVENT}\n\nevent: response.completed\ndata: {COMPLETED_EVENT}\n\n"
    );
    let url = serve_raw(body).await;
    let mut keyed = (*auth(&url)).clone();
    keyed.attributes.insert("api_key".into(), TOKEN.into());
    let request = Request {
        model: "gpt-5.6-terra".into(),
        payload: Bytes::from_static(br#"{"model":"gpt-5.6-terra","input":"hello"}"#),
    };
    let options = Options {
        stream: true,
        ..Options::new(Format::from("openai-response".to_owned()))
    };
    let response = started(
        buffering(true)
            .execute_stream(Arc::new(keyed), request, options)
            .await,
    );
    let (shown, error) = drain(response).await;
    assert!(error.is_none(), "unexpected chunk error: {error:?}");
    assert!(shown.contains(r#""type":"response.created""#), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(shown.contains("key [redacted]"), "{shown}");
}
