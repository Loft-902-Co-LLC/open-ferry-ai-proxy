// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_state_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! New credentials clear the cooldown a rejected token left behind; an
//! update that keeps the tokens leaves it.
//!
//! Deviations from upstream:
//! - In `manager_update_clears_persisted_cooldown_when_credentials_change`,
//!   upstream's saved cooldown records become the cooldowns the credential
//!   carries, picked as upstream's `cooldownStateRecordsForAuthLocked`
//!   picks records to save; `cooldown_state_store` ports it against a store
//!   as well.
//! - Ported, or dropped, in `cooldown_state_store`, as they test the
//!   cooldown state store:
//!   `FileCooldownStateStore_StateRelativePath`,
//!   `FileCooldownStateStore_SaveLoadAndCleanStale`,
//!   `FileCooldownStateStore_ConcurrentSave`,
//!   `Manager_MarkResult_PersistsCooldownOnlyWhenStateChanges`,
//!   `ManagerSetConfigSnapshotDefersCooldownPersistence`,
//!   `ManagerSwapCooldownStateStorePersistsOldStoreBeforeSwap`,
//!   `ManagerApplyConfigWithCooldownStoreSerializesTransitions`,
//!   `ManagerSwapCooldownStateStoreKeepsOldStoreWhenCanceled`,
//!   `Manager_RestoreCooldownStates`,
//!   `Manager_RestoreCooldownStatesCanonicalizesThinkingSuffixes` and
//!   `ManagerResultSaveWaitsForCooldownStoreTransition`.

use serde_json::json;

use super::support::*;
use crate::auth::{AuthError, Status};
use crate::manager::cooldown::cooldown_disabled_for_auth;
use crate::manager::{CallResult, Settings};

/// The cooldowns upstream would save for `id`: the credential's own and each
/// model's, while unavailable until a time still ahead (upstream's
/// `cooldownStateRecordsForAuthLocked`).
fn cooldown_records(h: &Harness, id: &str) -> Vec<String> {
    let auth = h.get(id);
    let now = h.now();
    if auth.disabled
        || auth.status == Status::Disabled
        || cooldown_disabled_for_auth(&h.manager.settings(), &auth)
    {
        return Vec::new();
    }
    let mut records = Vec::new();
    if auth.unavailable && auth.next_retry_after.is_some_and(|t| t > now) {
        records.push(String::new());
    }
    for (model, state) in &auth.model_states {
        if !model.trim().is_empty()
            && state.unavailable
            && state.next_retry_after.is_some_and(|t| t > now)
        {
            records.push(model.clone());
        }
    }
    records
}

#[tokio::test(start_paused = true)]
async fn manager_update_clears_persisted_cooldown_when_credentials_change() {
    let h = Harness::new(Settings::default());
    let mut first = auth_with_metadata("auth-codex-1", "codex", json!({"access_token": "token-1"}));
    first.status = Status::Active;
    let auth = h.add(first, &[]);

    // 1. Fail with 401.
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "codex".into(),
        model: "gpt-6-astra".into(),
        success: false,
        error: Some(AuthError {
            message: "invalidated token".into(),
            http_status: 401,
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    assert!(
        !cooldown_records(&h, &auth.id).is_empty(),
        "expected a cooldown after the unauthorized failure"
    );

    // 2. An update that keeps the credentials (a metadata note).
    let mut same_cred = auth_with_metadata(
        &auth.id,
        "codex",
        json!({"access_token": "token-1", "note": "updated note"}),
    );
    same_cred.status = Status::Active;
    h.manager.update(same_cred).expect("update");
    assert!(
        !cooldown_records(&h, &auth.id).is_empty(),
        "expected the cooldown to remain when credentials did not change"
    );

    // 3. An update with a new access token.
    let mut new_cred = auth_with_metadata(&auth.id, "codex", json!({"access_token": "token-2"}));
    new_cred.status = Status::Active;
    h.manager.update(new_cred).expect("update");
    let records = cooldown_records(&h, &auth.id);
    assert!(
        records.is_empty(),
        "expected cooldowns to be cleared after credential change, got {records:?}"
    );
}
