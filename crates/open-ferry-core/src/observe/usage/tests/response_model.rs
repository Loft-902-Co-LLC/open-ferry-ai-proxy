// Ported from CLIProxyAPI
// internal/runtime/executor/helps/response_model_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the Codex served model, the substitution check and its
//! warning.
//!
//! Deviations from upstream: the reporter tests run the attempt through the
//! usage tap, as a Codex stream; the concurrency test hands one call's
//! chunks to its tap from concurrent threads and ends it once, where
//! upstream's also publishes from each of its goroutines (only the first
//! publishes).

use std::sync::Barrier;

use super::super::response_model::{
    MAX_RESPONSE_MODEL_LENGTH, extract_codex_response_model_event, is_model_substituted,
};
use super::support::{ClientCall, Harness, Warnings, auth, int_at, str_field};
use crate::auth::Auth;
use crate::observe::{AttemptKind, Outcome};

/// Ports TestExtractCodexResponseModel.
#[test]
fn extract_codex_response_model() {
    let long = "m".repeat(MAX_RESPONSE_MODEL_LENGTH + 1);
    let limit = "m".repeat(MAX_RESPONSE_MODEL_LENGTH);
    let oversized = format!(r#"{{"type":"response.created","response":{{"model":"{long}"}}}}"#);
    let at_limit = format!(r#"{{"type":"response.created","response":{{"model":"{limit}"}}}}"#);
    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "sse response created",
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6-luna"}}"#,
            "gpt-5.6-luna",
        ),
        (
            "sse response created without space after data prefix",
            r#"data:{"type":"response.created","response":{"model":"gpt-6-astra"}}"#,
            "gpt-6-astra",
        ),
        (
            "raw json response completed",
            r#"{"type":"response.completed","response":{"model":"gpt-5.6-luna","usage":{"total_tokens":12}}}"#,
            "gpt-5.6-luna",
        ),
        (
            "response in progress",
            r#"{"type":"response.in_progress","response":{"model":"gpt-5.6-terra"}}"#,
            "gpt-5.6-terra",
        ),
        (
            "response incomplete",
            r#"{"type":"response.incomplete","response":{"model":"gpt-5.6-sol"}}"#,
            "gpt-5.6-sol",
        ),
        (
            "response done",
            r#"{"type":"response.done","response":{"model":"gpt-5.3-codex-spark"}}"#,
            "gpt-5.3-codex-spark",
        ),
        (
            "model value is trimmed",
            r#"{"type":"response.created","response":{"model":"  gpt-6-astra  "}}"#,
            "gpt-6-astra",
        ),
        (
            "output text delta is ignored",
            r#"data: {"type":"response.output_text.delta","delta":"hi","response":{"model":"gpt-5.6-luna"}}"#,
            "",
        ),
        (
            "output item done is ignored",
            r#"{"type":"response.output_item.done","item":{"type":"message"}}"#,
            "",
        ),
        (
            "rate limits event is ignored",
            r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":12}}}"#,
            "",
        ),
        (
            "terminal failure event is ignored",
            r#"{"type":"error","error":{"code":"server_is_overloaded"}}"#,
            "",
        ),
        (
            "responses compact object",
            r#"{"id":"resp_1","object":"response.compaction","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#,
            "",
        ),
        (
            "images api response",
            r#"{"created":1745539200,"data":[{"b64_json":"aGk="}]}"#,
            "",
        ),
        (
            "image generation completed event",
            r#"data: {"type":"image_generation.completed","b64_json":"aGk="}"#,
            "",
        ),
        (
            "non string model value",
            r#"{"type":"response.created","response":{"model":123}}"#,
            "",
        ),
        (
            "object model value",
            r#"{"type":"response.created","response":{"model":{"id":"gpt-5.6-luna"}}}"#,
            "",
        ),
        ("oversized model value", &oversized, ""),
        ("model value at the length limit", &at_limit, &limit),
        ("done marker", "data: [DONE]", ""),
        ("sse event line", "event: response.created", ""),
        ("empty payload", "", ""),
        (
            "malformed json",
            r#"{"type":"response.created","response":{"model":"gpt-5.6-luna""#,
            "",
        ),
        (
            "response without model",
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            "",
        ),
    ];
    for (name, payload, want) in cases {
        let (got, _) = extract_codex_response_model_event(payload.as_bytes());
        assert_eq!(got, want, "{name}");
    }
}

/// Ports TestIsCodexModelSubstituted.
#[test]
fn is_codex_model_substituted() {
    let cases = [
        ("silent substitution", "gpt-6-astra", "gpt-5.6-luna", true),
        ("same model", "gpt-6-astra", "gpt-6-astra", false),
        (
            "thinking suffix stripped from requested",
            "gpt-6-astra(high)",
            "gpt-6-astra",
            false,
        ),
        (
            "thinking suffix stripped from served",
            "gpt-6-astra",
            "gpt-6-astra(high)",
            false,
        ),
        (
            "thinking suffix on both sides",
            "gpt-6-astra(high)",
            "gpt-6-astra(low)",
            false,
        ),
        (
            "thinking suffix with substitution",
            "gpt-6-astra(high)",
            "gpt-5.6-luna",
            true,
        ),
        (
            "numeric thinking suffix",
            "gpt-5.6-terra(16384)",
            "gpt-5.6-terra",
            false,
        ),
        (
            "case insensitive requested",
            "GPT-6-Astra",
            "gpt-6-astra",
            false,
        ),
        (
            "case insensitive served",
            "gpt-6-astra",
            "  GPT-6-ASTRA  ",
            false,
        ),
        (
            "served pins dashed date",
            "gpt-5.6-terra",
            "gpt-5.6-terra-2026-05-13",
            false,
        ),
        (
            "served pins compact date",
            "gpt-5.6-sol",
            "gpt-5.6-sol-20260513",
            false,
        ),
        (
            "requested pins dashed date",
            "gpt-5.6-terra-2026-05-13",
            "gpt-5.6-terra",
            false,
        ),
        (
            "requested pins compact date",
            "gpt-5.6-sol-20260513",
            "gpt-5.6-sol",
            false,
        ),
        (
            "different dates on both sides",
            "gpt-5.6-terra-2026-05-13",
            "gpt-5.6-terra-2026-06-01",
            true,
        ),
        (
            "incomplete date suffix",
            "gpt-5.6-sol",
            "gpt-5.6-sol-2026-5-13",
            true,
        ),
        ("non date suffix", "gpt-5.5", "gpt-5.5-codex", true),
        (
            "date suffix with trailing tag",
            "gpt-5.6-luna",
            "gpt-5.6-luna-2026-05-13-preview",
            true,
        ),
        (
            "spark unchanged",
            "gpt-5.3-codex-spark",
            "gpt-5.3-codex-spark",
            false,
        ),
        (
            "review model unchanged",
            "codex-auto-review",
            "codex-auto-review",
            false,
        ),
        (
            "review model substituted",
            "codex-auto-review",
            "gpt-5.6-luna",
            true,
        ),
        ("empty served", "gpt-6-astra", "", false),
        ("blank served", "gpt-6-astra", "   ", false),
        ("empty requested", "", "gpt-5.6-luna", false),
    ];
    for (name, requested, served, want) in cases {
        assert_eq!(is_model_substituted(requested, served), want, "{name}");
    }
}

const CODEX_SUBSTITUTED_STREAM: &str = r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6-luna"}}
data: {"type":"response.in_progress","response":{"id":"resp_1","model":"gpt-5.6-luna"}}
data: {"type":"response.output_text.delta","delta":"hello"}
data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.6-luna","usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}}
data: [DONE]"#;

const SUBSTITUTION_WARNING: &str = r#"codex executor: upstream served model "gpt-5.6-luna" for requested model "gpt-6-astra" (auth_index=auth-index-7)"#;

fn substitution_auth() -> Auth {
    Auth {
        file_name: "/auths/codex-user@example.com.json".to_owned(),
        ..auth("codex-auth-1", "auth-index-7", "codex")
    }
}

/// A Codex stream for `requested` with `auth` that sends `lines`.
fn codex_attempt(harness: &Harness, requested: &str, auth: &Auth, lines: &[&str]) {
    let driver = ClientCall::new(requested).stream().tap(harness);
    driver.attempt(AttemptKind::Stream, "codex", requested, auth);
    for line in lines {
        driver.chunk(&format!("{line}\n"));
    }
    driver.finish(Outcome::Completed);
}

/// Ports TestUsageReporterRecordsSubstitutedCodexResponseModelAndWarnsOnce.
#[test]
fn records_substituted_codex_response_model_and_warns_once() {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-6-astra").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "codex",
        "gpt-6-astra",
        &substitution_auth(),
    );
    for line in CODEX_SUBSTITUTED_STREAM.lines() {
        driver.chunk(&format!("{line}\n"));
    }
    assert!(
        warnings.substitutions().is_empty(),
        "warned before the attempt published"
    );
    driver.finish(Outcome::Completed);

    let record = harness.record();
    assert_eq!(str_field(&record, "response_model"), "gpt-5.6-luna");
    assert_eq!(str_field(&record, "model"), "gpt-6-astra");
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 12);
    let warned = warnings.substitutions();
    assert_eq!(warned, [SUBSTITUTION_WARNING]);
    assert!(!warned[0].contains("example.com") && !warned[0].contains("auth_file"));
}

/// Ports TestUsageReporterDoesNotWarnWhenCodexResponseModelMatches.
#[test]
fn does_not_warn_when_codex_response_model_matches() {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    codex_attempt(
        &harness,
        "gpt-6-astra(high)",
        &Auth::default(),
        &[r#"data: {"type":"response.created","response":{"model":"gpt-6-astra-2026-05-13"}}"#],
    );
    let record = harness.record();
    assert_eq!(
        str_field(&record, "response_model"),
        "gpt-6-astra-2026-05-13"
    );
    assert!(warnings.substitutions().is_empty());
}

/// Ports TestUsageReporterIgnoresPayloadsWithoutResponseModel.
#[test]
fn ignores_payloads_without_response_model() {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    codex_attempt(
        &harness,
        "gpt-6-astra",
        &Auth::default(),
        &[
            r#"data: {"type":"response.created","response":{"model":"gpt-5.6-luna"}}"#,
            r#"data: {"type":"response.output_text.delta","delta":"hello"}"#,
        ],
    );
    assert_eq!(
        str_field(&harness.record(), "response_model"),
        "gpt-5.6-luna"
    );
    let warned = warnings.substitutions();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].ends_with("(auth_index=nil)"), "{}", warned[0]);
}

/// Ports TestUsageReporterWarnsOnceUnderConcurrentObservationsAndPublishes.
#[test]
fn warns_once_under_concurrent_observations_and_publishes() {
    const WORKERS: usize = 32;
    let warnings = Warnings::capture();
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-6-astra").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "codex",
        "gpt-6-astra",
        &substitution_auth(),
    );
    let start = Barrier::new(WORKERS);
    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let (driver, start) = (&driver, &start);
            scope.spawn(move || {
                start.wait();
                driver.chunk(if worker % 2 == 0 {
                    r#"data: {"type":"response.created","response":{"model":"gpt-5.6-luna"}}
"#
                } else {
                    r#"data: {"type":"response.completed","response":{"model":"gpt-5.6-luna","usage":{"total_tokens":4}}}
"#
                });
            });
        }
    });
    driver.finish(Outcome::Completed);
    let records = harness.records();
    assert_eq!(records.len(), 1);
    assert_eq!(str_field(&records[0], "response_model"), "gpt-5.6-luna");
    assert_eq!(warnings.substitutions(), [SUBSTITUTION_WARNING]);
}

/// Ports TestUsageReporterEmitsWarningOnRepeatedSubstitutionsWithoutThrottle.
#[test]
fn warns_on_every_repeated_substitution() {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    let publish = |id: &str, index: &str| {
        codex_attempt(
            &harness,
            "gpt-6-astra",
            &auth(id, index, "codex"),
            &[r#"data: {"type":"response.completed","response":{"model":"gpt-5.6-luna"}}"#],
        );
    };

    publish("codex-auth-1", "auth-index-7");
    publish("codex-auth-1", "auth-index-7");
    assert_eq!(warnings.substitutions().len(), 2, "without a throttle");

    publish("codex-auth-2", "auth-index-8");
    assert_eq!(warnings.substitutions().len(), 3, "a second credential");
}

/// Ports TestUsageReporterEmitsWarningAcrossServedModelCaseWithoutThrottle.
#[test]
fn warns_across_served_model_case() {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    let credential = auth("codex-auth-1", "auth-index-7", "codex");
    for served in ["gpt-5.6-luna", "GPT-5.6-LUNA"] {
        let line =
            format!(r#"data: {{"type":"response.completed","response":{{"model":"{served}"}}}}"#);
        codex_attempt(&harness, "gpt-6-astra", &credential, &[&line]);
    }
    let warned = warnings.substitutions();
    assert_eq!(warned.len(), 2, "{warned:?}");
    assert_eq!(warned[0], SUBSTITUTION_WARNING);
}

/// Not upstream's: a call that names no provider is warned of as
/// `unknown`'s (v8.0.20's fallback, which was `codex`).
#[test]
fn warns_of_a_call_without_a_provider_as_unknown() {
    let warnings = Warnings::capture();
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-6-astra").stream().tap(&harness);
    driver.attempt_with(
        AttemptKind::Stream,
        "  ",
        "gpt-6-astra",
        &crate::exec::Format::OPENAI,
        &auth("compat-auth-1", "auth-index-7", ""),
        &[],
        "{}",
    );
    driver.chunk(
        r#"data: {"id":"c1","object":"chat.completion.chunk","model":"gpt-5.6-luna","choices":[]}
"#,
    );
    driver.finish(Outcome::Completed);
    let warned = warnings.substitutions();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].starts_with("unknown executor: upstream served model"),
        "{}",
        warned[0]
    );
}

/// Ports TestUsageReporterAdditionalModelRecordOmitsResponseModel: the
/// image generation tool's record has the tool's model and no served model.
#[test]
fn additional_model_record_omits_response_model() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4-mini").stream().tap(&harness);
    driver.attempt_with(
        AttemptKind::Stream,
        "codex",
        "gpt-5.4-mini",
        &crate::exec::Format::CODEX,
        &Auth::default(),
        &[],
        r#"{"tools":[{"type":"image_generation","model":"gpt-image-1.5"}]}"#,
    );
    driver.chunk(
        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.4-mini\",\"usage\":{\"total_tokens\":12},\"tool_usage\":{\"image_gen\":{\"total_tokens\":5}}}}\n",
    );
    driver.finish(Outcome::Completed);

    let records = harness.records();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(str_field(&records[0], "response_model"), "gpt-5.4-mini");
    assert_eq!(str_field(&records[1], "model"), "gpt-image-1.5");
    // The served model is the text model's, never the tool's, so nothing
    // reads a substitution into the tool's record.
    assert!(records[1].get("response_model").is_none(), "{}", records[1]);
}
