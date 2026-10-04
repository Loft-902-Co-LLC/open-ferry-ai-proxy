// Ported from CLIProxyAPI internal/redisqueue/plugin_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the usage records as the queue holds them.
//!
//! Upstream's plugin fills a record's request fields from the Go context
//! when it is handled; open-ferry's record carries them, so these tests
//! encode records directly, or run a call through the usage tap where
//! upstream reads the context.
//!
//! Dropped:
//! - TestUsageQueuePluginAcceptsDeprecatedRequestTierRecordField: records
//!   have no deprecated request tier.
//! - TestUsageQueuePluginAsyncUsesRecordResponseHeaders and
//!   TestUsageQueuePluginAsyncIgnoresRecycledGinContext: records are
//!   encoded when published, from what they own; there is no asynchronous
//!   manager or request context to change under them.
//! - TestUsageQueuePluginPayloadIncludesExplicitSessionHierarchy,
//!   TestUsageQueuePluginPayloadNormalizesPrefixedNativeUUID,
//!   TestUsageQueuePluginPayloadPreventsSelfReferentialLoop and
//!   TestUsageQueuePluginPayloadSameOriginFallback: open-ferry never
//!   derives a session or its hierarchy (policy); see
//!   `session_id_is_only_a_client_header` instead.
//!
//! Deviations from upstream: a record's `failed` is set by the call's own
//! outcome, not by the status sent to the client.

use chrono::{TimeZone, Utc};
use serde_json::Value;

use super::super::accounting::{Detail, Quality, TOKEN_ACCOUNTING_SCHEMA_VERSION};
use super::super::record_json::Record;
use super::support::{ClientCall, Harness, auth, bool_at, int_at, str_field};
use crate::exec::{ExecError, Format};
use crate::observe::{AttemptKind, Outcome};

/// `record` as the queue holds it, parsed.
fn encoded(record: &Record) -> Value {
    serde_json::from_str(&record.encode()).expect("JSON")
}

/// Checks `payload`'s token breakdown has `quality` and `total`.
#[track_caller]
fn assert_breakdown(payload: &Value, quality: Quality, total: i64) {
    assert_eq!(
        str_field(&payload["token_breakdown"], "quality"),
        quality.as_str(),
        "{payload}"
    );
    assert_eq!(int_at(payload, "/token_breakdown/total_tokens"), total);
}

/// A record of OpenAI's `gpt-5.4` with `detail`.
fn openai_record(detail: Detail) -> Record {
    Record {
        provider: "openai".to_owned(),
        model: "gpt-5.4".to_owned(),
        generate: true,
        detail,
        ..Record::default()
    }
}

/// One input token.
fn one_token() -> Detail {
    Detail {
        input_tokens: 1,
        total_tokens: 1,
        ..Detail::default()
    }
}

/// Ports TestUsageQueuePluginPayloadIncludesStableFieldsAndSuccess.
#[test]
fn payload_includes_stable_fields_and_success() {
    let record = Record {
        request_id: "ctx-request-id".to_owned(),
        provider: "openai".to_owned(),
        executor_type: "KimiExecutor".to_owned(),
        model: "gpt-5.4".to_owned(),
        alias: "client-gpt".to_owned(),
        api_key: "test-key".to_owned(),
        auth_index: "0".to_owned(),
        access_token_sha256: "token-version-hash".to_owned(),
        auth_type: "apikey".to_owned(),
        source: "user@example.com".to_owned(),
        reasoning_effort: "medium".to_owned(),
        service_tier: "auto".to_owned(),
        response_model: "gpt-5.6-luna".to_owned(),
        generate: true,
        requested_at: Utc.with_ymd_and_hms(2026, 4, 25, 0, 0, 0).single(),
        latency: std::time::Duration::from_millis(1500),
        detail: Detail {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
            response_service_tier: "default".to_owned(),
            ..Detail::default()
        },
        response_headers: vec![
            ("Retry-After".to_owned(), vec!["30".to_owned()]),
            (
                "X-Upstream-Request-Id".to_owned(),
                vec!["upstream-req-1".to_owned()],
            ),
        ],
        endpoint: "POST /v1/chat/completions".to_owned(),
        client_ip: "192.0.2.10".to_owned(),
        resolved_client_ip: "203.0.113.5".to_owned(),
        forwarded_for: "203.0.113.5, 198.51.100.8".to_owned(),
        user_agent: "test-client/1.0".to_owned(),
        ..Record::default()
    };
    let payload = encoded(&record);
    for (key, want) in [
        ("provider", "openai"),
        ("executor_type", "KimiExecutor"),
        ("model", "gpt-5.4"),
        ("alias", "client-gpt"),
        ("endpoint", "POST /v1/chat/completions"),
        ("auth_type", "apikey"),
        ("access_token_sha256", "token-version-hash"),
        ("request_id", "ctx-request-id"),
        ("client_ip", "192.0.2.10"),
        ("resolved_client_ip", "203.0.113.5"),
        ("x_forwarded_for", "203.0.113.5, 198.51.100.8"),
        ("user_agent", "test-client/1.0"),
        ("reasoning_effort", "medium"),
        ("service_tier", "auto"),
        ("response_service_tier", "default"),
        ("response_model", "gpt-5.6-luna"),
        ("timestamp", "2026-04-25T00:00:00Z"),
    ] {
        assert_eq!(str_field(&payload, key), want, "{key}");
    }
    assert_eq!(payload.get("user_api_key"), None);
    assert_eq!(payload.get("request_service_tier"), None);
    assert_eq!(int_at(&payload, "/latency_ms"), 1500);
    assert_eq!(
        int_at(&payload, "/accounting_version"),
        TOKEN_ACCOUNTING_SCHEMA_VERSION
    );
    assert_breakdown(&payload, Quality::Complete, 30);
    assert!(bool_at(&payload, "/tokens/cache_read_tokens_present"));
    assert_eq!(
        payload["response_headers"]["X-Upstream-Request-Id"],
        serde_json::json!(["upstream-req-1"])
    );
    assert_eq!(
        payload["response_headers"]["Retry-After"],
        serde_json::json!(["30"])
    );
    assert!(!bool_at(&payload, "/failed"));
    assert!(bool_at(&payload, "/generate"));
    assert!(!bool_at(&payload, "/stream"));
    assert_eq!(int_at(&payload, "/fail/status_code"), 200);
    assert_eq!(str_field(&payload["fail"], "body"), "");
}

/// Ports TestUsageQueuePluginNormalizesDirectSDKUsageByProvider.
#[test]
fn normalizes_direct_sdk_usage_by_provider() {
    for (provider, total) in [("openai", 130), ("gemini", 142)] {
        let record = Record {
            provider: provider.to_owned(),
            model: "direct-sdk-model".to_owned(),
            detail: Detail {
                input_tokens: 100,
                output_tokens: 30,
                reasoning_tokens: 12,
                ..Detail::default()
            },
            ..Record::default()
        };
        let payload = encoded(&record);
        assert_eq!(
            int_at(&payload, "/tokens/total_tokens"),
            total,
            "{provider}"
        );
        assert_breakdown(&payload, Quality::Complete, total);
    }
}

/// Ports TestUsageQueuePluginPayloadIncludesGenerateFalse.
#[test]
fn payload_includes_generate_false() {
    let record = Record {
        generate: false,
        ..openai_record(one_token())
    };
    assert!(!bool_at(&encoded(&record), "/generate"));
}

/// Ports TestUsageQueuePluginPayloadDefaultsGenerateTrueWhenOmitted: a
/// call whose client sent no flag is recorded with generation on.
#[test]
fn payload_defaults_generate_true_when_omitted() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4")
        .body(r#"{"model":"gpt-5.4"}"#)
        .tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    driver.chunk(r#"{"usage":{"input_tokens":1,"total_tokens":1}}"#);
    driver.finish(Outcome::Completed);
    assert!(bool_at(&harness.record(), "/generate"));
}

/// Ports TestUsageQueuePluginPublishesStreamFlag.
#[test]
fn publishes_stream_flag() {
    for stream in [true, false] {
        let record = Record {
            stream,
            ..openai_record(one_token())
        };
        assert_eq!(bool_at(&encoded(&record), "/stream"), stream);
    }
}

/// Ports TestUsageQueuePluginPreservesLegacyCachedOnlyUsage.
#[test]
fn preserves_legacy_cached_only_usage() {
    let payload = encoded(&openai_record(Detail {
        cached_tokens: 13,
        ..Detail::default()
    }));
    assert!(bool_at(&payload, "/tokens/cache_read_tokens_present"));
    assert_eq!(int_at(&payload, "/tokens/cache_read_tokens"), 13);
    assert_eq!(int_at(&payload, "/tokens/total_tokens"), 13);
    assert_breakdown(&payload, Quality::Unclassified, 13);
}

/// Ports TestUsageQueuePluginEmitsSingleCanonicalAutoTier: a call whose
/// client named no tier is recorded with `auto`, once.
#[test]
fn emits_single_canonical_auto_tier() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4").tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    driver.chunk(r#"{"usage":{"input_tokens":1,"total_tokens":1}}"#);
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(str_field(&record, "service_tier"), "auto");
    assert_eq!(record.get("request_service_tier"), None);
}

/// Ports TestUsageQueuePluginPayloadIncludesStableFieldsAndFailureAndGinRequestID.
#[test]
fn payload_includes_stable_fields_and_failure_and_request_id() {
    let record = Record {
        request_id: "gin-request-id".to_owned(),
        provider: "openai".to_owned(),
        model: "gpt-5.4-mini".to_owned(),
        alias: "client-mini".to_owned(),
        api_key: "test-key".to_owned(),
        auth_index: "0".to_owned(),
        auth_type: "apikey".to_owned(),
        source: "user@example.com".to_owned(),
        requested_at: Utc.with_ymd_and_hms(2026, 4, 25, 0, 0, 0).single(),
        latency: std::time::Duration::from_millis(2500),
        failed: true,
        fail_status: 500,
        fail_body: "upstream failed".to_owned(),
        detail: Detail {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
            ..Detail::default()
        },
        endpoint: "GET /v1/responses".to_owned(),
        ..Record::default()
    };
    let payload = encoded(&record);
    assert_eq!(str_field(&payload, "provider"), "openai");
    assert_eq!(str_field(&payload, "model"), "gpt-5.4-mini");
    assert_eq!(str_field(&payload, "alias"), "client-mini");
    assert_eq!(str_field(&payload, "endpoint"), "GET /v1/responses");
    assert_eq!(str_field(&payload, "auth_type"), "apikey");
    assert_eq!(payload.get("user_api_key"), None);
    assert_eq!(str_field(&payload, "request_id"), "gin-request-id");
    assert!(bool_at(&payload, "/failed"));
    assert_eq!(int_at(&payload, "/fail/status_code"), 500);
    assert_eq!(str_field(&payload["fail"], "body"), "upstream failed");
}

/// Ports TestUsageQueuePlugin_SchemeB_ExecutionIDAndTraceID.
#[test]
fn scheme_b_execution_id_and_trace_id() {
    let execution = "12345678-1234-4234-8234-123456789abc";
    let record = Record {
        execution_id: execution.to_owned(),
        request_id: "000000ab".to_owned(),
        trace_id: "000000ab".to_owned(),
        ..openai_record(Detail {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Detail::default()
        })
    };
    let payload = encoded(&record);
    assert_eq!(str_field(&payload, "request_id"), "000000ab");
    assert_eq!(str_field(&payload, "execution_id"), execution);
    assert_eq!(str_field(&payload, "trace_id"), "000000ab");
}

/// Ports TestUsageQueuePlugin_SchemeB_StrictLegacyRequestIDPreservation.
#[test]
fn scheme_b_strict_legacy_request_id_preservation() {
    let execution = "12345678-1234-4234-8234-123456789abc";
    let record = Record {
        execution_id: execution.to_owned(),
        request_id: "legacy-log-id".to_owned(),
        trace_id: "custom-trace-id".to_owned(),
        ..openai_record(Detail {
            input_tokens: 5,
            ..Detail::default()
        })
    };
    let payload = encoded(&record);
    assert_eq!(str_field(&payload, "request_id"), "legacy-log-id");
    assert_eq!(str_field(&payload, "execution_id"), execution);
    assert_eq!(str_field(&payload, "trace_id"), "custom-trace-id");
}

/// The record of a call to OpenAI made by `call`.
fn record_of(call: ClientCall) -> Value {
    let harness = Harness::new();
    let driver = call.tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    driver.chunk(r#"{"usage":{"total_tokens":3}}"#);
    driver.finish(Outcome::Completed);
    harness.record()
}

/// Not upstream's: a record's `session_id` is only ever a session header
/// the client sent, the first that may be taken, a UUID in lower case;
/// without one there is none.
#[test]
fn session_id_is_only_a_client_header() {
    let record = record_of(
        ClientCall::new("gpt-5.4").header("Session-Id", "6F9619FF-8B86-D011-B42D-00C04FC964FF"),
    );
    assert_eq!(
        str_field(&record, "session_id"),
        "6f9619ff-8b86-d011-b42d-00c04fc964ff"
    );

    let record = record_of(
        ClientCall::new("gpt-5.4")
            .header("X-Session-Id", "last")
            .header("Session-Id", "middle")
            .header("X-Claude-Code-Session-Id", "first"),
    );
    assert_eq!(str_field(&record, "session_id"), "first");

    let too_long = "s".repeat(257);
    let record = record_of(
        ClientCall::new("gpt-5.4")
            .header("X-Claude-Code-Session-Id", "with\ttab")
            .header("Session-Id", &too_long)
            .header("X-Session-Id", "  kept  "),
    );
    assert_eq!(str_field(&record, "session_id"), "kept");

    let record = record_of(ClientCall::new("gpt-5.4").body(
        r#"{"metadata":{"user_id":"user_abc_account__session_6f9619ff-8b86-d011-b42d-00c04fc964ff"},"prompt_cache_key":"key-1"}"#,
    ));
    assert_eq!(record.get("session_id"), None, "{record}");
    assert_eq!(record.get("parent_session_id"), None);
    assert_eq!(record.get("node_kind"), None);
    assert_eq!(record.get("is_fork"), None);
    assert_eq!(record.get("is_compaction"), None);
}

/// Not upstream's: a record keeps the client's key and the credential's
/// account in clear, as upstream's does, but its failure body is scrubbed
/// of them and of the attempt's secrets, and the answer's credential
/// headers are masked.
#[test]
fn record_keeps_keys_in_clear_but_scrubs_failures_and_headers() {
    let harness = Harness::new();
    let call = ClientCall::new("gpt-5.4");
    call.context.set_client_key("sk-client-0123456789");
    let driver = call.tap(&harness);
    let mut credential = auth("openai-1", "0", "openai");
    credential
        .attributes
        .insert("api_key".to_owned(), "sk-upstream-0123456789".to_owned());
    driver.attempt_with(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &Format::OPENAI,
        &credential,
        &["sk-upstream-0123456789"],
        "{}",
    );
    driver.head(
        401,
        &[
            ("Set-Cookie", "session=cookie-0123456789"),
            ("X-Request-Id", "upstream-1"),
        ],
    );
    driver.fail(&ExecError::upstream(
        401,
        "bad keys sk-upstream-0123456789 and sk-client-0123456789",
    ));
    let record = harness.record();
    assert_eq!(str_field(&record, "api_key"), "sk-client-0123456789");
    assert_eq!(str_field(&record, "source"), "sk-upstream-0123456789");
    let body = str_field(&record["fail"], "body");
    assert!(!body.contains("sk-upstream-0123456789"), "{body}");
    assert!(!body.contains("sk-client-0123456789"), "{body}");
    assert!(body.starts_with("bad keys "), "{body}");
    let cookie = record
        .pointer("/response_headers/Set-Cookie/0")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(!cookie.contains("cookie-0123456789"), "{cookie}");
    assert_eq!(
        record.pointer("/response_headers/X-Request-Id/0"),
        Some(&serde_json::json!("upstream-1"))
    );
}

/// Not upstream's: a failure body is scrubbed of the credential's own
/// tokens, for a call that failed before it sent anything and for one
/// whose attempt sent only some of them.
#[test]
fn failure_bodies_are_scrubbed_of_the_credentials_tokens() {
    let mut credential = auth("claude-1", "0", "claude");
    for (key, token) in [
        ("access_token", "access-secret-123456789"),
        ("refresh_token", "refresh-secret-123456789"),
    ] {
        credential
            .metadata
            .insert(key.to_owned(), Value::String(token.to_owned()));
    }
    let message = "invalid access-secret-123456789 and refresh-secret-123456789";

    let harness = Harness::new();
    let call = ClientCall::new("claude-sonnet-4-6");
    call.context
        .select(crate::observe::SelectedAuth::new(std::sync::Arc::new(
            credential.clone(),
        )));
    let driver = call.tap(&harness);
    driver.fail(&ExecError::upstream(401, message));
    let record = harness.record();
    assert_eq!(
        str_field(&record["fail"], "body"),
        "invalid [redacted] and [redacted]"
    );

    let harness = Harness::new();
    let driver = ClientCall::new("claude-sonnet-4-6").tap(&harness);
    driver.attempt_with(
        AttemptKind::Execute,
        "claude",
        "claude-sonnet-4-6",
        &Format::CLAUDE,
        &credential,
        &["access-secret-123456789"],
        "{}",
    );
    driver.head(401, &[]);
    driver.fail(&ExecError::upstream(401, message));
    let record = harness.record();
    assert_eq!(
        str_field(&record["fail"], "body"),
        "invalid [redacted] and [redacted]"
    );
}

/// Not upstream's: a queued failure's body is scrubbed as a file is, so a
/// secret shorter than eight bytes goes too, the client's key as well.
#[test]
fn failure_bodies_are_scrubbed_of_short_secrets() {
    let harness = Harness::new();
    let call = ClientCall::new("gpt-5.4");
    call.context.set_client_key("ck-12");
    let driver = call.tap(&harness);
    let mut credential = auth("openai-1", "0", "openai");
    credential
        .attributes
        .insert("api_key".to_owned(), "k-123".to_owned());
    driver.attempt_with(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &Format::OPENAI,
        &credential,
        &["k-123"],
        "{}",
    );
    driver.head(401, &[]);
    driver.fail(&ExecError::upstream(401, "bad k-123 and ck-12"));
    let record = harness.record();
    assert_eq!(
        str_field(&record["fail"], "body"),
        "bad [redacted] and [redacted]"
    );
}
