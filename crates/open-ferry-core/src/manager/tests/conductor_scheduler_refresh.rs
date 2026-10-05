// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_scheduler_refresh_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Refresh failures and successes as picks see them: a 401 refresh failure
//! makes the credential terminal and stops its refreshes, an unexpired token
//! keeps it usable, a refresh folds into concurrent changes, and picks follow
//! the registry and token expiry without a scheduler rebuild.
//!
//! Deviations from upstream:
//! - Picks are calls through the manager (`execute`), so each test registers
//!   an executor for the provider; upstream's scheduler-only picks need none.
//!   A failed pick is the call's error; a successful one is the credential
//!   the executor got.
//! - `manager_refresh_scheduler_entry_rebuilds_supported_model_set_after_model_registration`
//!   and `manager_pick_next_rebuilds_scheduler_after_model_cooldown_error`:
//!   there's no scheduler index to lag behind the registry, so the "before"
//!   pick runs before the registry lists the model for the credential rather
//!   than before `RefreshSchedulerEntry`.
//! - `manager_terminal_oauth_failure_*`: upstream's nil selector falls back to
//!   round robin, so both cases take the same pick, as they do here. The
//!   cause is the summary the port keeps (`ExecError::cause`), not the
//!   wrapped error.
//! - Refreshes that upstream holds on a channel wait on a refresh delay in
//!   paused Tokio time; the concurrent change happens while they wait.
//! - `manager_persist_skip_persist_advances_watermark_and_blocks_older_generation`:
//!   the stale save is `persist` at generation 1.
//! - `TestManager_PrepareRequestAuth_DoesNotRollbackConcurrentlyRefreshedToken`
//!   is dropped: request preparation isn't ported.

use super::support::*;

use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta, TimeZone, Utc};
use serde_json::json;

use crate::auth::{Auth, AuthError, ModelState, QuotaState, Status, Timestamp};
use crate::exec::{ErrorKind, ExecError};
use crate::manager::classify::{ErrView, has_unauthorized_auth_failure};
use crate::manager::lifecycle::Save;
use crate::manager::refresh::{next_refresh_check_at, should_refresh};
use crate::manager::select::is_auth_blocked_for_model;
use crate::manager::{CallResult, ManagerError, Settings};

/// How long a refresh waits in the executor, so a test can change the
/// credential meanwhile.
const REFRESH_DELAY: Duration = Duration::from_secs(1);

/// A time as RFC 3339, as Go's `time.RFC3339` writes it.
fn rfc3339(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Upstream's `unauthorizedRefreshTestExecutor` error.
fn unauthorized_refresh_error() -> ExecError {
    ExecError::new(
        ErrorKind::Upstream,
        "token refresh failed with status 401: invalid_grant",
    )
}

/// An executor for `provider` whose refreshes fail with a 401.
fn unauthorized_refresh_executor(provider: &str) -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new(provider);
    executor.set_refresh(|_| Err(unauthorized_refresh_error()));
    executor
}

/// Upstream's `blockingSuccessRefreshExecutor`: the refresh returns the
/// credential with `access_token` set to `token` and an hour to live.
fn token_refresh_executor(h: &Harness, provider: &str, token: &'static str) -> Arc<FakeExecutor> {
    let executor = FakeExecutor::new(provider);
    let clock = h.clock.clone();
    executor.set_refresh(move |auth: &Auth| {
        let mut refreshed = auth.clone();
        refreshed
            .metadata
            .insert("access_token".into(), json!(token));
        refreshed.metadata.insert(
            "expired".into(),
            json!(rfc3339(clock.now() + TimeDelta::hours(1))),
        );
        Ok(refreshed)
    });
    executor
}

/// Refreshes `id` in a task, runs `during` while the executor's refresh
/// waits, then lets the refresh finish and returns its outcome.
async fn refresh_around(
    h: &Harness,
    executor: &FakeExecutor,
    id: &str,
    during: impl FnOnce(),
) -> Result<Arc<Auth>, ManagerError> {
    executor.set_refresh_delay(REFRESH_DELAY);
    let manager = h.manager.clone();
    let owned = id.to_owned();
    let task = tokio::spawn(async move { manager.force_refresh(&owned).await });
    settle().await;
    assert_eq!(executor.refresh_count(), 1, "the refresh started");
    during();
    tokio::time::advance(REFRESH_DELAY).await;
    task.await.expect("the refresh task")
}

/// The live credential `id`, to change before an `update`.
fn current(h: &Harness, id: &str) -> Auth {
    (*h.get(id)).clone()
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_unauthorized_failure_stops_auto_refresh_retry() {
    let h = Harness::new(Settings::default());
    let executor = unauthorized_refresh_executor("codex");
    h.executor(&executor);
    let id = "unauthorized-refresh";
    h.add(
        auth_with_metadata(id, "codex", json!({"email": "x@example.com"})),
        &[],
    );

    let _ = h.manager.force_refresh(id).await;

    let updated = h.get(id);
    let last = updated
        .last_error
        .as_ref()
        .expect("the unauthorized refresh failure is recorded");
    assert_eq!(last.http_status, 401);
    assert_eq!(last.code, "unauthorized");
    assert_eq!(updated.next_refresh_after, None);
    let now = h.now();
    // A lead makes the check meaningful: without the 401 it would refresh.
    assert!(!should_refresh(&updated, Some(Duration::from_secs(1)), now));
    assert_eq!(
        next_refresh_check_at(now, &updated, Some(Duration::from_secs(1))),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn manager_terminal_oauth_failure_returns_terminal_auth_error() {
    for case in ["fast_path_round_robin", "legacy_path_nil_selector"] {
        let h = Harness::new(Settings::default());
        let executor = unauthorized_refresh_executor("codex");
        h.executor(&executor);
        let auth_id = format!("unauthorized-terminal-{case}");
        let healthy_id = format!("healthy-{case}");
        h.add(
            auth_with_metadata(&auth_id, "codex", json!({"email": "x@example.com"})),
            &["any-model"],
        );

        let _ = h.manager.force_refresh(&auth_id).await;

        let err = pick_by_call(&h, &executor, "codex", "any-model")
            .await
            .expect_err(case);
        assert!(err.terminal_auth, "{case}: {err}");
        let parts = ErrView::Exec(&err)
            .auth_parts()
            .unwrap_or_else(|| panic!("{case}: not an auth error: {err}"));
        assert!(!parts.retryable, "{case}");
        assert_eq!(err.status, 503, "{case}");
        assert_eq!(
            err.to_string(),
            "auth_unavailable: no auth available (last upstream error: unauthorized: token: [REDACTED])",
            "{case}"
        );

        // A healthy credential is picked instead.
        let mut healthy = auth(&healthy_id, "codex");
        healthy.status = Status::Active;
        h.add(healthy, &["any-model"]);
        let picked = pick_by_call(&h, &executor, "codex", "any-model")
            .await
            .unwrap_or_else(|err| panic!("{case}: healthy pick: {err}"));
        assert_eq!(picked, healthy_id, "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_quota_cooldown_does_not_classify_as_terminal_auth() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("codex");
    h.executor(&executor);
    let mut cooling = auth("quota-cooldown-auth", "codex");
    cooling.unavailable = true;
    cooling.quota = QuotaState {
        exceeded: true,
        reason: "credential_quota".into(),
        next_recover_at: Some(h.now() + TimeDelta::minutes(1)),
        backoff_level: 0,
    };
    h.add(cooling, &["model-cooldown"]);

    let err = pick_by_call(&h, &executor, "codex", "model-cooldown")
        .await
        .expect_err("a cooling credential");
    assert!(!err.terminal_auth);
    assert_eq!(err.kind, ErrorKind::ModelCooldown);
    assert_eq!(err.status, 429);
    assert_eq!(err.retry_after, Some(Duration::from_secs(60)));
    assert_eq!(
        err.to_string(),
        r#"{"error":{"code":"model_cooldown","message":"All credentials for model model-cooldown are cooling down via provider codex","model":"model-cooldown","provider":"codex","reset_seconds":60,"reset_time":"1m0s"}}"#
    );
}

#[tokio::test(start_paused = true)]
async fn manager_terminal_oauth_failure_overrides_preexisting_model_state_error() {
    for case in ["fast_path_round_robin", "legacy_path_nil_selector"] {
        let h = Harness::new(Settings::default());
        let executor = unauthorized_refresh_executor("codex");
        h.executor(&executor);
        let auth_id = format!("unauthorized-stale-model-{case}");
        let mut stale = auth_with_metadata(&auth_id, "codex", json!({"email": "x@example.com"}));
        stale.model_states.insert(
            "any-model".into(),
            ModelState {
                last_error: Some(AuthError {
                    code: "rate_limit_exceeded".into(),
                    message: "historical rate limit".into(),
                    http_status: 429,
                    ..AuthError::default()
                }),
                updated_at: Some(h.now() - TimeDelta::minutes(10)),
                ..ModelState::default()
            },
        );
        h.add(stale, &["any-model"]);

        let _ = h.manager.force_refresh(&auth_id).await;

        let err = pick_by_call(&h, &executor, "codex", "any-model")
            .await
            .expect_err(case);
        assert!(err.terminal_auth, "{case}: {err}");
        let cause = err
            .cause
            .as_deref()
            .unwrap_or_else(|| panic!("{case}: no cause"));
        assert!(cause.contains("unauthorized"), "{case}: {cause}");
        assert!(!cause.contains("historical rate limit"), "{case}: {cause}");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_scheduler_entry_rebuilds_supported_model_set_after_model_registration() {
    for case in ["register", "update"] {
        let h = Harness::new(Settings::default());
        let executor = FakeExecutor::new("gemini");
        h.executor(&executor);
        let id = format!("refresh-entry-{case}");
        h.add(auth(&id, "gemini"), &[]);
        if case == "update" {
            let mut updated = current(&h, &id);
            updated.metadata = json!({"updated": true})
                .as_object()
                .cloned()
                .expect("an object");
            h.manager
                .update(updated)
                .expect("update")
                .expect("a registered credential");
        }

        let err = pick_by_call(&h, &executor, "gemini", "scheduler-refresh-model")
            .await
            .expect_err(case);
        assert_eq!(err.kind.code(), Some("auth_not_found"), "{case}");

        h.models.register(&id, &["scheduler-refresh-model"]);

        let picked = pick_by_call(&h, &executor, "gemini", "scheduler-refresh-model")
            .await
            .unwrap_or_else(|err| panic!("{case}: {err}"));
        assert_eq!(picked, id, "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn manager_pick_next_rebuilds_scheduler_after_model_cooldown_error() {
    const MODEL: &str = "scheduler-cooldown-rebuild-model";
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("gemini");
    h.executor(&executor);
    h.add(auth("cooldown-stale-old", "gemini"), &[MODEL]);
    h.manager.mark_result(&CallResult {
        auth_id: "cooldown-stale-old".into(),
        provider: "gemini".into(),
        model: MODEL.into(),
        success: false,
        error: Some(AuthError {
            http_status: 429,
            message: "quota".into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    });
    h.add(auth("cooldown-stale-new", "gemini"), &[]);

    let err = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect_err("only the cooling credential serves the model");
    assert_eq!(err.kind, ErrorKind::ModelCooldown);
    assert_eq!(err.retry_after, Some(Duration::from_secs(1)));

    h.models.register("cooldown-stale-new", &[MODEL]);

    let picked = pick_by_call(&h, &executor, "gemini", MODEL)
        .await
        .expect("the new credential");
    assert_eq!(picked, "cooldown-stale-new");
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_unauthorized_failure_retains_unexpired_access_token() {
    let h = Harness::new(Settings::default());
    h.executor(&unauthorized_refresh_executor("codex"));
    let id = "unauthorized-refresh-valid-token";
    let mut a = auth_with_metadata(
        id,
        "codex",
        json!({
            "email": "active@example.com",
            "access_token": "valid-future-access-token",
            "expired": rfc3339(h.now() + TimeDelta::hours(48)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = h.manager.force_refresh(id).await;

    let updated = h.get(id);
    assert!(!updated.unavailable);
    assert_ne!(updated.status, Status::Error);
    assert!(updated.next_refresh_after.is_some());
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_failure_preserves_preexisting_unavailable_state() {
    let h = Harness::new(Settings::default());
    h.executor(&unauthorized_refresh_executor("codex"));
    let id = "unauthorized-refresh-preexisting-unavailable";
    let mut a = auth_with_metadata(
        id,
        "codex",
        json!({
            "email": "quota@example.com",
            "access_token": "valid-future-access-token",
            "expired": rfc3339(h.now() + TimeDelta::hours(48)),
        }),
    );
    a.status = Status::Error;
    a.status_message = "quota_exceeded".into();
    a.unavailable = true;
    h.add(a, &[]);

    let _ = h.manager.force_refresh(id).await;

    let updated = h.get(id);
    assert!(updated.unavailable);
    assert_eq!(updated.status_message, "quota_exceeded");
    assert!(updated.next_refresh_after.is_some());
    assert!(!has_unauthorized_auth_failure(&updated));
    assert!(next_refresh_check_at(h.now(), &updated, Some(Duration::from_secs(1))).is_some());
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_transient_failure_expired_token_marked_unavailable_with_retry() {
    let h = Harness::new(Settings::default());
    let executor = FakeExecutor::new("codex");
    executor.set_refresh(|_| {
        Err(ExecError::new(
            ErrorKind::Upstream,
            "upstream 503 service unavailable",
        ))
    });
    h.executor(&executor);
    let id = "transient-refresh-expired-token";
    let mut a = auth_with_metadata(
        id,
        "codex",
        json!({
            "email": "expired@example.com",
            "access_token": "expired-access-token",
            "expired": rfc3339(h.now() - TimeDelta::hours(24)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = h.manager.force_refresh(id).await;

    let updated = h.get(id);
    assert!(updated.unavailable);
    assert_eq!(updated.status, Status::Error);
    assert!(updated.next_refresh_after.is_some());
}

#[test]
fn manager_refresh_auth_expired_access_token_blocked_from_selection() {
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("a valid time");
    let mut a = auth_with_metadata(
        "expired-token-blocked",
        "codex",
        json!({
            "email": "user@example.com",
            "access_token": "expired-token",
            "expired": rfc3339(now - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Active;

    let (blocked, _reason, _) = is_auth_blocked_for_model(&a, "gpt-5", now);
    assert!(blocked);
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_preserves_concurrent_cooldown_mutation_during_refresh() {
    let h = Harness::new(Settings::default());
    let executor = unauthorized_refresh_executor("codex");
    h.executor(&executor);
    let id = "concurrent-refresh-auth";
    let mut a = auth_with_metadata(
        id,
        "codex",
        json!({
            "email": "active@example.com",
            "access_token": "valid-future-access-token",
            "expired": rfc3339(h.now() + TimeDelta::hours(48)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        // A 503 cooldown lands on the live credential, as upstream writes it
        // under the manager's lock.
        let mut state = h.manager.lock();
        let entry = state.auths.get_mut(id).expect("registered");
        let live = Arc::make_mut(&mut entry.auth);
        live.unavailable = true;
        live.status = Status::Error;
        live.status_message = "cooling_503".into();
    })
    .await;

    let updated = h.get(id);
    assert!(updated.unavailable);
    assert_eq!(updated.status_message, "cooling_503");
}

#[tokio::test(start_paused = true)]
async fn scheduler_ready_auth_demoted_when_token_expires() {
    let h = Harness::new(Settings::default());
    let executor = unauthorized_refresh_executor("codex");
    h.executor(&executor);
    let id = "short-lived-auth";
    let mut a = auth_with_metadata(
        id,
        "codex",
        json!({
            "email": "short@example.com",
            "access_token": "short-lived-access-token",
            "expired": rfc3339(h.now() + TimeDelta::minutes(10)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &["gpt-5-short-test"]);

    let picked = pick_by_call(&h, &executor, "codex", "gpt-5-short-test")
        .await
        .expect("an unexpired token");
    assert_eq!(picked, id);

    // The token's expiry moves to the past without an update.
    {
        let mut state = h.manager.lock();
        let entry = state.auths.get_mut(id).expect("registered");
        Arc::make_mut(&mut entry.auth).metadata.insert(
            "expired".into(),
            json!(rfc3339(h.now() - TimeDelta::minutes(10))),
        );
    }

    let err = pick_by_call(&h, &executor, "codex", "gpt-5-short-test")
        .await
        .expect_err("an expired token");
    assert_eq!(err.kind.code(), Some("auth_unavailable"));
    assert_eq!(executor.ids(Kind::Execute), [id]);
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_preserves_concurrent_proxy_url_and_metadata() {
    let h = Harness::with_store(Settings::default());
    let executor = token_refresh_executor(&h, "antigravity", "refreshed-access-token");
    h.executor(&executor);
    let id = "test-antigravity";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "old-access-token",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        let mut changed = current(&h, id);
        changed.proxy_url = "http://127.0.0.1:7890".into();
        changed
            .metadata
            .insert("proxy_url".into(), json!("http://127.0.0.1:7890"));
        changed
            .metadata
            .insert("custom_field".into(), json!("custom_value"));
        h.manager
            .update(changed)
            .expect("concurrent update")
            .expect("a registered credential");
    })
    .await;

    let updated = h.get(id);
    assert_eq!(
        updated.metadata_str("access_token"),
        Some("refreshed-access-token")
    );
    assert_eq!(updated.proxy_url, "http://127.0.0.1:7890");
    assert_eq!(
        updated.metadata_str("proxy_url"),
        Some("http://127.0.0.1:7890")
    );
    assert_eq!(updated.metadata_str("custom_field"), Some("custom_value"));

    let persisted = h.store.stored(id).expect("a saved credential");
    assert_eq!(persisted.proxy_url, "http://127.0.0.1:7890");
    assert_eq!(
        persisted.metadata_str("proxy_url"),
        Some("http://127.0.0.1:7890")
    );
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_in_place_mutating_executor_non_token_metadata_and_concurrent_proxy() {
    let h = Harness::with_store(Settings::default());
    let executor = FakeExecutor::new("antigravity");
    let clock = h.clock.clone();
    executor.set_refresh(move |auth: &Auth| {
        let mut refreshed = auth.clone();
        let metadata = &mut refreshed.metadata;
        metadata.insert("access_token".into(), json!("new-access-token"));
        metadata.insert(
            "expired".into(),
            json!(rfc3339(clock.now() + TimeDelta::hours(1))),
        );
        metadata.insert("account_id".into(), json!("acc-456"));
        metadata.insert("project_id".into(), json!("proj-789"));
        Ok(refreshed)
    });
    h.executor(&executor);
    let id = "inplace-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "old-token",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        let mut changed = current(&h, id);
        changed.proxy_url = "http://10.0.0.1:8888".into();
        changed
            .metadata
            .insert("proxy_url".into(), json!("http://10.0.0.1:8888"));
        h.manager
            .update(changed)
            .expect("concurrent update")
            .expect("a registered credential");
    })
    .await;

    let updated = h.get(id);
    assert_eq!(
        updated.metadata_str("access_token"),
        Some("new-access-token")
    );
    assert_eq!(updated.metadata_str("account_id"), Some("acc-456"));
    assert_eq!(updated.metadata_str("project_id"), Some("proj-789"));
    assert_eq!(updated.proxy_url, "http://10.0.0.1:8888");
    assert_eq!(
        updated.metadata_str("proxy_url"),
        Some("http://10.0.0.1:8888")
    );
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_concurrently_cleared_proxy_not_resurrected() {
    let h = Harness::with_store(Settings::default());
    let executor = token_refresh_executor(&h, "antigravity", "refreshed-access-token");
    h.executor(&executor);
    let id = "clear-proxy-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "old-token",
            "proxy_url": "http://initial-proxy.local:8080",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Active;
    a.proxy_url = "http://initial-proxy.local:8080".into();
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        let mut changed = current(&h, id);
        changed.proxy_url.clear();
        changed.metadata.remove("proxy_url");
        h.manager
            .update(changed)
            .expect("concurrent clear")
            .expect("a registered credential");
    })
    .await;

    let updated = h.get(id);
    assert_eq!(updated.proxy_url, "");
    assert!(
        !updated.metadata.contains_key("proxy_url"),
        "proxy_url = {:?}",
        updated.metadata.get("proxy_url")
    );
    assert_eq!(
        updated.metadata_str("access_token"),
        Some("refreshed-access-token")
    );
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_stale_registration_epoch_discarded() {
    let h = Harness::with_store(Settings::default());
    let executor = token_refresh_executor(&h, "antigravity", "refreshed-access-token");
    h.executor(&executor);
    let id = "re-register-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "original-token",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        h.manager.remove(id);
        let mut again = auth_with_metadata(
            id,
            "antigravity",
            json!({
                "type": "antigravity",
                "access_token": "freshly-registered-different-token",
                "expired": rfc3339(h.now() + TimeDelta::hours(48)),
            }),
        );
        again.status = Status::Active;
        h.add(again, &[]);
    })
    .await;

    assert_eq!(
        h.get(id).metadata_str("access_token"),
        Some("freshly-registered-different-token")
    );
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_restores_status_from_error_to_active() {
    let h = Harness::new(Settings::default());
    let executor = token_refresh_executor(&h, "antigravity", "refreshed-access-token");
    h.executor(&executor);
    let id = "error-status-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "expired-token",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Error;
    a.status_message = "token expired".into();
    a.unavailable = true;
    a.last_error = Some(AuthError {
        message: "token expired".into(),
        ..AuthError::default()
    });
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {}).await;

    let updated = h.get(id);
    assert_eq!(updated.status, Status::Active);
    assert!(!updated.unavailable);
    assert_eq!(updated.status_message, "");
    assert_eq!(updated.last_error, None);
    assert!(updated.last_refreshed_at.is_some());
}

#[tokio::test(start_paused = true)]
async fn manager_persist_skip_persist_advances_watermark_and_blocks_older_generation() {
    let h = Harness::with_store(Settings::default());
    let id = "watermark-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "gen1-token",
            "proxy_url": "http://initial.proxy",
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);
    let (epoch, generation) = h.versions(id);
    assert_eq!(generation, 1);

    let mut gen1 = current(&h, id);
    gen1.metadata
        .insert("stale_sentinel".into(), json!("must_not_persist"));

    // The file watcher's update from disk, which isn't saved.
    let saves = h.store.save_count();
    let mut watcher = gen1.clone();
    watcher.metadata.remove("stale_sentinel");
    watcher.proxy_url = "http://user-disk-edit.proxy".into();
    watcher
        .metadata
        .insert("proxy_url".into(), json!("http://user-disk-edit.proxy"));
    h.manager
        .update_unsaved(watcher.clone())
        .expect("watcher update")
        .expect("a registered credential");
    assert_eq!(
        h.store.save_count(),
        saves,
        "the watcher's update was saved"
    );

    // The older generation-1 snapshot saves afterwards.
    h.manager
        .persist(&gen1, epoch, 1, Save::Yes)
        .expect("persist");

    assert_eq!(h.store.save_count(), saves, "the stale snapshot was saved");
    let saved = h.store.stored(id).expect("a saved credential");
    assert!(
        !saved.metadata.contains_key("stale_sentinel"),
        "stale generation-1 snapshot was persisted"
    );

    let mut gen3 = watcher.clone();
    gen3.metadata
        .insert("access_token".into(), json!("gen3-token"));
    h.manager
        .update(gen3)
        .expect("gen3 update")
        .expect("a registered credential");

    let saved = h.store.stored(id).expect("a saved credential");
    assert_eq!(
        saved.metadata_str("proxy_url"),
        Some("http://user-disk-edit.proxy")
    );
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_both_modify_proxy_user_takes_precedence_and_consistent() {
    let h = Harness::with_store(Settings::default());
    let executor = FakeExecutor::new("antigravity");
    let clock = h.clock.clone();
    executor.set_refresh(move |auth: &Auth| {
        let mut refreshed = auth.clone();
        refreshed.proxy_url = "http://executor-returned.proxy:8080".into();
        let metadata = &mut refreshed.metadata;
        metadata.insert(
            "proxy_url".into(),
            json!("http://executor-returned.proxy:8080"),
        );
        metadata.insert("access_token".into(), json!("refreshed-token"));
        metadata.insert(
            "expired".into(),
            json!(rfc3339(clock.now() + TimeDelta::hours(1))),
        );
        // The executor returns no status.
        refreshed.status = Status::Unknown;
        Ok(refreshed)
    });
    h.executor(&executor);
    let id = "conflict-proxy-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "old-token",
            "proxy_url": "http://base.proxy:8080",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Error;
    a.proxy_url = "http://base.proxy:8080".into();
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        let mut changed = current(&h, id);
        changed.proxy_url = "http://user-preferred.proxy:9090".into();
        changed.metadata.insert(
            "proxy_url".into(),
            json!("http://user-preferred.proxy:9090"),
        );
        h.manager
            .update(changed)
            .expect("concurrent update")
            .expect("a registered credential");
    })
    .await;

    let final_auth = h.get(id);
    assert_eq!(final_auth.proxy_url, "http://user-preferred.proxy:9090");
    assert_eq!(
        final_auth.metadata_str("proxy_url"),
        Some("http://user-preferred.proxy:9090")
    );
    assert_eq!(
        final_auth.metadata_str("access_token"),
        Some("refreshed-token")
    );
    assert_eq!(final_auth.status, Status::Active);
}

#[tokio::test(start_paused = true)]
async fn manager_refresh_auth_preserves_concurrent_model_cooldown() {
    let h = Harness::new(Settings::default());
    let executor = token_refresh_executor(&h, "antigravity", "refreshed-access-token");
    h.executor(&executor);
    let id = "model-cooldown-auth";
    let mut a = auth_with_metadata(
        id,
        "antigravity",
        json!({
            "type": "antigravity",
            "access_token": "old-token",
            "expired": rfc3339(h.now() - TimeDelta::hours(1)),
        }),
    );
    a.status = Status::Active;
    h.add(a, &[]);

    let _ = refresh_around(&h, &executor, id, || {
        let mut changed = current(&h, id);
        changed.model_states.insert(
            "gemini-pro".into(),
            ModelState {
                status: Status::Error,
                unavailable: true,
                next_retry_after: Some(h.now() + TimeDelta::minutes(10)),
                status_message: "quota exceeded".into(),
                ..ModelState::default()
            },
        );
        h.manager
            .update(changed)
            .expect("update with model cooldown")
            .expect("a registered credential");
    })
    .await;

    let final_auth = h.get(id);
    let state = final_auth
        .model_states
        .get("gemini-pro")
        .expect("the model cooldown is kept");
    assert!(state.unavailable);
    assert_eq!(state.status_message, "quota exceeded");
}
