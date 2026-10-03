// Ported from CLIProxyAPI sdk/cliproxy/auth/auto_refresh_loop_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! When the refresh loop next looks at a credential, and how long its timer
//! sleeps; and that dropping the manager stops it.
//!
//! Deviations from upstream:
//! - `next_refresh_check_at` takes the executor's refresh lead where
//!   upstream takes the loop interval and looks the lead up per provider, so
//!   each test passes the lead its provider would have: the lead upstream
//!   registers, or none for an unregistered provider.
//! - `NextRefreshCheckAt_RefreshEvaluatorFallback` is dropped: runtime
//!   refresh evaluators aren't ported (refresh.rs).

use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta, TimeZone, Utc};
use serde_json::json;

use super::support::*;
use crate::auth::{AuthError, Status, Timestamp};
use crate::manager::Settings;
use crate::manager::refresh::{LoopShared, next_refresh_check_at};

fn april_12() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 4, 12, 0, 0, 0)
        .single()
        .expect("valid date")
}

fn rfc3339(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn minutes(n: i64) -> TimeDelta {
    TimeDelta::minutes(n)
}

#[test]
fn next_refresh_check_at_disabled_with_invalid_grant_unschedule() {
    let now = april_12();
    let expiry = now + TimeDelta::hours(1);
    let lead = Duration::from_secs(10 * 60);

    // A disabled credential without invalid_grant is still scheduled.
    let mut normal_disabled = auth_with_metadata(
        "normal-disabled",
        "disabled-schedule",
        json!({"email": "x@example.com", "expires_at": rfc3339(expiry)}),
    );
    normal_disabled.disabled = true;
    normal_disabled.status = Status::Disabled;
    let got = next_refresh_check_at(now, &normal_disabled, Some(lead));
    assert_eq!(got, Some(expiry - minutes(10)));

    // One disabled after invalid_grant is never scheduled.
    let mut invalid_grant_disabled = auth_with_metadata(
        "invalid-grant-disabled",
        "disabled-schedule",
        json!({"email": "x@example.com", "expires_at": rfc3339(expiry)}),
    );
    invalid_grant_disabled.disabled = true;
    invalid_grant_disabled.status = Status::Disabled;
    invalid_grant_disabled.last_error = Some(AuthError {
        http_status: 400,
        message: r#"{"error": "invalid_grant", "error_description": "Bad Request"}"#.into(),
        ..AuthError::default()
    });
    assert_eq!(
        next_refresh_check_at(now, &invalid_grant_disabled, Some(lead)),
        None
    );
}

#[test]
fn next_refresh_check_at_api_key_unschedule() {
    let now = april_12();
    let mut auth = auth("a1", "test");
    auth.attributes.insert("api_key".into(), "k".into());
    // A lead would schedule an OAuth credential; an API key never is.
    assert_eq!(
        next_refresh_check_at(now, &auth, Some(Duration::from_secs(15 * 60))),
        None
    );
    assert_eq!(next_refresh_check_at(now, &auth, None), None);
}

#[test]
fn next_refresh_check_at_next_refresh_after_gate() {
    let now = april_12();
    let next_after = now + minutes(30);
    let mut auth = auth_with_metadata("a1", "test", json!({"email": "x@example.com"}));
    auth.next_refresh_after = Some(next_after);
    assert_eq!(next_refresh_check_at(now, &auth, None), Some(next_after));
}

#[test]
fn next_refresh_check_at_preferred_interval_picks_earliest_candidate() {
    let now = april_12();
    let expiry = now + minutes(20);
    let mut auth = auth_with_metadata(
        "a1",
        "test",
        json!({
            "email": "x@example.com",
            "expires_at": rfc3339(expiry),
            "refresh_interval_seconds": 900,
        }),
    );
    auth.last_refreshed_at = Some(now);
    assert_eq!(
        next_refresh_check_at(now, &auth, None),
        Some(expiry - minutes(15))
    );
}

#[test]
fn next_refresh_check_at_provider_lead_expiry() {
    let now = april_12();
    let expiry = now + TimeDelta::hours(1);
    let auth = auth_with_metadata(
        "a1",
        "provider-lead-expiry",
        json!({"email": "x@example.com", "expires_at": rfc3339(expiry)}),
    );
    assert_eq!(
        next_refresh_check_at(now, &auth, Some(Duration::from_secs(10 * 60))),
        Some(expiry - minutes(10))
    );
}

#[test]
fn next_refresh_check_at_relative_expiry() {
    let now = april_12();
    let issued_at = now - minutes(15);
    let auth = auth_with_metadata(
        "relative-expiry-auth",
        "relative-expiry",
        json!({
            "access_token": "test-access",
            "expires_in": 3600,
            "timestamp": issued_at.timestamp_millis(),
        }),
    );
    assert_eq!(
        next_refresh_check_at(now, &auth, Some(Duration::from_secs(30 * 60))),
        Some(issued_at + TimeDelta::hours(1) - minutes(30))
    );
}

#[test]
fn auth_auto_refresh_loop_timer_wait_clamped_for_long_next_expiry() {
    // Upstream's maxRefreshTimerWait.
    let max_wait = Duration::from_secs(30);
    let queue = LoopShared::new(Duration::from_secs(5));
    let now = april_12();
    queue.upsert("a1", now + TimeDelta::hours(1));
    assert_eq!(queue.next_wait(now), Some(max_wait));

    // A short wait isn't clamped.
    queue.upsert("a2", now + TimeDelta::seconds(10));
    assert_eq!(queue.next_wait(now), Some(Duration::from_secs(10)));
}

#[test]
fn auth_auto_refresh_loop_pop_due_after_system_suspend_resume() {
    let max_wait = Duration::from_secs(30);
    let queue = LoopShared::new(Duration::from_secs(5));
    let before_sleep = Utc
        .with_ymd_and_hms(2026, 4, 12, 10, 0, 0)
        .single()
        .expect("valid date");

    // Due 50 minutes later.
    queue.upsert("gemini-oauth", before_sleep + minutes(50));

    // Before the sleep the wait is clamped, and nothing is due.
    assert_eq!(queue.next_wait(before_sleep), Some(max_wait));
    assert!(queue.pop_due(before_sleep).is_empty());

    // The machine wakes two hours later: the wall clock jumped while the
    // monotonic one stood still. The clamped timer fires and finds it due.
    let after_resume = before_sleep + TimeDelta::hours(2);
    assert_eq!(queue.next_wait(after_resume), Some(Duration::ZERO));
    assert_eq!(queue.pop_due(after_resume), ["gemini-oauth"]);
}

/// Not an upstream test: dropping the last manager handle stops the loop
/// while its workers are refreshing, though their own handles keep the
/// manager alive until those refreshes finish.
#[tokio::test(start_paused = true)]
async fn dropping_the_manager_stops_the_loop() {
    let settings = Settings {
        refresh_workers: 2,
        ..Settings::default()
    };
    let h = Harness::new(settings);
    let executor = FakeExecutor::new("codex");
    executor.set_refresh_delay(Duration::from_millis(20));
    h.executor(&executor);
    for id in ["drop-a", "drop-b", "drop-c"] {
        h.add(
            auth_with_metadata(
                id,
                "codex",
                json!({"refresh_token": "r", "refresh_interval_seconds": 0.001}),
            ),
            &[],
        );
    }
    h.manager
        .start_auto_refresh(Duration::from_millis(1))
        .expect("start auto refresh");
    while executor.refresh_count() < 2 {
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
    }

    drop(h);
    for _ in 0..150 {
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
    }
    assert_eq!(executor.refresh_count(), 2, "refreshes after the drop");
}
