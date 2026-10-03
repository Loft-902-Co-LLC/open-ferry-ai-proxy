// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_recent_requests_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Counts and recent requests in the credential list.
//!
//! Deviations from upstream:
//! - `TestListAuthFiles_IncludesRecentRequestsBuckets` no longer sets or
//!   checks quota observations, which the core doesn't record: `quota` is
//!   checked to be `{"signals":{}}` and `model_quotas` to be missing. It
//!   also records a call, and checks it is counted in the last bucket.

use http::StatusCode;
use open_ferry_core::manager::CallResult;
use serde_json::json;

use super::{Api, object, runtime_auth};

#[tokio::test]
async fn list_auth_files_includes_recent_requests_buckets() {
    let api = Api::new();
    let mut auth = runtime_auth("runtime-only-auth-1");
    auth.metadata.insert("type".into(), json!("codex"));
    api.register(auth);
    api.manager.mark_result(&CallResult {
        auth_id: "runtime-only-auth-1".into(),
        provider: "codex".into(),
        model: "gpt-5".into(),
        success: true,
        ..CallResult::default()
    });

    let payload = api
        .get("/v0/management/auth-files")
        .await
        .expect(StatusCode::OK);
    let files = payload["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1);
    let entry = object(&files[0]);

    assert_eq!(entry["success"], json!(1));
    assert_eq!(entry["failed"], json!(0));
    assert_eq!(entry["quota"], json!({ "signals": {} }));
    assert!(!entry.contains_key("model_quotas"));

    let recent = entry["recent_requests"]
        .as_array()
        .expect("recent_requests");
    assert_eq!(recent.len(), 20);
    for (index, bucket) in recent.iter().enumerate() {
        let bucket = object(bucket);
        let keys: Vec<_> = bucket.keys().map(String::as_str).collect();
        assert_eq!(keys, ["time", "success", "failed"], "bucket {index}");
        assert!(bucket["time"].is_string(), "bucket {index}");
        assert!(bucket["success"].is_i64(), "bucket {index}");
        assert!(bucket["failed"].is_i64(), "bucket {index}");
    }
    assert_eq!(recent[19]["success"], json!(1));
}
