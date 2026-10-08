// Ported from CLIProxyAPI sdk/cliproxy/auth/openai_compat_pool_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI-compatible model pools: the upstream models configured under one
//! alias take turns per credential, a failure falls back to the pool's next
//! model (or stops on a bad request), a model the provider doesn't support
//! is suspended for later calls, and a forced alias is written back into
//! the response.
//!
//! Deviations from upstream:
//! - `TestManagerExecuteStream_OpenAICompatAliasPoolPreservesEarlierUpstreamError`
//!   is adapted. Upstream's executor marks only the first model's failure as
//!   an upstream attempt, so its 502 is reported over the second model's
//!   local error. The port counts every executor error as an upstream
//!   attempt (a listed deviation of `execute.rs`), so the last error, the
//!   second model's, is reported. The test checks that the call still comes
//!   back as a stream holding only that error. The upstream-attempt marker
//!   upstream checks for doesn't exist in the port.
//! - The pool credentials keep upstream's IDs, which end in the Go test's
//!   name.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};

use super::support::*;
use crate::auth::{Auth, AuthError, Status};
use crate::exec::{Dispatcher, ErrorKind, ExecError, StreamResponse};
use crate::manager::credential::is_zero;
use crate::manager::models;
use crate::manager::{CallResult, ModelAlias, OpenAiCompat, Settings};

const POOL_PROVIDER_KEY: &str = "openai-compatible-pool";
const ALIAS: &str = "claude-opus-4.66";

/// What the pool executor answers per upstream model (upstream's
/// `openAICompatPoolExecutor` maps). A model without an entry answers with
/// its own name.
#[derive(Default)]
struct PoolBehaviour {
    execute_payloads: HashMap<&'static str, &'static str>,
    execute_errors: HashMap<&'static str, ExecError>,
    count_errors: HashMap<&'static str, ExecError>,
    stream_first_errors: HashMap<&'static str, ExecError>,
    /// Custom stream chunks; an empty list is a stream with no chunks.
    stream_payloads: HashMap<&'static str, Vec<Result<Bytes, ExecError>>>,
}

fn pool_executor(behaviour: PoolBehaviour) -> Arc<FakeExecutor> {
    FakeExecutor::with(POOL_PROVIDER_KEY, move |call| {
        let model = call.model.as_str();
        match call.kind {
            Kind::Execute => {
                if let Some(err) = behaviour.execute_errors.get(model) {
                    return Reply::Err(err.clone());
                }
                match behaviour.execute_payloads.get(model) {
                    Some(payload) => Reply::ok(*payload),
                    None => Reply::ok(model.to_owned()),
                }
            }
            Kind::Count => match behaviour.count_errors.get(model) {
                Some(err) => Reply::Err(err.clone()),
                None => Reply::ok(model.to_owned()),
            },
            Kind::Stream => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "X-Model",
                    HeaderValue::from_str(model).expect("model is a header value"),
                );
                let chunks = if let Some(err) = behaviour.stream_first_errors.get(model) {
                    vec![Err(err.clone())]
                } else if let Some(chunks) = behaviour.stream_payloads.get(model) {
                    chunks.clone()
                } else {
                    vec![Ok(Bytes::from(model.to_owned()))]
                };
                Reply::Stream { headers, chunks }
            }
        }
    })
}

fn model(name: &str, alias: &str, force_mapping: bool) -> ModelAlias {
    ModelAlias {
        name: name.to_owned(),
        alias: alias.to_owned(),
        force_mapping,
    }
}

/// The two-model pool most tests use, under [`ALIAS`].
fn two_model_pool() -> Vec<ModelAlias> {
    vec![
        model("deepseek-v3.1", ALIAS, false),
        model("glm-5", ALIAS, false),
    ]
}

fn pool_settings(models: Vec<ModelAlias>) -> Settings {
    Settings {
        openai_compatibility: vec![OpenAiCompat {
            name: "pool".into(),
            models,
            ..OpenAiCompat::default()
        }],
        ..Settings::default()
    }
}

/// An active credential of the `pool` provider with `api_key`.
fn pool_auth(id: &str, api_key: &str) -> Auth {
    let mut credential = auth(id, POOL_PROVIDER_KEY);
    credential.status = Status::Active;
    for (key, value) in [
        ("api_key", api_key),
        ("compat_name", "pool"),
        ("provider_key", POOL_PROVIDER_KEY),
    ] {
        credential.attributes.insert(key.into(), value.into());
    }
    credential
}

/// A manager with one pool credential serving `alias` (upstream's
/// `newOpenAICompatPoolTestManager`); `test_name` is the Go test's name.
fn new_pool_test_manager(
    test_name: &str,
    alias: &str,
    models: Vec<ModelAlias>,
    executor: &Arc<FakeExecutor>,
) -> Harness {
    let h = Harness::new(pool_settings(models));
    h.executor(executor);
    h.add(
        pool_auth(&format!("pool-auth-{test_name}"), "test-key"),
        &[alias],
    );
    h
}

/// The stream's chunks joined, failing on a stream error (upstream's
/// `readOpenAICompatStreamPayload`).
async fn read_stream_payload(stream: StreamResponse) -> String {
    let (chunks, err) = collect(stream).await;
    assert!(err.is_none(), "unexpected stream error: {err:?}");
    chunks.concat()
}

/// Upstream compares these errors by identity; the port's errors are values.
fn assert_same_error(got: &ExecError, want: &ExecError) {
    assert_eq!(got.kind, want.kind, "error = {got}, want {want}");
    assert_eq!(got.status, want.status, "error = {got}, want {want}");
    assert_eq!(got.to_string(), want.to_string());
}

fn header(stream: &StreamResponse, name: &str) -> String {
    stream
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(start_paused = true)]
async fn manager_execute_count_openai_compat_alias_pool_stops_on_invalid_request() {
    let invalid_err = ExecError::upstream(422, "unprocessable entity");
    let executor = pool_executor(PoolBehaviour {
        count_errors: HashMap::from([("deepseek-v3.1", invalid_err.clone())]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteCount_OpenAICompatAliasPoolStopsOnInvalidRequest",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let err = h
        .manager
        .count_tokens(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .expect_err("execute count error = nil, want unprocessable entity");
    assert_eq!(err.to_string(), invalid_err.to_string());
    assert_eq!(
        executor.models(Kind::Count),
        ["deepseek-v3.1"],
        "count calls, want only first invalid model"
    );
}

#[test]
fn resolve_model_alias_pool_from_config_models() {
    let models = vec![
        model("deepseek-v3.1", ALIAS, false),
        model("glm-5", ALIAS, false),
        model("kimi-k2.5", ALIAS, false),
    ];
    let got =
        models::resolve_model_alias_pool_from_config_models("claude-opus-4.66(8192)", &models);
    assert_eq!(
        got,
        ["deepseek-v3.1(8192)", "glm-5(8192)", "kimi-k2.5(8192)"]
    );
}

#[test]
fn resolve_model_alias_pool_prefers_exact_suffixed_alias() {
    let models = vec![
        model("base-model", "public", false),
        model("low-model", "public(low)", true),
    ];
    let got = models::resolve_model_alias_pool_from_config_models("public(low)", &models);
    assert_eq!(got, ["low-model(low)"], "exact suffixed pool");
    let result = models::resolve_model_alias_result_from_config_models("public(low)", &models);
    assert!(
        result.upstream_model == "low-model(low)" && result.force_mapping,
        "exact suffixed alias result = {result:?}, want low-model(low) with force mapping"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_rotates_within_auth() {
    let executor = pool_executor(PoolBehaviour::default());
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolRotatesWithinAuth",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    for i in 0..3 {
        let resp = h
            .manager
            .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
            .await
            .unwrap_or_else(|err| panic!("execute {i}: {err}"));
        assert!(
            !resp.payload.is_empty(),
            "execute {i} returned empty payload"
        );
    }
    assert_eq!(
        executor.models(Kind::Execute),
        ["deepseek-v3.1", "glm-5", "deepseek-v3.1"]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_force_mapping_rotates_and_rewrites_response() {
    let executor = pool_executor(PoolBehaviour {
        execute_payloads: HashMap::from([
            ("deepseek-v3.1", r#"{"model":"deepseek-v3.1"}"#),
            ("glm-5", r#"{"model":"glm-5"}"#),
        ]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolForceMappingRotatesAndRewritesResponse",
        ALIAS,
        vec![
            model("deepseek-v3.1", ALIAS, true),
            model("glm-5", ALIAS, true),
        ],
        &executor,
    );

    let mut payloads = Vec::new();
    for i in 0..2 {
        let resp = h
            .manager
            .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
            .await
            .unwrap_or_else(|err| panic!("execute {i}: {err}"));
        payloads.push(String::from_utf8_lossy(&resp.payload).into_owned());
    }

    let got = executor.models(Kind::Execute);
    assert_eq!(got[..2], ["deepseek-v3.1", "glm-5"]);
    assert_eq!(
        payloads,
        [
            r#"{"model":"claude-opus-4.66"}"#,
            r#"{"model":"claude-opus-4.66"}"#
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_stops_on_bad_request() {
    let invalid_err = ExecError::upstream(400, "invalid_request_error: malformed payload");
    let executor = pool_executor(PoolBehaviour {
        execute_errors: HashMap::from([("deepseek-v3.1", invalid_err.clone())]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolStopsOnBadRequest",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let err = h
        .manager
        .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .expect_err("execute error = nil, want malformed payload");
    assert_eq!(err.to_string(), invalid_err.to_string());
    assert_eq!(
        executor.models(Kind::Execute),
        ["deepseek-v3.1"],
        "execute calls, want only first invalid model"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_falls_back_on_model_support_bad_request() {
    const TEST_NAME: &str =
        "TestManagerExecute_OpenAICompatAliasPoolFallsBackOnModelSupportBadRequest";
    let model_support_err = ExecError::upstream(
        400,
        "invalid_request_error: The requested model is not supported.",
    );
    let executor = pool_executor(PoolBehaviour {
        execute_errors: HashMap::from([("deepseek-v3.1", model_support_err)]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(TEST_NAME, ALIAS, two_model_pool(), &executor);

    let resp = h
        .manager
        .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("execute error = {err}, want fallback success"));
    assert_eq!(String::from_utf8_lossy(&resp.payload), "glm-5");
    assert_eq!(executor.models(Kind::Execute), ["deepseek-v3.1", "glm-5"]);

    let updated = h
        .manager
        .get(&format!("pool-auth-{TEST_NAME}"))
        .expect("expected auth to remain registered");
    let state = updated
        .model_states
        .get("deepseek-v3.1")
        .expect("expected suspended upstream model state");
    assert!(
        state.unavailable && !is_zero(state.next_retry_after),
        "expected upstream model suspension, got {state:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_falls_back_on_model_support_unprocessable_entity()
{
    let model_support_err = ExecError::upstream(422, "The requested model is not supported.");
    let executor = pool_executor(PoolBehaviour {
        execute_errors: HashMap::from([("deepseek-v3.1", model_support_err)]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolFallsBackOnModelSupportUnprocessableEntity",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let resp = h
        .manager
        .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("execute error = {err}, want fallback success"));
    assert_eq!(String::from_utf8_lossy(&resp.payload), "glm-5");
    assert_eq!(executor.models(Kind::Execute), ["deepseek-v3.1", "glm-5"]);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_falls_back_within_same_auth() {
    let executor = pool_executor(PoolBehaviour {
        execute_errors: HashMap::from([("deepseek-v3.1", ExecError::upstream(429, "quota"))]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolFallsBackWithinSameAuth",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let resp = h
        .manager
        .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("execute: {err}"));
    assert_eq!(String::from_utf8_lossy(&resp.payload), "glm-5");
    let got = executor.models(Kind::Execute);
    assert_eq!(got[..2], ["deepseek-v3.1", "glm-5"]);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_uses_selected_model_force_mapping() {
    let alias = "public-model";
    let executor = pool_executor(PoolBehaviour {
        execute_errors: HashMap::from([("first-upstream", ExecError::upstream(429, "quota"))]),
        execute_payloads: HashMap::from([("second-upstream", r#"{"model":"second-upstream"}"#)]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolUsesSelectedModelForceMapping",
        alias,
        vec![
            model("first-upstream", alias, true),
            model("second-upstream", alias, false),
        ],
        &executor,
    );

    let response = h
        .manager
        .execute(&providers(&[POOL_PROVIDER_KEY]), request(alias), options())
        .await
        .unwrap_or_else(|err| panic!("Execute() error = {err}"));
    assert_eq!(
        String::from_utf8_lossy(&response.payload),
        r#"{"model":"second-upstream"}"#,
        "payload, want selected model without force mapping"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_openai_compat_alias_pool_retries_on_empty_bootstrap() {
    let executor = pool_executor(PoolBehaviour {
        stream_payloads: HashMap::from([("deepseek-v3.1", Vec::new())]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteStream_OpenAICompatAliasPoolRetriesOnEmptyBootstrap",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let stream = h
        .manager
        .execute_stream(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("execute stream: {err}"));
    assert_eq!(read_stream_payload(stream).await, "glm-5");
    let got = executor.models(Kind::Stream);
    assert_eq!(got[..2], ["deepseek-v3.1", "glm-5"]);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_openai_compat_alias_pool_falls_back_before_first_byte() {
    let executor = pool_executor(PoolBehaviour {
        stream_first_errors: HashMap::from([("deepseek-v3.1", ExecError::upstream(429, "quota"))]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteStream_OpenAICompatAliasPoolFallsBackBeforeFirstByte",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let stream = h
        .manager
        .execute_stream(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("execute stream: {err}"));
    let x_model = header(&stream, "X-Model");
    assert_eq!(read_stream_payload(stream).await, "glm-5");
    let got = executor.models(Kind::Stream);
    assert_eq!(got[..2], ["deepseek-v3.1", "glm-5"]);
    assert_eq!(x_model, "glm-5", "header X-Model");
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_openai_compat_alias_pool_stops_on_invalid_request() {
    let invalid_err = ExecError::upstream(422, "unprocessable entity");
    let executor = pool_executor(PoolBehaviour {
        stream_first_errors: HashMap::from([("deepseek-v3.1", invalid_err.clone())]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteStream_OpenAICompatAliasPoolStopsOnInvalidRequest",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let Err(err) = h
        .manager
        .execute_stream(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
    else {
        panic!("execute stream error = nil, want {invalid_err}");
    };
    assert_eq!(err.to_string(), invalid_err.to_string());
    assert_eq!(
        executor.models(Kind::Stream),
        ["deepseek-v3.1"],
        "stream calls, want only first invalid model"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_skips_suspended_upstream_on_later_requests() {
    let model_support_err = ExecError::upstream(
        400,
        "invalid_request_error: The requested model is not supported.",
    );
    let executor = pool_executor(PoolBehaviour {
        execute_errors: HashMap::from([("deepseek-v3.1", model_support_err)]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecute_OpenAICompatAliasPoolSkipsSuspendedUpstreamOnLaterRequests",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    for i in 0..3 {
        let resp = h
            .manager
            .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
            .await
            .unwrap_or_else(|err| panic!("execute {i}: {err}"));
        assert_eq!(
            String::from_utf8_lossy(&resp.payload),
            "glm-5",
            "execute {i} payload"
        );
    }
    assert_eq!(
        executor.models(Kind::Execute),
        ["deepseek-v3.1", "glm-5", "glm-5", "glm-5"]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_openai_compat_alias_pool_skips_suspended_upstream_on_later_requests()
 {
    let model_support_err = ExecError::upstream(422, "The requested model is not supported.");
    let executor = pool_executor(PoolBehaviour {
        stream_first_errors: HashMap::from([("deepseek-v3.1", model_support_err)]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteStream_OpenAICompatAliasPoolSkipsSuspendedUpstreamOnLaterRequests",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    for i in 0..3 {
        let stream = h
            .manager
            .execute_stream(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
            .await
            .unwrap_or_else(|err| panic!("execute stream {i}: {err}"));
        let x_model = header(&stream, "X-Model");
        assert_eq!(
            read_stream_payload(stream).await,
            "glm-5",
            "execute stream {i} payload"
        );
        assert_eq!(x_model, "glm-5", "execute stream {i} header X-Model");
    }
    assert_eq!(
        executor.models(Kind::Stream),
        ["deepseek-v3.1", "glm-5", "glm-5", "glm-5"]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_count_openai_compat_alias_pool_rotates_within_auth() {
    let executor = pool_executor(PoolBehaviour::default());
    let h = new_pool_test_manager(
        "TestManagerExecuteCount_OpenAICompatAliasPoolRotatesWithinAuth",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    for i in 0..2 {
        let resp = h
            .manager
            .count_tokens(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
            .await
            .unwrap_or_else(|err| panic!("execute count {i}: {err}"));
        assert!(
            !resp.payload.is_empty(),
            "execute count {i} returned empty payload"
        );
    }
    let got = executor.models(Kind::Count);
    assert_eq!(got[..2], ["deepseek-v3.1", "glm-5"]);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_count_openai_compat_alias_pool_skips_suspended_upstream_on_later_requests()
{
    let model_support_err = ExecError::upstream(
        400,
        "invalid_request_error: The requested model is unsupported.",
    );
    let executor = pool_executor(PoolBehaviour {
        count_errors: HashMap::from([("deepseek-v3.1", model_support_err)]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteCount_OpenAICompatAliasPoolSkipsSuspendedUpstreamOnLaterRequests",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    for i in 0..3 {
        let resp = h
            .manager
            .count_tokens(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
            .await
            .unwrap_or_else(|err| panic!("execute count {i}: {err}"));
        assert_eq!(
            String::from_utf8_lossy(&resp.payload),
            "glm-5",
            "execute count {i} payload"
        );
    }
    assert_eq!(
        executor.models(Kind::Count),
        ["deepseek-v3.1", "glm-5", "glm-5", "glm-5"]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_openai_compat_alias_pool_blocked_auth_does_not_consume_retry_budget() {
    // Upstream's SetRetryConfig(0, 0, 1).
    let h = Harness::new(Settings {
        request_retry: 0,
        max_retry_interval: Duration::ZERO,
        max_retry_credentials: 1,
        ..pool_settings(two_model_pool())
    });

    // Upstream's authScopedOpenAICompatPoolExecutor.
    let executor = FakeExecutor::with(POOL_PROVIDER_KEY, |call| match call.kind {
        Kind::Execute => Reply::ok(format!("{}|{}", call.auth_id, call.model)),
        Kind::Stream => Reply::status(501, "ExecuteStream not implemented"),
        Kind::Count => Reply::status(501, "CountTokens not implemented"),
    });
    h.executor(&executor);

    h.add(pool_auth("aa-blocked-auth", "bad-key"), &[ALIAS]);
    h.add(pool_auth("bb-good-auth", "good-key"), &[ALIAS]);

    for upstream_model in ["deepseek-v3.1", "glm-5"] {
        h.manager.mark_result(&CallResult {
            auth_id: "aa-blocked-auth".into(),
            provider: POOL_PROVIDER_KEY.into(),
            model: upstream_model.into(),
            success: false,
            error: Some(AuthError {
                http_status: 400,
                message: "invalid_request_error: The requested model is not supported.".into(),
                ..AuthError::default()
            }),
            ..CallResult::default()
        });
    }

    let resp = h
        .manager
        .execute(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("execute error = {err}, want success via fallback auth"));
    let payload = String::from_utf8_lossy(&resp.payload);
    assert!(
        payload.starts_with("bb-good-auth|"),
        "payload = {payload:?}, want auth \"bb-good-auth\""
    );

    let got: Vec<String> = executor
        .calls()
        .iter()
        .filter(|call| call.kind == Kind::Execute)
        .map(|call| format!("{}|{}", call.auth_id, call.model))
        .collect();
    assert_eq!(
        got.len(),
        1,
        "execute calls = {got:?}, want only one real execution on fallback auth"
    );
    assert!(
        got[0].starts_with("bb-good-auth|"),
        "execute call = {:?}, want fallback auth \"bb-good-auth\"",
        got[0]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_openai_compat_alias_pool_stops_on_invalid_bootstrap() {
    let invalid_err = ExecError::upstream(400, "invalid_request_error: malformed payload");
    let executor = pool_executor(PoolBehaviour {
        stream_first_errors: HashMap::from([("deepseek-v3.1", invalid_err.clone())]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteStream_OpenAICompatAliasPoolStopsOnInvalidBootstrap",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    // An error and no stream (upstream's nil stream result).
    let Err(err) = h
        .manager
        .execute_stream(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
    else {
        panic!("expected invalid request error, got a stream");
    };
    assert_same_error(&err, &invalid_err);
    assert_eq!(
        executor.models(Kind::Stream),
        ["deepseek-v3.1"],
        "stream calls, want only first upstream model"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_openai_compat_alias_pool_preserves_earlier_upstream_error() {
    let upstream_err = ExecError::upstream(502, "first model upstream failed");
    // Upstream's errors.New: no status.
    let internal_err = ExecError::new(ErrorKind::Upstream, "second model failed before upstream");
    let executor = pool_executor(PoolBehaviour {
        stream_first_errors: HashMap::from([
            ("deepseek-v3.1", upstream_err),
            ("glm-5", internal_err.clone()),
        ]),
        ..PoolBehaviour::default()
    });
    let h = new_pool_test_manager(
        "TestManagerExecuteStream_OpenAICompatAliasPoolPreservesEarlierUpstreamError",
        ALIAS,
        two_model_pool(),
        &executor,
    );

    let stream = h
        .manager
        .execute_stream(&providers(&[POOL_PROVIDER_KEY]), request(ALIAS), options())
        .await
        .unwrap_or_else(|err| panic!("ExecuteStream() = {err}, want an error stream"));
    let (chunks, err) = collect(stream).await;
    assert!(chunks.is_empty(), "chunks = {chunks:?}, want only an error");
    let err = err.expect("stream error = none, want an error stream");
    // Upstream reports the earlier 502 here; the port reports the last
    // executor error (see the module docs).
    assert_same_error(&err, &internal_err);
    assert_eq!(executor.models(Kind::Stream), ["deepseek-v3.1", "glm-5"]);
}
