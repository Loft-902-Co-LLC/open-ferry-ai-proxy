// Ported from CLIProxyAPI sdk/cliproxy/auth/force_refresh_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Forced refreshes: of one credential, which clears its error, and of
//! every credential that can refresh, a bounded number at a time.
//!
//! Deviations from upstream:
//! - `ForceRefreshAll_CanceledContextSkipsExecutors`: cancellation is
//!   dropping the future (mod.rs), so a call dropped before it runs reaches
//!   no executor, as upstream's canceled context does, but gives no results
//!   to check for `context canceled`.
//! - `ForceRefreshAll_DynamicCancellationSkipsRemaining`: dropping the call
//!   while two refreshes run stops the other four from starting, as
//!   upstream does, but also drops the two running, so there are no results
//!   and no successes to count. Upstream's two finish, and it checks their
//!   rotated tokens are saved and published whatever the cancellation; here
//!   neither is, as their refreshes never return.
//! - `RefreshWorkersResolution`: the worker count is unsigned, so upstream's
//!   negative case can't be expressed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde_json::json;

use super::support::*;
use crate::auth::{Auth, AuthError, Status};
use crate::manager::Settings;
use crate::manager::classify::has_unauthorized_auth_failure;

const PROVIDER: &str = "antigravity";
/// Long enough to hold a refresh until the test lets it go.
const HELD: Duration = Duration::from_secs(3600);

/// Upstream's `countingRefreshExecutor`.
fn counting_refresh_executor() -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new(PROVIDER);
    executor.set_refresh(|auth: &Auth| {
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("refreshed-token"));
        Ok(auth)
    });
    executor
}

/// Upstream's `concurrencyTrackingRefreshExecutor`: refreshes are held for
/// [`HELD`], and the returned counter keeps the most that ran at once.
fn concurrency_tracking_refresh_executor() -> (Arc<FakeExecutor>, Arc<AtomicUsize>) {
    let executor = FakeExecutor::new(PROVIDER);
    executor.set_refresh_delay(HELD);
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let tracker: Weak<FakeExecutor> = Arc::downgrade(&executor);
    let max = max_concurrent.clone();
    executor.set_refresh(move |auth: &Auth| {
        // A refresh is recorded when it enters and gets here as it ends, so
        // the ones entered and not yet ended, this one included, are those
        // running now. Only entries happen between two ends, so this sees
        // the peak.
        let entered = tracker.upgrade().map_or(0, |e| e.refresh_count());
        let running = entered - completed.fetch_add(1, Ordering::SeqCst);
        max.fetch_max(running, Ordering::SeqCst);
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("refreshed-token"));
        Ok(auth)
    });
    (executor, max_concurrent)
}

fn unauthorized_auth(id: &str) -> Auth {
    let mut auth = auth_with_metadata(
        id,
        PROVIDER,
        json!({"access_token": "old-tok", "refresh_token": "ref-tok"}),
    );
    auth.status = Status::Error;
    auth.unavailable = true;
    auth.last_error = Some(AuthError {
        code: "unauthorized".into(),
        message: "token revoked".into(),
        ..AuthError::default()
    });
    auth
}

fn two_workers() -> Settings {
    Settings {
        refresh_workers: 2,
        ..Settings::default()
    }
}

fn add_six(h: &Harness) {
    for i in 0..6 {
        h.add(
            auth_with_metadata(
                &format!("ag-{i}"),
                PROVIDER,
                json!({"refresh_token": format!("ref-{i}")}),
            ),
            &[],
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_auth_clears_error_and_refreshes() {
    let h = Harness::new(Settings::default());
    let executor = counting_refresh_executor();
    h.executor(&executor);

    let auth = unauthorized_auth("ag-err");
    assert!(
        has_unauthorized_auth_failure(&auth),
        "expected has_unauthorized_auth_failure to be true initially"
    );
    h.add(auth, &[]);

    let refreshed = h
        .manager
        .force_refresh("ag-err")
        .await
        .expect("force refresh");
    assert_eq!(refreshed.status, Status::Active);
    assert!(
        !refreshed.unavailable,
        "expected unavailable to be false after force refresh"
    );
    assert!(
        refreshed.last_error.is_none(),
        "expected last_error to be cleared"
    );
    assert_eq!(executor.refresh_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_all() {
    let h = Harness::new(Settings::default());
    let executor = counting_refresh_executor();
    h.executor(&executor);

    h.add(
        auth_with_metadata("ag-1", PROVIDER, json!({"refresh_token": "ref-1"})),
        &[],
    );
    h.add(
        auth_with_metadata("ag-2", PROVIDER, json!({"refresh_token": "ref-2"})),
        &[],
    );
    h.add(
        auth_with_metadata("ag-no-ref", PROVIDER, json!({"access_token": "no-refresh"})),
        &[],
    );

    let results = h.manager.force_refresh_all().await;
    assert_eq!(
        results.len(),
        2,
        "only credentials with refresh_token are refreshed"
    );
    for result in &results {
        assert!(
            result.success,
            "expected success for {}, got error: {:?}",
            result.id, result.error
        );
    }
    assert_eq!(executor.refresh_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_auth_preserves_error_on_failure() {
    // No executor is registered, so the refresh fails.
    let h = Harness::new(Settings::default());
    h.add(unauthorized_auth("ag-fail"), &[]);

    assert!(
        h.manager.force_refresh("ag-fail").await.is_err(),
        "expected force refresh to fail when executor is missing"
    );

    let current = h.get("ag-fail");
    assert_eq!(
        current.status,
        Status::Error,
        "status should remain Error on failure"
    );
    assert!(
        current.unavailable,
        "unavailable should remain true on failure"
    );
    assert!(
        current.last_error.is_some(),
        "last_error should not be wiped on failure"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_all_workers_bounded() {
    let h = Harness::new(two_workers());
    let (executor, max_concurrent) = concurrency_tracking_refresh_executor();
    h.executor(&executor);
    add_six(&h);

    let manager = h.manager.clone();
    let task = tokio::spawn(async move { manager.force_refresh_all().await });

    // Two workers enter the refresh; unbounded, the rest would too.
    settle().await;
    assert_eq!(executor.refresh_count(), 2, "two workers entered");

    // Release the held refreshes; the rest aren't held.
    executor.set_refresh_delay(Duration::ZERO);
    tokio::time::advance(HELD).await;
    let results = task.await.expect("force refresh task");

    assert_eq!(results.len(), 6);
    for result in &results {
        assert!(
            result.success,
            "expected success for {}, got error: {:?}",
            result.id, result.error
        );
    }
    let max = max_concurrent.load(Ordering::SeqCst);
    assert!(max <= 2, "expected max concurrent calls <= 2, got {max}");
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_all_canceled_context_skips_executors() {
    let h = Harness::new(two_workers());
    let executor = counting_refresh_executor();
    h.executor(&executor);
    add_six(&h);

    // Canceled before it runs.
    drop(h.manager.force_refresh_all());
    settle().await;

    assert_eq!(
        executor.refresh_count(),
        0,
        "expected 0 refresh calls when canceled"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_force_refresh_all_dynamic_cancellation_skips_remaining() {
    let h = Harness::new(two_workers());
    let (executor, _max) = concurrency_tracking_refresh_executor();
    h.executor(&executor);
    add_six(&h);

    let manager = h.manager.clone();
    let task = tokio::spawn(async move { manager.force_refresh_all().await });

    // Wait for two workers to enter the refresh.
    settle().await;
    assert_eq!(executor.refresh_count(), 2);

    // Cancel while the first two are running, then release them.
    task.abort();
    assert!(
        task.await.expect_err("aborted").is_cancelled(),
        "the call was canceled"
    );
    executor.set_refresh_delay(Duration::ZERO);
    tokio::time::advance(HELD).await;
    settle().await;

    // Exactly two entered; the other four were canceled in the queue.
    assert_eq!(executor.refresh_count(), 2, "expected exactly 2 entered");
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_workers_resolution() {
    let h = Harness::new(Settings::default());

    // Default when unset.
    assert_eq!(h.manager.refresh_workers(), 16);

    // Zero keeps the default.
    h.manager.set_settings(Settings {
        refresh_workers: 0,
        ..Settings::default()
    });
    assert_eq!(h.manager.refresh_workers(), 16);

    // A positive count overrides it.
    h.manager.set_settings(Settings {
        refresh_workers: 4,
        ..Settings::default()
    });
    assert_eq!(h.manager.refresh_workers(), 4);
}
