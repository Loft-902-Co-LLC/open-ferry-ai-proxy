// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_unauthorized_refresh_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A 401 refreshes the credential and retries it once before falling back
//! to the next one; a credential without a refresh token falls back
//! straight away.
//!
//! Deviations from upstream:
//! None.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use http::{HeaderMap, HeaderValue};
use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{Dispatcher, ExecError};
use crate::manager::credential::access_token;
use crate::manager::{Settings, lock};

const MODEL: &str = "gpt-5.5";
const PRIMARY: &str = "aa-primary";
const BACKUP: &str = "bb-backup";
const INVALIDATED: &str =
    "Your authentication token has been invalidated. Please try signing in again.";

/// The mutable state of upstream's `unauthorizedRefreshExecutor`.
#[derive(Default)]
struct Tokens {
    invalid: HashSet<String>,
    refresh_fail: bool,
    refresh_tokens: HashMap<String, String>,
}

/// Upstream's `unauthorizedRefreshExecutor`: a call with an invalidated
/// token gets a 401, any other gets `<auth id>:<token>`; a refresh fails
/// with a 401 or hands out the next token.
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
fn new_unauthorized_refresh_fixture(
    refresh_fail: bool,
) -> (Harness, Arc<FakeExecutor>, Arc<Mutex<Tokens>>) {
    let tokens = Arc::new(Mutex::new(Tokens {
        invalid: HashSet::from(["stale-access-token".to_owned()]),
        refresh_fail,
        refresh_tokens: HashMap::from([(PRIMARY.to_owned(), "fresh-access-token".to_owned())]),
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
