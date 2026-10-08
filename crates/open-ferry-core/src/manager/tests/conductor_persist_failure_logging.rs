// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_persist_failure_logging_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A failed save doesn't fail a register, an update or a refresh: the
//! change still lands in memory.
//!
//! Deviations from upstream:
//! - The warn-log assertions are logging-only and dropped; the outcome of
//!   each call is kept.
//! - `TestMetaRefreshPersistFailureLogsWarn` is dropped: the Meta key mint
//!   save inside the lock isn't ported (lifecycle.rs).

use serde_json::{Value, json};

use super::support::*;
use crate::manager::Settings;

fn failing_store() -> Harness {
    let h = Harness::with_store(Settings::default());
    h.store.set_fail_saves(true);
    h
}

#[tokio::test(start_paused = true)]
async fn register_persist_failure_logs_warn() {
    let h = failing_store();
    let auth = auth_with_metadata(
        "auth-register-persist-1",
        "claude",
        json!({"access_token": "at", "refresh_token": "rt"}),
    );

    let registered = h
        .manager
        .register(auth)
        .expect("Register must stay non-fatal on persist failure");
    assert_eq!(registered.id, "auth-register-persist-1");
    assert!(h.manager.get("auth-register-persist-1").is_some());
    assert!(h.store.stored("auth-register-persist-1").is_none());
}

#[tokio::test(start_paused = true)]
async fn update_persist_failure_logs_warn() {
    let h = failing_store();
    let auth = auth_with_metadata(
        "auth-update-persist-1",
        "claude",
        json!({"access_token": "at", "refresh_token": "rt"}),
    );
    h.manager.register(auth.clone()).expect("register");

    let updated = h
        .manager
        .update(auth)
        .expect("Update must stay non-fatal on persist failure");
    assert!(
        updated.is_some(),
        "Update returned no auth on persist failure"
    );
    assert!(h.store.stored("auth-update-persist-1").is_none());
}

#[tokio::test(start_paused = true)]
async fn refresh_persist_failure_logs_warn() {
    let h = failing_store();
    let auth = auth_with_metadata(
        "auth-refresh-persist-1",
        "claude",
        json!({"access_token": "stale-access-token", "refresh_token": "rt"}),
    );
    h.manager.register(auth).expect("register");
    let executor = FakeExecutor::new("claude");
    executor.set_refresh(|auth| {
        let mut updated = auth.clone();
        updated
            .metadata
            .insert("access_token".into(), json!("refreshed-access-token"));
        Ok(updated)
    });
    h.executor(&executor);

    let refreshed = h
        .manager
        .force_refresh("auth-refresh-persist-1")
        .await
        .expect("ForceRefreshAuth must stay non-fatal on persist failure");
    assert_eq!(
        refreshed.metadata.get("access_token"),
        Some(&Value::from("refreshed-access-token"))
    );
    assert_eq!(
        h.get("auth-refresh-persist-1").metadata.get("access_token"),
        Some(&Value::from("refreshed-access-token"))
    );
    assert!(h.store.stored("auth-refresh-persist-1").is_none());
}
