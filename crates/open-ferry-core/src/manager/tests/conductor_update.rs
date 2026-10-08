// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_update_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Registering and updating credentials: thinking-suffix model states fold
//! into one, and an update keeps the old model states only when both sides
//! are enabled.
//!
//! Deviations from upstream:
//! - None.

use std::collections::BTreeMap;

use chrono::TimeDelta;
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, ModelState, QuotaState, Status};
use crate::manager::Settings;

/// One model in a quota backoff at `level`.
fn backoff_states(model: &str, level: u32) -> BTreeMap<String, ModelState> {
    BTreeMap::from([(
        model.to_owned(),
        ModelState {
            quota: QuotaState {
                backoff_level: level,
                ..QuotaState::default()
            },
            ..ModelState::default()
        },
    )])
}

fn claude(id: &str, status: Status, disabled: bool) -> Auth {
    Auth {
        status,
        disabled,
        ..auth(id, "claude")
    }
}

#[tokio::test(start_paused = true)]
async fn manager_register_canonicalizes_thinking_suffix_model_states() {
    let h = Harness::new(Settings::default());
    let now = h.now();
    let later_retry = now + TimeDelta::hours(2);

    let registered = h
        .manager
        .register(Auth {
            model_states: BTreeMap::from([
                (
                    "gemini-3.1-pro-preview(high)".to_owned(),
                    ModelState {
                        status: Status::Error,
                        unavailable: true,
                        next_retry_after: Some(now + TimeDelta::hours(1)),
                        quota: QuotaState {
                            exceeded: true,
                            next_recover_at: Some(now + TimeDelta::hours(1)),
                            backoff_level: 1,
                            ..QuotaState::default()
                        },
                        updated_at: Some(now),
                        ..ModelState::default()
                    },
                ),
                (
                    "gemini-3.1-pro-preview(low)".to_owned(),
                    ModelState {
                        status: Status::Error,
                        unavailable: true,
                        next_retry_after: Some(later_retry),
                        quota: QuotaState {
                            exceeded: true,
                            next_recover_at: Some(later_retry),
                            backoff_level: 2,
                            ..QuotaState::default()
                        },
                        updated_at: Some(now + TimeDelta::minutes(1)),
                        ..ModelState::default()
                    },
                ),
            ]),
            ..auth("auth-thinking-states", "gemini")
        })
        .expect("register");

    assert_eq!(
        registered.model_states.len(),
        1,
        "model states: {:?}",
        registered.model_states
    );
    let state = registered
        .model_states
        .get("gemini-3.1-pro-preview")
        .expect("canonical model state");
    assert!(state.unavailable, "canonical model state = {state:?}");
    assert_eq!(state.next_retry_after, Some(later_retry));
    assert_eq!(state.quota.backoff_level, 2, "quota = {:?}", state.quota);
    assert_eq!(state.quota.next_recover_at, Some(later_retry));
}

#[tokio::test(start_paused = true)]
async fn manager_update_preserves_model_states() {
    let h = Harness::new(Settings::default());
    let model = "test-model";
    let backoff_level = 7;

    h.manager
        .register(Auth {
            model_states: backoff_states(model, backoff_level),
            ..auth_with_metadata("auth-1", "claude", json!({"k": "v"}))
        })
        .expect("register auth");

    h.manager
        .update(auth_with_metadata("auth-1", "claude", json!({"k": "v2"})))
        .expect("update auth");

    let updated = h
        .manager
        .get("auth-1")
        .expect("expected auth to be present");
    assert!(
        !updated.model_states.is_empty(),
        "expected ModelStates to be preserved"
    );
    let state = updated
        .model_states
        .get(model)
        .expect("expected model state to be present");
    assert_eq!(state.quota.backoff_level, backoff_level);
}

#[tokio::test(start_paused = true)]
async fn manager_update_disabled_existing_does_not_inherit_model_states() {
    let h = Harness::new(Settings::default());
    h.manager
        .register(Auth {
            model_states: backoff_states("stale-model", 5),
            ..claude("auth-disabled", Status::Disabled, true)
        })
        .expect("register auth");

    h.manager
        .update(claude("auth-disabled", Status::Disabled, true))
        .expect("update auth");

    let updated = h
        .manager
        .get("auth-disabled")
        .expect("expected auth to be present");
    assert_eq!(
        updated.model_states.len(),
        0,
        "expected disabled auth NOT to inherit ModelStates"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_update_active_to_disabled_does_not_inherit_model_states() {
    let h = Harness::new(Settings::default());
    h.manager
        .register(Auth {
            model_states: backoff_states("stale-model", 9),
            ..claude("auth-a2d", Status::Active, false)
        })
        .expect("register auth");

    h.manager
        .update(claude("auth-a2d", Status::Disabled, true))
        .expect("update auth");

    let updated = h
        .manager
        .get("auth-a2d")
        .expect("expected auth to be present");
    assert_eq!(
        updated.model_states.len(),
        0,
        "expected active to disabled transition NOT to inherit ModelStates"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_update_disabled_to_active_does_not_inherit_stale_model_states() {
    let h = Harness::new(Settings::default());
    h.manager
        .register(Auth {
            model_states: backoff_states("stale-model", 4),
            ..claude("auth-d2a", Status::Disabled, true)
        })
        .expect("register auth");

    h.manager
        .update(claude("auth-d2a", Status::Active, false))
        .expect("update auth");

    let updated = h
        .manager
        .get("auth-d2a")
        .expect("expected auth to be present");
    assert_eq!(
        updated.model_states.len(),
        0,
        "expected disabled to active transition NOT to inherit stale ModelStates"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_update_active_inherits_model_states() {
    let h = Harness::new(Settings::default());
    let model = "active-model";
    let backoff_level = 3;

    h.manager
        .register(Auth {
            model_states: backoff_states(model, backoff_level),
            ..claude("auth-active", Status::Active, false)
        })
        .expect("register auth");

    h.manager
        .update(claude("auth-active", Status::Active, false))
        .expect("update auth");

    let updated = h
        .manager
        .get("auth-active")
        .expect("expected auth to be present");
    assert!(
        !updated.model_states.is_empty(),
        "expected active auth to inherit ModelStates"
    );
    let state = updated
        .model_states
        .get(model)
        .expect("expected model state to be present");
    assert_eq!(state.quota.backoff_level, backoff_level);
}
