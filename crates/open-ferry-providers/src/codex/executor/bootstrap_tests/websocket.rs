// Ported from CLIProxyAPI internal/runtime/executor/codex_stream_bootstrap_buffering_test.go
// (the TestCodexWebsocketsExecutor_* tests) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Stream bootstrap buffering over the Responses WebSocket, against a mock
//! Codex WebSocket on 127.0.0.1 that sends its messages after reading the
//! client's.
//!
//! Upstream's tests call its WebSocket executor with any client; here only a
//! client on the Responses WebSocket takes that route, so the calls are
//! from one, and its chunks are Codex's events.
//!
//! Deviations from upstream:
//! - `TestCodexWebsocketsExecutor_BootstrapOverload_DoesNotNotifyDownstreamDisconnect`
//!   and `TestCodexWebsocketsExecutor_BootstrapNonOverload_StillNotifiesDownstreamDisconnect`
//!   are dropped: `UpstreamDisconnectChan` isn't ported.
//! - The "budget counts the upstream message" case of
//!   `TestCodexWebsocketsExecutor_BootstrapBuffering_ByteCapOrderingAndSeed`
//!   is from an OpenAI Responses client, whose chunks are the messages, not
//!   from a Chat Completions client, which doesn't take this route.

use super::*;
use crate::codex::websocket::mock::{Answer, Server};

/// An API key credential for `base_url`, with websockets on.
fn ws_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = Arc::unwrap_or_clone(auth(base_url));
    auth.attributes.insert("websockets".into(), "true".into());
    Arc::new(auth)
}

/// Starts upstream's `codexWebsocketRequest` from a client on the Responses
/// WebSocket.
async fn start_ws(executor: &CodexExecutor, server: &Server) -> Result<StreamResponse, ExecError> {
    let request = Request {
        model: "gpt-5.6-terra".into(),
        payload: Bytes::from_static(
            br#"{"model":"gpt-5.6-terra","input":[{"type":"message","role":"user","content":"hello"}]}"#,
        ),
    };
    let options = Options {
        stream: true,
        downstream_websocket: true,
        ..Options::new(Format::OPENAI_RESPONSE)
    };
    executor
        .execute_stream(ws_auth(&server.url), request, options)
        .await
}

/// A server that sends `frames` after the client's message, then drops the
/// connection (upstream's `codexWebsocketServer` and
/// `codexWebsocketRawServer`).
async fn serve_frames(frames: &[&str]) -> Server {
    Server::once(frames).await
}

/// `count` copies of `frame`, then `tail`.
fn repeated(frame: &str, count: usize, tail: &str) -> Vec<String> {
    let mut frames = vec![frame.to_owned(); count];
    frames.push(tail.to_owned());
    frames
}

async fn serve_owned(frames: &[String]) -> Server {
    let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
    serve_frames(&frames).await
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_FrameBudgetReleasesStream
#[tokio::test]
async fn frame_budget_releases_stream() {
    let server = serve_owned(&repeated(
        IN_PROGRESS_EVENT,
        MAX_BOOTSTRAP_FRAMES + 1,
        OVERLOAD_EVENT,
    ))
    .await;
    let response = start_ws(&buffering(true), &server)
        .await
        .unwrap_or_else(|error| {
            panic!("the frame budget must release the stream before the overload: {error:?}")
        });
    drain(response).await;
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_BudgetBoundaryIsExact
#[tokio::test]
async fn budget_boundary_is_exact() {
    async fn fails_over(count: usize) -> bool {
        let server = serve_owned(&repeated(IN_PROGRESS_EVENT, count, OVERLOAD_EVENT)).await;
        match start_ws(&buffering(true), &server).await {
            Ok(response) => {
                drain(response).await;
                false
            }
            Err(_) => true,
        }
    }
    assert!(
        fails_over(MAX_BOOTSTRAP_FRAMES - 1).await,
        "messages inside the budget must still fail over"
    );
    assert!(
        fails_over(MAX_BOOTSTRAP_FRAMES).await,
        "messages exactly filling the budget must still fail over"
    );
    assert!(
        !fails_over(MAX_BOOTSTRAP_FRAMES + 1).await,
        "messages past the budget must release the stream"
    );
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_ByteCapOrderingAndSeed
#[tokio::test]
async fn byte_cap_ordering_and_seed() {
    // oversized message is not admitted
    let oversized = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "q".repeat(MAX_BOOTSTRAP_BYTES * 2)
    );
    let server = serve_frames(&[&oversized, OVERLOAD_EVENT]).await;
    let response = start_ws(&buffering(true), &server)
        .await
        .unwrap_or_else(|error| {
            panic!("a message larger than the cap must be released, not admitted: {error:?}")
        });
    drain(response).await;

    // budget counts the upstream message
    let frame = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "w".repeat(MAX_BOOTSTRAP_BYTES / 8)
    );
    let server = serve_owned(&repeated(&frame, 10, OVERLOAD_EVENT)).await;
    let response = start_ws(&buffering(true), &server)
        .await
        .unwrap_or_else(|error| {
            panic!("ten messages of cap/8 must exhaust the byte budget: {error:?}")
        });
    drain(response).await;
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_BudgetFrameIsNotDropped
#[tokio::test]
async fn budget_frame_is_not_dropped() {
    let mut frames = vec![IN_PROGRESS_EVENT.to_owned(); MAX_BOOTSTRAP_FRAMES];
    frames.push(
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"FIRSTTOKEN"}"#
            .to_owned(),
    );
    frames.push(COMPLETED_EVENT.to_owned());
    let server = serve_owned(&frames).await;
    let (combined, _) = drain(started(start_ws(&buffering(true), &server).await)).await;
    assert!(
        combined.contains("FIRSTTOKEN"),
        "the frame that tripped the budget was dropped: {combined}"
    );
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_SkippedFramesExhaustTheWindow
#[tokio::test]
async fn skipped_frames_exhaust_the_window() {
    for (name, frame) in [("whitespace only frames", "   \n\t "), ("empty frames", "")] {
        let server = serve_owned(&repeated(frame, MAX_BOOTSTRAP_FRAMES + 2, OVERLOAD_EVENT)).await;
        let response = start_ws(&buffering(true), &server)
            .await
            .unwrap_or_else(|error| {
                panic!("{name}: the window must release before the overload: {error:?}")
            });
        drain(response).await;
    }
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_ByteCapReleasesStream
#[tokio::test]
async fn byte_cap_releases_stream() {
    let third = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "z".repeat(MAX_BOOTSTRAP_BYTES / 3)
    );
    let server = serve_owned(&repeated(&third, 4, OVERLOAD_EVENT)).await;
    let response = start_ws(&buffering(true), &server)
        .await
        .unwrap_or_else(|error| {
            panic!("the byte cap must release the stream before the overload: {error:?}")
        });
    drain(response).await;
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_NonContentFramesDoNotReleaseStream
#[tokio::test]
async fn non_content_frames_do_not_release_stream() {
    for frame in [KEEPALIVE_EVENT, OUTPUT_ADDED_EVENT] {
        let server = serve_frames(&[CREATED_EVENT, IN_PROGRESS_EVENT, frame, OVERLOAD_EVENT]).await;
        let error = failed_over(start_ws(&buffering(true), &server).await);
        assert_eq!(error.status, 503, "{frame}");
    }
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_ContentFrameReleasesBeforeOverload
#[tokio::test]
async fn content_frame_releases_before_overload() {
    for (name, frame) in [
        ("output text delta", OUTPUT_DELTA_EVENT),
        (
            "web search call announced",
            r#"{"type":"response.output_item.added","item":{"id":"ws_1","type":"web_search_call","status":"in_progress"},"output_index":0}"#,
        ),
    ] {
        let server = serve_frames(&[CREATED_EVENT, IN_PROGRESS_EVENT, frame, OVERLOAD_EVENT]).await;
        let response = start_ws(&buffering(true), &server)
            .await
            .unwrap_or_else(|error| panic!("a {name} frame must release the stream: {error:?}"));
        let (_, error) = drain(response).await;
        assert!(
            error.is_some(),
            "the overload after a {name} frame must come in-stream"
        );
    }
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_ByteCapCountsTranslatedChunks
#[tokio::test]
async fn byte_cap_counts_translated_chunks() {
    let padded = format!(
        r#"{{"type":"response.in_progress","response":{{"id":"{}"}}}}"#,
        "p".repeat(300 << 10)
    );
    let server = serve_owned(&repeated(&padded, 2, OVERLOAD_EVENT)).await;
    let response = start_ws(&buffering(true), &server)
        .await
        .unwrap_or_else(|error| {
            panic!("messages whose bytes and chunks pass the cap must release: {error:?}")
        });
    let (_, error) = drain(response).await;
    assert!(
        error.is_some(),
        "the overload must come in-stream after the byte budget released"
    );
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_OverloadFailsAttempt
#[tokio::test]
async fn overload_fails_attempt() {
    let server = serve_frames(&[CREATED_EVENT, IN_PROGRESS_EVENT, OVERLOAD_EVENT]).await;
    let error = failed_over(start_ws(&buffering(true), &server).await);
    assert_eq!(error.status, 503);
    let record = server.record();
    assert_eq!(
        record.handshakes.len(),
        1,
        "the call didn't go over the WebSocket"
    );
    assert_eq!(record.messages.len(), 1);
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_CapacityFailsAttempt
#[tokio::test]
async fn capacity_fails_attempt() {
    let server = serve_frames(&[CREATED_EVENT, IN_PROGRESS_EVENT, CAPACITY_EVENT]).await;
    let error = failed_over(start_ws(&buffering(true), &server).await);
    assert_eq!(error.status, 429);
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_PrivateHandshakeFramesDoNotExhaustWindow
#[tokio::test]
async fn private_handshake_frames_do_not_exhaust_window() {
    let server = serve_frames(&[
        r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":1}}}"#,
        r#"{"type":"codex.response.metadata","metadata":{"conversation_id":"conv_1"}}"#,
        CREATED_EVENT,
        IN_PROGRESS_EVENT,
        OVERLOAD_EVENT,
    ])
    .await;
    let error = failed_over(start_ws(&buffering(true), &server).await);
    assert_eq!(error.status, 503);
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_NonOverloadStaysInStream
#[tokio::test]
async fn non_overload_stays_in_stream() {
    let server = serve_frames(&[CREATED_EVENT, IN_PROGRESS_EVENT, INVALID_EVENT]).await;
    let (combined, error) = drain(started(start_ws(&buffering(true), &server).await)).await;
    assert!(
        error.is_some(),
        "the invalid request must come as an in-stream error"
    );
    assert!(
        combined.contains(r#""type":"response.created""#),
        "the held handshake must come before the in-stream error: {combined}"
    );
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_FlushesInOrderOnFirstOutput
#[tokio::test]
async fn flushes_in_order_on_first_output() {
    let server = serve_frames(&[
        CREATED_EVENT,
        IN_PROGRESS_EVENT,
        OUTPUT_ADDED_EVENT,
        OUTPUT_DELTA_EVENT,
        r#"{"type":"response.completed","response":{"id":"resp_1","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}"#,
    ])
    .await;
    let (combined, error) = drain(started(start_ws(&buffering(true), &server).await)).await;
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

// TestCodexWebsocketsExecutor_BootstrapBuffering_DefaultDisabledPassthrough
#[tokio::test]
async fn default_disabled_passthrough() {
    let server = serve_frames(&[CREATED_EVENT, OVERLOAD_EVENT]).await;
    let executor = executor_with(Config::default());
    let (_, error) = drain(started(start_ws(&executor, &server).await)).await;
    let error = error.expect("an in-stream error while buffering is off");
    assert_eq!(error.status, 502);
}

/// A server that sends an in-progress event, moves `clock` on by `by`
/// (once the executor has read it, when `wait` is set), then sends another
/// and an overload.
async fn serve_overload_after(clock: &MockClock, wait: bool, by: Duration) -> Server {
    let clock = clock.clone();
    Server::start(move |_| {
        let clock = clock.clone();
        Answer::accept(move |mut peer| {
            let clock = clock.clone();
            async move {
                if peer.recv().await.is_none() {
                    return;
                }
                peer.send(IN_PROGRESS_EVENT).await;
                if wait {
                    clock.started().await;
                }
                clock.advance(by);
                peer.send(IN_PROGRESS_EVENT).await;
                peer.send(OVERLOAD_EVENT).await;
            }
        })
    })
    .await
}

/// A server that waits for the executor to read `clock`, moves it past
/// the time limit, then sends `frame`.
async fn serve_after_timeout(clock: &MockClock, frame: &'static str) -> Server {
    let clock = clock.clone();
    Server::start(move |_| {
        let clock = clock.clone();
        Answer::accept(move |mut peer| {
            let clock = clock.clone();
            async move {
                if peer.recv().await.is_none() {
                    return;
                }
                clock.started().await;
                clock.advance(Duration::from_secs(11));
                peer.send(frame).await;
            }
        })
    })
    .await
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_TimeBudgetReleasesStream
#[tokio::test]
async fn time_budget_releases_stream() {
    let clock = MockClock::new();
    let server = serve_overload_after(&clock, true, Duration::from_secs(11)).await;
    let response = started(start_ws(&timed("10s", &clock), &server).await);
    let (_, error) = drain(response).await;
    assert!(
        error.is_some(),
        "the overload must come in-stream after the time budget released"
    );
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_DisabledTimeBudget
#[tokio::test]
async fn disabled_time_budget() {
    let clock = MockClock::new();
    let server = serve_overload_after(&clock, false, Duration::from_secs(100)).await;
    assert!(
        start_ws(&timed("none", &clock), &server).await.is_err(),
        "the call must fail over when the time budget is off"
    );
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_OverloadDirectlyAfterTimeoutDeliveredInStream
#[tokio::test]
async fn overload_directly_after_timeout_delivered_in_stream() {
    let clock = MockClock::new();
    let server = serve_after_timeout(&clock, OVERLOAD_EVENT).await;
    let response = started(start_ws(&timed("10s", &clock), &server).await);
    let (_, error) = drain(response).await;
    assert!(error.is_some(), "the overload must come in-stream");
}

// TestCodexWebsocketsExecutor_BootstrapBuffering_StatusBearingErrorAfterTimeoutDeliveredInStream
#[tokio::test]
async fn status_bearing_error_after_timeout_delivered_in_stream() {
    let clock = MockClock::new();
    let server = serve_after_timeout(
        &clock,
        r#"{"type":"error","status":429,"error":{"message":"Rate limit exceeded","type":"requests","code":"rate_limit_exceeded"}}"#,
    )
    .await;
    let response = started(start_ws(&timed("10s", &clock), &server).await);
    let (_, error) = drain(response).await;
    let error = error.expect("the status-bearing error must come in-stream");
    assert_eq!(error.status, 429);
}
