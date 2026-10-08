// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_pagination_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Paging the credential list.
//!
//! Deviations from upstream:
//! - `TestListAuthFilesPaginationFromDisk` is dropped: this port has no
//!   disk listing for a missing manager.
//! - `TestListAuthFilesPaginationDefaultsAndCompatibility` also checks the
//!   order of an unpaged list, and
//!   `TestListAuthFilesPaginationRejectsInvalidValues` the error bodies.

use std::path::Path;

use http::StatusCode;
use serde_json::{Value, json};

use super::{Api, file_auth};

fn register_paginated_auth_files(api: &Api, dir: &Path) {
    for (ordinal, name) in [
        "delta.json",
        "Alpha.json",
        "echo.json",
        "Charlie.json",
        "bravo.json",
    ]
    .into_iter()
    .enumerate()
    {
        let mut auth = file_auth(
            dir,
            name,
            name,
            r#"{"type":"codex","email":"user@example.com"}"#,
        );
        auth.index = format!("idx-{name}");
        auth.metadata.insert("ordinal".into(), json!(ordinal));
        api.register(auth);
    }
}

fn names(payload: &Value) -> Vec<&str> {
    payload["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect()
}

/// `total`, `page`, `page_size` and `has_more`.
fn paging(payload: &Value) -> [&Value; 4] {
    [
        &payload["total"],
        &payload["page"],
        &payload["page_size"],
        &payload["has_more"],
    ]
}

#[tokio::test]
async fn list_auth_files_pagination_from_manager() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::new();
    register_paginated_auth_files(&api, dir.path());

    let payload = api.list("?page=2&page_size=2").await;
    assert_eq!(
        paging(&payload),
        [&json!(5), &json!(2), &json!(2), &json!(true)]
    );
    assert_eq!(names(&payload), ["Charlie.json", "delta.json"]);

    let payload = api.list("?page=99&page_size=2").await;
    assert_eq!(
        paging(&payload),
        [&json!(5), &json!(99), &json!(2), &json!(false)]
    );
    assert!(names(&payload).is_empty());
}

#[tokio::test]
async fn list_auth_files_pagination_applies_lookup_filters_before_paging() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::new();
    for id in ["auth-b", "auth-a"] {
        let mut auth = file_auth(dir.path(), id, "shared.json", r#"{"type":"codex"}"#);
        auth.index = format!("idx-{id}");
        api.register(auth);
    }

    let payload = api.list("?name=shared.json&page=2&page_size=1").await;
    assert_eq!(
        paging(&payload),
        [&json!(2), &json!(2), &json!(1), &json!(false)]
    );
    let files = payload["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["id"], json!("auth-b"));
}

#[tokio::test]
async fn list_auth_files_pagination_defaults_and_compatibility() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::new();
    register_paginated_auth_files(&api, dir.path());

    let payload = api.list("?page=1").await;
    assert_eq!(
        paging(&payload),
        [&json!(5), &json!(1), &json!(50), &json!(false)]
    );
    assert_eq!(names(&payload).len(), 5);

    let payload = api.list("").await;
    let keys: Vec<_> = payload.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["files", "observed_at"]);
    assert_eq!(
        names(&payload),
        [
            "Alpha.json",
            "bravo.json",
            "Charlie.json",
            "delta.json",
            "echo.json"
        ]
    );
}

#[tokio::test]
async fn list_auth_files_pagination_rejects_invalid_values() {
    let api = Api::new();
    for (query, message) in [
        ("?page=0&page_size=10", "page must be a positive integer"),
        (
            "?page=invalid&page_size=10",
            "page must be a positive integer",
        ),
        (
            "?page=1&page_size=0",
            "page_size must be a positive integer",
        ),
        (
            "?page=1&page_size=invalid",
            "page_size must be a positive integer",
        ),
    ] {
        let answer = api.get(&format!("/v0/management/auth-files{query}")).await;
        answer.assert(
            StatusCode::BAD_REQUEST,
            &format!(r#"{{"error":"{message}"}}"#),
        );
    }
}
