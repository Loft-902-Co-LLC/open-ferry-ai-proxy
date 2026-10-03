// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_refresh_disabled_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Refreshing disabled credentials, and the backoff after `invalid_grant`:
//! a disabled credential still refreshes, but one disabled after
//! `invalid_grant` never does; an enabled one backs off, doubling from a
//! minute.
//!
//! Deviations from upstream:
//! - Upstream checks the backoff within ten seconds of a minute, two and
//!   four, as wall time passes; the clock here stands still, so the checks
//!   are exact.
//! - `AutoRefreshLoop_DisabledAuthWithInvalidGrantNeverRefreshes` gives the
//!   executor a refresh lead. Upstream's test provider has none, so its
//!   credential wouldn't be scheduled anyway; with a lead, the
//!   `invalid_grant` check is what keeps it off the schedule.

use std::time::Duration;

use chrono::{SecondsFormat, TimeDelta};
use serde_json::json;

use super::support::*;
use crate::auth::{Auth, AuthError, Status};
use crate::exec::{ErrorKind, ExecError};
use crate::manager::Settings;

const PROVIDER: &str = "test-provider";
const INVALID_GRANT_BODY: &str =
    r#"{"error": "invalid_grant", "error_description": "Bad Request"}"#;

/// Upstream's `oauthStatusError`: a status and a message, whose text is
/// `status <code>: <message>`.
fn oauth_status_error(code: u16, msg: &str) -> ExecError {
    ExecError::upstream(code, format!("status {code}: {msg}"))
}

/// Upstream's `mockOAuthErrorExecutor`: a refresh fails with `err`, or sets
/// a new access token.
fn oauth_error_executor(err: Option<ExecError>) -> std::sync::Arc<FakeExecutor> {
    let executor = FakeExecutor::new(PROVIDER);
    executor.set_refresh(move |auth: &Auth| {
        if let Some(err) = &err {
            return Err(err.clone());
        }
        let mut auth = auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("new-valid-token"));
        Ok(auth)
    });
    executor
}

fn oauth_auth(id: &str, extra: serde_json::Value) -> Auth {
    let mut metadata = json!({
        "access_token": "expired-token",
        "refresh_token": "refresh-1",
    });
    if let (Some(map), serde_json::Value::Object(extra)) = (metadata.as_object_mut(), extra) {
        map.extend(extra);
    }
    auth_with_metadata(id, PROVIDER, metadata)
}

#[tokio::test(start_paused = true)]
async fn refresh_auth_for_request_normal_disabled_auth_refreshes_token_successfully() {
    let h = Harness::new(Settings::default());
    let executor = oauth_error_executor(None);
    h.executor(&executor);

    // A disabled credential without invalid_grant refreshes as usual.
    let mut auth = oauth_auth("normal-disabled-auth", json!({}));
    auth.disabled = true;
    auth.status = Status::Disabled;
    h.add(auth, &[]);

    let refreshed = h
        .manager
        .refresh_at_epoch("normal-disabled-auth", "", 0)
        .await
        .expect("successful refresh for normal disabled auth");
    assert_eq!(executor.refresh_count(), 1);
    assert_eq!(
        refreshed.metadata.get("access_token"),
        Some(&json!("new-valid-token"))
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_auth_for_request_disabled_auth_invalid_grant_never_retries() {
    let h = Harness::new(Settings::default());
    let executor = oauth_error_executor(Some(oauth_status_error(400, INVALID_GRANT_BODY)));
    h.executor(&executor);

    let mut auth = oauth_auth("disabled-invalid-grant", json!({}));
    auth.disabled = true;
    auth.status = Status::Disabled;
    h.add(auth, &[]);

    // The first call meets invalid_grant.
    let first = h
        .manager
        .refresh_at_epoch("disabled-invalid-grant", "", 0)
        .await;
    assert!(first.is_err(), "expected refresh error");

    let current = h.get("disabled-invalid-grant");
    // Disabled with invalid_grant: never scheduled again.
    assert_eq!(current.status, Status::Disabled);
    assert_eq!(current.next_refresh_after, None);

    // The second call is refused without reaching the executor.
    let calls_before = executor.refresh_count();
    let second = h
        .manager
        .refresh_at_epoch("disabled-invalid-grant", "", 0)
        .await;
    assert!(second.is_err(), "expected second call to fail");
    assert_eq!(
        executor.refresh_count(),
        calls_before,
        "executor was called again for disabled+invalid_grant"
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_auth_for_request_enabled_auth_invalid_grant_exponential_backoff() {
    let h = Harness::new(Settings::default());
    let executor = oauth_error_executor(Some(oauth_status_error(400, INVALID_GRANT_BODY)));
    h.executor(&executor);

    let expired_at = (h.now() - TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut auth = oauth_auth(
        "enabled-auth-invalid-grant-exp",
        json!({"expires_at": expired_at}),
    );
    auth.status = Status::Active;
    h.add(auth, &[]);

    let id = "enabled-auth-invalid-grant-exp";
    for (attempt, backoff_secs) in [(1, 60), (2, 120), (3, 240)] {
        let started = h.now();
        let _ = h.manager.refresh_at_epoch(id, "", 0).await;
        let current = h.get(id);
        if attempt == 1 {
            assert_eq!(current.status, Status::Error, "attempt 1: status");
        }
        assert_eq!(
            h.refresh_failures(id),
            attempt,
            "attempt {attempt}: refresh failures"
        );
        assert_eq!(
            current.next_refresh_after,
            Some(started + TimeDelta::seconds(backoff_secs)),
            "attempt {attempt}: backoff"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_auto_refresh_loop_disabled_auth_with_invalid_grant_never_refreshes() {
    let h = Harness::new(Settings::default());
    let executor = oauth_error_executor(None);
    executor.set_lead(Some(Duration::from_secs(5 * 60)));
    h.executor(&executor);

    let expired_at = (h.now() - TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut auth = oauth_auth(
        "disabled-invalid-grant-loop",
        json!({"expires_at": expired_at}),
    );
    auth.disabled = true;
    auth.status = Status::Disabled;
    auth.last_error = Some(AuthError {
        http_status: 400,
        message: INVALID_GRANT_BODY.into(),
        ..AuthError::default()
    });
    h.add(auth, &[]);

    h.manager
        .start_auto_refresh(Duration::from_millis(10))
        .expect("start auto refresh");
    // Let the loop run for 100ms.
    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(10)).await;
        settle().await;
    }
    h.manager.stop_auto_refresh();

    assert_eq!(
        executor.refresh_count(),
        0,
        "executor.Refresh called for disabled+invalid_grant auth"
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_auth_for_request_raw_fmt_error_recognizes_invalid_grant() {
    let h = Harness::new(Settings::default());
    // An error with no status, as fmt.Errorf makes it.
    let raw = ExecError::new(
        ErrorKind::Upstream,
        "oauth token refresh failed: invalid_grant: account checkpoint required",
    );
    let executor = oauth_error_executor(Some(raw));
    h.executor(&executor);

    let now = h.now();
    let expired_at = (now - TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut auth = oauth_auth("raw-fmt-invalid-grant", json!({"expires_at": expired_at}));
    auth.status = Status::Active;
    h.add(auth, &[]);

    let _ = h
        .manager
        .refresh_at_epoch("raw-fmt-invalid-grant", "", 0)
        .await;
    assert_eq!(
        h.refresh_failures("raw-fmt-invalid-grant"),
        1,
        "should recognize raw invalid_grant"
    );
    assert_eq!(
        h.get("raw-fmt-invalid-grant").next_refresh_after,
        Some(now + TimeDelta::seconds(60)),
        "backoff, want 1m"
    );
}
