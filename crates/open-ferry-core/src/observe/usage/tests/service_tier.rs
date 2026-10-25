//! Final outbound service tiers, independent of the client and response tiers.

use super::super::record_json::Record;
use super::support::{ClientCall, Harness, auth, str_field};
use crate::exec::Format;
use crate::observe::redact::REDACTED;
use crate::observe::{AttemptKind, Outcome};

#[test]
fn effective_tier_reads_final_outbound_shapes() {
    for (body, want) in [
        (r#"{"service_tier":" priority "}"#, "priority"),
        (r#"{"request":{"service_tier":"priority"}}"#, "priority"),
        (r#"{"interaction":{"service_tier":"standard"}}"#, "standard"),
        (
            r#"{"service_tier":"auto","request":{"service_tier":"priority"}}"#,
            "auto",
        ),
        (
            r#"{"service_tier":" ","request":{"service_tier":7},"interaction":{"service_tier":"standard"}}"#,
            "standard",
        ),
        (
            r#"{"service_tier":true,"request":{"service_tier":"priority"}}"#,
            "priority",
        ),
        (r#"{"service_tier":7}"#, ""),
        (r#"{"service_tier":null}"#, ""),
        (r#"{"service_tier":{}}"#, ""),
        (r#"{"service_tier":[]}"#, ""),
        (r#"{"service_tier":" \t\n"}"#, ""),
        (r#"{"model":"model"}"#, ""),
        (r#"{"service_tier":"priority"} trailing"#, ""),
        (r#"{"service_tier":"priority","#, ""),
        ("", ""),
    ] {
        let harness = Harness::new();
        let driver = ClientCall::new("model")
            .body(r#"{"service_tier":"auto","reasoning_effort":"high"}"#)
            .tap(&harness);
        driver.attempt_with(
            AttemptKind::Execute,
            "openai",
            "model",
            &Format::OPENAI,
            &auth("id", "0", "openai"),
            &[],
            body,
        );
        driver.chunk(r#"{"service_tier":"default","usage":{"prompt_tokens":1}}"#);
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_eq!(
            record
                .get("effective_service_tier")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            want,
            "{body}"
        );
        assert_eq!(
            record.get("effective_service_tier").is_some(),
            !want.is_empty(),
            "{body}"
        );
        assert_eq!(str_field(&record, "service_tier"), "auto");
        assert_eq!(str_field(&record, "response_service_tier"), "default");
        assert_eq!(str_field(&record, "reasoning_effort"), "");
        assert_eq!(record.get("request_service_tier"), None);
    }
}

#[test]
fn effective_tier_retries_use_the_last_outbound_body() {
    for final_body in [r#"{"service_tier":"flex"}"#, "{}", ""] {
        let harness = Harness::new();
        let driver = ClientCall::new("model")
            .body(r#"{"service_tier":"auto"}"#)
            .tap(&harness);
        let credential = auth("id", "0", "openai");
        for body in [r#"{"service_tier":"priority"}"#, final_body] {
            driver.attempt_with(
                AttemptKind::Execute,
                "openai",
                "model",
                &Format::OPENAI,
                &credential,
                &[],
                body,
            );
        }
        driver.chunk(r#"{"usage":{"prompt_tokens":1}}"#);
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_eq!(str_field(&record, "service_tier"), "auto");
        assert_eq!(
            record
                .get("effective_service_tier")
                .and_then(|v| v.as_str()),
            final_body.contains("flex").then_some("flex")
        );
    }
}

#[test]
fn effective_tier_json_media_and_bodyless_retrieval() {
    for (body, want) in [
        (
            r#"{"service_tier":"priority","prompt":"a picture"}"#,
            Some("priority"),
        ),
        ("", None),
    ] {
        let harness = Harness::new();
        let driver = ClientCall::new("grok-imagine-image").tap(&harness);
        driver.attempt_with(
            AttemptKind::Execute,
            "xai",
            "grok-imagine-image",
            &Format::OPENAI_IMAGE,
            &auth("id", "0", "xai"),
            &[],
            body,
        );
        driver.chunk(r#"{"data":[{"url":"https://example.invalid/image"}]}"#);
        driver.finish(Outcome::Completed);
        assert_eq!(
            harness
                .record()
                .get("effective_service_tier")
                .and_then(|v| v.as_str()),
            want
        );
    }
}

#[test]
fn effective_tier_scrubs_attempt_secrets() {
    let harness = Harness::new();
    let driver = ClientCall::new("model").tap(&harness);
    driver.attempt_with(
        AttemptKind::Execute,
        "openai",
        "model",
        &Format::OPENAI,
        &auth("id", "0", "openai"),
        &["made-up-secret"],
        r#"{"service_tier":"priority-made-up-secret"}"#,
    );
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(
        str_field(&record, "effective_service_tier"),
        format!("priority-{REDACTED}")
    );
    assert!(!record.to_string().contains("made-up-secret"));
}

#[test]
fn effective_tier_queue_omits_blank_values() {
    for tier in ["", " \t\n"] {
        let record = Record {
            effective_service_tier: tier.to_owned(),
            ..Record::default()
        };
        let encoded: serde_json::Value = serde_json::from_str(&record.encode()).unwrap();
        assert!(encoded.get("effective_service_tier").is_none());
        assert_eq!(str_field(&encoded, "service_tier"), "");
        assert_eq!(str_field(&encoded, "reasoning_effort"), "");
    }
}
