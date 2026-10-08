// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_request_scoped_errors_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Request-scoped error rules through the manager: a matching rule stops the
//! call or moves to the next credential, with or without a cooldown, on
//! every call kind and stream phase; rules come from the credential's
//! metadata, its API key or its OpenAI-compatible provider; a cooldown rule
//! wins over `disable_cooling` and a disabled transient cooldown; an
//! unmatched error keeps the default handling.
//!
//! Deviations from upstream:
//! - Upstream puts typed `[]RequestScopedErrorRule` values in the metadata;
//!   the port's metadata is JSON, so the rules are written as the JSON those
//!   values serialize to (`status`, `match`, `match-regexr`, `action`).
//! - "The error is the original `customStatusError`" is checked as the
//!   caller getting the executor's `ExecError` unchanged: kind, status and
//!   body.
//! - `TestRequestScopedErrors_ResponseBodyProvider_MatchesUnderlyingPayload`:
//!   an `ExecError` has one text, the provider's body, so there is no
//!   wrapper text separate from the body; the error carries the body and
//!   the rule must match it.
//! - `TestUnwrapExecutionBoundaryErrorRemovesInternalMarkers` is dropped
//!   (Go-only): the "attempted" and "stop" marks are fields of the manager's
//!   internal failure, not wrappers around the error, so there is nothing to
//!   unwrap.
//! - `TestExtractRequestScopedErrorRulesSupportsLegacyMetadataKey` is
//!   already ported as `metadata_rules_win_and_decode_like_go` in
//!   `scoped.rs`.

use std::time::Duration;

use bytes::Bytes;
use serde_json::{Value, json};

use super::conductor_stream_overload_failover::event_stream_headers;
use super::support::*;
use crate::auth::{Auth, Status};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::{ApiKeyEntry, OpenAiCompat, RequestScopedErrorRule, Settings};

const MODEL: &str = "claude-3";

/// A rule as upstream's `RequestScopedErrorRule` serializes into metadata.
fn rule(status: u16, matches: &[&str], action: &str) -> Value {
    json!({"status": status, "match": matches, "action": action})
}

/// An active Claude credential with priority 10, so it is picked first,
/// and `rules` (a JSON array) as its `request_scoped_errors` metadata.
fn primary(id: &str, rules: Value) -> Auth {
    let mut credential = auth(id, "claude");
    credential.status = Status::Active;
    credential.attributes.insert("priority".into(), "10".into());
    credential
        .metadata
        .insert("request_scoped_errors".into(), rules);
    credential
}

/// An active Claude credential with no rules and the default priority.
fn secondary(id: &str) -> Auth {
    let mut credential = auth(id, "claude");
    credential.status = Status::Active;
    credential
}

fn config_rule(status: u16, matches: &[&str], action: &str) -> RequestScopedErrorRule {
    RequestScopedErrorRule {
        status,
        matches: matches.iter().map(|m| (*m).to_owned()).collect(),
        match_regex: Vec::new(),
        action: action.into(),
    }
}

fn assert_cooled(h: &Harness, id: &str, what: &str) {
    let got = h.get(id);
    assert!(
        got.unavailable && got.next_retry_after.is_some(),
        "{what}: got unavailable={}, nextRetry={:?}",
        got.unavailable,
        got.next_retry_after
    );
}

fn assert_not_cooled(h: &Harness, id: &str, what: &str) {
    let got = h.get(id);
    assert!(
        !got.unavailable && got.next_retry_after.is_none(),
        "{what}: got unavailable={}, nextRetry={:?}",
        got.unavailable,
        got.next_retry_after
    );
}

/// The caller got the executor's error unchanged.
fn assert_original(err: &ExecError, status: u16, body: &str) {
    assert!(
        err.kind == ErrorKind::Upstream && err.status == status && err.message == body,
        "error = {err:?}, want the executor's original {status} error"
    );
}

async fn execute(h: &Harness, provider: &str, model: &str) -> Result<Bytes, ExecError> {
    h.manager
        .execute(&providers(&[provider]), request(model), options())
        .await
        .map(|response| response.payload)
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_action_stop() {
    const BODY: &str = r#"{"error": {"message": "maximum_context_length exceeded"}}"#;
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-claude-1",
            json!([{
                "status": 400,
                "match": ["maximum_context_length", "context_length_exceeded"],
                "match-regexr": ["maximum_context_length$", "^context_length_exceeded"],
                "action": "stop",
            }]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-claude-2"), &[MODEL]);
    let executor = FakeExecutor::with("claude", |_| Reply::status(400, BODY));
    h.executor(&executor);

    let err = execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got a response");
    assert_original(&err, 400, BODY);
    // A stop returns at once on the first credential, without trying the
    // second.
    assert_eq!(
        executor.calls().len(),
        1,
        "execCount, want 1 (should stop immediately)"
    );
    assert_not_cooled(&h, "auth-claude-1", "expected auth1 not to be in cooldown");
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_action_stop_and_cooldown() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-claude-stop-cool",
            json!([rule(400, &["context_window_exceeded"], "stop-and-cooldown")]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-claude-second"), &[MODEL]);
    let executor = FakeExecutor::with("claude", |_| {
        Reply::status(400, r#"{"error": {"message": "context_window_exceeded"}}"#)
    });
    h.executor(&executor);

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    assert_eq!(
        executor.calls().len(),
        1,
        "execCount, want 1 (should stop immediately)"
    );
    assert_cooled(
        &h,
        "auth-claude-stop-cool",
        "expected auth1 to be in cooldown",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_action_continue() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-claude-continue-1",
            json!([rule(400, &["try_another_key"], "continue")]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-claude-continue-2"), &[MODEL]);
    let executor = FakeExecutor::with("claude", |call| {
        if call.auth_id == "auth-claude-continue-1" {
            Reply::status(400, r#"{"error": {"message": "try_another_key"}}"#)
        } else {
            Reply::ok(r#"{"result":"success"}"#)
        }
    });
    h.executor(&executor);

    let payload = execute(&h, "claude", MODEL)
        .await
        .unwrap_or_else(|err| panic!("unexpected error: {err}"));
    assert_eq!(
        &payload[..],
        br#"{"result":"success"}"#,
        "unexpected response"
    );
    assert_eq!(executor.calls().len(), 2, "execCount, want 2");
    assert_not_cooled(
        &h,
        "auth-claude-continue-1",
        "expected auth1 not to be in cooldown",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_action_continue_and_cooldown() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-claude-continue-cool-1",
            json!([rule(
                400,
                &["balance_insufficient"],
                "continue-and-cooldown"
            )]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-claude-continue-cool-2"), &[MODEL]);
    let executor = FakeExecutor::with("claude", |call| {
        if call.auth_id == "auth-claude-continue-cool-1" {
            Reply::status(400, r#"{"error": {"message": "balance_insufficient"}}"#)
        } else {
            Reply::ok(r#"{"result":"success"}"#)
        }
    });
    h.executor(&executor);

    let payload = execute(&h, "claude", MODEL)
        .await
        .unwrap_or_else(|err| panic!("unexpected error: {err}"));
    assert_eq!(
        &payload[..],
        br#"{"result":"success"}"#,
        "unexpected response"
    );
    assert_eq!(executor.calls().len(), 2, "execCount, want 2");
    assert_cooled(
        &h,
        "auth-claude-continue-cool-1",
        "expected auth1 to be in cooldown",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_match_regexr() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-regex-1",
            json!([{
                "status": 400,
                "match-regexr": [r"context_length_exceeded:\s*\d+"],
                "action": "stop",
            }]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-regex-2"), &[MODEL]);
    let executor = FakeExecutor::with("claude", |_| {
        Reply::status(
            400,
            r#"{"error": {"message": "context_length_exceeded: 128000"}}"#,
        )
    });
    h.executor(&executor);

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    assert_eq!(executor.calls().len(), 1, "execCount, want 1");
    assert!(
        !h.get("auth-regex-1").unavailable,
        "expected auth1 not to be in cooldown"
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_stream_action_stop() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-stream-1",
            json!([rule(400, &["stream_context_overflow"], "stop")]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-stream-2"), &[MODEL]);
    // The stream fails before it starts.
    let executor = FakeExecutor::with("claude", |_| Reply::status(400, "stream_context_overflow"));
    h.executor(&executor);

    let result = h
        .manager
        .execute_stream(&providers(&["claude"]), request(MODEL), options())
        .await;
    let err = match result {
        Ok(_) => panic!("expected error, got nil"),
        Err(err) => err,
    };
    assert_original(&err, 400, "stream_context_overflow");
    assert_eq!(
        executor.calls().len(),
        1,
        "execCount, want 1 (should stop immediately)"
    );
    assert_not_cooled(&h, "auth-stream-1", "expected auth1 not to be in cooldown");
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_stream_bootstrap_stop_and_cooldown() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-stream-boot-1",
            json!([rule(400, &["bootstrap_chunk_error"], "stop-and-cooldown")]),
        ),
        &[MODEL],
    );
    h.add(secondary("auth-stream-boot-2"), &[MODEL]);
    let executor = FakeExecutor::with("claude", |_| Reply::Stream {
        headers: event_stream_headers(),
        chunks: vec![Err(ExecError::upstream(400, "bootstrap_chunk_error"))],
    });
    h.executor(&executor);

    let result = h
        .manager
        .execute_stream(&providers(&["claude"]), request(MODEL), options())
        .await;
    assert!(result.is_err(), "expected error, got nil");
    assert_eq!(executor.calls().len(), 1, "execCount, want 1");
    assert_cooled(
        &h,
        "auth-stream-boot-1",
        "expected auth1 to be in cooldown from bootstrap chunk error",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_stop_stops_outer_retry_on429() {
    let h = Harness::new(Settings {
        request_retry: 3,
        max_retry_interval: Duration::from_secs(5),
        max_retry_credentials: 5,
        ..Settings::default()
    });
    h.add(
        primary(
            "auth-retry-stop-1",
            json!([rule(429, &["rate_limit_stop"], "stop")]),
        ),
        &[MODEL],
    );
    let executor = FakeExecutor::with("claude", |_| {
        let mut err = ExecError::upstream(429, r#"{"error": {"message": "rate_limit_stop"}}"#);
        err.retry_after = Some(Duration::from_millis(100));
        Reply::Err(err)
    });
    h.executor(&executor);

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    // A 429 with a retry-after would normally get three more rounds; a stop
    // ends the outer retries at once.
    assert_eq!(
        executor.calls().len(),
        1,
        "execCount, want 1 (should stop outer retries immediately)"
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_cooldown_overrides_disable_cooling() {
    let h = Harness::new(Settings::default());
    let mut credential = primary(
        "auth-disable-cooling-override",
        json!([rule(400, &["cooldown_anyway"], "stop-and-cooldown")]),
    );
    credential
        .metadata
        .insert("disable_cooling".into(), json!(true));
    h.add(credential, &[MODEL]);
    h.executor(&FakeExecutor::with("claude", |_| {
        Reply::status(400, r#"{"error": {"message": "cooldown_anyway"}}"#)
    }));

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    assert_cooled(
        &h,
        "auth-disable-cooling-override",
        "expected auth1 to be in cooldown despite disable_cooling=true",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_count_tokens_stop_and_cooldown() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-count-1",
            json!([rule(404, &["count_endpoint_cooldown"], "stop-and-cooldown")]),
        ),
        &[MODEL],
    );
    h.executor(&FakeExecutor::with("claude", |_| {
        Reply::status(404, "count_endpoint_cooldown")
    }));

    let err = h
        .manager
        .count_tokens(&providers(&["claude"]), request(MODEL), options())
        .await
        .expect_err("expected error, got nil");
    assert_original(&err, 404, "count_endpoint_cooldown");
    assert_cooled(
        &h,
        "auth-count-1",
        "expected auth1 to be in cooldown from CountTokens",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_resolved_from_manager_config() {
    let mut settings = Settings::default();
    settings.api_keys.insert(
        "claude".into(),
        vec![ApiKeyEntry {
            api_key: "sk-ant-test".into(),
            request_scoped_errors: vec![config_rule(400, &["from_config_rule"], "stop")],
            ..ApiKeyEntry::default()
        }],
    );
    settings.openai_compatibility.push(OpenAiCompat {
        name: "my-compat".into(),
        request_scoped_errors: vec![config_rule(400, &["from_compat_config_rule"], "stop")],
        ..OpenAiCompat::default()
    });
    let h = Harness::new(settings);

    let mut auth1 = auth("auth-config-resolve-1", "claude");
    auth1.status = Status::Active;
    auth1.attributes.insert("config_index".into(), "0".into());
    auth1.attributes.insert("priority".into(), "10".into());
    h.add(auth1, &[MODEL]);
    let mut auth_compat = auth("auth-config-resolve-compat", "openai-compatible-my-compat");
    auth_compat.status = Status::Active;
    for (key, value) in [
        ("config_index", "0"),
        ("compat_name", "my-compat"),
        ("priority", "10"),
    ] {
        auth_compat.attributes.insert(key.into(), value.into());
    }
    h.add(auth_compat, &["compat-model"]);

    h.executor(&FakeExecutor::with("claude", |_| {
        Reply::status(400, "from_config_rule occurred")
    }));
    h.executor(&FakeExecutor::with("openai-compatible-my-compat", |_| {
        Reply::status(400, "from_compat_config_rule occurred")
    }));

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    assert_not_cooled(
        &h,
        "auth-config-resolve-1",
        "expected auth1 not to be in cooldown when resolved from manager config",
    );

    execute(&h, "openai-compatible-my-compat", "compat-model")
        .await
        .expect_err("expected error, got nil");
    assert_not_cooled(
        &h,
        "auth-config-resolve-compat",
        "expected aCompat not to be in cooldown when resolved from manager config",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_non_matching_falls_back_to_default() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-nomatch-1",
            json!([rule(500, &["some_500_error"], "stop")]),
        ),
        &[MODEL],
    );
    // A 400 request fault the rule doesn't match.
    h.executor(&FakeExecutor::with("claude", |_| {
        Reply::status(
            400,
            r#"{"error": {"message": "Invalid request parameter", "type": "invalid_request_error"}}"#,
        )
    }));

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    // The default request-fault handling applies: no cooldown.
    assert_not_cooled(
        &h,
        "auth-nomatch-1",
        "expected auth1 not to be in cooldown under default fallback",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_stream_subsequent_chunk_error() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-stream-subsequent-1",
            json!([rule(
                400,
                &["mid_stream_context_length"],
                "stop-and-cooldown"
            )]),
        ),
        &[MODEL],
    );
    h.executor(&FakeExecutor::with("claude", |_| Reply::Stream {
        headers: event_stream_headers(),
        chunks: vec![
            Ok(Bytes::from_static(br#"data: {"type":"message_start"}\n\n"#)),
            Err(ExecError::upstream(400, "mid_stream_context_length")),
        ],
    }));

    let result = h
        .manager
        .execute_stream(&providers(&["claude"]), request(MODEL), options())
        .await;
    let stream = match result {
        Ok(stream) => stream,
        Err(err) => panic!("unexpected stream start error: {err}"),
    };
    collect(stream).await;
    settle().await;

    // The stream wrapper applied the stop-and-cooldown action.
    assert_cooled(
        &h,
        "auth-stream-subsequent-1",
        "expected auth1 to be in cooldown after mid-stream chunk error",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_unmatched_bootstrap_error_preserves_default() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-unmatched-boot-1",
            json!([rule(500, &["rule_does_not_match"], "stop")]),
        ),
        &[MODEL],
    );
    h.executor(&FakeExecutor::with("claude", |_| Reply::Stream {
        headers: event_stream_headers(),
        chunks: vec![Err(ExecError::upstream(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"Unmatched bad request"}}"#,
        ))],
    }));

    let result = h
        .manager
        .execute_stream(&providers(&["claude"]), request(MODEL), options())
        .await;
    assert!(result.is_err(), "expected error, got nil");
    // The default invalid-request handling skips the cooldown.
    assert_not_cooled(
        &h,
        "auth-unmatched-boot-1",
        "expected auth1 not to be cooled down under default fallback for 400 bootstrap error",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_transient_cooldown_disabled_force_cooldown_still_applies() {
    let h = Harness::new(Settings {
        transient_error_cooldown_seconds: -1,
        ..Settings::default()
    });
    h.add(
        primary(
            "auth-transient-disabled-1",
            json!([rule(500, &["cooldown_on_500"], "stop-and-cooldown")]),
        ),
        &[MODEL],
    );
    h.executor(&FakeExecutor::with("claude", |_| {
        Reply::status(500, "cooldown_on_500")
    }));

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    assert_cooled(
        &h,
        "auth-transient-disabled-1",
        "expected auth1 to be in cooldown despite transientErrorCooldownSeconds=-1",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_open_ai_compat_bare_provider_key_fallback() {
    let mut settings = Settings::default();
    settings.openai_compatibility.push(OpenAiCompat {
        name: "bare-compat".into(),
        request_scoped_errors: vec![config_rule(400, &["from_bare_compat_rule"], "stop")],
        ..OpenAiCompat::default()
    });
    let h = Harness::new(settings);
    let mut auth_compat = auth("auth-bare-compat", "openai-compatible-bare-compat");
    auth_compat.status = Status::Active;
    h.add(auth_compat, &["bare-model"]);
    h.executor(&FakeExecutor::with("openai-compatible-bare-compat", |_| {
        Reply::status(400, "from_bare_compat_rule")
    }));

    execute(&h, "openai-compatible-bare-compat", "bare-model")
        .await
        .expect_err("expected error, got nil");
    assert_not_cooled(
        &h,
        "auth-bare-compat",
        "expected aCompat not to be in cooldown from bare provider fallback",
    );
}

#[tokio::test(start_paused = true)]
async fn request_scoped_errors_response_body_provider_matches_underlying_payload() {
    let h = Harness::new(Settings::default());
    h.add(
        primary(
            "auth-fast-wrapped-1",
            json!([rule(400, &["claude_fast_overload"], "stop-and-cooldown")]),
        ),
        &[MODEL],
    );
    h.executor(&FakeExecutor::with("claude", |_| {
        Reply::status(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"claude_fast_overload"}}"#,
        )
    }));

    execute(&h, "claude", MODEL)
        .await
        .expect_err("expected error, got nil");
    assert_cooled(
        &h,
        "auth-fast-wrapped-1",
        "expected auth1 to be in cooldown when matching ResponseBody()",
    );
}
