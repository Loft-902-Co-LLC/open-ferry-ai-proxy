// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_filter_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Narrowing the credential list by name and index.
//!
//! Deviations from upstream:
//! - `TestListAuthFilesFromDiskFiltersByNameAndRejectsAuthIndex` is
//!   dropped: this port has no disk listing for a missing manager.
//! - `TestPatchAuthFileStatusVerifiesAuthIndex` and
//!   `TestPatchAuthFileStatusRejectsMismatchedAuthIndex` turn credentials
//!   on and off through the router, over an [`AuthDir`]: the status route
//!   needs a credential store.
//! - `TestAuthFileLookupAndEntryBuildConcurrentEnsureIndex` calls
//!   `matchesAuthFileLookup` and `buildAuthFileEntry` from 32 goroutines on
//!   a credential outside the manager. Here 32 tasks list one credential by
//!   name and index through the router at once.

use std::sync::Arc;

use http::{Method, StatusCode};
use open_ferry_core::auth::Status;
use serde_json::json;

use super::{Api, AuthDir, file_auth, keyed};

#[tokio::test]
async fn list_auth_files_filters_by_name_and_auth_index() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::new();
    for (id, index) in [("auth-a", "idx-a"), ("auth-b", "idx-b")] {
        let mut auth = file_auth(dir.path(), id, "shared-codex.json", r#"{"type":"codex"}"#);
        auth.index = index.into();
        api.register(auth);
    }

    let files = api.files("?name=shared-codex.json&auth_index=idx-b").await;
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0]["id"], json!("auth-b"));
    assert_eq!(files[0]["auth_index"], json!("idx-b"));
}

/// An API over `dir` with an active Codex credential for each `(id, index)`,
/// all from file `shared-codex.json`.
fn shared_credentials(dir: &AuthDir, credentials: &[(&str, &str)]) -> Api {
    let api = Api::over(dir);
    for (id, index) in credentials {
        let mut auth = file_auth(&dir.path(), id, "shared-codex.json", "{}");
        auth.index = (*index).into();
        api.register(auth);
    }
    api
}

// TestPatchAuthFileStatusVerifiesAuthIndex
#[tokio::test]
async fn patch_auth_file_status_verifies_auth_index() {
    let dir = AuthDir::new();
    let api = shared_credentials(&dir, &[("auth-a", "idx-a"), ("auth-b", "idx-b")]);

    let body = r#"{"name":"shared-codex.json","auth_index":"idx-b","disabled":true}"#;
    let request = keyed(Method::PATCH, "/v0/management/auth-files/status", body);
    api.send(request).await.expect(StatusCode::OK);

    let auth_a = api.manager.get("auth-a").unwrap();
    let auth_b = api.manager.get("auth-b").unwrap();
    assert!(!auth_a.disabled && auth_a.status != Status::Disabled);
    assert!(auth_b.disabled && auth_b.status == Status::Disabled);
}

// TestPatchAuthFileStatusRejectsMismatchedAuthIndex
#[tokio::test]
async fn patch_auth_file_status_rejects_mismatched_auth_index() {
    let dir = AuthDir::new();
    let api = shared_credentials(&dir, &[("auth-a", "idx-a")]);

    let body = r#"{"name":"shared-codex.json","auth_index":"idx-missing","disabled":true}"#;
    let request = keyed(Method::PATCH, "/v0/management/auth-files/status", body);
    api.send(request).await.expect(StatusCode::NOT_FOUND);

    let auth_a = api.manager.get("auth-a").unwrap();
    assert!(!auth_a.disabled && auth_a.status != Status::Disabled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_file_lookup_and_entry_build_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let api = Arc::new(Api::new());
    let name = "concurrent-codex.json";
    let mut auth = file_auth(dir.path(), "auth-concurrent", name, r#"{"type":"codex"}"#);
    auth.index = "idx-concurrent".into();
    api.register(auth);

    let tasks: Vec<_> = (0..32)
        .map(|_| {
            let api = Arc::clone(&api);
            tokio::spawn(async move {
                for _ in 0..10 {
                    let files = api
                        .files("?name=concurrent-codex.json&auth_index=idx-concurrent")
                        .await;
                    assert_eq!(files.len(), 1);
                    assert_eq!(files[0]["auth_index"], json!("idx-concurrent"));
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
}
