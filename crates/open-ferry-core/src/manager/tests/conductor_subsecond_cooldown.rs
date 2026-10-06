// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_subsecond_cooldown_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The ten-second floor on a provider's 429 retry hint, for model and
//! credential cooldowns, and how retry rounds honour it: a credential the
//! failed round tried after a 429 never makes the next round start at once.
//!
//! Deviations from upstream:
//! - `closestCooldownWaitWithAttempted` returns `(wait, found)`; here
//!   `closest_cooldown_wait` returns `Option<Duration>`. Upstream's
//!   `shouldRetryAfterErrorWithAttempted` maps to `should_retry_after_error`
//!   over a selection built from the manager's state, as the manager's own
//!   `retry_wait` builds it. The eligibility argument is the query's
//!   `eligibility`, which allows every credential here as upstream's empty
//!   one does.
//! - Upstream's slow-pool executor mutates the first credential under the
//!   manager's lock; here the fake executor's handler does the same through
//!   the manager's state lock.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;

use super::support::*;
use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::cooldown::{MIN_QUOTA_COOLDOWN_FLOOR, apply_auth_failure_state};
use crate::manager::models::Resolver;
use crate::manager::retry::{RetryQuery, closest_cooldown_wait, should_retry_after_error};
use crate::manager::select::Selection;
use crate::manager::{CallResult, OpenAiCompat, Settings};

const MODEL: &str = "gemini-3.1-flash-image";
const SUB_SECOND: Duration = Duration::from_millis(708);

fn resource_exhausted() -> AuthError {
    AuthError {
        http_status: 429,
        message: "RESOURCE_EXHAUSTED".into(),
        ..AuthError::default()
    }
}

/// A 429 with the provider's retry hint, as upstream's
/// `retryAfterStatusError`.
fn quota_exhausted(retry_after: Duration) -> ExecError {
    let mut err = ExecError::upstream(429, "quota exhausted");
    err.retry_after = Some(retry_after);
    err
}

/// A credential whose quota cooldown for `model` ended five seconds ago.
fn expired_cooldown_auth(h: &Harness, id: &str, provider: &str, model: &str) -> Auth {
    let expired = h.now() - TimeDelta::seconds(5);
    let mut auth = auth(id, provider);
    auth.model_states.insert(
        model.into(),
        ModelState {
            status: Status::Error,
            unavailable: true,
            next_retry_after: Some(expired),
            quota: QuotaState {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(expired),
                ..QuotaState::default()
            },
            ..ModelState::default()
        },
    );
    auth
}

/// Runs `f` over a selection of the manager's current state.
fn with_selection<R>(h: &Harness, f: impl FnOnce(&Selection<'_>) -> R) -> R {
    let now = h.manager.now();
    let state = h.manager.lock();
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
    f(&selection)
}

/// Upstream's `closestCooldownWaitWithAttempted(providers, model, 0,
/// eligibility, "", 5, status, attempted)`.
fn closest_wait(
    h: &Harness,
    provider_names: &[&str],
    model: &str,
    status: u16,
    attempted: &HashSet<String>,
) -> Option<Duration> {
    let providers = providers(provider_names);
    with_selection(h, |selection| {
        let query = RetryQuery {
            providers: &providers,
            model,
            pinned: "",
            attempt: 0,
            default_retry: 5,
            eligibility: Default::default(),
            attempted,
        };
        closest_cooldown_wait(selection, &query, status)
    })
}

#[tokio::test(start_paused = true)]
async fn mark_result_429_sub_second_retry_after_enforces_minimum_cooldown_floor() {
    let h = Harness::new(Settings::default());
    let auth = h.add(auth("auth-gemini-subsecond", "google"), &[MODEL]);

    let now = h.now();
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "google".into(),
        model: MODEL.into(),
        success: false,
        retry_after: Some(SUB_SECOND),
        error: Some(resource_exhausted()),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    let next_retry_after = state
        .next_retry_after
        .expect("expected next_retry_after to be set");
    let min_expected = now + TimeDelta::seconds(10);
    assert!(
        next_retry_after >= min_expected,
        "next_retry_after {next_retry_after} is under the 10s floor"
    );
    assert!(
        state
            .quota
            .next_recover_at
            .is_some_and(|t| t >= min_expected),
        "quota.next_recover_at {:?} is under the 10s floor",
        state.quota.next_recover_at
    );
}

#[tokio::test(start_paused = true)]
async fn mark_result_429_longer_retry_after_preserved_above_floor() {
    let h = Harness::new(Settings::default());
    let auth = h.add(auth("auth-gemini-longer", "google"), &[MODEL]);

    let now = h.now();
    let long_duration = Duration::from_secs(5 * 60);
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: "google".into(),
        model: MODEL.into(),
        success: false,
        retry_after: Some(long_duration),
        error: Some(resource_exhausted()),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    let expected = now + TimeDelta::minutes(5);
    let next_retry_after = state.next_retry_after.expect("next_retry_after unset");
    assert!(
        next_retry_after >= expected - TimeDelta::seconds(1)
            && next_retry_after <= expected + TimeDelta::seconds(1),
        "expected next_retry_after ~ {expected}, got {next_retry_after}"
    );
}

#[test]
fn apply_auth_failure_state_429_sub_second_retry_after_enforces_minimum_cooldown_floor() {
    let now = chrono::DateTime::from_timestamp(1_780_272_000, 0).expect("timestamp");
    let mut auth = Auth {
        id: "auth-level-subsecond".into(),
        ..Auth::default()
    };
    let quota_err = resource_exhausted();

    apply_auth_failure_state(
        &Settings::default(),
        &mut auth,
        Some(&quota_err),
        Some(SUB_SECOND),
        now,
        false,
    );

    let next_retry_after = auth
        .next_retry_after
        .expect("expected next_retry_after to be set");
    let min_expected = now + TimeDelta::seconds(10);
    assert!(
        next_retry_after >= min_expected,
        "auth next_retry_after {next_retry_after} is under the 10s floor"
    );
    assert!(
        auth.quota
            .next_recover_at
            .is_some_and(|t| t >= min_expected),
        "auth quota.next_recover_at {:?} is under the 10s floor",
        auth.quota.next_recover_at
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_429_sub_second_retry_after_prevents_retry_storm() {
    let h = Harness::new(Settings {
        request_retry: 5,
        max_retry_interval: Duration::from_secs(5),
        max_retry_credentials: 6,
        ..Settings::default()
    });
    let executor = FakeExecutor::with("google", |call: &Call| match call.auth_id.as_str() {
        "auth-storm-1" | "auth-storm-2" => Reply::Err(quota_exhausted(SUB_SECOND)),
        _ => Reply::ok("ok"),
    });
    h.executor(&executor);
    for id in ["auth-storm-1", "auth-storm-2"] {
        h.add(auth(id, "google"), &[MODEL]);
    }

    let err = h
        .manager
        .execute(&providers(&["google"]), request(MODEL), options())
        .await
        .expect_err("expected execute error");
    assert_eq!(err.status, 429);

    // Each credential is tried once in round 0; the 10s floor is past the
    // 5s max wait, so no further round starts.
    assert_eq!(
        executor.calls().len(),
        2,
        "execute calls: want exactly 2 (initial round only, no retry storm)"
    );
}

#[tokio::test(start_paused = true)]
async fn closest_cooldown_wait_with_attempted_expired_cooldown_does_not_trigger_zero_wait_for_attempted_auth()
 {
    let h = Harness::new(Settings::default());
    let auth = expired_cooldown_auth(&h, "auth-attempted-expired", "google", MODEL);
    let auth = h.add(auth, &[MODEL]);

    // Not tried in the failed round: its cooldown is over, so it is ready.
    let untried = closest_wait(&h, &["google"], MODEL, 429, &HashSet::new());
    assert_eq!(untried, Some(Duration::ZERO), "untried credential");

    // Tried in the round that failed with 429: at least the floor, even
    // though its own cooldown is over.
    let attempted: HashSet<String> = HashSet::from([auth.id.clone()]);
    let wait = closest_wait(&h, &["google"], MODEL, 429, &attempted)
        .expect("expected candidate to be found for retry after cooldown");
    assert!(
        wait >= MIN_QUOTA_COOLDOWN_FLOOR,
        "attempted credential after 429: wait = {wait:?}, want >= {MIN_QUOTA_COOLDOWN_FLOOR:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_execute_slow_pool_expired_cooldown_prevents_retry_storm() {
    let h = Harness::new(Settings {
        request_retry: 5,
        max_retry_interval: Duration::from_secs(5),
        max_retry_credentials: 6,
        ..Settings::default()
    });
    let auth_ids = ["auth-slow-1", "auth-slow-2"];
    let manager = h.manager.clone();
    let executor = FakeExecutor::with("google", move |call: &Call| {
        // When the second credential runs, the first has failed and been
        // marked; its cooldown is pushed into the past, as if the round
        // were slow enough for it to end.
        if call.auth_id == auth_ids[1] {
            let expired = manager.now() - TimeDelta::seconds(5);
            let mut state = manager.lock();
            if let Some(entry) = state.auths.get_mut(auth_ids[0]) {
                let first = Arc::make_mut(&mut entry.auth);
                if let Some(model_state) = first.model_states.get_mut(MODEL) {
                    model_state.next_retry_after = Some(expired);
                    model_state.quota.next_recover_at = Some(expired);
                }
            }
        }
        Reply::Err(quota_exhausted(SUB_SECOND))
    });
    h.executor(&executor);
    for id in auth_ids {
        h.add(auth(id, "google"), &[MODEL]);
    }

    let err = h
        .manager
        .execute(&providers(&["google"]), request(MODEL), options())
        .await
        .expect_err("expected execute error");
    // Drop the handler's manager handle, which the manager itself holds
    // through the executor.
    executor.set_handler(|_: &Call| Reply::ok("ok"));
    assert_eq!(err.status, 429);

    let first = h.get(auth_ids[0]);
    let first_state = first
        .model_states
        .get(MODEL)
        .expect("expected auth-slow-1 state to exist");
    assert!(
        first_state.next_retry_after.is_some_and(|t| t < h.now()),
        "expected auth-slow-1 next_retry_after in the past, got {:?}",
        first_state.next_retry_after
    );

    // Both credentials were tried in round 0 and failed with 429, so the
    // floor (10s) applies to both and is past the max wait (5s).
    assert_eq!(
        executor.ids(Kind::Execute),
        auth_ids,
        "want exactly 2 calls (round 0 only)"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_429_enforces_cooldown_wait_even_with_large_max_wait() {
    let h = Harness::new(Settings::default());
    let auth = expired_cooldown_auth(&h, "auth-large-maxwait", "google", MODEL);
    let auth = h.add(auth, &[MODEL]);

    let err429 = ExecError::upstream(429, "RESOURCE_EXHAUSTED");
    let attempted: HashSet<String> = HashSet::from([auth.id.clone()]);
    let providers = providers(&["google"]);

    // With a 30s max wait, a credential tried in the round that failed with
    // 429 must wait at least the floor, not zero.
    let wait = with_selection(&h, |selection| {
        let query = RetryQuery {
            providers: &providers,
            model: MODEL,
            pinned: "",
            attempt: 0,
            default_retry: 5,
            eligibility: Default::default(),
            attempted: &attempted,
        };
        should_retry_after_error(selection, &query, &err429, Duration::from_secs(30))
    })
    .expect("expected a retry when the max wait (30s) covers the floor (10s)");
    assert!(
        wait >= MIN_QUOTA_COOLDOWN_FLOOR,
        "wait = {wait:?}, want >= {MIN_QUOTA_COOLDOWN_FLOOR:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn closest_cooldown_wait_with_attempted_respects_manager_and_provider_cooling_overrides() {
    let model = "compat-model-override";
    let h = Harness::new(Settings::default());
    let mut auth = expired_cooldown_auth(&h, "auth-compat-override", "openai-compatibility", model);
    auth.attributes
        .insert("provider_key".into(), "custom-llm".into());
    let auth = h.add(auth, &[model]);
    let attempted: HashSet<String> = HashSet::from([auth.id.clone()]);

    // Cooling on: the tried credential waits at least the floor after 429.
    let wait = closest_wait(&h, &["openai-compatibility"], model, 429, &attempted);
    assert!(
        wait.is_some_and(|w| w >= MIN_QUOTA_COOLDOWN_FLOOR),
        "cooling enabled: wait = {wait:?}, want >= {MIN_QUOTA_COOLDOWN_FLOOR:?}"
    );

    // The provider turns cooling off.
    h.manager.set_settings(Settings {
        openai_compatibility: vec![OpenAiCompat {
            name: "custom-llm".into(),
            disable_cooling: Some(true),
            ..OpenAiCompat::default()
        }],
        ..Settings::default()
    });

    let wait = closest_wait(&h, &["openai-compatibility"], model, 429, &attempted);
    assert_eq!(
        wait,
        Some(Duration::ZERO),
        "provider cooling disabled: want an immediate retry"
    );
}
