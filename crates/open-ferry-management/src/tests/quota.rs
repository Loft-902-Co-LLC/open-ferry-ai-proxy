// Ported from CLIProxyAPI internal/api/handlers/management/quota_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v0/management/reset-quota`.
//!
//! Deviations from upstream: none, but for tests added at the end for the
//! answers upstream's tests don't check.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use http::StatusCode;
use open_ferry_core::auth::{Auth, ModelState, QuotaState, Status};
use serde_json::json;

use super::Api;

const PATH: &str = "/v0/management/reset-quota";

fn exhausted(next: chrono::DateTime<Utc>) -> QuotaState {
    QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(next),
        backoff_level: 2,
    }
}

#[tokio::test]
async fn reset_quota_uses_auth_index() {
    let api = Api::new();
    let next = Utc::now() + Duration::hours(1);
    let auth = Auth {
        id: "reset-auth-id".into(),
        file_name: "reset-auth-file.json".into(),
        provider: "claude".into(),
        status: Status::Error,
        status_message: "quota exhausted".into(),
        unavailable: true,
        next_retry_after: Some(next),
        quota: exhausted(next),
        model_states: BTreeMap::from([(
            "claude-reset-model".to_owned(),
            ModelState {
                status: Status::Error,
                status_message: "quota exhausted".into(),
                unavailable: true,
                next_retry_after: Some(next),
                quota: exhausted(next),
                ..ModelState::default()
            },
        )]),
        ..Auth::default()
    };
    let index = api.register(auth);

    let body = json!({ "auth_index": index }).to_string();
    let payload = api.post(PATH, &body).await.expect(StatusCode::OK);
    assert_eq!(payload["auth_index"], json!(index));

    let updated = api.manager.get("reset-auth-id").expect("auth to remain");
    assert_eq!(updated.status, Status::Active);
    assert_eq!(updated.status_message, "");
    assert!(!updated.unavailable);
    assert_eq!(updated.next_retry_after, None);
    assert_eq!(updated.quota, QuotaState::default());
    let state = &updated.model_states["claude-reset-model"];
    assert_eq!(state.status, Status::Active);
    assert_eq!(state.status_message, "");
    assert!(!state.unavailable);
    assert_eq!(state.next_retry_after, None);
    assert_eq!(state.quota, QuotaState::default());
}

#[tokio::test]
async fn reset_quota_does_not_accept_auth_id_or_file_name() {
    let api = Api::new();
    let index = api.register(Auth {
        id: "reset-auth-id-only".into(),
        file_name: "reset-auth-file-only.json".into(),
        provider: "claude".into(),
        status: Status::Error,
        ..Auth::default()
    });
    assert_ne!(index, "reset-auth-id-only");
    assert_ne!(index, "reset-auth-file-only.json");

    for (name, body, want) in [
        (
            "auth_id field ignored",
            r#"{"auth_id":"reset-auth-id-only"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "id field ignored",
            r#"{"id":"reset-auth-id-only"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "file name is not an index",
            r#"{"auth_index":"reset-auth-file-only.json"}"#,
            StatusCode::NOT_FOUND,
        ),
        (
            "auth id is not an index",
            r#"{"auth_index":"reset-auth-id-only"}"#,
            StatusCode::NOT_FOUND,
        ),
    ] {
        let answer = api.post(PATH, body).await;
        assert_eq!(answer.status, want, "{name}: {}", answer.body);
    }
}

#[tokio::test]
async fn reset_quota_answers_as_upstream() {
    let api = Api::new();
    let index = api.register(Auth {
        id: "reset-auth".into(),
        provider: "claude".into(),
        model_states: BTreeMap::from([
            ("b-model".to_owned(), ModelState::default()),
            ("a-model".to_owned(), ModelState::default()),
        ]),
        ..Auth::default()
    });

    for (body, status, want) in [
        ("", 400, r#"{"error":"invalid request body"}"#),
        ("[]", 400, r#"{"error":"invalid request body"}"#),
        (
            r#"{"auth_index":1}"#,
            400,
            r#"{"error":"invalid request body"}"#,
        ),
        ("{}", 400, r#"{"error":"auth_index is required"}"#),
        (
            r#"{"auth_index":" "}"#,
            400,
            r#"{"error":"auth_index is required"}"#,
        ),
        (
            r#"{"auth_index":"missing"}"#,
            404,
            r#"{"error":"auth not found"}"#,
        ),
    ] {
        let answer = api.post(PATH, body).await;
        assert_eq!(answer.status.as_u16(), status, "{body}");
        assert_eq!(answer.body, want, "{body}");
    }

    // gin.H writes its keys sorted; the index is matched once trimmed.
    let body = format!(r#"{{"AUTH_INDEX":" {index} "}}"#);
    let answer = api.post(PATH, &body).await;
    answer.assert(
        StatusCode::OK,
        &format!(r#"{{"auth_index":"{index}","models":["a-model","b-model"],"status":"ok"}}"#),
    );

    // The v8 alias.
    let body = format!(r#"{{"auth_index":"{index}"}}"#);
    let answer = api
        .post("/v8/management/routing/cooldown/reset", &body)
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
}
