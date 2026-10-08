// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_overrides_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Per-credential overrides and the error handling around them: request
//! retry rounds and their limits, `disable_cooling`, the cooldown each kind
//! of failure leaves on a credential or model, credential fallback, and the
//! count-tokens route 404 that must not suspend a model.
//!
//! Deviations from upstream:
//! - `TestManager_ShouldRetryAfterError_SkipsWrappedHomeConcurrencyBusy` is
//!   dropped: the Home dispatcher isn't ported.
//! - The retry decision is asked of `retry::should_retry_after_error` with a
//!   selection over the manager's state (upstream's `shouldRetryAfterError`):
//!   `None` is upstream's `(0, false)`. The pinned credential is passed
//!   straight to the query rather than through the options' metadata.
//! - `IgnoresRequestIneligibleOverrides` keeps the pinned-credential case
//!   only; a credential policy only narrows Codex Alpha Search's pick, which
//!   doesn't retry.
//! - Result hooks aren't ported. Where upstream captures the hook's result,
//!   the tests check the credential and model state that result left
//!   instead.
//! - `DeepSeekInsufficientBalanceRotatesCredentialAndRebindsSession` turns
//!   session affinity on through the settings, and names its session with
//!   an `X-Session-Id` header where upstream sets a derived session in the
//!   metadata (the manager derives its own). It also checks where the
//!   session is bound.
//! - `RecordResult_AvailabilityNeutralSkipsSchedulerUpdate`: there is no
//!   scheduler index to compare snapshots of; the test checks that the
//!   result changed no availability state and published no model
//!   availability.
//! - `ExecuteCount_ExplicitModelNotFoundSuspendsModel`: upstream's executor
//!   error carries a `model_not_found` code, and the test checks the model
//!   state and hook kept it. An `ExecError` has no code, so the test checks
//!   the recorded failure is the executor's 404 and message instead.
//! - `MarkResult_RequestFaultBodyDoesNotCooldownModelOrAuth`: upstream's
//!   `NewRequestScopedError` and `MarkRequestScoped` constructors aren't
//!   ported; their result is built as an `AuthError` with the
//!   `request_scoped` code, and its request scope checked through the
//!   classifier.
//! - `IsCountTokensEndpointNotFoundError`: upstream's `*Error` cases go
//!   through the classifier as recorded failures (`ErrView::Auth`, which
//!   carries a code), and the code-less ones also as executor errors. Rust
//!   errors don't wrap, so the wrapped and joined cases are the inner
//!   request-scoped status error on its own.
//! - Registry counts (`GetModelCount`, `IsModelSuspendedForClient`) are read
//!   from the model availability the manager last published to the fake
//!   registry.
//! - Model names and credential IDs drop upstream's random suffixes, which
//!   only kept tests apart in Go's global registry.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::json;

use super::affinity::{bound, headers};
use super::support::*;
use crate::auth::{AuthError, ModelState, QuotaState, Status, Timestamp};
use crate::exec::{Dispatcher, ErrorKind, ExecError, Options};
use crate::manager::classify::{self, CODE_REQUEST_SCOPED, ErrView, is_request_scoped_error};
use crate::manager::cooldown::add;
use crate::manager::models::Resolver;
use crate::manager::retry::{RetryQuery, should_retry_after_error};
use crate::manager::select::{Selection, is_auth_blocked_for_model};
use crate::manager::{CallResult, ClientModels, ModelAlias, Settings};

const REQUEST_SCOPED_NOT_FOUND_MESSAGE: &str = "Item with id 'rs_0b5f3eb6f51f175c0169ca74e4a85881998539920821603a74' not found. Items are not persisted when `store` is set to false. Try again with `store` set to true, or remove this item from your input.";

const SECOND: Duration = Duration::from_secs(1);
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(60 * 60);

/// Upstream's `SetRetryConfig(retry, maxWait, maxCredentials)`.
fn retry_settings(request_retry: usize, max_wait: Duration, max_credentials: usize) -> Settings {
    Settings {
        request_retry,
        max_retry_interval: max_wait,
        max_retry_credentials: max_credentials,
        ..Settings::default()
    }
}

/// Upstream's `&Error{HTTPStatus: status, Message: message}`, as a result.
fn failure(status: u16, message: &str) -> AuthError {
    AuthError {
        message: message.to_owned(),
        http_status: status,
        ..AuthError::default()
    }
}

/// A failed result for `model` of credential `auth_id`.
fn failed(auth_id: &str, provider: &str, model: &str, error: AuthError) -> CallResult {
    CallResult {
        auth_id: auth_id.to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        success: false,
        error: Some(error),
        ..CallResult::default()
    }
}

/// Upstream's `retryAfterStatusError`.
fn retry_after_error(status: u16, message: &str, retry_after: Duration) -> ExecError {
    let mut err = ExecError::upstream(status, message);
    err.retry_after = Some(retry_after);
    err
}

/// Upstream's `requestScopedStatusError`.
fn request_scoped_error(status: u16, message: &str) -> ExecError {
    ExecError::upstream(status, message).with_request_scoped()
}

/// Whether to start another retry round, and after how long (upstream's
/// `shouldRetryAfterError`, with no credential tried yet).
fn should_retry(
    h: &Harness,
    err: &ExecError,
    attempt: usize,
    provider_names: &[&str],
    model: &str,
    max_wait: Duration,
) -> Option<Duration> {
    should_retry_pinned(h, err, attempt, provider_names, model, "", max_wait)
}

fn should_retry_pinned(
    h: &Harness,
    err: &ExecError,
    attempt: usize,
    provider_names: &[&str],
    model: &str,
    pinned: &str,
    max_wait: Duration,
) -> Option<Duration> {
    let provider_list = providers(provider_names);
    let attempted = HashSet::new();
    let now = h.now();
    let state = h.manager.lock();
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models: h.manager.models(),
        resolver: Resolver {
            settings: &state.settings,
            oauth: &state.oauth,
        },
        strategy: state.settings.routing_strategy,
        now,
    };
    let query = RetryQuery {
        providers: &provider_list,
        model,
        pinned,
        attempt,
        default_retry: state.settings.request_retry,
        eligibility: Default::default(),
        attempted: &attempted,
    };
    should_retry_after_error(&selection, &query, err, max_wait)
}

/// The manager's `max_retry_interval` (upstream's `retrySettings()` wait).
fn max_wait(h: &Harness) -> Duration {
    h.manager.settings().max_retry_interval
}

/// Which calls a [`fallback_executor`] fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailOn {
    Execute,
    /// The stream's first chunk is the error.
    StreamFirst,
    /// The stream gives the credential's ID, then the error.
    StreamTail,
    Count,
}

/// Upstream's `authFallbackExecutor`: answers with the credential's ID, and
/// fails one kind of call for the credentials in `failing`.
fn fallback_executor(
    id: &str,
    fail_on: FailOn,
    failing: &[(&str, ExecError)],
) -> Arc<FakeExecutor> {
    let errors: HashMap<String, ExecError> = failing
        .iter()
        .map(|(auth_id, err)| ((*auth_id).to_owned(), err.clone()))
        .collect();
    FakeExecutor::with(id, move |call| {
        let payload = Bytes::from(call.auth_id.clone());
        let Some(err) = errors.get(&call.auth_id).cloned() else {
            return Reply::ok(payload);
        };
        match (call.kind, fail_on) {
            (Kind::Execute, FailOn::Execute) | (Kind::Count, FailOn::Count) => Reply::Err(err),
            (Kind::Stream, FailOn::StreamFirst) => Reply::chunks(vec![Err(err)]),
            (Kind::Stream, FailOn::StreamTail) => Reply::chunks(vec![Ok(payload), Err(err)]),
            _ => Reply::ok(payload),
        }
    })
}

/// Upstream's `credentialRetryLimitExecutor`: every call fails with 500.
fn failing_executor(id: &str) -> Arc<FakeExecutor> {
    FakeExecutor::with(id, |_| Reply::status(500, "boom"))
}

fn stream_options() -> Options {
    let mut opts = options();
    opts.stream = true;
    opts
}

/// Makes a call of `kind` and reads it to its end: the payload, or the
/// error it ended with (for a stream, the call's or the stream's).
async fn run(
    h: &Harness,
    kind: Kind,
    provider_names: &[&str],
    model: &str,
    opts: Options,
) -> Result<String, ExecError> {
    let provider_list = providers(provider_names);
    let text = |payload: &Bytes| String::from_utf8_lossy(payload).into_owned();
    match kind {
        Kind::Execute => h
            .manager
            .execute(&provider_list, request(model), opts)
            .await
            .map(|response| text(&response.payload)),
        Kind::Count => h
            .manager
            .count_tokens(&provider_list, request(model), opts)
            .await
            .map(|response| text(&response.payload)),
        Kind::Stream => {
            let stream = h
                .manager
                .execute_stream(&provider_list, request(model), opts)
                .await?;
            match collect(stream).await {
                (_, Some(err)) => Err(err),
                (chunks, None) => Ok(chunks.concat()),
            }
        }
    }
}

/// The time `d` from now on the manager's clock.
fn from_now(h: &Harness, d: Duration) -> Timestamp {
    add(h.now(), d)
}

/// How long until `t` (upstream's `time.Until`); zero when it is past.
fn until(h: &Harness, t: Option<Timestamp>) -> Duration {
    let t = t.expect("a time");
    (t - h.now()).to_std().unwrap_or_default()
}

/// The registry's `GetModelCount(model)` over `ids`: the credentials that
/// registered `model` and whose last published availability doesn't hold it
/// back for a suspension or quota.
fn model_count(h: &Harness, ids: &[&str], model: &str) -> usize {
    ids.iter()
        .filter(|id| h.models.client_supports_model(id, model))
        .filter(|id| {
            h.models
                .projection(id, model)
                .is_none_or(|p| !p.suspended && !p.quota_exceeded)
        })
        .count()
}

/// The registry's `IsModelSuspendedForClient(id, model)`.
fn is_model_suspended_for_client(h: &Harness, id: &str, model: &str) -> bool {
    h.models.projection(id, model).is_some_and(|p| p.suspended)
}

/// The state of `model` on credential `id`.
fn model_state(h: &Harness, id: &str, model: &str) -> Option<ModelState> {
    h.get(id).model_states.get(model).cloned()
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_respects_auth_request_retry_override() {
    let h = Harness::new(retry_settings(3, 30 * SECOND, 0));
    let model = "test-model";
    let next = from_now(&h, 5 * SECOND);
    let mut auth = auth_with_metadata("auth-1", "claude", json!({"request_retry": 0.0}));
    auth.model_states.insert(
        model.to_owned(),
        ModelState {
            unavailable: true,
            status: Status::Error,
            next_retry_after: Some(next),
            last_error: Some(failure(500, "upstream unavailable")),
            ..ModelState::default()
        },
    );
    h.add(auth.clone(), &[model]);

    let boom = ExecError::upstream(500, "boom");
    let wait = should_retry(&h, &boom, 0, &["claude"], model, max_wait(&h));
    assert_eq!(wait, None, "request_retry=0 must not retry");

    auth.metadata.insert("request_retry".into(), json!(1.0));
    h.manager
        .update(auth)
        .expect("update auth")
        .expect("auth registered");

    let wait = should_retry(&h, &boom, 0, &["claude"], model, max_wait(&h))
        .expect("request_retry=1 retries");
    assert!(wait > Duration::ZERO, "wait = {wait:?}, want > 0");

    let wait = should_retry(&h, &boom, 1, &["claude"], model, max_wait(&h));
    assert_eq!(wait, None, "attempt=1 with request_retry=1 must not retry");
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_retries_local_round_without_cooldown() {
    let h = Harness::new(retry_settings(1, Duration::ZERO, 0));
    let model = "gpt-retry-without-cooldown";
    h.add(auth("retry-auth", "codex"), &[model]);

    for status in [429, 502] {
        let err = ExecError::upstream(status, "retryable failure");
        assert_eq!(
            should_retry(&h, &err, 0, &["codex"], model, Duration::ZERO),
            Some(Duration::ZERO),
            "status {status}"
        );
        assert_eq!(
            should_retry(&h, &err, 1, &["codex"], model, Duration::ZERO),
            None,
            "status {status} retried after the configured additional round"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_does_not_wait_when_another_credential_is_available() {
    let h = Harness::new(retry_settings(1, MINUTE, 1));
    let model = "retry-available-credential";
    let mut cooling = auth("cooling-auth", "codex");
    cooling.model_states.insert(
        model.to_owned(),
        ModelState {
            unavailable: true,
            status: Status::Error,
            next_retry_after: Some(from_now(&h, 30 * SECOND)),
            ..ModelState::default()
        },
    );
    h.add(cooling, &[model]);
    h.add(auth("available-auth", "codex"), &[model]);

    let err = ExecError::upstream(429, "rate limited");
    assert_eq!(
        should_retry(&h, &err, 0, &["codex"], model, MINUTE),
        Some(Duration::ZERO),
        "retry with an available credential must be immediate"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_ignores_unrelated_model_override() {
    let h = Harness::new(retry_settings(0, Duration::ZERO, 0));
    let target_model = "retry-target";
    let unrelated_model = "retry-unrelated";
    h.add(
        auth_with_metadata("target-auth", "codex", json!({"request_retry": 0})),
        &[target_model],
    );
    h.add(
        auth_with_metadata("unrelated-auth", "codex", json!({"request_retry": 2})),
        &[unrelated_model],
    );

    let err = ExecError::upstream(502, "retryable failure");
    assert_eq!(
        should_retry(&h, &err, 0, &["codex"], target_model, Duration::ZERO),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_ignores_disabled_retry_override() {
    let h = Harness::new(retry_settings(0, Duration::ZERO, 0));
    h.add(
        auth_with_metadata("active-auth", "codex", json!({"request_retry": 0})),
        &[],
    );
    let mut disabled = auth_with_metadata("disabled-auth", "codex", json!({"request_retry": 2}));
    disabled.disabled = true;
    h.add(disabled, &[]);

    let err = ExecError::upstream(502, "retryable failure");
    assert_eq!(
        should_retry(&h, &err, 0, &["codex"], "", Duration::ZERO),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_ignores_non_round_cooldown_overrides() {
    let blocked = |status: u16, message: &str| ModelState {
        status: Status::Error,
        unavailable: true,
        last_error: Some(failure(status, message)),
        ..ModelState::default()
    };
    let cases = [
        (
            "model disabled",
            ModelState {
                status: Status::Disabled,
                ..ModelState::default()
            },
        ),
        ("unauthorized", blocked(401, "unauthorized")),
        ("payment required", blocked(402, "payment required")),
        ("not found", blocked(404, "not found")),
        ("model unsupported", blocked(400, "model not supported")),
    ];
    for (name, mut state) in cases {
        let h = Harness::new(retry_settings(0, MINUTE, 0));
        let model = "retry-non-round";
        if state.status != Status::Disabled {
            state.next_retry_after = Some(from_now(&h, MINUTE));
        }
        h.add(
            auth_with_metadata("retry-round-eligible", "codex", json!({"request_retry": 0})),
            &[model],
        );
        let mut ineligible = auth_with_metadata(
            "retry-round-ineligible",
            "codex",
            json!({"request_retry": 2}),
        );
        ineligible.model_states.insert(model.to_owned(), state);
        h.add(ineligible, &[model]);

        let err = ExecError::upstream(502, "upstream unavailable");
        assert_eq!(
            should_retry(&h, &err, 0, &["codex"], model, MINUTE),
            None,
            "{name}: a non-round cooldown override must not retry"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_ignores_request_ineligible_overrides() {
    // Upstream's "credential policy" case is left out: a credential policy
    // only narrows Codex Alpha Search's pick, which doesn't retry. This is
    // the "pinned credential" case.
    let h = Harness::new(retry_settings(0, Duration::ZERO, 0));
    let model = "retry-eligibility";
    h.add(
        auth_with_metadata(
            "retry-pinned-eligible",
            "codex",
            json!({"request_retry": 0}),
        ),
        &[model],
    );
    h.add(
        auth_with_metadata(
            "retry-pinned-ineligible",
            "codex",
            json!({"request_retry": 2}),
        ),
        &[model],
    );

    let err = ExecError::upstream(502, "retryable failure");
    assert_eq!(
        should_retry_pinned(
            &h,
            &err,
            0,
            &["codex"],
            model,
            "retry-pinned-eligible",
            Duration::ZERO
        ),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn manager_request_retry_runs_additional_local_round_without_cooldown() {
    for (name, kind, opts) in [
        ("nonstream", Kind::Execute, options()),
        ("count tokens", Kind::Count, options()),
        ("stream", Kind::Stream, stream_options()),
    ] {
        let h = Harness::new(retry_settings(1, Duration::ZERO, 0));
        let executor = failing_executor("claude");
        h.executor(&executor);
        let model = "retry-model";
        h.add(
            auth_with_metadata("retry-auth", "claude", json!({"disable_cooling": true})),
            &[model],
        );

        let err = run(&h, kind, &["claude"], model, opts)
            .await
            .expect_err("want an error");
        assert_eq!(err.http_status(), 500, "{name}: error = {err}");
        assert_eq!(
            executor.calls().len(),
            2,
            "{name}: want the initial round plus one additional round"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_uses_oauth_model_alias_for_cooldown() {
    let mut settings = retry_settings(3, 30 * SECOND, 0);
    settings.oauth_model_alias.insert(
        "kimi".into(),
        vec![ModelAlias {
            name: "deepseek-v3.1".into(),
            alias: "pool-model".into(),
            force_mapping: false,
        }],
    );
    let h = Harness::new(settings);
    let route_model = "pool-model";
    let upstream_model = "deepseek-v3.1";
    let next = from_now(&h, 5 * SECOND);
    let mut auth = auth("auth-1", "kimi");
    auth.model_states.insert(
        upstream_model.to_owned(),
        ModelState {
            unavailable: true,
            status: Status::Error,
            next_retry_after: Some(next),
            quota: QuotaState {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(next),
                ..QuotaState::default()
            },
            ..ModelState::default()
        },
    );
    h.add(auth, &[upstream_model]);

    let err = ExecError::upstream(429, "quota");
    let wait =
        should_retry(&h, &err, 0, &["kimi"], route_model, max_wait(&h)).expect("want a retry");
    assert!(wait > Duration::ZERO, "wait = {wait:?}, want > 0");
}

/// Upstream's `newCredentialRetryLimitTestManager`.
fn credential_retry_limit_manager(max_credentials: usize) -> (Harness, Arc<FakeExecutor>) {
    let h = Harness::new(retry_settings(0, Duration::ZERO, max_credentials));
    let executor = failing_executor("claude");
    h.executor(&executor);
    h.add(auth("retry-limit-auth-1", "claude"), &["test-model"]);
    h.add(auth("retry-limit-auth-2", "claude"), &["test-model"]);
    (h, executor)
}

#[tokio::test(start_paused = true)]
async fn manager_max_retry_credentials_limits_cross_credential_retries() {
    for (name, kind) in [
        ("execute", Kind::Execute),
        ("execute_count", Kind::Count),
        ("execute_stream", Kind::Stream),
    ] {
        let (limited, limited_executor) = credential_retry_limit_manager(1);
        let result = run(&limited, kind, &["claude"], "test-model", options()).await;
        assert!(result.is_err(), "{name}: want an error with a limit");
        assert_eq!(
            limited_executor.calls().len(),
            1,
            "{name}: calls with max-retry-credentials=1"
        );

        let (unlimited, unlimited_executor) = credential_retry_limit_manager(0);
        let result = run(&unlimited, kind, &["claude"], "test-model", options()).await;
        assert!(result.is_err(), "{name}: want an error without a limit");
        assert_eq!(
            unlimited_executor.calls().len(),
            2,
            "{name}: calls with max-retry-credentials=0"
        );
    }
}

/// Two credentials of `provider` serving `model`, the first failing with
/// `err` on `fail_on` calls. Calls twice and checks both reach the good
/// credential, after one call on the bad one. Returns the harness.
async fn falls_back_twice(
    provider: &str,
    model: &str,
    kind: Kind,
    fail_on: FailOn,
    err: ExecError,
) -> Harness {
    let h = Harness::new(Settings::default());
    let executor = fallback_executor(provider, fail_on, &[("aa-bad-auth", err)]);
    h.executor(&executor);
    h.add(auth("aa-bad-auth", provider), &[model]);
    h.add(auth("bb-good-auth", provider), &[model]);

    for i in 0..2 {
        let payload = run(&h, kind, &[provider], model, options())
            .await
            .unwrap_or_else(|err| panic!("call {i} error = {err}, want success"));
        assert_eq!(payload, "bb-good-auth", "call {i} payload");
    }
    assert_eq!(
        executor.ids(kind),
        ["aa-bad-auth", "bb-good-auth", "bb-good-auth"]
    );

    let state = model_state(&h, "aa-bad-auth", model).expect("bad auth model state");
    assert!(
        state.unavailable,
        "bad auth model state must be unavailable"
    );
    assert!(
        state.next_retry_after.is_some(),
        "bad auth model state cooldown must be set"
    );
    h
}

#[tokio::test(start_paused = true)]
async fn manager_model_support_bad_request_falls_back_and_suspends_auth() {
    falls_back_twice(
        "claude",
        "claude-opus-4-6",
        Kind::Execute,
        FailOn::Execute,
        ExecError::upstream(
            400,
            "invalid_request_error: The requested model is not supported.",
        ),
    )
    .await;
}

const INVALID_GRANT_MESSAGE: &str = r#"bad response status code 400, message: {"error":"invalid_grant","error_description":"Bad Request"}, body: {"type":"error","error":{"type":"invalid_request_error","message":"{\"error\":\"invalid_grant\"}"}}"#;

#[tokio::test(start_paused = true)]
async fn manager_execute_antigravity_invalid_grant_falls_back_and_suspends_auth() {
    let model = "gemini-3-pro-preview";
    let h = falls_back_twice(
        "antigravity",
        model,
        Kind::Execute,
        FailOn::Execute,
        ExecError::upstream(400, INVALID_GRANT_MESSAGE),
    )
    .await;
    let state = model_state(&h, "aa-bad-auth", model).expect("bad auth model state");
    assert_eq!(state.status_message, INVALID_GRANT_MESSAGE);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_antigravity_invalid_grant_falls_back_and_suspends_auth() {
    falls_back_twice(
        "antigravity",
        "gemini-3-pro-preview",
        Kind::Stream,
        FailOn::StreamFirst,
        ExecError::upstream(400, INVALID_GRANT_MESSAGE),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_model_support_bad_request_falls_back_and_suspends_auth() {
    falls_back_twice(
        "claude",
        "claude-opus-4-6",
        Kind::Stream,
        FailOn::StreamFirst,
        ExecError::upstream(
            400,
            "invalid_request_error: The requested model is not supported.",
        ),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_respects_auth_disable_cooling_override() {
    let h = Harness::new(Settings::default());
    h.add(
        auth_with_metadata("auth-1", "claude", json!({"disable_cooling": true})),
        &[],
    );

    let model = "test-model";
    h.manager
        .mark_result(&failed("auth-1", "claude", model, failure(500, "boom")));

    let state = model_state(&h, "auth-1", model).expect("model state");
    assert_eq!(
        state.next_retry_after, None,
        "NextRetryAfter must be zero when disable_cooling=true"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_transient_error_cooldown_default() {
    let h = Harness::new(Settings {
        transient_error_cooldown_seconds: 0,
        ..Settings::default()
    });
    let id = "auth-transient-default";
    h.add(auth(id, "claude"), &[]);

    let model = "test-model-transient-default";
    h.manager
        .mark_result(&failed(id, "claude", model, failure(502, "bad gateway")));

    let state = model_state(&h, id, model).expect("model state");
    assert!(
        state.next_retry_after.is_some(),
        "transient error cooldown must keep the legacy default"
    );
    let diff = until(&h, state.next_retry_after);
    assert!(
        (55 * SECOND..=65 * SECOND).contains(&diff),
        "transient error cooldown = {diff:?}, want ~60s"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_transient_error_cooldown_disabled() {
    let h = Harness::new(Settings {
        transient_error_cooldown_seconds: -1,
        ..Settings::default()
    });
    let model_auth = "auth-transient-model-disabled";
    h.add(auth(model_auth, "claude"), &[]);

    let model = "test-model-transient-disabled";
    h.manager.mark_result(&failed(
        model_auth,
        "claude",
        model,
        failure(502, "bad gateway"),
    ));
    let state = model_state(&h, model_auth, model).expect("model state");
    assert_eq!(
        state.next_retry_after, None,
        "transient model cooldown must be disabled"
    );

    let auth_level = "auth-transient-auth-disabled";
    h.add(auth(auth_level, "claude"), &[]);
    h.manager.mark_result(&failed(
        auth_level,
        "claude",
        "",
        failure(503, "unavailable"),
    ));
    assert_eq!(
        h.get(auth_level).next_retry_after,
        None,
        "transient auth cooldown must be disabled"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_transient_error_cooldown_does_not_disable_auth_errors() {
    let h = Harness::new(Settings {
        transient_error_cooldown_seconds: -1,
        ..Settings::default()
    });
    let id = "auth-transient-auth-error";
    h.add(auth(id, "claude"), &[]);

    let model = "test-model-auth-error";
    h.manager
        .mark_result(&failed(id, "claude", model, failure(403, "forbidden")));

    let state = model_state(&h, id, model).expect("model state");
    assert!(
        state.next_retry_after.is_some(),
        "auth error cooldown must remain enabled"
    );
    let diff = until(&h, state.next_retry_after);
    assert!(
        (29 * MINUTE..=31 * MINUTE).contains(&diff),
        "auth error cooldown = {diff:?}, want ~30 minutes"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_respects_auth_disable_cooling_override_on403() {
    let h = Harness::new(Settings::default());
    let id = "auth-403";
    h.add(
        auth_with_metadata(id, "claude", json!({"disable_cooling": true})),
        &[],
    );
    let model = "test-model-403";
    h.models.register(id, &[model]);

    h.manager
        .mark_result(&failed(id, "claude", model, failure(403, "forbidden")));

    let state = model_state(&h, id, model).expect("model state");
    assert_eq!(
        state.next_retry_after, None,
        "NextRetryAfter must be zero when disable_cooling=true"
    );
    let count = model_count(&h, &[id], model);
    assert!(
        count > 0,
        "model count = {count}, want > 0 when disable_cooling=true"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare_challenge_on403() {
    let h = Harness::new(Settings::default());
    let id = "auth-cf-403";
    h.add(auth(id, "claude"), &[]);
    let model = "test-model-cf-403";
    h.models.register(id, &[model]);

    h.manager.mark_result(&failed(
        id,
        "claude",
        model,
        failure(403, "cf-mitigated: challenge"),
    ));

    let state = model_state(&h, id, model).expect("model state");
    assert!(
        state.next_retry_after.is_some(),
        "NextRetryAfter must be set for a cloudflare challenge"
    );
    let diff = until(&h, state.next_retry_after);
    assert!(
        (5 * SECOND..=25 * SECOND).contains(&diff),
        "NextRetryAfter in {diff:?}, want ~10 seconds"
    );
    assert_eq!(state.status_message, "cloudflare challenge");
    // The challenge sets Unavailable and NextRetryAfter on the model state,
    // so the registry suspends the model for this credential.
    assert!(
        is_model_suspended_for_client(&h, id, model),
        "model must be suspended in the registry for a cloudflare challenge"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_disable_cooling_does_not_blackout_after403() {
    let h = Harness::new(Settings::default());
    let id = "auth-403-exec";
    let executor = fallback_executor(
        "claude",
        FailOn::Execute,
        &[(id, ExecError::upstream(403, "forbidden"))],
    );
    h.executor(&executor);
    h.add(
        auth_with_metadata(id, "claude", json!({"disable_cooling": true})),
        &[],
    );
    let model = "test-model-403-exec";
    h.models.register(id, &[model]);

    for call in ["first", "second"] {
        let err = run(&h, Kind::Execute, &["claude"], model, options())
            .await
            .expect_err("want an error");
        assert_eq!(err.http_status(), 403, "{call} execute status");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_disable_cooling_does_not_blackout_after429_retry_after() {
    let h = Harness::new(Settings::default());
    let id = "auth-429-exec";
    let executor = fallback_executor(
        "claude",
        FailOn::Execute,
        &[(id, retry_after_error(429, "quota exhausted", 2 * MINUTE))],
    );
    h.executor(&executor);
    h.add(
        auth_with_metadata(id, "claude", json!({"disable_cooling": true})),
        &[],
    );
    let model = "test-model-429-exec";
    h.models.register(id, &[model]);

    for call in ["first", "second"] {
        let err = run(&h, Kind::Execute, &["claude"], model, options())
            .await
            .expect_err("want an error");
        assert_eq!(err.http_status(), 429, "{call} execute status");
    }
    assert_eq!(executor.ids(Kind::Execute).len(), 2, "execute calls");

    let state = model_state(&h, id, model).expect("model state");
    assert_eq!(
        state.next_retry_after, None,
        "NextRetryAfter must be zero when disable_cooling=true"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_disable_cooling_retries_after429_retry_after() {
    let h = Harness::new(retry_settings(3, Duration::from_millis(100), 0));
    let id = "auth-429-retryafter-exec";
    let executor = fallback_executor(
        "claude",
        FailOn::Execute,
        &[(
            id,
            retry_after_error(429, "quota exhausted", Duration::from_millis(5)),
        )],
    );
    h.executor(&executor);
    h.add(
        auth_with_metadata(id, "claude", json!({"disable_cooling": true})),
        &[],
    );
    let model = "test-model-429-retryafter-exec";
    h.models.register(id, &[model]);

    let err = run(&h, Kind::Execute, &["claude"], model, options())
        .await
        .expect_err("want an error");
    assert_eq!(err.http_status(), 429, "execute status");
    assert_eq!(
        executor.ids(Kind::Execute).len(),
        4,
        "execute calls: initial + 3 retries"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_request_scoped_error_stops_credential_fallback_without_suspending_auth() {
    let incomplete = request_scoped_error(
        408,
        "stream error: stream disconnected before completion: stream closed before response.completed",
    );
    let message_too_big = request_scoped_error(
        413,
        r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#,
    );
    let invalid_request = ExecError::upstream(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."}}"#,
    );
    let bad_request = ExecError::upstream(
        400,
        r#"{"error":{"type":"bad_request_error","code":"invalid_value","message":"Bad input."}}"#,
    );
    let cyber_policy = ExecError::upstream(
        502,
        r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk."}}"#,
    );
    // A frame or payload over the upstream size limit fails the same on
    // every credential, so it must not rotate or punish the pool.
    let too_large = ExecError::upstream(
        413,
        r#"{"error":{"code":"message_too_big","message":"upstream websocket message too big"}}"#,
    );
    let plain_bad_request = ExecError::upstream(400, "bad request");
    let conflict = ExecError::upstream(
        409,
        r#"{"error":{"type":"conflict_error","code":"conflict","message":"request conflict"}}"#,
    );
    let context_length = ExecError::upstream(
        502,
        r#"{"error":{"type":"server_error","code":"context_length_exceeded","message":"input too long"}}"#,
    );
    let invalid_request_type = ExecError::upstream(
        502,
        r#"{"body":{"error":{"type":"invalid_request","message":"invalid input"}}}"#,
    );
    // Upstream sends this one as plain text rather than a JSON error body.
    let item_not_persisted = ExecError::upstream(404, REQUEST_SCOPED_NOT_FOUND_MESSAGE);

    // (name, provider, stream, stream after payload, error, status)
    let cases = [
        (
            "non-streaming incomplete",
            "",
            false,
            false,
            &incomplete,
            408,
        ),
        ("streaming incomplete", "", true, false, &incomplete, 408),
        (
            "streaming codex websocket message too big",
            "codex",
            true,
            false,
            &message_too_big,
            413,
        ),
        (
            "streaming xai websocket message too big",
            "xai",
            true,
            false,
            &message_too_big,
            413,
        ),
        (
            "non-streaming invalid request",
            "",
            false,
            false,
            &invalid_request,
            400,
        ),
        (
            "streaming invalid request",
            "",
            true,
            false,
            &invalid_request,
            400,
        ),
        (
            "non-streaming bad request",
            "",
            false,
            false,
            &bad_request,
            400,
        ),
        ("streaming bad request", "", true, false, &bad_request, 400),
        (
            "streaming cyber policy",
            "codex",
            true,
            false,
            &cyber_policy,
            502,
        ),
        (
            "non-streaming message too big",
            "codex",
            false,
            false,
            &too_large,
            413,
        ),
        (
            "streaming message too big",
            "codex",
            true,
            false,
            &too_large,
            413,
        ),
        (
            "non-streaming plain bad request",
            "",
            false,
            false,
            &plain_bad_request,
            400,
        ),
        (
            "streaming plain bad request",
            "",
            true,
            false,
            &plain_bad_request,
            400,
        ),
        ("non-streaming conflict", "", false, false, &conflict, 409),
        ("streaming conflict", "", true, false, &conflict, 409),
        (
            "streaming conflict after payload",
            "",
            true,
            true,
            &conflict,
            409,
        ),
        (
            "non-streaming context length behind bad gateway",
            "",
            false,
            false,
            &context_length,
            502,
        ),
        (
            "streaming context length behind bad gateway",
            "",
            true,
            false,
            &context_length,
            502,
        ),
        (
            "streaming invalid request type behind bad gateway",
            "",
            true,
            false,
            &invalid_request_type,
            502,
        ),
        (
            "non-streaming item not persisted",
            "",
            false,
            false,
            &item_not_persisted,
            404,
        ),
        (
            "streaming item not persisted",
            "",
            true,
            false,
            &item_not_persisted,
            404,
        ),
        (
            "streaming item not persisted after payload",
            "",
            true,
            true,
            &item_not_persisted,
            404,
        ),
    ];

    for (name, provider, stream, after_payload, err, want_status) in cases {
        let provider = if provider.is_empty() {
            "codex"
        } else {
            provider
        };
        let h = Harness::new(retry_settings(2, 30 * SECOND, 0));
        let fail_on = match (stream, after_payload) {
            (_, true) => FailOn::StreamTail,
            (true, false) => FailOn::StreamFirst,
            (false, false) => FailOn::Execute,
        };
        let executor = fallback_executor(provider, fail_on, &[("aa-bad-auth", err.clone())]);
        h.executor(&executor);
        let model = "gpt-5.5";
        h.add(auth("aa-bad-auth", provider), &[model]);
        h.add(auth("bb-good-auth", provider), &[model]);

        let (kind, opts) = if stream {
            (Kind::Stream, stream_options())
        } else {
            (Kind::Execute, options())
        };
        let got = run(&h, kind, &[provider], model, opts)
            .await
            .expect_err("want a request-scoped error");
        assert_eq!(got.http_status(), want_status, "{name}: status");
        assert_eq!(
            executor.ids(kind),
            ["aa-bad-auth"],
            "{name}: credential calls"
        );

        let bad = h.get("aa-bad-auth");
        assert!(!bad.unavailable, "{name}: auth must stay available");
        assert_eq!(
            bad.next_retry_after, None,
            "{name}: auth cooldown must stay unset"
        );
        assert!(
            !bad.model_states.contains_key(model),
            "{name}: no model cooldown state, got {:?}",
            bad.model_states.get(model)
        );
        assert_eq!(bad.failed, 1, "{name}: failed count");
        let good = h.get("bb-good-auth");
        assert_eq!(good.failed, 0, "{name}: fallback auth failed count");
        assert!(good.last_error.is_none(), "{name}: fallback auth error");
        assert!(good.model_states.is_empty(), "{name}: fallback auth states");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_deep_seek_insufficient_balance_rotates_credential_and_rebinds_session() {
    let h = Harness::new(Settings {
        session_affinity: true,
        session_affinity_ttl: HOUR,
        ..retry_settings(2, 30 * SECOND, 0)
    });
    let provider = "openai-compatibility";
    let model = "deepseek-v4-pro";
    let executor = fallback_executor(
        provider,
        FailOn::Execute,
        &[(
            "aa-empty-balance",
            ExecError::upstream(
                402,
                r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#,
            ),
        )],
    );
    h.executor(&executor);
    h.add(auth("aa-empty-balance", provider), &[model]);
    h.add(auth("bb-available-balance", provider), &[model]);

    let opts = || {
        let mut opts = options();
        opts.headers = headers(&[("X-Session-Id", "deepseek-insufficient-balance")]);
        opts
    };
    let before = h.now();
    let served = run(&h, Kind::Execute, &[provider], model, opts())
        .await
        .expect("fallback to the next credential");
    assert_eq!(served, "bb-available-balance");
    let key = "mixed::header:deepseek-insufficient-balance::deepseek-v4-pro";
    assert_eq!(
        bound(&h, key).as_deref(),
        Some("bb-available-balance"),
        "the session moved"
    );
    let served = run(&h, Kind::Execute, &[provider], model, opts())
        .await
        .expect("the rebound session to use the next credential");
    assert_eq!(served, "bb-available-balance");
    assert_eq!(
        executor.ids(Kind::Execute),
        [
            "aa-empty-balance",
            "bb-available-balance",
            "bb-available-balance"
        ]
    );

    let state =
        model_state(&h, "aa-empty-balance", model).expect("depleted credential cooled down");
    assert!(state.unavailable, "depleted credential must be unavailable");
    let next = state.next_retry_after.expect("a cooldown");
    assert!(
        next >= add(before, 29 * MINUTE),
        "cooldown expires at {next}, want about 30 minutes"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_deep_seek_credential_failures_rotate_credential() {
    let cases = [
        (
            "authentication failure",
            401,
            r#"{"error":{"code":"invalid_request_error","message":"Authentication Fails, Your api key: ****heck is invalid","param":null,"type":"authentication_error"}}"#,
            false,
        ),
        (
            "rate limit with generic request error code",
            429,
            r#"{"error":{"code":"invalid_request_error","message":"Rate Limit Reached","param":null,"type":"unknown_error"}}"#,
            true,
        ),
    ];
    for (name, status, message, want_quota) in cases {
        let h = Harness::new(retry_settings(2, 30 * SECOND, 0));
        let provider = "openai-compatibility";
        let model = "deepseek-v4-pro";
        let executor = fallback_executor(
            provider,
            FailOn::Execute,
            &[("aa-failed-key", ExecError::upstream(status, message))],
        );
        h.executor(&executor);
        h.add(auth("aa-failed-key", provider), &[model]);
        h.add(auth("bb-valid-key", provider), &[model]);

        let served = run(&h, Kind::Execute, &[provider], model, options())
            .await
            .unwrap_or_else(|err| panic!("{name}: want fallback, got {err}"));
        assert_eq!(served, "bb-valid-key", "{name}");
        assert_eq!(
            executor.ids(Kind::Execute),
            ["aa-failed-key", "bb-valid-key"],
            "{name}"
        );

        let state = model_state(&h, "aa-failed-key", model);
        assert!(
            state
                .as_ref()
                .is_some_and(|s| s.unavailable && s.next_retry_after.is_some()),
            "{name}: failed auth model state = {state:?}, want an active cooldown"
        );
        if want_quota {
            let quota = state.map(|s| s.quota).unwrap_or_default();
            assert!(
                quota.exceeded && quota.reason == "quota",
                "{name}: failed auth quota = {quota:?}, want exceeded quota"
            );
        }
    }
}

/// An upstream 500 `"status":"UNKNOWN"` is an internal failure, not the
/// request's fault: the call falls through to the next credential, and the
/// cooldown lands on the (credential, model) pair only.
#[tokio::test(start_paused = true)]
async fn manager_unknown_upstream_error_rotates_and_penalizes_model_only() {
    let h = Harness::new(retry_settings(3, 30 * SECOND, 0));
    let provider = "gemini";
    let model = "gemini-3.6-pro";
    let sibling_model = "gemini-3.6-flash";
    let executor = fallback_executor(
        provider,
        FailOn::Execute,
        &[(
            "aa-bad-auth",
            ExecError::upstream(
                500,
                r#"{"error":{"code":500,"message":"Internal error encountered.","status":"UNKNOWN"}}"#,
            ),
        )],
    );
    h.executor(&executor);
    h.add(auth("aa-bad-auth", provider), &[model, sibling_model]);
    h.add(auth("bb-good-auth", provider), &[model, sibling_model]);

    let served = run(&h, Kind::Execute, &[provider], model, options())
        .await
        .expect("fallback to the next credential");
    assert_eq!(served, "bb-good-auth");
    assert_eq!(executor.ids(Kind::Execute), ["aa-bad-auth", "bb-good-auth"]);

    let bad = h.get("aa-bad-auth");
    let state = bad
        .model_states
        .get(model)
        .expect("the failing (credential, model) pair is penalized");
    assert!(
        state.next_retry_after.is_some(),
        "the failing (credential, model) pair must cool down"
    );

    let now = h.now();
    assert!(
        is_auth_blocked_for_model(&bad, model, now).0,
        "the failing model must be blocked on that credential"
    );
    let (blocked, reason, _) = is_auth_blocked_for_model(&bad, sibling_model, now);
    assert!(
        !blocked,
        "sibling model blocked on the same credential ({reason:?}); the penalty must stay with (credential, model)"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_request_scoped_not_found_does_not_cooldown_auth() {
    let h = Harness::new(Settings::default());
    h.add(auth("auth-1", "openai"), &[]);

    let model = "gpt-4.1";
    h.manager.mark_result(&failed(
        "auth-1",
        "openai",
        model,
        failure(404, REQUEST_SCOPED_NOT_FOUND_MESSAGE),
    ));

    let updated = h.get("auth-1");
    assert!(
        !updated.unavailable,
        "a request-scoped 404 keeps the auth available"
    );
    assert_eq!(
        updated.next_retry_after, None,
        "auth cooldown must stay unset"
    );
    assert!(
        !updated.model_states.contains_key(model),
        "no model cooldown state, got {:?}",
        updated.model_states.get(model)
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_count_generic_route_not_found_does_not_suspend_model() {
    let h = Harness::new(Settings::default());
    let id = "count-route-not-found-auth";
    let executor = fallback_executor(
        "claude",
        FailOn::Count,
        &[(id, ExecError::upstream(404, "404 page not found"))],
    );
    h.executor(&executor);
    let model = "count-route-not-found-model";
    h.add(auth(id, "claude"), &[model]);

    let err = run(&h, Kind::Count, &["claude"], model, options())
        .await
        .expect_err("want the count_tokens route 404");
    // Upstream also checks the one failed 404 result its hook saw; hooks
    // aren't ported.
    assert_eq!(err.http_status(), 404);

    let updated = h.get(id);
    assert_eq!(updated.failed, 1, "failed request count");
    assert!(!updated.unavailable, "a route 404 keeps the auth available");
    assert!(
        !updated.model_states.contains_key(model),
        "no model cooldown state, got {:?}",
        updated.model_states.get(model)
    );
    assert_eq!(model_count(&h, &[id], model), 1, "available model count");

    let served = run(&h, Kind::Execute, &["claude"], model, options())
        .await
        .expect("execute after the count_tokens route 404");
    assert_eq!(served, id);
}

#[tokio::test(start_paused = true)]
async fn manager_execute_count_explicit_model_not_found_suspends_model() {
    let h = Harness::new(Settings::default());
    let id = "count-model-not-found-auth";
    let message = r#"{"type":"error","error":{"type":"not_found_error","message":"model count-explicitly-missing-model was not found"}}"#;
    let executor = fallback_executor(
        "claude",
        FailOn::Count,
        &[(id, ExecError::upstream(404, message))],
    );
    h.executor(&executor);
    let model = "count-explicitly-missing-model";
    h.add(auth(id, "claude"), &[model]);

    run(&h, Kind::Count, &["claude"], model, options())
        .await
        .expect_err("want the count_tokens model-not-found error");

    let state = model_state(&h, id, model);
    assert!(
        state.as_ref().is_some_and(|s| s.unavailable),
        "want a model-not-found cooldown state, got {state:?}"
    );
    let state = state.unwrap_or_default();
    // Upstream's executor error carries the `model_not_found` code, and the
    // test checks the model state and the hook kept it. An `ExecError` has
    // no code, so the recorded failure is checked to be the executor's 404.
    // (Given the same error without a code, upstream records an empty code
    // too.)
    let last_error = state.last_error.as_ref().expect("model state error");
    assert_eq!(last_error.http_status, 404, "model state error status");
    assert_eq!(last_error.message, message, "model state error message");
    let remaining = until(&h, state.next_retry_after);
    assert!(
        (11 * HOUR..=12 * HOUR).contains(&remaining),
        "model-not-found cooldown = {remaining:?}, want about 12h"
    );
    assert_eq!(model_count(&h, &[id], model), 0, "available model count");
}

#[test]
fn is_count_tokens_endpoint_not_found_error() {
    let typed = |code: &str, status: u16, message: &str| AuthError {
        code: code.to_owned(),
        message: message.to_owned(),
        http_status: status,
        ..AuthError::default()
    };
    let plain = |status: u16, message: &str| typed("", status, message);
    // (name, error, model, want); an empty model is "claude-missing".
    let cases: Vec<(&str, AuthError, &str, bool)> = vec![
        ("empty router 404", plain(404, ""), "", true),
        (
            "plain router 404",
            plain(404, "404 page not found"),
            "",
            true,
        ),
        (
            "wrapped router 404",
            plain(404, "upstream request failed: 404 page not found"),
            "",
            true,
        ),
        (
            "fastapi route 404",
            plain(404, r#"{"detail":"Not Found"}"#),
            "",
            true,
        ),
        (
            "problem details route 404",
            plain(404, r#"{"title":"Not Found","status":404}"#),
            "",
            true,
        ),
        (
            "nested generic route 404",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"Not Found"}}"#,
            ),
            "",
            true,
        ),
        (
            "generic model api route 404",
            plain(
                404,
                r#"{"type":"not_found_error","title":"Model API","detail":"Not Found"}"#,
            ),
            "",
            true,
        ),
        (
            "generic model metadata route 404",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"model metadata route not found"}}"#,
            ),
            "",
            true,
        ),
        (
            "generic model provider 404",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"model provider was not found"}}"#,
            ),
            "",
            true,
        ),
        (
            "generic route with misleading metadata",
            plain(
                404,
                r#"{"message":"Not Found","request_id":"model_not_found"}"#,
            ),
            "",
            true,
        ),
        (
            "express count route 404",
            plain(404, "Cannot POST /v1/messages/count_tokens"),
            "",
            true,
        ),
        (
            "html route 404",
            plain(404, "<html><title>404 Not Found</title></html>"),
            "",
            true,
        ),
        (
            "structured model 404",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"model claude-missing was not found"}}"#,
            ),
            "",
            false,
        ),
        (
            "anthropic exact model reference",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"model: claude-missing"}}"#,
            ),
            "",
            false,
        ),
        (
            "anthropic model reference with thinking suffix",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"model: claude-missing"}}"#,
            ),
            "claude-missing(high)",
            false,
        ),
        (
            "requested model does not exist",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"The requested model does not exist"}}"#,
            ),
            "",
            false,
        ),
        (
            "requested quoted model could not be found",
            plain(
                404,
                r#"{"error":{"type":"not_found_error","message":"The requested model 'foo' could not be found"}}"#,
            ),
            "foo",
            false,
        ),
        (
            "problem details model type uri",
            plain(
                404,
                r#"{"type":"https://example.com/problems/model-not-found","title":"Not Found","status":404}"#,
            ),
            "",
            false,
        ),
        (
            "structured model error string",
            plain(404, r#"{"error":"model claude-missing does not exist"}"#),
            "",
            false,
        ),
        (
            "model code with generic message",
            plain(
                404,
                r#"{"message":"Not Found","code":"model_not_found","model":"claude-missing"}"#,
            ),
            "",
            false,
        ),
        (
            "typed model not found code",
            typed("model_not_found", 404, "Not Found"),
            "",
            false,
        ),
        (
            "typed wrapper with structured model code",
            typed(
                "not_found",
                404,
                r#"{"error":{"code":"model_not_found","message":"Not Found"}}"#,
            ),
            "",
            false,
        ),
        (
            "outer generic inner model 404",
            plain(
                404,
                r#"{"message":"Not Found","error":{"type":"not_found_error","message":"model claude-missing does not exist"}}"#,
            ),
            "",
            false,
        ),
        (
            "unstructured model text",
            plain(404, "model claude-missing was not found"),
            "",
            true,
        ),
        ("non 404", plain(500, "404 page not found"), "", false),
    ];
    for (name, err, model, want) in cases {
        let model = if model.is_empty() {
            "claude-missing"
        } else {
            model
        };
        assert_eq!(
            classify::is_count_tokens_endpoint_not_found_error(ErrView::Auth(&err), model),
            want,
            "{name}"
        );
        // The manager asks this of the executor's error; a code-less case
        // must read the same that way.
        if err.code.is_empty() {
            let exec = ExecError::upstream(err.http_status, &err.message);
            assert_eq!(
                classify::is_count_tokens_endpoint_not_found_error(ErrView::Exec(&exec), model),
                want,
                "{name} (executor error)"
            );
        }
    }

    // Upstream's "wrapped structured model code" and "joined structured
    // model code" cases wrap this request-scoped status error; Rust errors
    // don't wrap, so the inner error is checked on its own.
    let inner = request_scoped_error(
        404,
        r#"{"error":{"code":"model_not_found","message":"Not Found"}}"#,
    );
    for name in [
        "wrapped structured model code",
        "joined structured model code",
    ] {
        assert!(
            !classify::is_count_tokens_endpoint_not_found_error(
                ErrView::Exec(&inner),
                "claude-missing"
            ),
            "{name}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_generic_route_not_found_still_suspends_model() {
    let h = Harness::new(Settings::default());
    let id = "messages-route-not-found-auth";
    let executor = fallback_executor(
        "claude",
        FailOn::Execute,
        &[(id, ExecError::upstream(404, "404 page not found"))],
    );
    h.executor(&executor);
    let model = "messages-route-not-found-model";
    h.add(auth(id, "claude"), &[model]);

    run(&h, Kind::Execute, &["claude"], model, options())
        .await
        .expect_err("want the messages route 404");

    let state = model_state(&h, id, model);
    assert!(
        state
            .as_ref()
            .is_some_and(|s| s.unavailable && s.next_retry_after.is_some()),
        "an ordinary messages 404 must suspend the model, got {state:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_record_result_availability_neutral_skips_scheduler_update() {
    let h = Harness::new(Settings::default());
    let id = "availability-neutral-auth";
    let model = "availability-neutral-model";
    // Unlike upstream, the model is registered, so a result that changed its
    // availability would publish it.
    let before = h.add(auth(id, "claude"), &[model]);
    let published = h.models.published().len();

    h.manager.record_availability_neutral_result(&failed(
        id,
        "claude",
        model,
        failure(404, "404 page not found"),
    ));

    // Upstream also checks an unchanged scheduler snapshot. The scheduler
    // index isn't ported; the result must leave availability alone and
    // publish nothing.
    let updated = h.get(id);
    assert_eq!(updated.failed, 1, "recorded failures");
    assert_eq!(updated.status, before.status);
    assert!(!updated.unavailable);
    assert_eq!(updated.next_retry_after, None);
    assert!(
        updated.model_states.is_empty(),
        "{:?}",
        updated.model_states
    );
    assert_eq!(updated.last_error, None);
    assert_eq!(
        h.models.published().len(),
        published,
        "published availability"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_request_scoped_not_found_stops_retry_without_suspending_auth() {
    let h = Harness::new(Settings::default());
    let executor = fallback_executor(
        "openai",
        FailOn::Execute,
        &[(
            "aa-bad-auth",
            ExecError::upstream(404, REQUEST_SCOPED_NOT_FOUND_MESSAGE),
        )],
    );
    h.executor(&executor);
    let model = "gpt-4.1";
    h.add(auth("aa-bad-auth", "openai"), &[model]);
    h.add(auth("bb-good-auth", "openai"), &[model]);

    let err = run(&h, Kind::Execute, &["openai"], model, options())
        .await
        .expect_err("want the request-scoped not-found error");
    // Upstream checks the executor's own error comes back.
    assert_eq!(err.kind, ErrorKind::Upstream, "{err:?}");
    assert_eq!(err.http_status(), 404);
    assert_eq!(err.message, REQUEST_SCOPED_NOT_FOUND_MESSAGE);
    assert_eq!(executor.ids(Kind::Execute), ["aa-bad-auth"]);

    let bad = h.get("aa-bad-auth");
    assert!(
        !bad.unavailable,
        "a request-scoped 404 keeps the bad auth available"
    );
    assert_eq!(
        bad.next_retry_after, None,
        "bad auth cooldown must stay unset"
    );
    assert!(
        !bad.model_states.contains_key(model),
        "no bad auth model cooldown state, got {:?}",
        bad.model_states.get(model)
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_request_fault_body_does_not_cooldown_model_or_auth() {
    let h = Harness::new(Settings::default());
    let id = "auth-request-fault";
    h.add(auth(id, "deepseek"), &[]);
    let model = "deepseek-chat";
    let request_fault =
        r#"{"error":{"message":"Invalid request parameter","type":"invalid_request_error"}}"#;
    let keeps_available = |what: &str| {
        let updated = h.get(id);
        assert!(!updated.unavailable, "{what}: auth must stay available");
        assert_eq!(
            updated.next_retry_after, None,
            "{what}: auth cooldown must stay unset"
        );
    };

    // An SDK consumer reports a 401 request-fault body without the
    // request_scoped code.
    h.manager
        .mark_result(&failed(id, "deepseek", model, failure(401, request_fault)));
    keeps_available("request-fault 401");
    let state = model_state(&h, id, model);
    assert!(
        state
            .as_ref()
            .is_none_or(|s| !s.unavailable && s.next_retry_after.is_none()),
        "request-fault 401 must not cool the model down, got {state:?}"
    );

    // Upstream's NewRequestScopedError and MarkRequestScoped give an error
    // with the request_scoped code.
    let explicit = AuthError {
        code: CODE_REQUEST_SCOPED.to_owned(),
        ..failure(401, "explicit request fault")
    };
    assert!(is_request_scoped_error(ErrView::Auth(&explicit)));
    assert_eq!(explicit.code, "request_scoped");
    let custom = AuthError {
        code: CODE_REQUEST_SCOPED.to_owned(),
        ..failure(401, "custom fault")
    };
    assert!(is_request_scoped_error(ErrView::Auth(&custom)));
    assert_eq!(custom.code, "request_scoped");

    h.manager
        .mark_result(&failed(id, "deepseek", model, explicit));
    keeps_available("explicit request-scoped error");
    h.manager
        .mark_result(&failed(id, "deepseek", model, custom));
    keeps_available("MarkRequestScoped error");

    // A custom code with a request-fault body.
    h.manager.mark_result(&failed(
        id,
        "deepseek",
        model,
        AuthError {
            code: "custom_upstream_code".into(),
            ..failure(401, request_fault)
        },
    ));
    keeps_available("custom code with a request-fault body");

    // A request fault for the whole credential (no model) must not cool it
    // down either.
    let empty_model = "auth-empty-model";
    h.add(auth(empty_model, "deepseek"), &[]);
    h.manager.mark_result(&failed(
        empty_model,
        "deepseek",
        "",
        failure(401, request_fault),
    ));
    let updated = h.get(empty_model);
    assert!(
        !updated.unavailable && updated.next_retry_after.is_none(),
        "an auth-level request-fault 401 must keep the auth available"
    );

    // A real authentication error still cools down.
    let real_fail = "auth-real-fail";
    h.add(auth(real_fail, "deepseek"), &[]);
    h.manager.mark_result(&failed(
        real_fail,
        "deepseek",
        model,
        failure(
            401,
            r#"{"error":{"message":"Authentication Fails, Your api key is invalid","type":"authentication_error"}}"#,
        ),
    ));
    let updated = h.get(real_fail);
    assert!(
        updated.unavailable,
        "a real 401 must mark the auth unavailable"
    );
    assert!(
        updated.next_retry_after.is_some(),
        "a real 401 must set the auth cooldown"
    );
    let state = updated.model_states.get(model);
    assert!(
        state.is_some_and(|s| s.unavailable && s.next_retry_after.is_some()),
        "a real 401 must set the model cooldown, got {state:?}"
    );
}
