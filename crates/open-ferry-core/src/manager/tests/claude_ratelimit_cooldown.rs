// Ported from CLIProxyAPI sdk/cliproxy/auth/claude_ratelimit_cooldown_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential-scoped 429 blocks every model of the credential, and neither a
//! late success nor an update clears it; a model-scoped 429 blocks only its
//! model, and disabled cooling blocks nothing.
//!
//! Deviations from upstream:
//! - `TestAuthManager_CooldownPersistenceAcrossRestore` hands the new manager
//!   the records of `cooldown_store::snapshot` through a `RecordingStore`,
//!   installed with `install_store` and restored from with `restore_now`,
//!   where upstream's mock store is set with `SetCooldownStateStore` and
//!   `RestoreCooldownStates`.
//! - Upstream's global `SetQuotaCooldownDisabled(true)` is
//!   `Settings::disable_cooling`.

use super::cooldown_state_store::RecordingStore;
use super::support::*;

use std::time::Duration;

use chrono::TimeDelta;

use crate::auth::{Auth, AuthError};
use crate::manager::cooldown_store::{install_store, restore_now, snapshot};
use crate::manager::select::{BlockReason, is_auth_blocked_for_model};
use crate::manager::{CallResult, Settings};

const SONNET: &str = "claude-3-5-sonnet-20241022";
const OPUS: &str = "claude-3-opus-20240229";
const SEVEN_DAYS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn api_key_auth(id: &str, provider: &str, key: &str) -> Auth {
    let mut a = auth(id, provider);
    a.attributes.insert("api_key".into(), key.into());
    a
}

fn credential_429(auth_id: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: "claude".into(),
        model: model.into(),
        success: false,
        retry_after: Some(SEVEN_DAYS),
        credential_scope: true,
        error: Some(AuthError {
            http_status: 429,
            message: "7d limit rejected".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

#[tokio::test(start_paused = true)]
async fn auth_manager_concurrent_success_does_not_clear_active_credential_cooldown() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let seven_day_reset = now + TimeDelta::days(7);
    let id = "claude-concurrent";
    h.add(api_key_auth(id, "claude", "test-key"), &[SONNET, OPUS]);

    h.manager.mark_result(&credential_429(id, SONNET));
    // An earlier in-flight request on opus succeeds after the 429.
    h.manager.mark_result(&CallResult {
        auth_id: id.into(),
        provider: "claude".into(),
        model: OPUS.into(),
        success: true,
        ..CallResult::default()
    });

    let updated = h.get(id);
    assert!(
        updated.quota.exceeded
            && updated
                .quota
                .next_recover_at
                .is_some_and(|t| t > now + TimeDelta::days(6)),
        "auth quota was cleared or shortened by concurrent success: quota={:?}",
        updated.quota
    );

    for model in [SONNET, OPUS, "claude-3-7-sonnet-20250219"] {
        let (blocked, reason, next) = is_auth_blocked_for_model(&updated, model, h.now());
        assert!(
            blocked,
            "model {model:?} was unblocked despite active 7d credential cooldown"
        );
        let next = next.expect("block deadline");
        assert!(
            reason == BlockReason::Cooldown && next >= seven_day_reset - TimeDelta::minutes(1),
            "model {model:?} block reason={reason:?} next={next}, want cooldown ~7d"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn auth_manager_update_preserves_active_credential_cooldown() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let id = "claude-update";
    h.add(api_key_auth(id, "claude", "test-key"), &[SONNET]);

    h.manager.mark_result(&credential_429(id, SONNET));

    // A reload or token refresh replaces the credential.
    h.manager
        .update(api_key_auth(id, "claude", "test-key-updated"))
        .expect("update auth");

    let persisted = h.get(id);
    assert!(
        persisted.quota.exceeded
            && persisted.quota.reason == "credential_quota"
            && persisted
                .quota
                .next_recover_at
                .is_some_and(|t| t > now + TimeDelta::days(6)),
        "credential cooldown was lost after update: quota={:?}",
        persisted.quota
    );

    let (blocked, reason, _) = is_auth_blocked_for_model(&persisted, SONNET, h.now());
    assert!(
        blocked && reason == BlockReason::Cooldown,
        "model unblocked after update: blocked={blocked} reason={reason:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn auth_manager_disable_cooling_does_not_permanently_block() {
    let h = Harness::new(Settings {
        disable_cooling: true,
        ..Settings::default()
    });
    let id = "claude-disable-cooling";
    h.add(api_key_auth(id, "claude", "test-key"), &[SONNET, OPUS]);

    h.manager.mark_result(&credential_429(id, SONNET));

    for model in [SONNET, OPUS] {
        let updated = h.get(id);
        let (blocked, _, _) = is_auth_blocked_for_model(&updated, model, h.now());
        assert!(
            !blocked,
            "model {model:?} was blocked even though cooling is disabled"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn auth_manager_non_claude_provider_model429_does_not_block_sibling_models() {
    let h = Harness::new(Settings::default());
    let id = "openai-auth";
    h.add(
        api_key_auth(id, "openai", "test-key"),
        &["gpt-4o", "gpt-4o-mini"],
    );

    h.manager.mark_result(&CallResult {
        auth_id: id.into(),
        provider: "openai".into(),
        model: "gpt-4o".into(),
        success: false,
        credential_scope: false,
        error: Some(AuthError {
            http_status: 429,
            message: "rate limit".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    });

    let updated = h.get(id);
    let (blocked_4o, _, _) = is_auth_blocked_for_model(&updated, "gpt-4o", h.now());
    assert!(blocked_4o, "gpt-4o should be blocked after 429");

    let (blocked_mini, _, _) = is_auth_blocked_for_model(&updated, "gpt-4o-mini", h.now());
    assert!(
        !blocked_mini,
        "gpt-4o-mini was incorrectly blocked by sibling model 429"
    );
}

#[tokio::test(start_paused = true)]
async fn auth_manager_cooldown_persistence_across_restore() {
    let h = Harness::new(Settings::default());
    let id = "claude-persistence-test";
    h.add(api_key_auth(id, "claude", "k"), &[SONNET]);

    h.manager.mark_result(&credential_429(id, SONNET));

    let records = snapshot(&h.manager, h.now());
    assert!(
        !records.is_empty(),
        "expected cooldown state records to be captured"
    );

    // A new manager restores them.
    let restarted = Harness::new(Settings::default());
    restarted.add(auth(id, "claude"), &[]);
    install_store(&restarted.manager, RecordingStore::with_load(records));
    restore_now(&restarted.manager);

    let restored = restarted.get(id);
    assert!(
        restored.quota.exceeded
            && restored
                .quota
                .next_recover_at
                .is_some_and(|t| t >= restarted.now() + TimeDelta::days(6)),
        "restored auth quota was not preserved: quota={:?}",
        restored.quota
    );
}
