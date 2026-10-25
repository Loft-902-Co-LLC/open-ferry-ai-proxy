//! Final outbound service tiers, independent of the client and response tiers.

use super::super::record_json::Record;
use super::super::{json, request_tier};
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
        (
            r#"{"request":{"service_tier":"priority"},"interaction":{"service_tier":"standard"}}"#,
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

fn outbound_tier(body: &str) -> Option<String> {
    let harness = Harness::new();
    let driver = ClientCall::new("model").tap(&harness);
    driver.attempt_with(
        AttemptKind::Execute,
        "openai",
        "model",
        &Format::OPENAI,
        &auth("id", "0", "openai"),
        &[],
        body,
    );
    driver.finish(Outcome::Completed);
    harness.record()["effective_service_tier"]
        .as_str()
        .map(str::to_owned)
}

#[test]
fn effective_tier_without_key_is_empty() {
    assert_eq!(outbound_tier(r#"{"model":"model","messages":[]}"#), None);
    let body = format!(r#"{{"input":"{}"}}"#, "x".repeat(2 << 20));
    assert_eq!(outbound_tier(&body), None);
}

#[test]
fn effective_tier_large_unrelated_field_is_empty() {
    let body = format!(
        r#"{{"other":{{"service_tier":"unrelated"}},"input":"{}"}}"#,
        "x".repeat(2 << 20)
    );
    assert_eq!(outbound_tier(&body), None);
}

#[test]
fn effective_tier_requires_literal_key_bytes_before_reading() {
    let escaped = r#"{"\u0073ervice_tier":"priority"}"#;
    assert_eq!(outbound_tier(escaped), None);
    assert_eq!(tree_tier(escaped.as_bytes()), "priority");
    let literal_elsewhere = r#"{"\u0073ervice_tier":"priority","other":"service_tier"}"#;
    assert_eq!(
        outbound_tier(literal_elsewhere).as_deref(),
        Some("priority")
    );
}

/// The old tree reader is the oracle for duplicates and validation limits.
fn tree_tier(body: &[u8]) -> String {
    if !json::valid(body) {
        return String::new();
    }
    let doc = json::Doc::parse(body);
    [
        "service_tier",
        "request.service_tier",
        "interaction.service_tier",
    ]
    .into_iter()
    .map(|path| doc.get(path))
    .filter(|node| node.is_string())
    .map(|node| node.string().trim().to_owned())
    .find(|tier| !tier.is_empty())
    .unwrap_or_default()
}

#[test]
fn effective_tier_keeps_last_duplicate_keys() {
    for (body, want) in [
        (r#"{"service_tier":"first","service_tier":"last"}"#, "last"),
        (
            r#"{"service_tier":"first","service_tier":false,"request":{"service_tier":"nested"}}"#,
            "nested",
        ),
        (
            r#"{"request":{"service_tier":"first"},"request":{"service_tier":"last"}}"#,
            "last",
        ),
        (
            r#"{"request":{"service_tier":"first"},"request":null,"interaction":{"service_tier":"last"}}"#,
            "last",
        ),
        (
            r#"{"request":{"service_tier":"first"},"request":{"other":0},"interaction":{"service_tier":"last"}}"#,
            "last",
        ),
        (
            r#"{"interaction":{"service_tier":"first","service_tier":"last"}}"#,
            "last",
        ),
        (
            r#"{"service_tier":1e400,"request":{"service_tier":"nested"},"other":[null,true,-1,2.5,18446744073709551616]}"#,
            "nested",
        ),
    ] {
        assert_eq!(outbound_tier(body).as_deref(), Some(want));
        assert_eq!(
            request_tier::read(body.as_bytes()),
            tree_tier(body.as_bytes())
        );
    }
}

#[test]
fn effective_tier_keeps_the_tree_readers_depth_limit() {
    for depth in [125, 126, 127, 128, 129] {
        for (open, close) in [("[", "]"), (r#"{"child":"#, "}")] {
            let body = format!(
                r#"{{"service_tier":"priority","other":{}0{}}}"#,
                open.repeat(depth),
                close.repeat(depth)
            );
            let want = tree_tier(body.as_bytes());
            let tier = request_tier::read(body.as_bytes());
            if depth == 126 {
                assert_eq!(tier, "priority", "root plus 126 containers");
            } else if depth == 127 {
                assert_eq!(tier, "", "root plus 127 containers");
            }
            assert_eq!(tier, want, "depth {depth}");
            assert_eq!(
                outbound_tier(&body).unwrap_or_default(),
                want,
                "depth {depth}"
            );
        }
    }
}

#[test]
fn effective_tier_keeps_whole_json_and_string_validation() {
    for body in [
        br#"{"service_tier":"priority","other":"\uD800"}"#.as_slice(),
        br#"{"service_tier":"priority","other":{"\uDC00":0}}"#,
        b"{\"service_tier\":\"priority\",\"other\":\"\xff\"}",
        br#"{"service_tier":"priority","other":[1,]}"#,
        br#"{"service_tier":"priority"} []"#,
    ] {
        assert_eq!(tree_tier(body), "");
        assert_eq!(request_tier::read(body), "");
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
