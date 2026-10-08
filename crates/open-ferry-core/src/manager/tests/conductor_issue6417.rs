// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_issue6417_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A refresh that started before the credential's tokens were replaced
//! doesn't put its own back: the new tokens stay, live and saved.
//!
//! Deviations from upstream:
//! - The refresh waits on a delay in paused Tokio time, where upstream
//!   holds it on a channel; the provider is `codex`, as Antigravity isn't
//!   ported. Upstream's `refreshAuth` is `refresh_at_epoch(id, "", 0)`.

use std::time::Duration;

use chrono::TimeDelta;
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, Status};
use crate::manager::Settings;
use crate::manager::credential::{access_token, refresh_token};

const ID: &str = "issue-6417-auth";

#[tokio::test(start_paused = true)]
async fn refresh_auth_drops_stale_concurrent_credential() {
    let h = Harness::with_store(Settings::default());
    let executor = FakeExecutor::new("codex");
    let expires = (h.now() + TimeDelta::hours(1)).to_rfc3339();
    executor.set_refresh(move |auth: &Auth| {
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("access-token-a-refreshed"));
        auth.metadata
            .insert("refresh_token".into(), json!("refresh-token-a-refreshed"));
        auth.metadata.insert("expired".into(), json!(expires));
        Ok(auth)
    });
    executor.set_refresh_delay(Duration::from_secs(10));
    h.executor(&executor);

    let mut initial = auth_with_metadata(
        ID,
        "codex",
        json!({
            "access_token": "access-token-a",
            "refresh_token": "refresh-token-a",
            "expired": (h.now() - TimeDelta::hours(1)).to_rfc3339(),
        }),
    );
    initial.status = Status::Active;
    h.add(initial, &[]);

    let manager = h.manager.clone();
    let refresh = tokio::spawn(async move { manager.refresh_at_epoch(ID, "", 0).await });
    while executor.refresh_count() == 0 {
        tokio::task::yield_now().await;
    }

    let mut current = (*h.get(ID)).clone();
    current
        .metadata
        .insert("access_token".into(), json!("access-token-b"));
    current
        .metadata
        .insert("refresh_token".into(), json!("refresh-token-b"));
    h.manager
        .update(current)
        .expect("update concurrent credential");

    let _ = refresh.await.expect("refresh task");

    let updated = h.get(ID);
    assert_eq!(access_token(&updated), "access-token-b");
    assert_eq!(refresh_token(&updated), "refresh-token-b");

    let persisted = h.store.stored(ID).expect("persisted auth");
    assert_eq!(access_token(&persisted), "access-token-b");
    assert_eq!(refresh_token(&persisted), "refresh-token-b");
}
