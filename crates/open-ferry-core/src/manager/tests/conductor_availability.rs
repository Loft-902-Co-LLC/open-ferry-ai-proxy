// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_availability_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's availability as its model states make it, disabled
//! credentials not counting for their provider, and resetting a quota.
//!
//! Deviations from upstream:
//! - `Manager_AvailableProvidersAndHasProviderAuth_ExcludeDisabled`: the port
//!   has no `AvailableProviders` or `HasProviderAuth` (upstream serves them
//!   to its plugin host). The test checks what they report through calls:
//!   the active credential's provider serves, while a provider whose only
//!   credential is disabled (by flag or status) has none to pick.
//! - `Manager_ResetQuotaClearsRuntimeAndRegistryState`: the registry is the
//!   harness's [`FakeModels`]. Upstream's `SetModelQuotaExceeded` and
//!   `SuspendClientModel` are a publish of the credential's projections, and
//!   "model count 0, then 1" is the model's projection suspended with its
//!   quota exceeded, then neither.

use std::time::Duration;

use chrono::TimeDelta;

use super::support::*;
use crate::auth::{Auth, ModelState, QuotaState, Status};
use crate::exec::{Dispatcher, ErrorKind};
use crate::manager::Settings;
use crate::manager::cooldown::update_aggregated_availability;
use crate::manager::credential::is_zero;

fn minutes(n: i64) -> TimeDelta {
    TimeDelta::minutes(n)
}

#[test]
fn update_aggregated_availability_unavailable_without_next_retry_does_not_block_auth() {
    let now = TestClock::new().now();
    let mut auth = auth("a", "");
    auth.model_states.insert(
        "test-model".into(),
        ModelState {
            status: Status::Error,
            unavailable: true,
            ..ModelState::default()
        },
    );

    update_aggregated_availability(&mut auth, now);

    assert!(!auth.unavailable, "auth.unavailable = true, want false");
    assert!(
        is_zero(auth.next_retry_after),
        "auth.next_retry_after = {:?}, want zero",
        auth.next_retry_after
    );
}

#[test]
fn update_aggregated_availability_future_next_retry_blocks_auth() {
    let now = TestClock::new().now();
    let next = now + minutes(5);
    let mut auth = auth("a", "");
    auth.model_states.insert(
        "test-model".into(),
        ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(next),
            ..ModelState::default()
        },
    );

    update_aggregated_availability(&mut auth, now);

    assert!(auth.unavailable, "auth.unavailable = false, want true");
    assert_eq!(auth.next_retry_after, Some(next));
}

#[tokio::test(start_paused = true)]
async fn manager_available_providers_and_has_provider_auth_exclude_disabled() {
    let h = Harness::new(Settings::default());
    for provider in ["claude", "gemini", "codex"] {
        h.executor(&FakeExecutor::new(provider));
    }
    let mut active = auth("active", "claude");
    active.status = Status::Active;
    h.add(active, &["m"]);
    // Provider gemini only has a credential with the disabled flag set.
    let mut flag_disabled = auth("flag-disabled", "gemini");
    flag_disabled.disabled = true;
    h.add(flag_disabled, &["m"]);
    // Provider codex only has a credential whose status is disabled.
    let mut status_disabled = auth("status-disabled", "codex");
    status_disabled.status = Status::Disabled;
    h.add(status_disabled, &["m"]);

    h.manager
        .execute(&providers(&["claude"]), request("m"), options())
        .await
        .expect("active provider claude serves");
    for provider in ["gemini", "codex"] {
        let err = h
            .manager
            .execute(&providers(&[provider]), request("m"), options())
            .await
            .expect_err("provider with only a disabled credential");
        assert_eq!(err.kind, ErrorKind::AuthNotFound, "{provider}: {err}");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_reset_quota_clears_runtime_and_registry_state() {
    let h = Harness::new(Settings::default());
    let auth_id = "reset-quota-auth";
    let model = "reset-quota-model";
    let next = h.now() + TimeDelta::from_std(Duration::from_secs(3600)).expect("hour");
    let quota = QuotaState {
        exceeded: true,
        reason: "quota".into(),
        next_recover_at: Some(next),
        backoff_level: 2,
        ..Default::default()
    };
    let mut auth = Auth {
        status: Status::Error,
        status_message: "quota exhausted".into(),
        unavailable: true,
        next_retry_after: Some(next),
        quota: quota.clone(),
        ..auth(auth_id, "claude")
    };
    auth.model_states.insert(
        model.into(),
        ModelState {
            status: Status::Error,
            status_message: "quota exhausted".into(),
            unavailable: true,
            next_retry_after: Some(next),
            quota,
            updated_at: Some(next),
            ..ModelState::default()
        },
    );
    h.add(auth, &[model]);

    // Upstream marks the model's quota exceeded and suspends it in the
    // registry; here that is a publish of the credential's projections.
    let (_, generation) = h.versions(auth_id);
    h.manager
        .publish_projections(&h.get(auth_id), generation, h.now(), false);
    let before = h.models.projection(auth_id, model).expect("projection");
    assert!(
        before.suspended && before.quota_exceeded,
        "registry projection before reset = {before:?}, want suspended with quota exceeded"
    );

    let reset = h
        .manager
        .reset_quota(auth_id)
        .expect("reset quota")
        .expect("updated auth");
    assert_eq!(reset.models, [model]);
    let updated = &reset.auth;
    assert!(
        updated.status == Status::Active
            && updated.status_message.is_empty()
            && !updated.unavailable
            && is_zero(updated.next_retry_after),
        "updated auth state = status {} message {:?} unavailable {} next {:?}",
        updated.status,
        updated.status_message,
        updated.unavailable,
        updated.next_retry_after
    );
    assert!(
        !updated.quota.exceeded
            && updated.quota.reason.is_empty()
            && is_zero(updated.quota.next_recover_at)
            && updated.quota.backoff_level == 0,
        "updated auth quota = {:?}, want cleared",
        updated.quota
    );
    let state = updated
        .model_states
        .get(model)
        .expect("updated model state");
    assert!(
        state.status == Status::Active
            && state.status_message.is_empty()
            && !state.unavailable
            && is_zero(state.next_retry_after),
        "updated model state = status {} message {:?} unavailable {} next {:?}",
        state.status,
        state.status_message,
        state.unavailable,
        state.next_retry_after
    );
    assert!(
        !state.quota.exceeded
            && state.quota.reason.is_empty()
            && is_zero(state.quota.next_recover_at)
            && state.quota.backoff_level == 0,
        "updated model quota = {:?}, want cleared",
        state.quota
    );
    let after = h.models.projection(auth_id, model).expect("projection");
    assert!(
        !after.suspended && !after.quota_exceeded,
        "registry projection after reset = {after:?}, want available"
    );
}
