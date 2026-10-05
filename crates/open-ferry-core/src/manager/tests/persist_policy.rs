// Ported from CLIProxyAPI sdk/cliproxy/auth/persist_policy_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! When a register or update saves the credential: never for a config API
//! key, and not through `register_unsaved` or `update_unsaved` (upstream's
//! `WithSkipPersist`).

use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::manager::{CallResult, Settings};

fn antigravity() -> Auth {
    auth_with_metadata("auth-1", "antigravity", json!({"type": "antigravity"}))
}

#[tokio::test(start_paused = true)]
async fn with_skip_persist_disables_update_persistence() {
    let h = Harness::with_store(Settings::default());

    h.manager.register_unsaved(antigravity()).expect("register");
    assert_eq!(h.store.save_count(), 0, "expected 0 Save calls");

    h.manager.update(antigravity()).expect("update");
    assert_eq!(h.store.save_count(), 1, "expected 1 Save call");

    h.manager.update_unsaved(antigravity()).expect("update");
    assert_eq!(
        h.store.save_count(),
        1,
        "expected Save call count to remain 1"
    );
}

#[tokio::test(start_paused = true)]
async fn with_skip_persist_disables_register_persistence() {
    let h = Harness::with_store(Settings::default());

    h.manager.register_unsaved(antigravity()).expect("register");
    assert_eq!(h.store.save_count(), 0, "expected 0 Save calls");
}

#[tokio::test(start_paused = true)]
async fn persist_skips_config_api_key_auth() {
    let h = Harness::with_store(Settings::default());
    let mut auth = auth_with_metadata(
        "codex:apikey:abc",
        "codex",
        json!({"disable_cooling": true}),
    );
    auth.attributes.insert("api_key".into(), "secret".into());
    auth.attributes
        .insert("source".into(), "config:codex[abc]".into());

    h.manager.register(auth).expect("register");
    assert_eq!(
        h.store.save_count(),
        0,
        "expected 0 Save calls for config api key"
    );

    h.manager.mark_result(&CallResult {
        auth_id: "codex:apikey:abc".into(),
        provider: "codex".into(),
        model: "gpt-5".into(),
        success: true,
        ..CallResult::default()
    });
    assert_eq!(
        h.store.save_count(),
        0,
        "expected MarkResult to skip persist for config api key"
    );
}
