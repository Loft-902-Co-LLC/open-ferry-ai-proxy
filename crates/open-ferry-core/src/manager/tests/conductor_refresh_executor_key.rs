// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_refresh_executor_key_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A refresh goes to the executor the credential's attributes name, not
//! the one its provider would.
//!
//! Deviations from upstream:
//! None.

use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::manager::Settings;

#[tokio::test(start_paused = true)]
async fn refresh_auth_for_request_uses_executor_key_from_auth() {
    let h = Harness::new(Settings::default());
    // Upstream's countingRefreshExecutor.
    let executor = FakeExecutor::new("openai-compatible-custom");
    executor.set_refresh(|auth: &Auth| {
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("refreshed-token"));
        Ok(auth)
    });
    h.executor(&executor);

    let mut auth = auth_with_metadata(
        "compat-oauth",
        "plugin-provider",
        json!({"access_token": "old-token", "refresh_token": "refresh-1"}),
    );
    auth.attributes
        .insert("compat_name".into(), "custom".into());
    auth.attributes
        .insert("provider_key".into(), "custom".into());
    auth.attributes
        .insert("base_url".into(), "https://compat.example.com/v1".into());
    h.add(auth, &[]);

    let refreshed = h
        .manager
        .refresh_at_epoch("compat-oauth", "old-token", 0)
        .await
        .expect("refresh_at_epoch");
    assert_eq!(executor.refresh_count(), 1, "refresh calls");
    assert_eq!(
        refreshed.metadata.get("access_token"),
        Some(&json!("refreshed-token")),
        "want updated access_token"
    );
}
