// Ported from CLIProxyAPI sdk/cliproxy/auth/cooldown_backoff_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Quota backoff that rises once per cooldown window, finite cooldowns for
//! failures nobody can classify, and the jitter added to cooldown waits.
//!
//! Deviations from upstream:
//! - `scheduler_promotes_unknown_failure_after_retry_deadline`: the
//!   scheduler index isn't ported. Upstream checks the scheduler entry is
//!   blocked with a finite retry time and promoted to ready just after it;
//!   here the credential is blocked (not cooling down) until a finite
//!   deadline, a pick fails before it and picks the credential one
//!   nanosecond after it.
//! - `jittered_cooldown_wait_bounds`: the negative-wait case is dropped;
//!   a `Duration` can't be negative.

use std::collections::HashSet;
use std::time::Duration;

use chrono::TimeDelta;
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status, Timestamp};
use crate::exec::ExecError;
use crate::manager::cooldown::apply_auth_failure_state;
use crate::manager::models::Resolver;
use crate::manager::retry::jittered_cooldown_wait;
use crate::manager::select::{BlockReason, PickArgs, Picked, Selection, is_auth_blocked_for_model};
use crate::manager::{CallResult, Settings};

fn quota_error() -> AuthError {
    AuthError {
        code: "rate_limit".into(),
        message: "quota".into(),
        retryable: true,
        http_status: 429,
    }
}

/// Upstream's `quotaResult`.
fn quota_result(auth_id: &str, model: &str) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: "codex".into(),
        model: model.into(),
        success: false,
        error: Some(quota_error()),
        ..CallResult::default()
    }
}

/// Picks a credential for `model` as of `now` (upstream's scheduler pick).
fn pick_at(h: &Harness, provider: &str, model: &str, now: Timestamp) -> Result<Picked, ExecError> {
    let providers = providers(&[provider]);
    let tried = HashSet::new();
    let mut guard = h.manager.lock();
    let state = &mut *guard;
    let selection = Selection {
        auths: &state.auths,
        executors: &state.executors,
        models: h.manager.models(),
        resolver: Resolver {
            settings: &state.settings,
            oauth: &state.oauth,
        },
        strategy: state.settings.routing_strategy,
        now,
    };
    let args = PickArgs {
        model,
        pinned: "",
        downstream_websocket: false,
        tried: &tried,
    };
    selection.pick_next_mixed(&mut state.selector, &providers, &args)
}

#[tokio::test(start_paused = true)]
async fn mark_result_quota_backoff_escalates_once_per_window() {
    let h = Harness::new(Settings::default());
    let auth = h.add(
        auth_with_metadata("auth-quota-window", "codex", json!({"type": "codex"})),
        &[],
    );

    h.manager.mark_result(&quota_result(&auth.id, "gpt-5"));
    let first = h.get(&auth.id);
    let first_state = first
        .model_states
        .get("gpt-5")
        .expect("expected model state after first failure");
    assert_eq!(
        first_state.quota.backoff_level, 1,
        "expected backoff level 1 after first failure"
    );
    assert!(
        first_state
            .quota
            .next_recover_at
            .is_some_and(|t| t > h.now()),
        "expected open cooldown window after first failure, got {:?}",
        first_state.quota.next_recover_at
    );

    // A second in-flight failure lands while the first window is still open.
    h.manager.mark_result(&quota_result(&auth.id, "gpt-5"));
    let second = h.get(&auth.id);
    let second_state = second
        .model_states
        .get("gpt-5")
        .expect("expected model state after second failure");
    assert_eq!(
        second_state.quota.backoff_level, 1,
        "expected backoff level to stay 1 for in-window failure"
    );
    assert_eq!(
        second_state.quota.next_recover_at, first_state.quota.next_recover_at,
        "expected next_recover_at to stay for in-window failure"
    );
    assert_eq!(
        second_state.next_retry_after, first_state.next_retry_after,
        "expected next_retry_after to stay for in-window failure"
    );
}

#[tokio::test(start_paused = true)]
async fn mark_result_quota_backoff_escalates_after_window_expiry() {
    let h = Harness::new(Settings::default());
    let expired = h.now() - TimeDelta::seconds(1);
    let mut auth = auth_with_metadata("auth-quota-expired", "codex", json!({"type": "codex"}));
    auth.model_states.insert(
        "gpt-5".into(),
        ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(expired),
            quota: QuotaState {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(expired),
                backoff_level: 3,
            },
            ..ModelState::default()
        },
    );
    let auth = h.add(auth, &[]);

    h.manager.mark_result(&quota_result(&auth.id, "gpt-5"));
    let updated = h.get(&auth.id);
    let state = updated
        .model_states
        .get("gpt-5")
        .expect("expected model state after failure");
    assert_eq!(
        state.quota.backoff_level, 4,
        "expected backoff level 4 after post-window failure"
    );
    assert!(
        state.quota.next_recover_at.is_some_and(|t| t > h.now()),
        "expected a fresh cooldown window, got {:?}",
        state.quota.next_recover_at
    );
}

#[test]
fn apply_auth_failure_state_quota_backoff_once_per_window() {
    let now = chrono::DateTime::from_timestamp(1_780_272_000, 0).expect("timestamp");
    let settings = Settings::default();
    let quota_err = AuthError {
        code: "rate_limit".into(),
        message: "quota".into(),
        http_status: 429,
        ..AuthError::default()
    };
    let mut auth = Auth {
        id: "auth-level-quota".into(),
        ..Auth::default()
    };

    apply_auth_failure_state(&settings, &mut auth, Some(&quota_err), None, now, false);
    assert_eq!(
        auth.quota.backoff_level, 1,
        "expected backoff level 1 after first failure"
    );
    let first_recover = auth.quota.next_recover_at;
    assert_eq!(
        first_recover,
        Some(now + TimeDelta::seconds(1)),
        "expected first window to close a second later"
    );

    // An in-window failure keeps the current window and level.
    apply_auth_failure_state(
        &settings,
        &mut auth,
        Some(&quota_err),
        None,
        now + TimeDelta::milliseconds(100),
        false,
    );
    assert_eq!(
        auth.quota.backoff_level, 1,
        "expected backoff level to stay 1 for in-window failure"
    );
    assert_eq!(
        auth.quota.next_recover_at, first_recover,
        "expected next_recover_at to stay for in-window failure"
    );

    // A failure after the window expired escalates to the next level.
    apply_auth_failure_state(
        &settings,
        &mut auth,
        Some(&quota_err),
        None,
        now + TimeDelta::seconds(2),
        false,
    );
    assert_eq!(
        auth.quota.backoff_level, 2,
        "expected backoff level 2 after post-window failure"
    );
    assert_eq!(
        auth.quota.next_recover_at,
        Some(now + TimeDelta::seconds(4)),
        "expected second window to close at now+4s"
    );

    // A provider's retry hint always takes effect, even in-window.
    apply_auth_failure_state(
        &settings,
        &mut auth,
        Some(&quota_err),
        Some(Duration::from_secs(10)),
        now + TimeDelta::seconds(3),
        false,
    );
    assert_eq!(
        auth.quota.backoff_level, 2,
        "expected backoff level to stay 2 with retry hint"
    );
    assert_eq!(
        auth.quota.next_recover_at,
        Some(now + TimeDelta::seconds(13)),
        "expected retry hint window to close at now+13s"
    );
}

#[tokio::test(start_paused = true)]
async fn recoverable_unknown_failures_have_finite_cooldown() {
    // (name, model, error)
    let cases = [
        ("model failure without error details", "gpt-5", None),
        (
            "auth failure without status or transport signature",
            "",
            Some(AuthError {
                message: "upstream exploded".into(),
                ..AuthError::default()
            }),
        ),
    ];
    for (name, model, result_err) in cases {
        let h = Harness::new(Settings {
            transient_error_cooldown_seconds: 0,
            ..Settings::default()
        });
        let auth = h.add(auth(&format!("auth-unknown-{name}"), "codex"), &[]);

        h.manager.mark_result(&CallResult {
            auth_id: auth.id.clone(),
            provider: auth.provider.clone(),
            model: model.into(),
            success: false,
            error: result_err,
            ..CallResult::default()
        });

        let updated = h.get(&auth.id);
        let next_retry_after = if model.is_empty() {
            updated.next_retry_after
        } else {
            updated
                .model_states
                .get(model)
                .unwrap_or_else(|| panic!("{name}: expected model state for {model:?}"))
                .next_retry_after
        };
        let next_retry_after = next_retry_after
            .unwrap_or_else(|| panic!("{name}: recoverable failure has no retry deadline"));
        assert!(
            is_auth_blocked_for_model(&updated, model, h.now()).0,
            "{name}: auth was not blocked during recoverable failure cooldown"
        );
        assert!(
            !is_auth_blocked_for_model(
                &updated,
                model,
                next_retry_after + TimeDelta::nanoseconds(1)
            )
            .0,
            "{name}: auth did not automatically recover after retry deadline"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_promotes_unknown_failure_after_retry_deadline() {
    const PROVIDER: &str = "gemini";
    const MODEL: &str = "scheduler-unknown-recovery-model";
    const AUTH_ID: &str = "scheduler-unknown-recovery-auth";

    let h = Harness::new(Settings {
        transient_error_cooldown_seconds: 0,
        ..Settings::default()
    });
    h.executor(&FakeExecutor::new(PROVIDER));
    h.add(auth(AUTH_ID, PROVIDER), &[MODEL]);
    let picked = pick_at(&h, PROVIDER, MODEL, h.now()).expect("initial pick");
    assert_eq!(picked.auth.id, AUTH_ID);

    h.manager.mark_result(&CallResult {
        auth_id: AUTH_ID.into(),
        provider: PROVIDER.into(),
        model: MODEL.into(),
        success: false,
        error: Some(AuthError {
            message: "transport closed".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    });

    // Upstream: the scheduler entry is blocked (not cooling down) with a
    // finite retry time.
    let updated = h.get(AUTH_ID);
    let (blocked, reason, next_retry_at) = is_auth_blocked_for_model(&updated, MODEL, h.now());
    assert!(
        blocked && reason == BlockReason::Other,
        "state = ({blocked}, {reason:?}); want blocked"
    );
    let next_retry_at = next_retry_at.expect("want a finite retry time");
    assert!(
        pick_at(&h, PROVIDER, MODEL, h.now()).is_err(),
        "a blocked credential was picked before its retry time"
    );

    // Upstream: promoted to ready just after the retry time.
    let picked = pick_at(
        &h,
        PROVIDER,
        MODEL,
        next_retry_at + TimeDelta::nanoseconds(1),
    )
    .expect("pick after the retry time");
    assert_eq!(picked.auth.id, AUTH_ID);
}

#[test]
fn jittered_cooldown_wait_bounds() {
    let ms = Duration::from_millis;
    let secs = Duration::from_secs;
    // (wait, max wait, the most jitter)
    let cases = [
        (secs(1), Duration::ZERO, ms(250)),
        (secs(8), Duration::ZERO, secs(2)),
        (secs(30), Duration::ZERO, secs(2)),
        (secs(1), secs(30), ms(250)),
        (secs(29), secs(30), secs(1)),
    ];
    for (wait, max_wait, max_jitter) in cases {
        for _ in 0..200 {
            let got = jittered_cooldown_wait(wait, max_wait);
            assert!(
                got >= wait && got < wait + max_jitter,
                "jittered_cooldown_wait({wait:?}, {max_wait:?}) = {got:?}, want in [{wait:?}, {:?})",
                wait + max_jitter
            );
            if !max_wait.is_zero() {
                assert!(
                    got <= max_wait,
                    "jittered_cooldown_wait({wait:?}, {max_wait:?}) = {got:?} exceeds max wait"
                );
            }
        }
    }

    // The max wait is a hard ceiling: no headroom, no jitter.
    for _ in 0..50 {
        assert_eq!(jittered_cooldown_wait(secs(30), secs(30)), secs(30));
    }

    assert_eq!(
        jittered_cooldown_wait(Duration::ZERO, secs(60)),
        Duration::ZERO
    );
    assert_eq!(
        jittered_cooldown_wait(Duration::from_nanos(3), Duration::ZERO),
        Duration::from_nanos(3),
        "expected sub-4ns wait to stay unchanged"
    );
}
