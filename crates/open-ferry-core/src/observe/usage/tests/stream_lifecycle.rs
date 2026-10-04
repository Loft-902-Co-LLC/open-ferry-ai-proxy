//! Tests of how a stream's record follows the stream's life: where the
//! reading stops, what a call dropped early publishes, and when a Codex
//! WebSocket's time to first token starts.
//!
//! Upstream has no test file for these; each follows the behaviour of its
//! executors that the test names.
//!
//! Deviations from upstream: the whole module, as its tests are not
//! upstream's.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use http::HeaderMap;
use serde_json::Value;

use super::support::{ClientCall, Harness, auth, bool_at, int_at, str_field};
use crate::exec::{ExecError, StreamResponse};
use crate::observe::{AttemptKind, CallReport, Observation, Outcome};

/// The record of a stream to `provider` for `model` that sends `chunks`, one
/// after the other, and ends.
fn stream_record(provider: &str, model: &str, chunks: &[&str]) -> Value {
    let harness = Harness::new();
    let driver = ClientCall::new(model).stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        provider,
        model,
        &auth("auth-1", "0", provider),
    );
    for chunk in chunks {
        driver.chunk(chunk);
    }
    driver.finish(Outcome::Completed);
    harness.record()
}

const OPENAI_USAGE: &str = "data: {\"usage\":{\"total_tokens\":3}}\n\n";
const OPENAI_DONE: &str = "data: [DONE]\n\n";
const OPENAI_LATE: &str = "data: {\"model\":\"late-model\",\"usage\":{\"total_tokens\":99}}\n\n";
const OPENAI_LATE_TAIL: &str = "data: {\"model\":\"late-model\",\"usage\":{\"total_tokens\":55}}";

/// Not upstream's: an OpenAI-compatible stream's counts and model are those
/// read up to its `[DONE]`, as upstream's executor stops reading there,
/// whether more comes later in the chunk, in a later chunk, or as a last
/// line without its newline.
#[test]
fn openai_stream_stops_at_done() {
    let same_chunk = format!("{OPENAI_USAGE}{OPENAI_DONE}{OPENAI_LATE}");
    let record = stream_record("openai", "gpt-5.4", &[&same_chunk, OPENAI_LATE]);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 3);
    assert!(record.get("response_model").is_none(), "{record}");

    let record = stream_record(
        "openai",
        "gpt-5.4",
        &[OPENAI_USAGE, OPENAI_DONE, OPENAI_LATE, OPENAI_LATE_TAIL],
    );
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 3);
    assert!(record.get("response_model").is_none(), "{record}");

    // The `[DONE]` line is the end, without waiting for the blank line.
    let record = stream_record(
        "openai",
        "gpt-5.4",
        &[OPENAI_USAGE, "data: [DONE]\n", OPENAI_LATE],
    );
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 3);

    // Without a `[DONE]`, the latest counts are the ones kept.
    let record = stream_record("openai", "gpt-5.4", &[OPENAI_USAGE, OPENAI_LATE]);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 99);
}

const CLAUDE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\n";
const CLAUDE_DELTA: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n";
const CLAUDE_STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const CLAUDE_LATE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"model\":\"late-model\",\"usage\":{\"input_tokens\":77,\"output_tokens\":5}}}\n\n";
const CLAUDE_LATE_DELTA: &str =
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":99}}\n\n";
const CLAUDE_LATE_TAIL: &str =
    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":55}}";

/// Not upstream's: a Claude stream's counts are those read up to its
/// `message_stop`, as upstream's executor, which marks the reply complete
/// there, reads its events; later lines, in the same chunk, in a later one
/// or unterminated at the end, change nothing.
#[test]
fn claude_stream_stops_at_message_stop() {
    let same_chunk = format!("{CLAUDE_STOP}{CLAUDE_LATE_START}{CLAUDE_LATE_DELTA}");
    let record = stream_record(
        "claude",
        "claude-opus-5",
        &[CLAUDE_START, CLAUDE_DELTA, &same_chunk],
    );
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 20);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 15);
    assert_eq!(str_field(&record, "response_model"), "claude-opus-5");

    let record = stream_record(
        "claude",
        "claude-opus-5",
        &[
            CLAUDE_START,
            CLAUDE_DELTA,
            CLAUDE_STOP,
            CLAUDE_LATE_START,
            CLAUDE_LATE_DELTA,
            CLAUDE_LATE_TAIL,
        ],
    );
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 20);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 15);
    assert!(!bool_at(&record, "/failed"));

    // The stop's data line is the end, without waiting for the blank line.
    let record = stream_record(
        "claude",
        "claude-opus-5",
        &[
            CLAUDE_START,
            CLAUDE_DELTA,
            "data: {\"type\":\"message_stop\"}\n",
            CLAUDE_LATE_DELTA,
        ],
    );
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 15);
}

/// Not upstream's: an answer a Claude call reads whole, as event stream
/// text, is read to its last line, as upstream's `Execute` does.
#[test]
fn claude_execute_reads_every_line_of_an_event_stream() {
    let harness = Harness::new();
    let driver = ClientCall::new("claude-opus-5").tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "claude",
        "claude-opus-5",
        &auth("claude-1", "0", "claude"),
    );
    driver.head(200, &[("content-type", "text/event-stream")]);
    driver.chunk(&format!(
        "{CLAUDE_START}{CLAUDE_DELTA}{CLAUDE_STOP}{CLAUDE_LATE_DELTA}"
    ));
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 99);
}

const CODEX_COMPLETED: &str =
    "{\"type\":\"response.completed\",\"response\":{\"usage\":{\"total_tokens\":3}}}";
const CODEX_LATE: &str = "{\"type\":\"response.completed\",\"response\":{\"model\":\"late-model\",\"usage\":{\"total_tokens\":99}}}";

/// Not upstream's: a Codex stream's counts are those of its first terminal
/// event, as upstream's executor stops at it; later events change neither
/// the counts nor the model.
#[test]
fn codex_stream_keeps_its_first_terminal_event() {
    let events = format!("data: {CODEX_COMPLETED}\n\ndata: {CODEX_LATE}\n\n");
    let record = stream_record("codex", "gpt-5.6-luna", &[&events]);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 3);
    assert!(record.get("response_model").is_none(), "{record}");

    let first = format!("data: {CODEX_COMPLETED}\n\n");
    let late = format!("data: {CODEX_LATE}\n\n");
    let tail = format!("data: {CODEX_LATE}");
    let record = stream_record("codex", "gpt-5.6-luna", &[&first, &late, &tail]);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 3);
    assert!(record.get("response_model").is_none(), "{record}");
}

/// Not upstream's: each message on a Codex WebSocket is read as an event,
/// and the counts are those of the first terminal one.
#[test]
fn codex_websocket_keeps_its_first_terminal_event() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.6-luna").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Websocket,
        "codex",
        "gpt-5.6-luna",
        &auth("codex-1", "0", "codex"),
    );
    driver.request_sent();
    driver.chunk(CODEX_COMPLETED);
    driver.chunk(CODEX_LATE);
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 3);
    assert!(record.get("response_model").is_none(), "{record}");
}

/// The streams a client can be dropped from before the answer's first
/// part, as a provider and how the attempt is made.
const STREAMS: [(&str, AttemptKind); 5] = [
    ("openai", AttemptKind::Stream),
    ("gemini", AttemptKind::Stream),
    ("codex", AttemptKind::Stream),
    ("claude", AttemptKind::Stream),
    ("codex", AttemptKind::Websocket),
];

/// Not upstream's: a call canceled while its executor was still connecting,
/// or waiting for the answer's head, or for its first part, is a failure
/// with status 499 and `context canceled`, whatever it streams; upstream's
/// executors record the error their canceled send or read returns
/// (`TrackFailure`, `PublishFailure`).
#[test]
fn a_stream_canceled_before_its_first_part_is_a_failure() {
    for (provider, kind) in STREAMS {
        for head in [false, true] {
            let harness = Harness::new();
            let driver = ClientCall::new("model-1").stream().tap(&harness);
            driver.attempt(kind, provider, "model-1", &auth("auth-1", "0", provider));
            if head {
                driver.head(200, &[("content-type", "text/event-stream")]);
            }
            driver.finish(Outcome::Canceled);
            let record = harness.record();
            let name = format!("{provider} {kind:?} head {head}");
            assert!(bool_at(&record, "/failed"), "{name}: {record}");
            assert_eq!(int_at(&record, "/fail/status_code"), 499, "{name}");
            assert_eq!(
                str_field(&record["fail"], "body"),
                "context canceled",
                "{name}"
            );
        }
    }
}

/// Not upstream's: a Claude stream canceled after some of its answer, and
/// before its `message_stop`, is a failure that keeps the counts read, as
/// upstream's executor publishes a canceled stream
/// (`StreamUsageBuffer.PublishFailure`); one canceled after its
/// `message_stop` is complete.
#[test]
fn a_canceled_claude_stream_is_a_failure_that_keeps_its_usage() {
    let harness = Harness::new();
    let driver = ClientCall::new("claude-opus-5").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "claude",
        "claude-opus-5",
        &auth("claude-1", "0", "claude"),
    );
    driver.chunk(CLAUDE_START);
    driver.chunk(CLAUDE_DELTA);
    driver.finish(Outcome::Canceled);
    let record = harness.record();
    assert!(bool_at(&record, "/failed"), "{record}");
    assert_eq!(int_at(&record, "/fail/status_code"), 499);
    assert_eq!(str_field(&record["fail"], "body"), "context canceled");
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 20);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 15);
    assert_eq!(str_field(&record, "response_model"), "claude-opus-5");

    let harness = Harness::new();
    let driver = ClientCall::new("claude-opus-5").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "claude",
        "claude-opus-5",
        &auth("claude-1", "0", "claude"),
    );
    driver.chunk(CLAUDE_START);
    driver.chunk(CLAUDE_DELTA);
    driver.chunk(CLAUDE_STOP);
    driver.finish(Outcome::Canceled);
    let record = harness.record();
    assert!(!bool_at(&record, "/failed"), "{record}");
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 15);
}

/// Not upstream's: a Claude stream whose reader goes away, with the
/// manager's report around the executor's stream as it makes it, publishes
/// the same failure, with the counts read.
#[tokio::test]
async fn a_dropped_claude_stream_is_a_failure_that_keeps_its_usage() {
    let harness = Harness::new();
    let driver = ClientCall::new("claude-opus-5").stream().tap(&harness);
    let observation = Arc::new(Observation::new(
        Arc::clone(&driver.context),
        vec![Arc::clone(&driver.tap)],
    ));
    let report = CallReport::observing(Some(&observation));
    driver.attempt(
        AttemptKind::Stream,
        "claude",
        "claude-opus-5",
        &auth("claude-1", "0", "claude"),
    );
    // The executor reads the first two parts, and the upstream goes quiet.
    let read = Arc::clone(&observation);
    let parts = futures_util::stream::iter([CLAUDE_START, CLAUDE_DELTA])
        .map(|part| Ok::<_, ExecError>(Bytes::from_static(part.as_bytes())))
        .inspect(move |part| {
            if let Ok(part) = part {
                read.chunk(part);
            }
        })
        .chain(futures_util::stream::pending())
        .boxed();
    let mut response = report
        .stream(Ok(StreamResponse {
            headers: HeaderMap::new(),
            chunks: parts,
        }))
        .expect("a stream");
    assert!(response.chunks.next().await.is_some());
    assert!(response.chunks.next().await.is_some());
    assert!(harness.records().is_empty(), "nothing is published yet");

    drop(response);
    let record = harness.record();
    assert!(bool_at(&record, "/failed"), "{record}");
    assert_eq!(int_at(&record, "/fail/status_code"), 499);
    assert_eq!(str_field(&record["fail"], "body"), "context canceled");
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 20);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 15);
}

/// A Codex WebSocket call's record: its request is announced, `dial_ms`
/// pass before it is sent, and its first token event comes `wait_ms` after
/// that.
fn websocket_record(dial_ms: u64, wait_ms: u64) -> Value {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.6-luna").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Websocket,
        "codex",
        "gpt-5.6-luna",
        &auth("codex-1", "0", "codex"),
    );
    harness.advance_ms(dial_ms);
    driver.request_sent();
    harness.advance_ms(wait_ms);
    driver.chunk("{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}");
    driver.chunk(CODEX_COMPLETED);
    driver.finish(Outcome::Completed);
    harness.record()
}

/// Not upstream's: a Codex WebSocket's time to first token counts from when
/// its request is sent on the open connection, as upstream starts it
/// (`StartResponseTTFT`), not from the dial, which the call's latency
/// still includes.
#[test]
fn websocket_ttft_starts_when_the_request_is_sent() {
    let record = websocket_record(300, 40);
    assert_eq!(int_at(&record, "/ttft_ms"), 40);
    assert_eq!(int_at(&record, "/latency_ms"), 340);

    let record = websocket_record(0, 25);
    assert_eq!(int_at(&record, "/ttft_ms"), 25);
}

/// Not upstream's: a send tried again on a new connection tells its request
/// is going out again, and the time to first token keeps its first start.
#[test]
fn websocket_ttft_keeps_the_first_send() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.6-luna").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Websocket,
        "codex",
        "gpt-5.6-luna",
        &auth("codex-1", "0", "codex"),
    );
    harness.advance_ms(100);
    driver.request_sent();
    harness.advance_ms(200);
    driver.request_sent();
    harness.advance_ms(30);
    driver.chunk("{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}");
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(int_at(&record, "/ttft_ms"), 230);
}
