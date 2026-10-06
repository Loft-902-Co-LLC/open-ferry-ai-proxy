// Ported from CLIProxyAPI
// internal/runtime/executor/codex_executor_execute_usage_test.go and
// internal/runtime/executor/codex_response_model_test.go
// (TestCodexUsageRecordsCarryResponseModelPerModel) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the image generation tool's record, which a Codex call over
//! HTTP makes after its own when its terminal event has the tool's counts.
//!
//! Deviations from upstream: the tests run the attempt through the usage
//! tap and read the records it queues, where upstream runs the executor
//! against a test server and captures the records with a usage plugin.

use serde_json::Value;

use super::support::{ClientCall, Harness, auth, int_at, str_field};
use crate::exec::Format;
use crate::observe::{AttemptKind, Outcome};

/// The body a Codex call sends, with the tool the executor adds.
const SENT_WITH_TOOL: &str = r#"{"model":"gpt-5.5","input":"hi","tools":[{"type":"image_generation","output_format":"png"}]}"#;

const MAIN_USAGE: &str = r#","usage":{"input_tokens":100,"output_tokens":40,"total_tokens":140,"input_tokens_details":{"cached_tokens":25}}"#;
const ZERO_IMAGE_USAGE: &str =
    r#","tool_usage":{"image_gen":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}"#;
const IMAGE_USAGE: &str = r#","tool_usage":{"image_gen":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":3}}}"#;

/// A `response.completed` event with `fields` added to its response.
fn completed(fields: &str) -> String {
    format!(
        r#"data: {{"type":"response.completed","response":{{"id":"resp_usage","object":"response","status":"completed","model":"gpt-5.5","output":[{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"ok"}}]}}]{fields}}}}}"#
    )
}

/// The records of one attempt of `kind` to `provider` that sends `body` and
/// is answered with `lines`.
fn records(kind: AttemptKind, provider: &str, body: &str, lines: &[String]) -> Vec<Value> {
    let harness = Harness::new();
    let call = ClientCall::new("gpt-5.5");
    let call = if kind == AttemptKind::Execute {
        call
    } else {
        call.stream()
    };
    let driver = call.tap(&harness);
    driver.attempt_with(
        kind,
        provider,
        "gpt-5.5",
        &Format::CODEX,
        &auth("codex-auth-1", "auth-index-7", provider),
        &[],
        body,
    );
    for line in lines {
        driver.chunk(&format!("{line}\n\n"));
    }
    driver.finish(Outcome::Completed);
    harness.records()
}

/// `record`'s counts: input, output, total, cached and cache read.
#[track_caller]
fn counts(record: &Value) -> [i64; 5] {
    [
        "input_tokens",
        "output_tokens",
        "total_tokens",
        "cached_tokens",
        "cache_read_tokens",
    ]
    .map(|name| int_at(record, &format!("/tokens/{name}")))
}

/// Ports TestCodexExecutorExecutePublishesMainUsageBeforeImageUsage, for a
/// stream as well as a whole answer.
#[test]
fn publishes_main_usage_before_image_usage() {
    for kind in [AttemptKind::Execute, AttemptKind::Stream] {
        for main in [true, false] {
            for (image, fields) in [
                ("zero", ZERO_IMAGE_USAGE),
                ("nonzero", IMAGE_USAGE),
                ("missing", ""),
            ] {
                if !main && fields.is_empty() {
                    continue;
                }
                let case = format!("{kind:?}/main_{main}/image_{image}");
                let main_fields = if main { MAIN_USAGE } else { "" };
                let event = completed(&format!("{main_fields}{fields}"));
                let records = records(kind, "codex", SENT_WITH_TOOL, &[event]);
                let want = if image == "nonzero" { 2 } else { 1 };
                assert_eq!(records.len(), want, "{case}: {records:?}");

                let record = &records[0];
                assert_eq!(str_field(record, "model"), "gpt-5.5", "{case}");
                assert_eq!(record["failed"], false, "{case}");
                let want_main = if main { [100, 40, 140, 25, 25] } else { [0; 5] };
                assert_eq!(counts(record), want_main, "{case}");

                if let Some(record) = records.get(1) {
                    assert_eq!(str_field(record, "model"), "gpt-image-2", "{case}");
                    assert_eq!(record["failed"], false, "{case}");
                    assert_eq!(counts(record), [10, 20, 30, 3, 3], "{case}");
                    assert_ne!(
                        str_field(record, "execution_id"),
                        str_field(&records[0], "execution_id"),
                        "{case}: the tool's record is its own execution"
                    );
                    assert_eq!(
                        str_field(record, "request_id"),
                        str_field(&records[0], "request_id"),
                        "{case}: the same request"
                    );
                }
            }
        }
    }
}

/// Ports TestCodexUsageRecordsCarryResponseModelPerModel.
#[test]
fn records_carry_response_model_per_model() {
    let body = r#"{"model":"gpt-5.4-mini","tools":[{"type":"function","name":"f"},{"type":"image_generation","model":" gpt-image-1.5 "}]}"#;
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4-mini").stream().tap(&harness);
    driver.attempt_with(
        AttemptKind::Stream,
        "codex",
        "gpt-5.4-mini",
        &Format::CODEX,
        &auth("codex-auth-1", "auth-index-7", "codex"),
        &[],
        body,
    );
    driver.chunk(
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.4-mini\",\"usage\":{\"total_tokens\":12},\"tool_usage\":{\"image_gen\":{\"total_tokens\":5}}}}\n\n",
    );
    driver.finish(Outcome::Completed);

    let records = harness.records();
    assert_eq!(records.len(), 2, "{records:?}");
    let attempt = &records[0];
    assert_eq!(str_field(attempt, "model"), "gpt-5.4-mini");
    assert_eq!(str_field(attempt, "response_model"), "gpt-5.4-mini");
    assert_eq!(int_at(attempt, "/tokens/total_tokens"), 12);
    let image = &records[1];
    assert_eq!(str_field(image, "model"), "gpt-image-1.5");
    assert!(image.get("response_model").is_none(), "{image}");
    assert_eq!(int_at(image, "/tokens/total_tokens"), 5);
    assert_eq!(str_field(image, "auth_index"), "auth-index-7");
}

// Not upstream's: the tool's record keeps the served model when the tool's
// model is the one sent.
#[test]
fn a_tool_model_that_was_sent_keeps_the_served_model() {
    let body = r#"{"model":"gpt-image-2","tools":[{"type":"image_generation"}]}"#;
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-image-2").tap(&harness);
    driver.attempt_with(
        AttemptKind::Execute,
        "codex",
        "gpt-image-2",
        &Format::CODEX,
        &auth("a", "b", "codex"),
        &[],
        body,
    );
    driver.chunk(
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-image-2\",\"usage\":{\"total_tokens\":12},\"tool_usage\":{\"image_gen\":{\"total_tokens\":5}}}}\n",
    );
    driver.finish(Outcome::Completed);
    let records = harness.records();
    assert_eq!(records.len(), 2, "{records:?}");
    for record in &records {
        assert_eq!(str_field(record, "model"), "gpt-image-2");
        assert_eq!(str_field(record, "response_model"), "gpt-image-2");
    }
}

// Not upstream's: only Codex's own executor over HTTP reports the tool, as
// in upstream's `publishCodexImageToolUsage`: not a Codex WebSocket, nor
// xAI's or Meta's executors, which read the same events.
#[test]
fn only_codex_over_http_reports_the_tool() {
    let event = completed(&format!("{MAIN_USAGE}{IMAGE_USAGE}"));
    for (kind, provider) in [
        (AttemptKind::Websocket, "codex"),
        (AttemptKind::Execute, "xai"),
        (AttemptKind::Execute, "meta"),
        (AttemptKind::Stream, "meta"),
    ] {
        let lines = if kind == AttemptKind::Websocket {
            vec![event.trim_start_matches("data: ").to_owned()]
        } else {
            vec![event.clone()]
        };
        let records = records(kind, provider, SENT_WITH_TOOL, &lines);
        assert_eq!(records.len(), 1, "{kind:?} {provider}: {records:?}");
        assert_eq!(
            counts(&records[0])[2],
            140,
            "{kind:?} {provider}: the call's own record"
        );
    }
}

// Not upstream's: the tool's model is `gpt-image-2` when the request has
// no `image_generation` tool or the tool names an empty model.
#[test]
fn the_tool_model_defaults_to_gpt_image_2() {
    let event = completed(IMAGE_USAGE);
    for body in [
        r#"{"model":"gpt-5.5"}"#,
        r#"{"tools":[{"type":"image_generation","model":"  "},{"type":"image_generation","model":"gpt-image-1.5"}]}"#,
        "not json",
    ] {
        let records = records(
            AttemptKind::Execute,
            "codex",
            body,
            std::slice::from_ref(&event),
        );
        assert_eq!(records.len(), 2, "{body}: {records:?}");
        assert_eq!(str_field(&records[1], "model"), "gpt-image-2", "{body}");
    }
}

// Not upstream's: a call to Codex's Image API (upstream's
// `executeDirectOpenAIImage`) is read as an OpenAI answer, whole or as a
// stream, and makes one record, for the model it was made for.
#[test]
fn image_api_answers_are_read_as_openai_answers() {
    let usage = r#""usage":{"total_tokens":100,"input_tokens":50,"output_tokens":50}"#;
    let whole = format!(r#"{{"created":1,"data":[{{"b64_json":"AA=="}}],{usage}}}"#);
    let stream = format!(
        "event: image_generation.partial_image\ndata: {{\"type\":\"image_generation.partial_image\",\"b64_json\":\"AA==\"}}\n\nevent: image_generation.completed\ndata: {{\"type\":\"image_generation.completed\",\"b64_json\":\"BB==\",{usage}}}\n\n"
    );
    for (kind, answer) in [(AttemptKind::Execute, whole), (AttemptKind::Stream, stream)] {
        let harness = Harness::new();
        let call = ClientCall::new("codex/gpt-image-2");
        let call = if kind == AttemptKind::Execute {
            call
        } else {
            call.stream()
        };
        let driver = call.tap(&harness);
        driver.attempt_with(
            kind,
            "codex",
            "gpt-image-2",
            &Format::OPENAI_IMAGE,
            &auth("a", "b", "codex"),
            &[],
            r#"{"model":"gpt-image-2","prompt":"x"}"#,
        );
        driver.chunk(&answer);
        driver.finish(Outcome::Completed);
        let records = harness.records();
        assert_eq!(records.len(), 1, "{kind:?}: {records:?}");
        assert_eq!(str_field(&records[0], "model"), "gpt-image-2", "{kind:?}");
        assert_eq!(counts(&records[0])[..3], [50, 50, 100], "{kind:?}");
    }
}
