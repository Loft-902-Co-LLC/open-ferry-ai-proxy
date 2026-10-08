// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_issue6416_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A call's outcome counts only for the tokens or API key it ran with:
//! once they are replaced, a late 429 or 401 from the old ones neither
//! cools down nor takes out the credential, nor drops its session
//! bindings. Replacing anything else keeps the credential version, so the
//! outcome still counts.
//!
//! Deviations from upstream:
//! - The call waits on a delay in paused Tokio time, where upstream holds
//!   it on a channel, and the test replaces the credential meanwhile.
//! - The API key test's executor reads the key, where upstream's reads the
//!   access token only, so its old key really gets the 429.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use serde_json::json;

use super::affinity::{bound, headers, session};
use super::support::*;
use crate::auth::{Auth, AuthError, Status};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::credential::access_token;
use crate::manager::{CallResult, Settings};

const MODEL: &str = "gpt-5";
const HOUR: Duration = Duration::from_secs(60 * 60);
/// How long each call waits before answering, so the test can replace the
/// credential meanwhile.
const CALL_DELAY: Duration = Duration::from_secs(10);

/// The token or API key `auth` calls with.
fn secret(auth: &Auth) -> String {
    let token = access_token(auth);
    if token.is_empty() {
        auth.attributes.get("api_key").cloned().unwrap_or_default()
    } else {
        token
    }
}

/// Upstream's `issue6416TestExecutor`: the old token or API key gets a 429,
/// `invalid-old-token` a 401, and anything else succeeds; a stream with the
/// old token fails after its first chunk.
fn issue6416_executor() -> Arc<FakeExecutor> {
    FakeExecutor::with("codex", |call| {
        let token = secret(&call.auth);
        if call.kind == Kind::Stream {
            let last = if token == "old-access-token" {
                Err(ExecError::upstream(429, "rate limited on old token"))
            } else {
                Ok(Bytes::from_static(b"chunk-2"))
            };
            return Reply::Stream {
                headers: HeaderMap::new(),
                chunks: vec![Ok(Bytes::from_static(b"chunk-1")), last],
            };
        }
        match token.as_str() {
            "old-access-token" | "old-api-key" => {
                Reply::status(429, "rate limited on old credential")
            }
            "invalid-old-token" => Reply::status(401, "unauthorized on old token"),
            _ => Reply::ok(format!("{}:{token}", call.auth_id)),
        }
    })
}

/// A harness with [`issue6416_executor`], whose calls wait [`CALL_DELAY`].
fn harness(settings: Settings, with_store: bool) -> (Harness, Arc<FakeExecutor>) {
    let h = if with_store {
        Harness::with_store(settings)
    } else {
        Harness::new(settings)
    };
    let executor = issue6416_executor();
    executor.set_delay(CALL_DELAY);
    h.executor(&executor);
    (h, executor)
}

fn codex_auth(id: &str, token: &str) -> Auth {
    auth_with_metadata(id, "codex", json!({"access_token": token}))
}

/// Starts a call, waits until the executor has it, then runs `replace`
/// and lets the call finish. Returns whether the call succeeded.
async fn replace_during_call(
    h: &Harness,
    executor: &FakeExecutor,
    stream: bool,
    replace: impl FnOnce(),
) -> bool {
    let manager = h.manager.clone();
    let before = executor.calls().len();
    let call = tokio::spawn(async move {
        let pool = providers(&["codex"]);
        if stream {
            match manager
                .execute_stream(&pool, request(MODEL), options())
                .await
            {
                Ok(stream) => collect(stream).await.1.is_none(),
                Err(_) => false,
            }
        } else {
            manager
                .execute(&pool, request(MODEL), options())
                .await
                .is_ok()
        }
    });
    while executor.calls().len() == before {
        tokio::task::yield_now().await;
    }
    replace();
    call.await.expect("call task")
}

/// Replaces `id`'s metadata with an access token of `token`.
fn replace_token(h: &Harness, id: &str, token: &str) -> Arc<Auth> {
    let mut updated = (*h.get(id)).clone();
    updated.metadata.clear();
    updated.metadata.insert("access_token".into(), json!(token));
    h.manager.update(updated).expect("update").expect("auth")
}

/// Asserts that `id` and its [`MODEL`] are neither cooling down nor out.
fn assert_not_cooled(h: &Harness, id: &str, when: &str) {
    let auth = h.get(id);
    assert!(!auth.unavailable, "{when}: credential marked unavailable");
    assert_ne!(auth.status, Status::Error, "{when}: credential status");
    if let Some(state) = auth.model_states.get(MODEL) {
        assert!(
            !state.quota.exceeded,
            "{when}: model cooled down: {state:?}"
        );
        assert!(!state.unavailable, "{when}: model unavailable: {state:?}");
        assert!(
            state.next_retry_after.is_none_or(|t| t <= h.now()),
            "{when}: model given a retry deadline: {state:?}"
        );
    }
}

/// A 429 on [`MODEL`] from `id`, with `version` and `epoch`.
fn rate_limited(id: &str, version: u64, epoch: u64) -> CallResult {
    CallResult {
        auth_id: id.into(),
        provider: "codex".into(),
        model: MODEL.into(),
        success: false,
        error: Some(AuthError {
            http_status: 429,
            message: "429".into(),
            ..AuthError::default()
        }),
        credential_version: version,
        registration_epoch: epoch,
        ..CallResult::default()
    }
}

fn quota_exceeded(h: &Harness, id: &str) -> bool {
    h.get(id)
        .model_states
        .get(MODEL)
        .is_some_and(|state| state.quota.exceeded)
}

#[tokio::test(start_paused = true)]
async fn late_result_before_credential_update_does_not_cooldown_new_credential() {
    let (h, executor) = harness(Settings::default(), false);
    let id = "auth-issue-6416-exec";
    h.add(codex_auth(id, "old-access-token"), &[MODEL]);

    let ok = replace_during_call(&h, &executor, false, || {
        replace_token(&h, id, "new-access-token");
    })
    .await;
    assert!(!ok, "expected in-flight request to fail with 429");
    assert_not_cooled(&h, id, "after the late 429");
}

#[tokio::test(start_paused = true)]
async fn late_stream_result_before_credential_update_does_not_cooldown_new_credential() {
    let (h, executor) = harness(Settings::default(), false);
    let id = "auth-issue-6416-stream";
    h.add(codex_auth(id, "old-access-token"), &[MODEL]);

    let ok = replace_during_call(&h, &executor, true, || {
        replace_token(&h, id, "new-access-token");
    })
    .await;
    assert!(!ok, "expected the stream to fail after its first chunk");
    assert_not_cooled(&h, id, "after the late stream 429");
}

#[tokio::test(start_paused = true)]
async fn load_initializes_credential_version() {
    let (h, executor) = harness(Settings::default(), true);
    let id = "auth-issue-6416-load";
    // Saved without a version.
    h.store.put(codex_auth(id, "old-access-token"));
    h.models.register(id, &[MODEL]);
    h.manager.load().expect("load");
    assert_eq!(h.get(id).credential_version, 1, "loaded version");

    let mut updated_version = 0;
    replace_during_call(&h, &executor, false, || {
        updated_version = replace_token(&h, id, "new-access-token").credential_version;
    })
    .await;
    assert_eq!(updated_version, 2, "updated version");
    assert_not_cooled(&h, id, "after the late 429");
}

// Not upstream's: a reload keeps the version of a credential whose tokens
// didn't change, and moves it past the old one when they did.
#[tokio::test(start_paused = true)]
async fn load_keeps_or_advances_credential_version() {
    let (h, _executor) = harness(Settings::default(), true);
    let id = "auth-issue-6416-reload";
    h.store.put(codex_auth(id, "token-v1"));
    h.manager.load().expect("load");
    replace_token(&h, id, "token-v2");
    assert_eq!(h.get(id).credential_version, 2);

    h.manager.load().expect("reload");
    assert_eq!(h.get(id).credential_version, 2, "same tokens");

    h.store.put(codex_auth(id, "token-v3"));
    h.manager.load().expect("reload");
    assert_eq!(h.get(id).credential_version, 3, "new tokens");
}

#[tokio::test(start_paused = true)]
async fn late_401_result_before_credential_update_does_not_invalidate_new_credential() {
    let (h, executor) = harness(Settings::default(), false);
    let id = "auth-issue-6416-401";
    let mut initial = codex_auth(id, "invalid-old-token");
    initial.status = Status::Active;
    h.add(initial, &[MODEL]);

    replace_during_call(&h, &executor, false, || {
        replace_token(&h, id, "valid-new-token");
    })
    .await;
    assert_not_cooled(&h, id, "after the late 401");
}

#[tokio::test(start_paused = true)]
async fn api_key_rotation_late_result_does_not_cooldown_new_api_key() {
    let (h, executor) = harness(Settings::default(), false);
    let id = "auth-issue-6416-apikey";
    let mut initial = auth(id, "codex");
    initial
        .attributes
        .insert("api_key".into(), "old-api-key".into());
    let registered = h.add(initial, &[MODEL]);
    assert_eq!(registered.credential_version, 1, "registered version");

    let mut updated_version = 0;
    let ok = replace_during_call(&h, &executor, false, || {
        let mut rotated = (*h.get(id)).clone();
        rotated
            .attributes
            .insert("api_key".into(), "new-api-key".into());
        updated_version = h
            .manager
            .update(rotated)
            .expect("update")
            .expect("auth")
            .credential_version;
    })
    .await;
    assert!(!ok, "the old key's call gets the 429");
    assert_eq!(updated_version, 2, "updated version");
    assert_not_cooled(&h, id, "after the old key's 429");
}

#[tokio::test(start_paused = true)]
async fn direct_mark_result_stale_credential_version_ignored() {
    let (h, _executor) = harness(Settings::default(), false);
    let id = "auth-direct-stale-version";
    let registered = h.add(codex_auth(id, "token-v1"), &[MODEL]);
    assert_eq!(registered.credential_version, 1);
    let updated = replace_token(&h, id, "token-v2");
    assert_eq!(updated.credential_version, 2);
    let epoch = updated.registration_epoch;

    h.manager.mark_result(&rate_limited(id, 1, epoch));
    assert_not_cooled(&h, id, "after a version 1 result");

    // A result that doesn't say its version is stale too, once the
    // credential is past its first.
    h.manager.mark_result(&rate_limited(id, 0, 0));
    assert_not_cooled(&h, id, "after an unversioned result");

    h.manager.mark_result(&rate_limited(id, 2, epoch));
    assert!(
        quota_exceeded(&h, id),
        "a current 429 must cool the model down"
    );
}

// Not upstream's: an unversioned result counts while the credential is
// still at its first version.
#[tokio::test(start_paused = true)]
async fn direct_mark_result_unversioned_counts_at_first_version() {
    let (h, _executor) = harness(Settings::default(), false);
    let id = "auth-direct-first-version";
    h.add(codex_auth(id, "token-v1"), &[MODEL]);
    h.manager.mark_result(&rate_limited(id, 0, 0));
    assert!(quota_exceeded(&h, id));
}

#[tokio::test(start_paused = true)]
async fn direct_mark_result_stale_registration_epoch_ignored() {
    let (h, _executor) = harness(Settings::default(), false);
    let id = "auth-direct-stale-epoch";
    let initial = codex_auth(id, "token-v1");
    let epoch1 = h.add(initial.clone(), &[MODEL]).registration_epoch;

    h.manager.remove(id);
    let epoch2 = h.add(initial, &[MODEL]).registration_epoch;
    assert!(epoch2 > epoch1, "epoch {epoch2} must move past {epoch1}");

    h.manager.mark_result(&rate_limited(id, 0, epoch1));
    assert_not_cooled(&h, id, "after a result from the old registration");
}

#[tokio::test(start_paused = true)]
async fn late_result_before_credential_update_preserves_session_affinity() {
    let settings = Settings {
        session_affinity: true,
        session_affinity_ttl: HOUR,
        ..Settings::default()
    };
    let (h, executor) = harness(settings, false);
    executor.set_delay(Duration::ZERO);
    let id = "auth-session-affinity";
    let mut initial = codex_auth(id, "token-v1");
    initial.status = Status::Active;
    h.add(initial, &[MODEL]);

    let mut opts = options();
    opts.headers = headers(&[("X-Session-Id", "test-session-123")]);
    let key = "mixed::header:test-session-123::gpt-5";
    h.manager
        .execute(&providers(&["codex"]), request(MODEL), opts)
        .await
        .expect("first execute");
    assert_eq!(bound(&h, key).as_deref(), Some(id), "binding");

    let updated = replace_token(&h, id, "token-v2");
    let session = session(&[("X-Session-Id", "test-session-123")], "");
    let stale = rate_limited(id, 1, updated.registration_epoch);
    h.manager.mark_call_result(&stale, Some(&session));
    assert_eq!(
        bound(&h, key).as_deref(),
        Some(id),
        "a stale failure dropped the binding"
    );

    // Not upstream's: a current one drops it.
    let current = rate_limited(id, 2, updated.registration_epoch);
    h.manager.mark_call_result(&current, Some(&session));
    assert_eq!(bound(&h, key), None, "a current failure kept the binding");
}

#[tokio::test(start_paused = true)]
async fn non_secret_update_preserves_version_and_applies_cooldown() {
    let (h, executor) = harness(Settings::default(), false);
    let id = "auth-nonsecret-update";
    let mut initial = codex_auth(id, "old-access-token");
    initial.label = "initial-label".into();
    let registered = h.add(initial, &[MODEL]);
    assert_eq!(registered.credential_version, 1);

    let mut updated_version = 0;
    replace_during_call(&h, &executor, false, || {
        let mut relabeled = (*h.get(id)).clone();
        relabeled.label = "updated-label".into();
        updated_version = h
            .manager
            .update(relabeled)
            .expect("update")
            .expect("auth")
            .credential_version;
    })
    .await;
    assert_eq!(updated_version, 1, "version after a non-secret update");
    assert!(
        quota_exceeded(&h, id),
        "the 429 must cool the model down when the secrets didn't change"
    );
}
