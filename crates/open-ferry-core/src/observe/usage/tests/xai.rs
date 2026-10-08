//! Tests of how an xAI call's record is read: as upstream's `XAIExecutor`
//! reads its answers, with Codex usage for a turn and OpenAI usage for a
//! compaction.
//!
//! Deviations from upstream: the whole module. Upstream tests its xAI
//! usage only through the executor; these drive the tap as the xAI executor
//! reports its traffic.

use serde_json::Value;

use super::support::{ClientCall, Driver, Harness, auth, bool_at, int_at, str_field};
use crate::exec::{ExecError, Format};
use crate::observe::{AttemptKind, Outcome};

const MODEL: &str = "grok-4.3";

/// A turn's terminal event, with every count upstream reads.
const COMPLETED: &str = concat!(
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"grok-4.3-0709","#,
    r#""status":"completed","usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":80},"#,
    r#""output_tokens":30,"output_tokens_details":{"reasoning_tokens":12},"total_tokens":150}}}"#,
    "\n\n"
);

/// A compaction's answer, with every count upstream reads.
const COMPACTED: &str = concat!(
    r#"{"id":"resp_2","object":"response.compaction","model":"grok-4.3-0709","#,
    r#""output":[{"type":"compaction","encrypted_content":"e"}],"#,
    r#""usage":{"input_tokens":200,"input_tokens_details":{"cached_tokens":150},"#,
    r#""output_tokens":40,"output_tokens_details":{"reasoning_tokens":25},"total_tokens":240}}"#
);

/// A call to xAI of `kind`, its request in `format`.
fn xai_call(harness: &Harness, kind: AttemptKind, format: &Format) -> Driver {
    let call = ClientCall::new(MODEL);
    let call = if kind == AttemptKind::Stream {
        call.stream()
    } else {
        call
    };
    let driver = call.tap(harness);
    driver.attempt_with(
        kind,
        "xai",
        MODEL,
        format,
        &auth("xai-1", "0", "xai"),
        &[],
        "{}",
    );
    driver
}

/// Asserts `record` is the xAI executor's, with the given input, output,
/// total, cached and reasoning counts.
#[track_caller]
fn assert_counts(record: &Value, counts: [i64; 5]) {
    assert_eq!(
        str_field(record, "executor_type"),
        "XAIExecutor",
        "{record}"
    );
    assert_eq!(str_field(record, "provider"), "xai", "{record}");
    assert!(!bool_at(record, "/failed"), "{record}");
    for (name, count) in [
        "input_tokens",
        "output_tokens",
        "total_tokens",
        "cached_tokens",
        "reasoning_tokens",
    ]
    .into_iter()
    .zip(counts)
    {
        assert_eq!(int_at(record, &format!("/tokens/{name}")), count, "{name}");
    }
}

/// Not upstream's: a turn is read as Codex's, up to its
/// `response.completed`, and names the executor type upstream's records
/// name.
#[test]
fn execute_reads_codex_usage() {
    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Execute, &Format::CODEX);
    driver.chunk(concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"grok-4.3-0709"}}"#,
        "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","delta":"hi"}"#,
        "\n\n",
    ));
    driver.chunk(COMPLETED);
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_counts(&record, [120, 30, 150, 80, 12]);
    assert_eq!(str_field(&record, "response_model"), "grok-4.3-0709");
}

/// Not upstream's: an `response.incomplete` turn's counts are read too.
#[test]
fn execute_reads_an_incomplete_turn() {
    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Execute, &Format::CODEX);
    driver.chunk(&COMPLETED.replace("response.completed", "response.incomplete"));
    driver.finish(Outcome::Completed);
    assert_counts(&harness.record(), [120, 30, 150, 80, 12]);
}

/// Not upstream's: a stream's counts are those of its last terminal event,
/// published when it ends; its time to first token is its first byte, as
/// upstream's tracked HTTP client marks it, not its first token.
#[test]
fn stream_reads_codex_usage_at_its_end() {
    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Stream, &Format::CODEX);
    harness.advance_ms(15);
    driver.chunk(concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"grok-4.3-0709"}}"#,
        "\n\n",
    ));
    harness.advance_ms(30);
    driver.chunk(concat!(
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","delta":"hi"}"#,
        "\n\n",
    ));
    driver.chunk(&COMPLETED.replace("150}", "150},\"service_tier\":\"default\""));
    assert!(harness.records().is_empty(), "published before the end");
    harness.advance_ms(5);
    driver.chunk(COMPLETED);
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_counts(&record, [120, 30, 150, 80, 12]);
    assert_eq!(int_at(&record, "/ttft_ms"), 15);
    assert_eq!(int_at(&record, "/latency_ms"), 50);
    assert_eq!(str_field(&record, "response_model"), "grok-4.3-0709");
}

/// Not upstream's: a stream that names no counts publishes nothing, as
/// upstream's stream buffer has nothing to publish; a failed one is a
/// failure.
#[test]
fn stream_without_counts_or_failed() {
    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Stream, &Format::CODEX);
    driver.chunk("event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n");
    driver.finish(Outcome::Completed);
    assert!(harness.records().is_empty());

    let driver = xai_call(&harness, AttemptKind::Stream, &Format::CODEX);
    driver.chunk(COMPLETED);
    driver.fail(&ExecError::upstream(502, "upstream failed"));
    let record = harness.record();
    assert!(bool_at(&record, "/failed"), "{record}");
    assert_eq!(str_field(&record, "executor_type"), "XAIExecutor");
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 0);
}

/// Not upstream's: a compaction is read whole as OpenAI JSON, streamed or
/// not; only the compaction-trigger stream names its model, as upstream's
/// executor observes it there alone.
#[test]
fn compaction_reads_openai_usage() {
    for (kind, model) in [
        (AttemptKind::Execute, None),
        (AttemptKind::Stream, Some("grok-4.3-0709")),
    ] {
        let harness = Harness::new();
        let driver = xai_call(&harness, kind, &Format::OPENAI_RESPONSE);
        let (head, tail) = COMPACTED.split_at(40);
        driver.chunk(head);
        driver.chunk(tail);
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_counts(&record, [200, 40, 240, 150, 25]);
        assert_eq!(
            record.get("response_model").and_then(Value::as_str),
            model,
            "{kind:?}: {record}"
        );
    }
}

/// Not upstream's: an image or video call is read whole for the model its
/// answer names, if any, and no counts, even ones it names, as upstream's
/// `executeImages` and `executeVideos` read it (`ObserveResponseModel` and
/// `EnsurePublished`); a failed one is a failure.
#[test]
fn image_and_video_calls_name_their_model_and_no_counts() {
    for format in [Format::OPENAI_IMAGE, Format::OPENAI_VIDEO] {
        media_call_names_its_model_and_no_counts(&format);
    }
}

fn media_call_names_its_model_and_no_counts(format: &Format) {
    for (answer, model) in [
        (
            concat!(
                r#"{"created":123,"model":"grok-imagine-image-0801","data":[{"b64_json":"AA=="}],"#,
                r#""usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}"#
            ),
            Some("grok-imagine-image-0801"),
        ),
        (
            r#"{"created":123,"data":[{"b64_json":"AA=="}],"usage":{"cost_in_usd_ticks":250000}}"#,
            None,
        ),
    ] {
        let harness = Harness::new();
        let driver = xai_call(&harness, AttemptKind::Execute, format);
        let (head, tail) = answer.split_at(30);
        driver.chunk(head);
        driver.chunk(tail);
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_counts(&record, [0; 5]);
        assert_eq!(
            record.get("response_model").and_then(Value::as_str),
            model,
            "{format:?}: {record}"
        );
    }

    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Execute, format);
    driver.fail(&ExecError::upstream(429, r#"{"error":"rate limited"}"#));
    let record = harness.record();
    assert!(bool_at(&record, "/failed"), "{format:?}: {record}");
    assert_eq!(str_field(&record, "executor_type"), "XAIExecutor");
    assert_eq!(int_at(&record, "/fail/status_code"), 429);
}

/// Not upstream's test: a speech call's answer, audio, isn't read, so its
/// record names no response model and no counts, even when the answer
/// looks like JSON that names them, as upstream's `executeSpeech`
/// publishes it (`EnsurePublished`); a failed one is a failure.
#[test]
fn speech_calls_name_no_response_model_and_no_counts() {
    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Execute, &Format::OPENAI_SPEECH);
    driver.chunk(r#"{"model":"grok-tts-0801","#);
    driver.chunk(r#""usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}"#);
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_counts(&record, [0; 5]);
    assert_eq!(record.get("response_model"), None, "{record}");
    assert!(!bool_at(&record, "/failed"), "{record}");

    let harness = Harness::new();
    let driver = xai_call(&harness, AttemptKind::Execute, &Format::OPENAI_SPEECH);
    driver.fail(&ExecError::upstream(429, r#"{"error":"rate limited"}"#));
    let record = harness.record();
    assert!(bool_at(&record, "/failed"), "{record}");
    assert_eq!(str_field(&record, "executor_type"), "XAIExecutor");
    assert_eq!(int_at(&record, "/fail/status_code"), 429);
}

/// An xAI WebSocket call: its request is announced, sent `dial_ms` later,
/// and each of `messages` comes 10ms after the one before, the first
/// `wait_ms` after the send.
fn websocket_call(harness: &Harness, dial_ms: u64, wait_ms: u64, messages: &[&str]) -> Driver {
    let driver = ClientCall::new(MODEL).stream().tap(harness);
    driver.attempt_with(
        AttemptKind::Websocket,
        "xai",
        MODEL,
        &Format::CODEX,
        &auth("xai-1", "0", "xai"),
        &[],
        "{}",
    );
    harness.advance_ms(dial_ms);
    driver.request_sent();
    harness.advance_ms(wait_ms);
    for message in messages {
        driver.chunk(message);
        harness.advance_ms(10);
    }
    driver
}

/// Not upstream's: each message on an xAI WebSocket is read as an event,
/// and the counts are those of its `response.completed` or `response.done`,
/// published when it ends, as upstream's `XAIWebsocketsExecutor` names
/// itself; its time to first token runs from the send to the first
/// message, not from the dial.
#[test]
fn websocket_reads_codex_usage_at_its_end() {
    let created =
        r#"{"type":"response.created","response":{"id":"resp_1","model":"grok-4.3-0709"}}"#;
    let completed = COMPLETED
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    for terminal in ["response.completed", "response.done"] {
        let harness = Harness::new();
        let terminal_event = completed.replace("response.completed", terminal);
        let driver = websocket_call(&harness, 300, 25, &[created, &terminal_event]);
        assert!(harness.records().is_empty(), "published before the end");
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_eq!(
            str_field(&record, "executor_type"),
            "XAIWebsocketsExecutor",
            "{record}"
        );
        assert!(!bool_at(&record, "/failed"), "{record}");
        assert_eq!(int_at(&record, "/tokens/total_tokens"), 150, "{terminal}");
        assert_eq!(int_at(&record, "/tokens/cached_tokens"), 80, "{terminal}");
        assert_eq!(
            int_at(&record, "/tokens/reasoning_tokens"),
            12,
            "{terminal}"
        );
        assert_eq!(int_at(&record, "/ttft_ms"), 25, "{terminal}");
        assert_eq!(str_field(&record, "response_model"), "grok-4.3-0709");
    }
}

/// Not upstream's: an xAI WebSocket that named no counts (its
/// `response.incomplete` isn't read, as upstream's executor doesn't) publishes
/// nothing; a failed one is a failure.
#[test]
fn websocket_without_counts_or_failed() {
    let incomplete = COMPLETED
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap()
        .replace("response.completed", "response.incomplete");
    let harness = Harness::new();
    let driver = websocket_call(&harness, 0, 5, &[&incomplete]);
    driver.finish(Outcome::Completed);
    assert!(harness.records().is_empty());

    let driver = websocket_call(&harness, 0, 5, &[r#"{"type":"response.created"}"#]);
    driver.fail(&ExecError::upstream(502, "upstream failed"));
    let record = harness.record();
    assert!(bool_at(&record, "/failed"), "{record}");
    assert_eq!(str_field(&record, "executor_type"), "XAIWebsocketsExecutor");
    assert_eq!(int_at(&record, "/fail/status_code"), 502);
}
