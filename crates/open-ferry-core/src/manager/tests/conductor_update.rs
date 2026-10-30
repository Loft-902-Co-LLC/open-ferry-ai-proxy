// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_update_test.go (v8.0.42, MIT).
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

#[tokio::test(start_paused = true)]
async fn p47_relogin_recovers_refresh_failures_and_preserves_live_holds() {
    use crate::auth::{AuthError, ModelState, QuotaState};
    for invalid_grant in [false, true] {
        for quota in [false, true] {
            let h = Harness::new(Settings::default());
            let deadline = h.now() + TimeDelta::hours(1);
            let error = AuthError {
                http_status: if invalid_grant { 400 } else { 503 },
                message: if invalid_grant {
                    "invalid_grant"
                } else {
                    "token endpoint outage"
                }
                .into(),
                ..Default::default()
            };
            let mut old = auth_with_metadata(
                "a",
                "claude",
                json!({"access_token":"old", "refresh_token":"old-r"}),
            );
            old.status = Status::Error;
            old.unavailable = true;
            old.last_error = Some(error.clone());
            old.next_refresh_after = Some(deadline);
            if invalid_grant {
                old.next_retry_after = Some(deadline);
            }
            if quota {
                old.next_retry_after = Some(deadline);
                old.quota = QuotaState {
                    exceeded: true,
                    reason: "credential_quota".into(),
                    next_recover_at: Some(deadline),
                    backoff_level: 3,
                    ..Default::default()
                };
            }
            old.model_states.insert(
                "unauthorized".into(),
                ModelState {
                    unavailable: true,
                    status: Status::Error,
                    last_error: Some(AuthError {
                        http_status: 400,
                        message: "invalid_grant".into(),
                        ..Default::default()
                    }),
                    next_retry_after: Some(deadline),
                    ..Default::default()
                },
            );
            old.model_states.insert(
                "unrelated".into(),
                ModelState {
                    unavailable: true,
                    status: Status::Error,
                    next_retry_after: Some(deadline),
                    last_error: Some(AuthError {
                        http_status: 503,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            h.add(old, &[]);
            {
                h.manager
                    .lock()
                    .auths
                    .get_mut("a")
                    .unwrap()
                    .refresh_failures = 3;
            }
            h.manager
                .edit_unsaved("a", |auth| {
                    auth.prefix = "policy-edit".into();
                    Ok::<_, ()>(true)
                })
                .unwrap();
            assert_eq!(
                h.refresh_failures("a"),
                3,
                "policy edit retains refresh failures"
            );
            assert_eq!(h.get("a").next_refresh_after, Some(deadline));
            let unchanged = (*h.get("a")).clone();
            h.manager
                .adopt_observed(&unchanged, None, |_| {
                    Ok(crate::auth::AdoptionRead::Record(unchanged.clone()))
                })
                .unwrap();
            assert_eq!(h.refresh_failures("a"), 3);
            assert_eq!(h.get("a").next_refresh_after, Some(deadline));
            let mut fresh = (*h.get("a")).clone();
            fresh.metadata.insert("access_token".into(), json!("new"));
            h.manager
                .adopt_observed(&fresh, None, |_| {
                    Ok(crate::auth::AdoptionRead::Record(fresh.clone()))
                })
                .unwrap();
            let live = h.get("a");
            assert_eq!(
                h.refresh_failures("a"),
                0,
                "relogin clears refresh failures"
            );
            assert!(
                live.next_refresh_after.is_none(),
                "relogin clears refresh delay"
            );
            assert!(!live.model_states["unauthorized"].unavailable);
            assert_eq!(
                live.model_states["unrelated"].next_retry_after,
                Some(deadline)
            );
            if quota {
                assert!(live.quota.exceeded);
                assert_eq!(live.quota.next_recover_at, Some(deadline));
                assert_eq!(live.quota.backoff_level, 3);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn p47r2_token_changing_still_due_refresh_keeps_ineffective_backoff() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("claude");
    executor.set_lead(Some(std::time::Duration::from_secs(60)));
    executor.set_refresh(|before| {
        let mut after = before.clone();
        after
            .metadata
            .insert("access_token".into(), json!("dummy-rotated"));
        Ok(after)
    });
    h.executor(&executor);
    h.add(
        auth_with_metadata(
            "a",
            "claude",
            json!({
                "access_token":"dummy-old", "refresh_token":"dummy-refresh",
                "expires_at": (h.now() - TimeDelta::hours(1)).to_rfc3339()
            }),
        ),
        &[],
    );
    h.manager
        .lock()
        .auths
        .get_mut("a")
        .unwrap()
        .refresh_failures = 3;
    h.manager.force_refresh("a").await.unwrap();
    let live = h.get("a");
    assert_eq!(
        live.next_refresh_after,
        Some(h.now() + TimeDelta::seconds(30))
    );
    assert_eq!(h.refresh_failures("a"), 0);
    assert!(!crate::manager::refresh::should_refresh(
        &live,
        Some(std::time::Duration::from_secs(60)),
        h.now()
    ));
    assert_eq!(executor.refresh_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn p47r2_token_edit_keeps_its_explicit_refresh_delay() {
    for explicit in [false, true] {
        let h = Harness::new(Settings::default());
        let old_delay = h.now() + TimeDelta::hours(1);
        let new_delay = h.now() + TimeDelta::hours(2);
        let mut initial = auth_with_metadata("a", "claude", json!({"access_token":"dummy-old"}));
        initial.next_refresh_after = Some(old_delay);
        h.add(initial, &[]);
        h.manager
            .edit_unsaved("a", |auth| {
                auth.metadata
                    .insert("access_token".into(), json!("dummy-edit"));
                if explicit {
                    auth.next_refresh_after = Some(new_delay);
                }
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert_eq!(h.get("a").next_refresh_after, explicit.then_some(new_delay));
    }
}

#[tokio::test(start_paused = true)]
async fn p47r2_relogin_retains_aggregated_quota_status_without_auth_error() {
    for status in [400, 401] {
        let h = Harness::new(Settings::default());
        let deadline = h.now() + TimeDelta::hours(1);
        let mut initial = auth_with_metadata("a", "claude", json!({"access_token":"dummy-old"}));
        initial.status = Status::Error;
        initial.unavailable = true;
        initial.next_retry_after = Some(deadline);
        initial.quota = QuotaState {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(deadline),
            ..Default::default()
        };
        initial.last_error = Some(crate::auth::AuthError {
            http_status: status,
            message: if status == 400 {
                "invalid_grant"
            } else {
                "unauthorized"
            }
            .into(),
            ..Default::default()
        });
        h.add(initial, &[]);
        h.manager
            .edit_unsaved("a", |auth| {
                auth.metadata
                    .insert("access_token".into(), json!("dummy-login"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        let live = h.get("a");
        assert_eq!(live.status, Status::Error);
        assert!(live.unavailable);
        assert_eq!(live.next_retry_after, Some(deadline));
        assert!(live.last_error.is_none());
        assert_eq!(live.status_message, "quota");
    }
}
