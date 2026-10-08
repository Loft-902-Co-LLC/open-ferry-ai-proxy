// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_unauthorized_refresh_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A 401 refreshes the credential and retries it once before falling back
//! to the next one; a credential without a refresh token falls back
//! straight away. When the refresh token is refused too, the credential
//! stays out of rotation, whatever results still come in, until a forced
//! refresh works.
//!
//! Deviations from upstream:
//! - `manager_concurrent_unauthorized_barrier_execution_preserves_terminal_state`:
//!   the two calls meet at a call delay in paused Tokio time, where upstream
//!   holds them on a channel until both reached the credential. Upstream's
//!   background `refreshAuth` is `refresh_at_epoch(id, "", 0)`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta};
use http::{HeaderMap, HeaderValue};
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, AuthError, Status, Timestamp};
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::credential::access_token;
use crate::manager::{CallResult, RoutingStrategy, Settings, has_unauthorized_auth_failure, lock};

pub(super) const MODEL: &str = "gpt-5.5";
pub(super) const PRIMARY: &str = "aa-primary";
pub(super) const BACKUP: &str = "bb-backup";
const INVALIDATED: &str =
    "Your authentication token has been invalidated. Please try signing in again.";
/// The refresh failure upstream's tests use for a refused refresh token.
const INVALID_GRANT: &str = r#"token refresh failed with status 400: {"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}"#;

/// The mutable state of upstream's `unauthorizedRefreshExecutor`.
#[derive(Default)]
pub(super) struct Tokens {
    pub(super) invalid: HashSet<String>,
    pub(super) refresh_fail: bool,
    pub(super) refresh_err: Option<ExecError>,
    pub(super) refresh_tokens: HashMap<String, String>,
}

/// A refresh failure without a status, as upstream's `errors.New`.
pub(super) fn plain_error(message: &str) -> ExecError {
    ExecError::new(ErrorKind::Upstream, message)
}

/// A time as RFC 3339, as Go's `time.RFC3339` writes it.
fn rfc3339(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Upstream's `unauthorizedRefreshExecutor`: a call with an invalidated
/// token gets a 401, any other gets `<auth id>:<token>`; a refresh fails
/// with `refresh_err` or a 401, or hands out the next token.
fn unauthorized_refresh_executor(tokens: &Arc<Mutex<Tokens>>) -> Arc<FakeExecutor> {
    let calls = tokens.clone();
    let executor = FakeExecutor::with("codex", move |call| {
        let token = access_token(&call.auth);
        if lock(&calls).invalid.contains(&token) {
            return Reply::status(401, INVALIDATED);
        }
        let body = format!("{}:{token}", call.auth_id);
        match call.kind {
            Kind::Stream => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "X-Auth",
                    HeaderValue::from_str(&call.auth_id).expect("header value"),
                );
                Reply::Stream {
                    headers,
                    chunks: vec![Ok(body.into())],
                }
            }
            _ => Reply::ok(body),
        }
    });
    let refreshes = tokens.clone();
    executor.set_refresh(move |auth: &Auth| {
        let tokens = lock(&refreshes);
        if let Some(err) = &tokens.refresh_err {
            return Err(err.clone());
        }
        if tokens.refresh_fail {
            return Err(ExecError::upstream(401, "refresh token invalid"));
        }
        let next = tokens
            .refresh_tokens
            .get(&auth.id)
            .cloned()
            .unwrap_or_else(|| "refreshed-access-token".into());
        let mut auth = auth.clone();
        auth.metadata.insert("access_token".into(), json!(next));
        Ok(auth)
    });
    executor
}

/// Upstream's `newUnauthorizedRefreshFixture`.
pub(super) fn new_unauthorized_refresh_fixture(
    refresh_fail: bool,
) -> (Harness, Arc<FakeExecutor>, Arc<Mutex<Tokens>>) {
    let tokens = Arc::new(Mutex::new(Tokens {
        invalid: HashSet::from(["stale-access-token".to_owned()]),
        refresh_fail,
        refresh_tokens: HashMap::from([(PRIMARY.to_owned(), "fresh-access-token".to_owned())]),
        ..Tokens::default()
    }));
    let executor = unauthorized_refresh_executor(&tokens);
    let h = Harness::new(Settings::default());
    h.executor(&executor);
    h.add(
        auth_with_metadata(
            PRIMARY,
            "codex",
            json!({"access_token": "stale-access-token", "refresh_token": "primary-refresh-token"}),
        ),
        &[MODEL],
    );
    h.add(
        auth_with_metadata(
            BACKUP,
            "codex",
            json!({"access_token": "backup-access-token", "refresh_token": "backup-refresh-token"}),
        ),
        &[MODEL],
    );
    (h, executor, tokens)
}

#[tokio::test(start_paused = true)]
async fn manager_execute_unauthorized_refreshes_current_auth_before_fallback() {
    let (h, executor, _tokens) = new_unauthorized_refresh_fixture(false);

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("want success on refreshed primary");
    assert_eq!(
        &resp.payload[..],
        format!("{PRIMARY}:fresh-access-token").as_bytes(),
        "want refreshed primary response"
    );

    assert_eq!(executor.refresh_count(), 1, "refresh calls");
    assert_eq!(
        executor.ids(Kind::Execute),
        [PRIMARY, PRIMARY],
        "backup auth should not be used when refresh recovers primary"
    );

    let updated = h.get(PRIMARY);
    assert_eq!(access_token(&updated), "fresh-access-token");
    if let Some(state) = updated.model_states.get(MODEL) {
        assert!(
            !state.unavailable,
            "primary model should not remain suspended after successful refresh retry"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_execute_stream_unauthorized_refreshes_current_auth_before_fallback() {
    let (h, executor, _tokens) = new_unauthorized_refresh_fixture(false);

    let stream = h
        .manager
        .execute_stream(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("want success on refreshed primary");
    let (chunks, err) = collect(stream).await;
    assert!(err.is_none(), "stream chunk error = {err:?}");
    assert_eq!(
        chunks,
        [format!("{PRIMARY}:fresh-access-token")],
        "want refreshed primary response"
    );

    assert_eq!(executor.refresh_count(), 1, "refresh calls");
    assert_eq!(
        executor.ids(Kind::Stream),
        [PRIMARY, PRIMARY],
        "backup auth should not be used when refresh recovers primary"
    );
}

/// Gives the primary credential's (rejected) access token an expiry six
/// hours away, as a revoked token has in production.
fn extend_primary_expiry(h: &Harness) {
    let mut updated = (*h.get(PRIMARY)).clone();
    updated.metadata.insert(
        "expired".into(),
        json!(rfc3339(h.now() + TimeDelta::hours(6))),
    );
    h.manager.update(updated).expect("update primary");
}

/// Asserts that `id` is out of rotation for rejected tokens.
fn assert_terminal(h: &Harness, id: &str, when: &str) {
    let auth = h.get(id);
    assert!(
        has_unauthorized_auth_failure(&auth),
        "{when}: expected terminal unauthorized state, got unavailable={} status={:?} \
         next_refresh={:?} next_retry={:?} last_error={:?}",
        auth.unavailable,
        auth.status,
        auth.next_refresh_after,
        auth.next_retry_after,
        auth.last_error
    );
}

/// Runs one call and asserts that the backup credential answered it.
async fn execute_via_backup(h: &Harness, when: &str) {
    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .unwrap_or_else(|err| panic!("{when}: error = {err}, want success via backup"));
    assert_eq!(
        &resp.payload[..],
        format!("{BACKUP}:backup-access-token").as_bytes(),
        "{when}: want backup response"
    );
}

/// How many of the calls from `from` on went to the primary credential.
fn primary_calls_since(executor: &FakeExecutor, from: usize) -> usize {
    executor
        .ids(Kind::Execute)
        .iter()
        .skip(from)
        .filter(|id| *id == PRIMARY)
        .count()
}

#[tokio::test(start_paused = true)]
async fn manager_execute_rejected_token_with_invalid_grant_stops_selecting_auth() {
    let (h, executor, tokens) = new_unauthorized_refresh_fixture(false);
    lock(&tokens).refresh_err = Some(plain_error(INVALID_GRANT));
    extend_primary_expiry(&h);

    for i in 0..2 {
        execute_via_backup(&h, &format!("execute {i}")).await;
    }

    assert_eq!(
        primary_calls_since(&executor, 0),
        1,
        "primary executions; calls = {:?}",
        executor.ids(Kind::Execute)
    );
    assert_eq!(executor.refresh_count(), 1, "refresh calls");
    assert_terminal(&h, PRIMARY, "after refresh failure");
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_in_flight_result_does_not_revive_terminal_unauthorized_auth() {
    let (h, executor, tokens) = new_unauthorized_refresh_fixture(false);
    lock(&tokens).refresh_err = Some(plain_error(INVALID_GRANT));

    // The first call's 401 and refused refresh make the credential terminal.
    execute_via_backup(&h, "first execute").await;
    assert_terminal(&h, PRIMARY, "initially");

    // A call that was already running on the primary fails with a 500.
    h.manager.mark_result(&CallResult {
        auth_id: PRIMARY.into(),
        provider: "codex".into(),
        model: MODEL.into(),
        success: false,
        error: Some(AuthError {
            code: "internal_error".into(),
            message: "internal server error".into(),
            http_status: 500,
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    assert_terminal(&h, PRIMARY, "after in-flight 500");

    // Another one succeeds.
    h.manager.mark_result(&CallResult {
        auth_id: PRIMARY.into(),
        provider: "codex".into(),
        model: MODEL.into(),
        success: true,
        ..CallResult::default()
    });
    assert_terminal(&h, PRIMARY, "after in-flight success");

    // Clearing the cooldowns doesn't bring it back either.
    h.manager.reset_quota(PRIMARY).expect("reset quota");

    let before = executor.ids(Kind::Execute).len();
    execute_via_backup(&h, "second execute").await;
    assert_eq!(
        primary_calls_since(&executor, before),
        0,
        "primary was selected again after in-flight result"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_concurrent_unauthorized_barrier_execution_preserves_terminal_state() {
    let tokens = Arc::new(Mutex::new(Tokens {
        invalid: HashSet::from(["stale-access-token".to_owned()]),
        refresh_err: Some(plain_error(INVALID_GRANT)),
        ..Tokens::default()
    }));
    let executor = unauthorized_refresh_executor(&tokens);
    let h = Harness::new(Settings {
        routing_strategy: RoutingStrategy::FillFirst,
        ..Settings::default()
    });
    h.executor(&executor);
    h.add(
        auth_with_metadata(
            PRIMARY,
            "codex",
            json!({
                "access_token": "stale-access-token",
                "refresh_token": "primary-refresh-token",
                "expired": rfc3339(h.now() + TimeDelta::hours(6)),
            }),
        ),
        &[MODEL],
    );
    h.add(
        auth_with_metadata(
            BACKUP,
            "codex",
            json!({"access_token": "backup-access-token", "refresh_token": "backup-refresh-token"}),
        ),
        &[MODEL],
    );

    // Both calls reach the primary before either gets its 401.
    executor.set_delay(Duration::from_millis(100));
    let codex = providers(&["codex"]);
    let call = || h.manager.execute(&codex, request(MODEL), options());
    let (first, second) = tokio::join!(call(), call());
    for (i, result) in [first, second].into_iter().enumerate() {
        let resp = result.unwrap_or_else(|err| panic!("call {i}: error = {err}"));
        assert_eq!(
            &resp.payload[..],
            format!("{BACKUP}:backup-access-token").as_bytes(),
            "call {i}: want backup response"
        );
    }
    assert_eq!(
        primary_calls_since(&executor, 0),
        2,
        "both calls should have reached the primary"
    );
    assert_eq!(
        executor.refresh_count(),
        1,
        "refresh calls during concurrent execution"
    );
    assert_terminal(&h, PRIMARY, "after concurrent execution");

    // Neither a background refresh nor one after a 401 refreshes it.
    let before = executor.refresh_count();
    let _ = h.manager.refresh_at_epoch(PRIMARY, "", 0).await;
    assert_eq!(
        executor.refresh_count(),
        before,
        "background refresh called the executor on a terminal unauthorized auth"
    );
    let terminal = h.get(PRIMARY);
    let refreshed = h
        .manager
        .try_refresh_after_unauthorized(&terminal, &ExecError::upstream(401, "401"), false)
        .await;
    assert!(
        refreshed.is_none(),
        "a terminal unauthorized auth should not report refreshed"
    );
    assert_eq!(
        executor.refresh_count(),
        before,
        "refresh after a 401 called the executor on a terminal unauthorized auth"
    );

    // A forced refresh does call it.
    let _ = h.manager.force_refresh(PRIMARY).await;
    assert_eq!(executor.refresh_count(), before + 1, "forced refresh calls");

    let calls = executor.ids(Kind::Execute).len();
    execute_via_backup(&h, "third execute").await;
    assert_eq!(
        primary_calls_since(&executor, calls),
        0,
        "primary was selected again after concurrent execution"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_auth_failure_preserves_terminal_unauthorized_state() {
    let (h, executor, tokens) = new_unauthorized_refresh_fixture(false);
    extend_primary_expiry(&h);
    lock(&tokens).refresh_err = Some(plain_error(INVALID_GRANT));

    execute_via_backup(&h, "first execute").await;
    assert_terminal(&h, PRIMARY, "initially");

    // A forced refresh that fails with a 503 leaves it terminal.
    lock(&tokens).refresh_err = Some(plain_error("upstream 503 service unavailable"));
    h.manager
        .force_refresh(PRIMARY)
        .await
        .expect_err("expected the forced refresh to fail");
    assert_terminal(&h, PRIMARY, "after failed forced refresh");
    assert_eq!(
        h.get(PRIMARY).next_refresh_after,
        None,
        "next refresh after a failed forced refresh"
    );
    execute_via_backup(&h, "execute after failed forced refresh").await;

    // A forced refresh that works brings it back.
    {
        let mut tokens = lock(&tokens);
        tokens.refresh_err = None;
        tokens
            .refresh_tokens
            .insert(PRIMARY.to_owned(), "newly-minted-token".to_owned());
    }
    let refreshed = h
        .manager
        .force_refresh(PRIMARY)
        .await
        .expect("forced refresh should work");
    assert_eq!(refreshed.status, Status::Active);
    assert!(!refreshed.unavailable);
    assert!(!has_unauthorized_auth_failure(&refreshed));
    assert_eq!(executor.refresh_count(), 3, "refresh calls");
}

#[tokio::test(start_paused = true)]
async fn manager_execute_unauthorized_refresh_failure_falls_back_to_next_auth() {
    let (h, executor, _tokens) = new_unauthorized_refresh_fixture(true);

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("want success via backup");
    assert_eq!(
        &resp.payload[..],
        format!("{BACKUP}:backup-access-token").as_bytes(),
        "want backup response"
    );

    assert_eq!(executor.refresh_count(), 1, "refresh calls");
    assert_eq!(executor.ids(Kind::Execute), [PRIMARY, BACKUP]);

    let updated = h.get(PRIMARY);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected primary model to be suspended after refresh failure");
    assert!(
        state.unavailable,
        "expected primary model to be suspended after refresh failure"
    );
    assert!(
        state.status_message == "unauthorized"
            || state
                .last_error
                .as_ref()
                .is_some_and(|err| err.http_status == 401),
        "expected unauthorized suspension, got status_message={:?} last_error status={:?}",
        state.status_message,
        state.last_error.as_ref().map(|err| err.http_status)
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_unauthorized_without_refresh_token_does_not_call_refresh() {
    let tokens = Arc::new(Mutex::new(Tokens {
        invalid: HashSet::from(["stale-access-token".to_owned()]),
        ..Tokens::default()
    }));
    let executor = unauthorized_refresh_executor(&tokens);
    let h = Harness::new(Settings::default());
    h.executor(&executor);
    h.add(
        auth_with_metadata(
            "aa-primary-api-key",
            "codex",
            json!({"access_token": "stale-access-token"}),
        ),
        &[MODEL],
    );
    h.add(
        auth_with_metadata(
            "bb-backup-api-key",
            "codex",
            json!({"access_token": "backup-access-token"}),
        ),
        &[MODEL],
    );

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("want success via backup");
    assert_eq!(
        &resp.payload[..],
        b"bb-backup-api-key:backup-access-token",
        "want backup response"
    );
    assert_eq!(
        executor.refresh_count(),
        0,
        "want 0 when no refresh_token is present"
    );
    assert_eq!(
        executor.ids(Kind::Execute),
        ["aa-primary-api-key", "bb-backup-api-key"]
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_unauthorized_refresh_then_retry_still_fails_falls_back_once() {
    let (h, executor, tokens) = new_unauthorized_refresh_fixture(false);
    // The refresh "works" but hands back another invalidated token.
    {
        let mut tokens = lock(&tokens);
        tokens
            .refresh_tokens
            .insert(PRIMARY.to_owned(), "still-invalid-token".to_owned());
        tokens.invalid.insert("still-invalid-token".to_owned());
    }

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(MODEL), options())
        .await
        .expect("want success via backup");
    assert_eq!(
        &resp.payload[..],
        format!("{BACKUP}:backup-access-token").as_bytes(),
        "want backup response"
    );
    assert_eq!(executor.refresh_count(), 1, "want 1 (no refresh loop)");
    assert_eq!(executor.ids(Kind::Execute), [PRIMARY, PRIMARY, BACKUP]);
}
