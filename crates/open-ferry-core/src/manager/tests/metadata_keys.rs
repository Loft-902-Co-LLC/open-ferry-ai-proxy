// Ported from CLIProxyAPI sdk/cliproxy/auth/metadata_keys_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Legacy kebab-case credential metadata keys become snake_case, a
//! canonical key already present wins, and registering normalizes.
//!
//! Deviations from upstream:
//! - None. (merge.rs's own
//!   `normalize_renames_legacy_keys_and_keeps_canonical_values` covers part
//!   of the first test; this is the full upstream case.)

use serde_json::{Value, json};

use super::support::*;
use crate::manager::Settings;
use crate::manager::merge;

#[test]
fn normalize_credential_metadata() {
    let Value::Object(mut metadata) = json!({
        "api-key": "legacy-key",
        "base-url": "https://legacy.example",
        "disable-cooling": true,
        "excluded-models": ["legacy-model"],
        "fingerprint-profile": "claude-code-cli",
        "model-aliases": [{"name": "upstream", "alias": "public"}],
        "proxy-url": "http://legacy-proxy.example",
        "request-retry": 3,
        "request_retry": 0,
        "request-scoped-errors": [{"status": 429}],
        "tool-prefix-disabled": true,
        "provider_field": "preserved",
    }) else {
        unreachable!()
    };

    merge::normalize_credential_metadata(&mut metadata);

    let want = json!({
        "api_key": "legacy-key",
        "base_url": "https://legacy.example",
        "disable_cooling": true,
        "excluded_models": ["legacy-model"],
        "fingerprint_profile": "claude-code-cli",
        "model_aliases": [{"name": "upstream", "alias": "public"}],
        "proxy_url": "http://legacy-proxy.example",
        "request_retry": 0,
        "request_scoped_errors": [{"status": 429}],
        "tool_prefix_disabled": true,
        "provider_field": "preserved",
    });
    assert_eq!(Value::Object(metadata), want);
}

#[test]
fn canonical_credential_metadata_key_preserves_unknown_keys() {
    assert_eq!(
        merge::canonical_credential_metadata_key("provider-specific-key"),
        "provider-specific-key"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_register_normalizes_credential_metadata() {
    let h = Harness::new(Settings::default());
    let registered = h
        .manager
        .register(auth_with_metadata(
            "legacy-auth",
            "codex",
            json!({"request-retry": 2, "request_retry": 0}),
        ))
        .expect("Register()");

    assert_eq!(
        registered.metadata.get("request_retry"),
        Some(&json!(0)),
        "registered request_retry"
    );
    assert!(
        !registered.metadata.contains_key("request-retry"),
        "registered metadata retained legacy key: {:?}",
        registered.metadata
    );
}
