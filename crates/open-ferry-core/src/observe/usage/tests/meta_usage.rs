//! Not upstream's: Meta's calls are counted as the Responses format Meta
//! speaks, and recorded as its own executor's.
//!
//! Upstream's Meta executor publishes the counts of the `response.completed`
//! (or `response.incomplete`) event it reads, with `ParseCodexUsage`, and
//! its records name the `MetaExecutor` type. The tap reads the same events,
//! for an Execute, which Meta serves from the stream it always asks for, and
//! for a Stream, so these tests run both through it.

use super::support::{ClientCall, Harness, auth, int_at, str_field};
use crate::exec::ExecError;
use crate::observe::{AttemptKind, Outcome};

const CREATED: &str =
    r#"data: {"type":"response.created","response":{"id":"resp_1","model":"muse-spark"}}"#;
const DELTA: &str = r#"data: {"type":"response.output_text.delta","delta":"hi"}"#;
const COMPLETED: &str = r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"muse-spark","usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}}}"#;
const INCOMPLETE: &str = r#"data: {"type":"response.incomplete","response":{"id":"resp_1","model":"muse-spark","usage":{"input_tokens":5,"output_tokens":4,"total_tokens":9}}}"#;

/// Runs a Meta call of `kind` through the usage tap, the answer split in
/// the middle of its last event, and returns its one record.
fn meta_record(kind: AttemptKind, events: &[&str]) -> serde_json::Value {
    let harness = Harness::new();
    let call = ClientCall::new("muse-spark");
    let driver = match kind {
        AttemptKind::Stream => call.stream().tap(&harness),
        _ => call.tap(&harness),
    };
    driver.attempt(kind, "meta", "muse-spark", &auth("meta-1", "0", "meta"));
    driver.head(200, &[("content-type", "text/event-stream")]);
    let body = events.join("\n\n") + "\n\n";
    let (first, second) = body.split_at(body.len() - 40);
    driver.chunk(first);
    driver.chunk(second);
    driver.finish(Outcome::Completed);
    harness.record()
}

#[test]
fn an_execute_is_counted_from_its_completed_event() {
    let record = meta_record(AttemptKind::Execute, &[CREATED, DELTA, COMPLETED]);
    assert_eq!(str_field(&record, "provider"), "meta");
    assert_eq!(str_field(&record, "executor_type"), "MetaExecutor");
    assert_eq!(str_field(&record, "response_model"), "muse-spark");
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 11);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 7);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 18);
    assert_eq!(int_at(&record, "/tokens/cached_tokens"), 3);
    assert_eq!(int_at(&record, "/tokens/reasoning_tokens"), 2);
    assert_eq!(record.get("failed").and_then(|v| v.as_bool()), Some(false));
}

#[test]
fn a_stream_is_counted_from_its_completed_event() {
    let record = meta_record(AttemptKind::Stream, &[CREATED, DELTA, COMPLETED]);
    assert_eq!(str_field(&record, "provider"), "meta");
    assert_eq!(str_field(&record, "executor_type"), "MetaExecutor");
    assert_eq!(str_field(&record, "response_model"), "muse-spark");
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 11);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 7);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 18);
    assert_eq!(int_at(&record, "/tokens/cached_tokens"), 3);
    assert_eq!(int_at(&record, "/tokens/reasoning_tokens"), 2);
    assert_eq!(record.get("failed").and_then(|v| v.as_bool()), Some(false));
}

#[test]
fn an_incomplete_event_counts_too() {
    for kind in [AttemptKind::Execute, AttemptKind::Stream] {
        let record = meta_record(kind, &[CREATED, DELTA, INCOMPLETE]);
        assert_eq!(int_at(&record, "/tokens/input_tokens"), 5, "{kind:?}");
        assert_eq!(int_at(&record, "/tokens/output_tokens"), 4, "{kind:?}");
        assert_eq!(int_at(&record, "/tokens/total_tokens"), 9, "{kind:?}");
    }
}

/// Runs a Meta Execute whose answer is `body` through the usage tap, ending
/// as `end` says, and returns its one record.
fn meta_execute(body: &str, end: Option<&ExecError>) -> serde_json::Value {
    let harness = Harness::new();
    let driver = ClientCall::new("muse-spark").tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "meta",
        "muse-spark",
        &auth("meta-1", "0", "meta"),
    );
    driver.head(200, &[("content-type", "application/json")]);
    driver.chunk(body);
    match end {
        Some(error) => driver.fail(error),
        None => driver.finish(Outcome::Completed),
    }
    harness.record()
}

// Upstream's `metaAsCompletedEvent`: a reply that isn't a stream, a
// Responses object or a completed event, is counted as the completed event
// it stands for, or with no counts when it names none (`EnsurePublished`).
#[test]
fn a_plain_reply_is_counted_as_its_completed_event() {
    let object = r#"{"object":"response","model":"muse-spark","output":[],"usage":{"input_tokens":11,"output_tokens":7}}"#;
    let event = r#"{"type":"response.completed","response":{"model":"muse-spark","usage":{"input_tokens":11,"output_tokens":7}}}"#;
    for body in [object, event] {
        let record = meta_execute(body, None);
        assert_eq!(int_at(&record, "/tokens/input_tokens"), 11, "{body}");
        assert_eq!(int_at(&record, "/tokens/output_tokens"), 7, "{body}");
        assert_eq!(str_field(&record, "response_model"), "muse-spark", "{body}");
    }
    let record = meta_execute(r#"{"output":[]}"#, None);
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 0);
    assert_eq!(record.get("failed").and_then(|v| v.as_bool()), Some(false));
}

// v8.0.20's `upstreamUsage.PublishFailure`: an error that keeps the answer's
// usage makes a failure with its counts; any other error, one without.
#[test]
fn a_failure_keeps_the_counts_only_when_its_error_does() {
    let stream = [CREATED, DELTA, COMPLETED].join("\n\n") + "\n\n";
    let plain =
        r#"{"object":"response","output":[],"usage":{"input_tokens":11,"output_tokens":7}}"#;
    let kept = ExecError::upstream(502, "patch").with_usage_kept();
    let dropped = ExecError::upstream(502, "patch");
    for body in [stream.as_str(), plain] {
        let record = meta_execute(body, Some(&kept));
        assert_eq!(record.get("failed").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(int_at(&record, "/tokens/input_tokens"), 11, "{body}");
        assert_eq!(int_at(&record, "/tokens/output_tokens"), 7, "{body}");
        let record = meta_execute(body, Some(&dropped));
        assert_eq!(record.get("failed").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(int_at(&record, "/tokens/input_tokens"), 0, "{body}");
    }
    // A stream keeps what it had read.
    let harness = Harness::new();
    let driver = ClientCall::new("muse-spark").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "meta",
        "muse-spark",
        &auth("meta-1", "0", "meta"),
    );
    driver.head(200, &[("content-type", "text/event-stream")]);
    driver.chunk(&stream);
    driver.fail(&kept);
    let record = harness.record();
    assert_eq!(record.get("failed").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 11);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 7);
}
