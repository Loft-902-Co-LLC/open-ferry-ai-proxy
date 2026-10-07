// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_recent_requests_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Counts and recent requests in the credential list.
//!
//! Deviations from upstream:
//! - `TestListAuthFiles_IncludesRecentRequestsBuckets` also records a
//!   call, and checks it is counted in the last bucket and leaves the quota
//!   snapshots alone.

use std::collections::BTreeMap;

use chrono::{TimeZone as _, Utc};
use http::StatusCode;
use open_ferry_core::auth::{ModelState, QuotaState};
use open_ferry_core::manager::CallResult;
use serde_json::json;

use super::{Api, object, runtime_auth};

#[tokio::test]
async fn list_auth_files_includes_recent_requests_buckets() {
    let api = Api::new();
    let mut auth = runtime_auth("runtime-only-auth-1");
    auth.metadata.insert("type".into(), json!("codex"));
    auth.quota = QuotaState {
        observed_at: Some(Utc.with_ymd_and_hms(2026, 8, 22, 0, 0, 0).unwrap()),
        signals: BTreeMap::from([("X-Codex-Primary-Used-Percent".into(), "58".into())]),
        ..QuotaState::default()
    };
    auth.model_states = BTreeMap::from([(
        "gpt-5".to_owned(),
        ModelState {
            quota: QuotaState {
                observed_at: Some(Utc.with_ymd_and_hms(2026, 8, 22, 0, 1, 0).unwrap()),
                signals: BTreeMap::from([("Retry-After".into(), "120".into())]),
                ..QuotaState::default()
            },
            ..ModelState::default()
        },
    )]);
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
    assert_eq!(
        entry["quota"],
        json!({
            "observed_at": "2026-08-22T00:00:00Z",
            "signals": { "X-Codex-Primary-Used-Percent": "58" },
        })
    );
    assert_eq!(
        entry["model_quotas"],
        json!({
            "gpt-5": {
                "observed_at": "2026-08-22T00:01:00Z",
                "signals": { "Retry-After": "120" },
            },
        })
    );

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
