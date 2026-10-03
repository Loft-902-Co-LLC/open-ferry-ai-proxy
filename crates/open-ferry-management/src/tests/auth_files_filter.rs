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
//!   `TestPatchAuthFileStatusRejectsMismatchedAuthIndex` are dropped:
//!   `PATCH /v0/management/auth-files/status` isn't ported.
//! - `TestAuthFileLookupAndEntryBuildConcurrentEnsureIndex` calls
//!   `matchesAuthFileLookup` and `buildAuthFileEntry` from 32 goroutines on
//!   a credential outside the manager. Here 32 tasks list one credential by
//!   name and index through the router at once.

use std::sync::Arc;

use serde_json::json;

use super::{Api, file_auth};

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
