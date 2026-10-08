// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_remove_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Removing a credential: it is gone, a late update doesn't bring it back,
//! and the refresh loop forgets it.
//!
//! Deviations from upstream:
//! - `manager_remove_unschedules_auto_refresh` starts the real loop (one
//!   second) and lets it take the registration, where upstream builds the
//!   loop by hand and calls `applyDirty`. The refresh lead comes from the
//!   executor, so one is registered for the provider (a credential whose
//!   executor isn't registered isn't scheduled).

use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta};
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, Status};
use crate::manager::refresh::next_refresh_check_at;
use crate::manager::{Settings, lock};

#[tokio::test(start_paused = true)]
async fn manager_remove_deletes_runtime_auth() {
    let h = Harness::new(Settings::default());
    let mut auth = auth_with_metadata(
        "remove-runtime-auth",
        "claude",
        json!({"email": "x@example.com"}),
    );
    auth.status = Status::Active;
    h.manager.register(auth).expect("register auth");

    h.manager.remove("remove-runtime-auth");

    assert!(
        h.manager.get("remove-runtime-auth").is_none(),
        "expected auth \"remove-runtime-auth\" to be removed"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_update_missing_auth_is_no_op() {
    let h = Harness::new(Settings::default());
    let mut registered = auth("missing-update-auth", "claude");
    registered.status = Status::Active;
    h.manager.register(registered).expect("register auth");
    h.manager.remove("missing-update-auth");

    let late = Auth {
        status: Status::Disabled,
        disabled: true,
        ..auth("missing-update-auth", "claude")
    };
    let updated = h.manager.update(late).expect("update removed auth");
    assert!(
        updated.is_none(),
        "expected update on removed auth to be no-op, got {updated:?}"
    );
    assert!(
        h.manager.get("missing-update-auth").is_none(),
        "expected removed auth to stay absent after late update"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_remove_unschedules_auto_refresh() {
    let h = Harness::new(Settings::default());
    h.manager
        .start_auto_refresh(Duration::from_secs(1))
        .expect("start auto refresh");

    let lead = Duration::from_secs(10 * 60);
    let executor = FakeExecutor::new("provider-lead-expiry");
    executor.set_lead(Some(lead));
    h.executor(&executor);

    let expires_at = (h.now() + TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let auth = auth_with_metadata(
        "remove-refresh-auth",
        "provider-lead-expiry",
        json!({"email": "x@example.com", "expires_at": expires_at}),
    );
    h.manager.register(auth.clone()).expect("register auth");

    assert!(
        next_refresh_check_at(h.now(), &auth, Some(lead)).is_some(),
        "expected auth to be scheduled before removal"
    );
    settle().await;
    let queue = h
        .manager
        .refresh_loop_shared()
        .expect("refresh loop running");
    assert!(
        lock(&queue.queue).index.contains_key(&auth.id),
        "expected auth {:?} to be present in auto-refresh index before removal",
        auth.id
    );

    h.manager.remove(&auth.id);

    assert!(
        h.manager.get(&auth.id).is_none(),
        "expected auth to be removed"
    );
    assert!(
        !lock(&queue.queue).index.contains_key(&auth.id),
        "expected auth {:?} to be removed from auto-refresh index",
        auth.id
    );
    h.manager.stop_auto_refresh();
}
