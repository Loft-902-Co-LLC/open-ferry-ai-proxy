// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_cooldown_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Cooldowns, and the state they leave, in the credential list.
//!
//! Deviations from upstream:
//! - `TestListAuthFilesCooldownsSnapshot` checks that a listing leaves the
//!   stored credentials alone by their pointers rather than by
//!   `reflect.DeepEqual`.
//! - `TestListAuthFilesCooldownsCredentialKindsAndFilters` drops its
//!   plugin-virtual credential: the plugin host isn't ported.
//! - `TestListAuthFilesCooldownsUnknown` is dropped: its cases, a disk
//!   listing without a manager and Home mode, aren't ported, and this
//!   port's `cooldowns` is never `null`.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{Duration, Utc};
use open_ferry_core::auth::{Auth, AuthError, ModelState, QuotaState, Status, Timestamp};
use serde_json::{Value, json};

use super::{Api, object, time};

/// A runtime-only credential with `id`, `index` and `file_name`.
fn runtime(id: &str, index: &str, file_name: &str, provider: &str) -> Auth {
    let mut auth = super::runtime_auth(id);
    auth.index = index.into();
    auth.file_name = file_name.into();
    auth.provider = provider.into();
    auth
}

fn error(status: u16, message: &str) -> Option<AuthError> {
    Some(AuthError {
        http_status: status,
        message: message.into(),
        ..AuthError::default()
    })
}

fn quota(reason: &str, next: Timestamp) -> QuotaState {
    QuotaState {
        exceeded: true,
        reason: reason.into(),
        next_recover_at: Some(next),
        backoff_level: 0,
        ..QuotaState::default()
    }
}

/// `quota` with a snapshot taken at `at`.
fn with_observation(quota: QuotaState, at: Timestamp) -> QuotaState {
    QuotaState {
        observed_at: Some(at),
        signals: BTreeMap::from([("x-codex-primary-used-percent".into(), "90".into())]),
        ..quota
    }
}

fn models<const N: usize>(states: [(&str, ModelState); N]) -> BTreeMap<String, ModelState> {
    states
        .into_iter()
        .map(|(key, state)| (key.to_owned(), state))
        .collect()
}

/// Token metadata whose access token expires at `expired`.
fn token(access_token: &str, expired: Timestamp) -> serde_json::Map<String, Value> {
    let expired = expired.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    match json!({ "type": "codex", "access_token": access_token, "expired": expired }) {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

/// The list, after checking its `observed_at` is a UTC time.
async fn list(api: &Api, query: &str) -> (Timestamp, Vec<Value>) {
    let mut payload = api.list(query).await;
    let observed = payload["observed_at"].as_str().unwrap().to_owned();
    assert!(observed.ends_with('Z'), "{observed}");
    let observed_at = time(&payload["observed_at"]);
    let Value::Array(files) = payload["files"].take() else {
        panic!("files isn't an array");
    };
    (observed_at, files)
}

/// The one credential listed under `name`.
async fn only(api: &Api, name: &str) -> Value {
    let (_, mut files) = list(api, &format!("?name={name}")).await;
    assert_eq!(files.len(), 1, "{files:?}");
    files.remove(0)
}

/// `status`, `status_message` and `unavailable`.
fn state(file: &Value) -> (&str, &str, bool) {
    (
        file["status"].as_str().unwrap(),
        file["status_message"].as_str().unwrap(),
        file["unavailable"].as_bool().unwrap(),
    )
}

#[tokio::test]
async fn list_auth_files_cooldowns_snapshot() {
    let api = Api::new();
    let now = Utc::now();
    let next = now + Duration::hours(1);
    for id in ["a", "b"] {
        let mut auth = runtime(id, &format!("index-{id}"), "", "codex");
        auth.status = Status::Error;
        auth.unavailable = true;
        auth.next_retry_after = Some(next);
        auth.quota = with_observation(quota("quota", next), now);
        auth.model_states = models([
            (
                "model-a",
                ModelState {
                    unavailable: true,
                    next_retry_after: Some(next),
                    quota: with_observation(
                        QuotaState {
                            backoff_level: 6,
                            ..quota("quota", next)
                        },
                        now,
                    ),
                    last_error: error(429, "private upstream body"),
                    ..ModelState::default()
                },
            ),
            (
                "expired",
                ModelState {
                    status: Status::Error,
                    unavailable: true,
                    next_retry_after: Some(now - Duration::hours(1)),
                    quota: QuotaState {
                        backoff_level: 9,
                        ..quota("", now - Duration::hours(1))
                    },
                    ..ModelState::default()
                },
            ),
        ]);
        api.register(auth);
    }
    let before_a = api.manager.get("a").unwrap();
    let before_b = api.manager.get("b").unwrap();

    for _ in 0..2 {
        let (observed_at, files) = list(&api, "").await;
        assert_eq!(files.len(), 2);
        for (file, id) in files.iter().zip(["a", "b"]) {
            assert_eq!(file["id"], json!(id));
            assert_eq!(file["auth_index"], json!(format!("index-{id}")));
            assert_eq!(file["status"], json!("error"));
            assert_eq!(file["unavailable"], json!(true));
            assert_eq!(time(&file["next_retry_after"]), next);

            let views = file["cooldowns"].as_array().unwrap();
            assert_eq!(views.len(), 1, "{views:?}");
            let view = object(&views[0]);
            assert_eq!(view["scope"], json!("model"));
            assert_eq!(view["model_key"], json!("model-a"));
            assert_eq!(view["reason"], json!("quota"));
            assert_eq!(view["http_status"], json!(429));
            assert_eq!(view["backoff_level"], json!(6));
            let remaining = next - observed_at;
            let mut want = remaining.num_seconds();
            if remaining > Duration::seconds(want) {
                want += 1;
            }
            assert_eq!(view["remaining_seconds"], json!(want));
            assert_eq!(time(&view["retry_at"]), next);
            assert_eq!(view.len(), 7, "{view:?}");

            for quota in [&file["quota"], &file["model_quotas"]["model-a"]] {
                let quota = object(quota);
                assert_eq!(quota.len(), 2, "quota observation changed: {quota:?}");
                assert_eq!(
                    quota["signals"],
                    json!({ "x-codex-primary-used-percent": "90" })
                );
                assert_eq!(time(&quota["observed_at"]), now);
            }
        }
    }
    let after_a = api.manager.get("a").unwrap();
    let after_b = api.manager.get("b").unwrap();
    assert!(Arc::ptr_eq(&before_a, &after_a), "GET mutated auth state");
    assert!(Arc::ptr_eq(&before_b, &after_b), "GET mutated auth state");
}

#[tokio::test]
async fn list_auth_files_cooldowns_credential_kinds_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::new();
    for id in ["file", "runtime"] {
        let mut auth = super::file_auth(dir.path(), id, "shared.json", r#"{"type":"codex"}"#);
        auth.index = format!("index-{id}");
        if id == "runtime" {
            auth.attributes = BTreeMap::from([("runtime_only".into(), "true".into())]);
        }
        api.register(auth);
    }

    let (_, files) = list(&api, "?name=shared.json").await;
    assert_eq!(files.len(), 2, "{files:?}");
    for file in &files {
        assert_eq!(file["cooldowns"], json!([]));
        let index = file["auth_index"].as_str().unwrap();
        let (_, filtered) = list(&api, &format!("?name=shared.json&auth_index={index}")).await;
        assert_eq!(filtered.len(), 1, "{filtered:?}");
        assert_eq!(filtered[0]["id"], file["id"]);
        assert_eq!(filtered[0]["cooldowns"], json!([]));
    }
    let (_, missing) = list(&api, "?auth_index=missing").await;
    assert!(missing.is_empty(), "unknown index matched");
}

#[tokio::test]
async fn list_auth_files_expired_cooldown_reconciled_to_active() {
    let api = Api::new();
    let expired = Utc::now() - Duration::minutes(10);

    // A credential-wide cooldown that has run out, as after Codex quota
    // exhaustion.
    let mut auth = runtime(
        "codex-expired",
        "idx-codex-expired",
        "codex-expired.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "credential_quota".into();
    auth.unavailable = true;
    auth.next_retry_after = Some(expired);
    auth.quota = quota("credential_quota", expired);
    api.register(auth);

    // A model cooldown that has run out, on the only model.
    let mut auth = runtime(
        "model-expired",
        "idx-model-expired",
        "model-expired.json",
        "claude",
    );
    auth.status = Status::Error;
    auth.status_message = "rate limit exceeded".into();
    auth.unavailable = true;
    auth.next_retry_after = Some(expired);
    auth.model_states = models([(
        "claude-3-5-sonnet",
        ModelState {
            status: Status::Error,
            status_message: "rate limit exceeded".into(),
            unavailable: true,
            next_retry_after: Some(expired),
            ..ModelState::default()
        },
    )]);
    api.register(auth);

    let (_, files) = list(&api, "").await;
    assert_eq!(files.len(), 2);
    for file in &files {
        assert_eq!(state(file), ("active", "", false), "{file}");
        assert!(file.get("next_retry_after").is_none(), "{file}");
        assert_eq!(file["cooldowns"], json!([]), "{file}");
    }
}

#[tokio::test]
async fn list_auth_files_expired_cooldown_with_subsequent_token_failure() {
    let api = Api::new();
    let now = Utc::now();
    let mut auth = runtime(
        "codex-token-expired",
        "idx-codex-token-expired",
        "codex-token-expired.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "token expired".into();
    auth.unavailable = true;
    auth.next_retry_after = Some(now - Duration::minutes(10));
    auth.metadata = token("expired-access-token", now - Duration::minutes(5));
    api.register(auth);

    let file = only(&api, "codex-token-expired.json").await;
    assert_eq!(state(&file), ("error", "token expired", true));
    assert!(file.get("next_retry_after").is_none(), "{file}");
}

#[tokio::test]
async fn list_auth_files_partial_model_cooldown_with_auth_failure() {
    let api = Api::new();
    let mut auth = runtime(
        "auth-unauthorized-partial-model",
        "idx-auth-unauthorized",
        "auth-unauthorized.json",
        "claude",
    );
    auth.status = Status::Error;
    auth.status_message = "unauthorized".into();
    auth.unavailable = true;
    auth.last_error = error(401, "unauthorized");
    auth.model_states = models([
        (
            "model-cool",
            ModelState {
                status: Status::Error,
                unavailable: true,
                next_retry_after: Some(Utc::now() + Duration::minutes(10)),
                ..ModelState::default()
            },
        ),
        (
            "model-free",
            ModelState {
                status: Status::Active,
                ..ModelState::default()
            },
        ),
    ]);
    api.register(auth);

    let file = only(&api, "auth-unauthorized.json").await;
    assert_eq!(state(&file), ("error", "unauthorized", true));
}

#[tokio::test]
async fn list_auth_files_unexpired_token_with_refresh_401_no_cooldown() {
    let api = Api::new();
    let now = Utc::now();
    let mut auth = runtime(
        "auth-valid-token-refresh-401",
        "idx-valid-token",
        "auth-valid-token.json",
        "codex",
    );
    auth.status = Status::Active;
    auth.next_refresh_after = Some(now + Duration::minutes(5));
    auth.last_error = error(401, "401 unauthorized on refresh");
    auth.metadata = token("valid-future-access-token", now + Duration::hours(48));
    api.register(auth);

    let file = only(&api, "auth-valid-token.json").await;
    assert_eq!(file["status"], json!("active"));
    assert_eq!(file["unavailable"], json!(false));
}

#[tokio::test]
async fn list_auth_files_unexpired_token_with_refresh_401_expired_cooldown() {
    let api = Api::new();
    let now = Utc::now();
    let expired = now - Duration::minutes(10);
    let mut auth = runtime(
        "auth-valid-token-expired-cooldown",
        "idx-valid-token-exp-cool",
        "auth-valid-token-exp-cool.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "credential_quota".into();
    auth.unavailable = true;
    auth.next_retry_after = Some(expired);
    auth.next_refresh_after = Some(now + Duration::minutes(5));
    auth.last_error = error(401, "401 unauthorized on refresh");
    auth.quota = quota("credential_quota", expired);
    auth.metadata = token("valid-future-access-token", now + Duration::hours(48));
    api.register(auth);

    let file = only(&api, "auth-valid-token-exp-cool.json").await;
    assert_eq!(state(&file), ("active", "", false));
    assert!(file.get("next_retry_after").is_none(), "{file}");
}

#[tokio::test]
async fn list_auth_files_model_level_403_cooldown_expired() {
    let api = Api::new();
    let expired = Utc::now() - Duration::minutes(10);
    let mut auth = runtime(
        "auth-model-403-expired",
        "idx-model-403-exp",
        "auth-model-403-exp.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "forbidden".into();
    auth.unavailable = true;
    auth.next_retry_after = Some(expired);
    auth.last_error = error(403, "forbidden");
    auth.model_states = models([(
        "model-a",
        ModelState {
            status: Status::Error,
            status_message: "forbidden".into(),
            unavailable: true,
            next_retry_after: Some(expired),
            last_error: error(403, "forbidden"),
            ..ModelState::default()
        },
    )]);
    api.register(auth);

    let file = only(&api, "auth-model-403-exp.json").await;
    assert_eq!(state(&file), ("active", "", false));
    assert!(file.get("next_retry_after").is_none(), "{file}");
}

#[tokio::test]
async fn list_auth_files_single_model_403_cooling_other_model_active() {
    let api = Api::new();
    let mut auth = runtime(
        "auth-single-403-other-active",
        "idx-single-403",
        "auth-single-403.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "forbidden".into();
    auth.last_error = error(403, "forbidden");
    auth.model_states = models([
        (
            "model-forbidden",
            ModelState {
                status: Status::Error,
                status_message: "forbidden".into(),
                unavailable: true,
                next_retry_after: Some(Utc::now() + Duration::minutes(30)),
                last_error: error(403, "forbidden"),
                ..ModelState::default()
            },
        ),
        (
            "model-working",
            ModelState {
                status: Status::Active,
                ..ModelState::default()
            },
        ),
    ]);
    api.register(auth);

    let file = only(&api, "auth-single-403.json").await;
    assert_eq!(file["status"], json!("active"));
    assert_eq!(file["unavailable"], json!(false));
}

#[tokio::test]
async fn list_auth_files_expired_cooldown_expired_token_in_flight_refresh() {
    let api = Api::new();
    let now = Utc::now();
    let expired = now - Duration::minutes(10);
    let mut auth = runtime(
        "auth-expired-cooldown-expired-token",
        "idx-exp-cool-exp-tok",
        "auth-exp-cool-exp-tok.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "credential_quota".into();
    auth.unavailable = true;
    auth.next_retry_after = Some(expired);
    auth.next_refresh_after = Some(now + Duration::minutes(1));
    auth.quota = quota("credential_quota", expired);
    auth.metadata = token("expired-access-token", now - Duration::minutes(5));
    api.register(auth);

    let file = only(&api, "auth-exp-cool-exp-tok.json").await;
    assert_eq!(state(&file), ("error", "credential_quota", true));
}

#[tokio::test]
async fn list_auth_files_active_auth_inactive_future_timestamp() {
    let api = Api::new();
    let mut auth = runtime(
        "auth-active-inactive-future-timestamp",
        "idx-active-inactive-future",
        "auth-active-future.json",
        "codex",
    );
    auth.status = Status::Active;
    auth.next_retry_after = Some(Utc::now() + Duration::hours(1));
    api.register(auth);

    let file = only(&api, "auth-active-future.json").await;
    assert_eq!(file["status"], json!("active"));
    assert_eq!(file["unavailable"], json!(false));
}

#[tokio::test]
async fn list_auth_files_model_cooling_other_model_permanent_blocked() {
    let api = Api::new();
    let mut auth = runtime(
        "auth-all-blocked-cooling-and-perm",
        "idx-all-blocked",
        "auth-all-blocked.json",
        "codex",
    );
    auth.status = Status::Error;
    auth.status_message = "model failure".into();
    auth.unavailable = true;
    auth.model_states = models([
        (
            "model-cooling",
            ModelState {
                status: Status::Error,
                unavailable: true,
                next_retry_after: Some(Utc::now() + Duration::minutes(30)),
                ..ModelState::default()
            },
        ),
        (
            "model-perm-blocked",
            ModelState {
                status: Status::Error,
                unavailable: true,
                ..ModelState::default()
            },
        ),
    ]);
    api.register(auth);

    let file = only(&api, "auth-all-blocked.json").await;
    assert_eq!(file["status"], json!("error"));
    assert_eq!(file["unavailable"], json!(true));
}

#[tokio::test]
async fn list_auth_files_sparse_model_state_blocked_model_does_not_make_auth_unavailable() {
    let api = Api::new();
    let mut auth = runtime(
        "auth-sparse-models",
        "idx-sparse-models",
        "auth-sparse-models.json",
        "codex",
    );
    auth.status = Status::Active;
    auth.model_states = models([(
        "model-a",
        ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(Utc::now() + Duration::minutes(30)),
            ..ModelState::default()
        },
    )]);
    api.register(auth);

    let file = only(&api, "auth-sparse-models.json").await;
    assert_eq!(file["status"], json!("active"));
    assert_eq!(file["unavailable"], json!(false));
}
