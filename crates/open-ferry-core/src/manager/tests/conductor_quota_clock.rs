// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_quota_clock_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Quota deadlines set by `mark_result` for a model or a whole credential,
//! with and without the provider's retry hint: both deadlines are set and
//! agree.
//!
//! Deviations from upstream:
//! - Upstream also checks the deadlines carry no monotonic clock reading
//!   (`t == t.Round(0)`); chrono timestamps have none. In its place each
//!   test checks the deadline is the manager clock's time plus the
//!   cooldown, which is what reading the wall clock protects.

use std::time::Duration;

use super::support::*;
use crate::auth::AuthError;
use crate::manager::{CallResult, Settings};

fn quota_exceeded() -> Option<AuthError> {
    Some(AuthError {
        http_status: 429,
        message: "quota exceeded".into(),
        ..AuthError::default()
    })
}

#[tokio::test(start_paused = true)]
async fn quota_deadline_uses_wall_clock_model_explicit_retry() {
    let h = Harness::new(Settings::default());
    let auth = h.add(auth("quota-clock-model-explicit", "codex"), &[]);

    let now = h.now();
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        model: "gpt-5".into(),
        retry_after: Some(Duration::from_secs(3600)),
        error: quota_exceeded(),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let state = updated
        .model_states
        .get("gpt-5")
        .expect("missing model state for gpt-5");
    let retry_deadline = state
        .next_retry_after
        .expect("model next_retry_after is zero");
    let recover_deadline = state
        .quota
        .next_recover_at
        .expect("model quota.next_recover_at is zero");
    assert_eq!(retry_deadline, recover_deadline);
    assert_eq!(retry_deadline, now + chrono::TimeDelta::hours(1));
}

#[tokio::test(start_paused = true)]
async fn quota_deadline_uses_wall_clock_model_fallback_backoff() {
    let h = Harness::new(Settings::default());
    let auth = h.add(auth("quota-clock-model-fallback", "codex"), &[]);

    let now = h.now();
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        model: "gpt-5".into(),
        error: quota_exceeded(),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let state = updated
        .model_states
        .get("gpt-5")
        .expect("missing model state for gpt-5");
    let retry_deadline = state
        .next_retry_after
        .expect("fallback model next_retry_after is zero");
    let recover_deadline = state
        .quota
        .next_recover_at
        .expect("fallback model quota.next_recover_at is zero");
    assert_eq!(retry_deadline, recover_deadline);
    // The first backoff level is the one-second base.
    assert_eq!(retry_deadline, now + chrono::TimeDelta::seconds(1));
}

#[tokio::test(start_paused = true)]
async fn quota_deadline_uses_wall_clock_auth_explicit_retry() {
    let h = Harness::new(Settings::default());
    let auth = h.add(auth("quota-clock-auth-explicit", "codex"), &[]);

    let now = h.now();
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        model: "gpt-5".into(),
        credential_scope: true,
        retry_after: Some(Duration::from_secs(3600)),
        error: quota_exceeded(),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let retry_deadline = updated
        .next_retry_after
        .expect("auth next_retry_after is zero");
    let recover_deadline = updated
        .quota
        .next_recover_at
        .expect("auth quota.next_recover_at is zero");
    assert_eq!(retry_deadline, recover_deadline);
    assert_eq!(retry_deadline, now + chrono::TimeDelta::hours(1));
}

#[tokio::test(start_paused = true)]
async fn quota_deadline_uses_wall_clock_auth_fallback_backoff() {
    let h = Harness::new(Settings::default());
    let auth = h.add(auth("quota-clock-auth-fallback", "codex"), &[]);

    let now = h.now();
    // No model: an auth-level failure.
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        error: quota_exceeded(),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let retry_deadline = updated
        .next_retry_after
        .expect("auth fallback next_retry_after is zero");
    let recover_deadline = updated
        .quota
        .next_recover_at
        .expect("auth fallback quota.next_recover_at is zero");
    assert_eq!(retry_deadline, recover_deadline);
    assert_eq!(retry_deadline, now + chrono::TimeDelta::seconds(1));
}
