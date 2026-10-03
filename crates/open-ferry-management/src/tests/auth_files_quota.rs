// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_quota_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What `quota` shows in the credential list.
//!
//! Deviations from upstream:
//! - `TestModelQuotaObservationPayloadOmitsUnsupportedProviders` and
//!   `TestModelQuotaObservationPayloadSkipsNilAndEmptyStates` are dropped:
//!   the core records no passive quota observations, so `model_quotas`
//!   never appears.
//! - `TestQuotaObservationPayloadExcludesCooldownState` lists a credential
//!   whose quota is exceeded instead of calling
//!   `quotaObservationPayload`, and checks that `quota` is exactly
//!   `{"signals":{}}`.

use chrono::{TimeZone as _, Utc};
use http::StatusCode;
use open_ferry_core::auth::QuotaState;
use serde_json::json;

use super::{Api, runtime_auth};

#[tokio::test]
async fn quota_observation_payload_excludes_cooldown_state() {
    let api = Api::new();
    let mut auth = runtime_auth("quota-auth");
    auth.quota = QuotaState {
        exceeded: true,
        reason: "credential_quota".into(),
        next_recover_at: Some(Utc.timestamp_opt(20, 0).unwrap()),
        backoff_level: 3,
    };
    api.register(auth);

    let payload = api
        .get("/v0/management/auth-files")
        .await
        .expect(StatusCode::OK);
    let entry = &payload["files"][0];
    assert_eq!(entry["quota"], json!({ "signals": {} }));
    assert!(entry.get("model_quotas").is_none(), "{entry}");
}
