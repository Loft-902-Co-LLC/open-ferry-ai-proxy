// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_project_id_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `project_id` and `websockets` in the credential list.
//!
//! Deviations from upstream:
//! - `TestListAuthFilesFromDisk_IncludesProjectID` and
//!   `TestListAuthFilesFromDisk_IncludesWebsockets` are dropped: this port
//!   has no disk listing for a missing manager.

use serde_json::json;

use super::{Api, file_auth};

#[tokio::test]
async fn list_auth_files_includes_project_id_from_manager() {
    let dir = tempfile::tempdir().unwrap();
    let name = "antigravity-user@example.com-project-a.json";
    let contents = r#"{"type":"antigravity","email":"user@example.com","project_id":"project-a"}"#;
    let mut auth = file_auth(dir.path(), name, name, contents);
    auth.provider = "antigravity".into();
    auth.metadata = serde_json::from_str(contents).unwrap();
    let api = Api::new();
    api.register(auth);

    let files = api.files("").await;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["project_id"], json!("project-a"));
}

#[tokio::test]
async fn list_auth_files_includes_websockets_from_manager() {
    let dir = tempfile::tempdir().unwrap();
    let name = "codex-user@example.com-pro.json";
    let mut auth = file_auth(
        dir.path(),
        name,
        name,
        r#"{"type":"codex","email":"user@example.com"}"#,
    );
    auth.attributes.insert("websockets".into(), "true".into());
    auth.metadata.insert("type".into(), json!("codex"));
    let api = Api::new();
    api.register(auth);

    let files = api.files("").await;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["websockets"], json!(true));
}
