// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cloudflare_520_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A Cloudflare 520 origin error (and other 5xx) is a transient upstream
//! failure, not a Cloudflare challenge: it gets the transient cooldown, which
//! the settings can shorten or turn off and a retry hint can set.
//!
//! Deviations from upstream:
//! - Upstream's global `transientErrorCooldownSeconds` is
//!   `Settings::transient_error_cooldown_seconds`.
//! - Upstream passes `*Error` values as `error` to `isCloudflareChallengeError`;
//!   here they are `ErrView::Auth`, and `errors.New` values are
//!   `ExecError::upstream(0, ..)`.

use super::support::*;

use std::time::Duration;

use chrono::TimeDelta;

use crate::auth::{Auth, AuthError, Status, Timestamp};
use crate::exec::ExecError;
use crate::manager::classify::{
    self, ErrView, is_cloudflare_challenge_message, is_cloudflare_challenge_result_error,
};
use crate::manager::{CallResult, Settings};

const MODEL: &str = "gpt-5.6-sol";
const ORIGIN_520_SHORT: &str = r#"<html><body><div class="cf-error-details cf-error-520"><h1>Web server is returning an unknown error</h1></div></body></html>"#;
const ORIGIN_520_LONG: &str = r#"<html><body><div class="cf-error-details cf-error-520"><h1>Web server is returning an unknown error</h1><p>There is an unknown connection issue between Cloudflare and the origin web server.</p></div></body></html>"#;

fn transient_settings(seconds: i64) -> Settings {
    Settings {
        transient_error_cooldown_seconds: seconds,
        ..Settings::default()
    }
}

fn active(id: &str, provider: &str) -> Auth {
    let mut a = auth(id, provider);
    a.status = Status::Active;
    a
}

fn failure(
    auth_id: &str,
    provider: &str,
    model: &str,
    status: u16,
    message: &str,
    hint: Option<Duration>,
) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: provider.into(),
        model: model.into(),
        success: false,
        retry_after: hint,
        error: Some(AuthError {
            http_status: status,
            message: message.into(),
            ..AuthError::default()
        }),
        ..CallResult::default()
    }
}

fn until(h: &Harness, t: Option<Timestamp>) -> TimeDelta {
    t.expect("retry time") - h.now()
}

fn in_range(d: TimeDelta, lo: i64, hi: i64) -> bool {
    d >= TimeDelta::seconds(lo) && d <= TimeDelta::seconds(hi)
}

#[test]
fn is_cloudflare_challenge_error_message_excludes_origin_errors() {
    let origin520 = r#"<html><head><title>Web server is returning an unknown error</title></head><body><div class="cf-error-details cf-error-520"><h1>Web server is returning an unknown error</h1><p>There is an unknown connection issue between Cloudflare and the origin web server.</p><ul><li>Ray ID: a385507da9eeb4c4</li><li>Error reference number: 520</li><li>Cloudflare Location: Los Angeles</li></ul></div></body></html>"#;
    assert!(
        !is_cloudflare_challenge_message(origin520),
        "expected origin 520 error not to be classified as cloudflare challenge"
    );

    let minimal_origin = r#"<html><body><h1>Web server is returning an unknown error</h1><p>There is an unknown connection issue between Cloudflare and the origin web server.</p></body></html>"#;
    assert!(
        !is_cloudflare_challenge_message(minimal_origin),
        "expected minimal origin error not to be classified as cloudflare challenge"
    );

    let challenges = [
        "cf-mitigated: challenge",
        r#"<html><body><script src="/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1"></script></body></html>"#,
        "cloudflare challenge required",
        // "Just a moment..." without the "cloudflare challenge" phrase.
        r#"<html><head><title>Just a moment...</title></head><body>Checking your browser... cloudflare</body></html>"#,
    ];
    for challenge in challenges {
        assert!(
            is_cloudflare_challenge_message(challenge),
            "expected {challenge:?} to be classified as cloudflare challenge"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare520_origin_error_not_treated_as_challenge() {
    let h = Harness::new(Settings::default());
    let id = "auth-test-520";
    h.add(active(id, "codex"), &[]);

    h.manager
        .mark_result(&failure(id, "codex", MODEL, 520, ORIGIN_520_LONG, None));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");

    assert!(
        !state.quota.exceeded,
        "expected quota.exceeded to be false for 520 origin error"
    );
    assert_eq!(
        state.quota.reason, "",
        "expected quota.reason to be empty for 520 origin error"
    );
    assert_ne!(state.status_message, "cloudflare challenge");
    assert_eq!(
        state.status_message, ORIGIN_520_LONG,
        "expected status_message to preserve upstream error"
    );
    assert_eq!(
        state.last_error.as_ref().map(|e| e.http_status),
        Some(520),
        "expected last_error to retain HTTP 520"
    );
    assert!(
        state.unavailable,
        "expected model to be marked unavailable during transient cooldown"
    );
    let diff = until(&h, state.next_retry_after);
    assert!(
        in_range(diff, 45, 75),
        "expected default transient cooldown of ~60s, got {diff}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare520_custom_transient_cooldown() {
    let h = Harness::new(transient_settings(5));
    let id = "auth-test-520-custom-cooldown";
    h.add(active(id, "codex"), &[]);

    h.manager
        .mark_result(&failure(id, "codex", MODEL, 520, ORIGIN_520_SHORT, None));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    let diff = until(&h, state.next_retry_after);
    assert!(
        in_range(diff, 3, 7),
        "expected custom transient cooldown of ~5s, got {diff}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare520_disabled_transient_cooldown() {
    let h = Harness::new(transient_settings(-1));
    let id = "auth-test-520-disabled-cooldown";
    h.add(active(id, "codex"), &[]);

    h.manager
        .mark_result(&failure(id, "codex", MODEL, 520, ORIGIN_520_SHORT, None));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    assert_eq!(
        state.next_retry_after, None,
        "expected next_retry_after to be zero when transient cooldown disabled"
    );
    assert!(
        !state.unavailable,
        "expected model not to be unavailable when cooldown is disabled"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare520_disable_cooling_auth() {
    let h = Harness::new(Settings::default());
    let id = "auth-test-520-disable-cooling-auth";
    let mut a = auth_with_metadata(id, "codex", serde_json::json!({"disable_cooling": true}));
    a.status = Status::Active;
    h.add(a, &[]);

    h.manager
        .mark_result(&failure(id, "codex", MODEL, 520, ORIGIN_520_SHORT, None));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    assert_eq!(
        state.next_retry_after, None,
        "expected next_retry_after to be zero when disable_cooling is true"
    );
    assert!(
        !state.unavailable,
        "expected model not to be unavailable when disable_cooling is true"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare520_with_retry_after_hint() {
    let h = Harness::new(Settings::default());
    let id = "auth-test-520-hint";
    h.add(active(id, "codex"), &[]);

    h.manager.mark_result(&failure(
        id,
        "codex",
        MODEL,
        520,
        ORIGIN_520_SHORT,
        Some(Duration::from_secs(15)),
    ));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    let diff = until(&h, state.next_retry_after);
    assert!(
        in_range(diff, 10, 20),
        "expected hint-based cooldown of ~15s, got {diff}"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_cloudflare520_disabled_transient_cooldown_ignores_hint() {
    let h = Harness::new(transient_settings(-1));
    let id = "auth-test-520-disabled-with-hint";
    h.add(active(id, "codex"), &[]);

    h.manager.mark_result(&failure(
        id,
        "codex",
        MODEL,
        520,
        ORIGIN_520_SHORT,
        Some(Duration::from_secs(15)),
    ));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    assert_eq!(
        state.next_retry_after, None,
        "expected next_retry_after to be zero when transient cooldown is disabled (-1) despite the hint"
    );
    assert!(
        !state.unavailable,
        "expected model not to be unavailable when transient cooldown is disabled"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_auth_level_cloudflare520_disabled_transient_cooldown_ignores_hint() {
    let h = Harness::new(transient_settings(-1));
    let id = "auth-test-520-auth-disabled-with-hint";
    h.add(active(id, "codex"), &[]);

    h.manager.mark_result(&failure(
        id,
        "codex",
        "",
        520,
        ORIGIN_520_SHORT,
        Some(Duration::from_secs(15)),
    ));

    let updated = h.get(id);
    assert_eq!(
        updated.next_retry_after, None,
        "expected auth next_retry_after to be zero when transient cooldown is disabled (-1) despite the hint"
    );
    assert!(
        !updated.unavailable,
        "expected auth not to be unavailable when transient cooldown is disabled"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_http503_transient_cooldown_respects_disabled_and_hint() {
    let model = "claude-3-5-sonnet";
    let hint = Some(Duration::from_secs(20));

    // Disabled (-1): a 503 with a hint applies no cooldown.
    {
        let h = Harness::new(transient_settings(-1));
        let id = "auth-test-503-disabled";
        h.add(active(id, "claude"), &[]);

        h.manager.mark_result(&failure(
            id,
            "claude",
            model,
            503,
            "service unavailable",
            hint,
        ));

        let updated = h.get(id);
        let state = updated
            .model_states
            .get(model)
            .expect("expected model state to be present");
        assert_eq!(
            state.next_retry_after, None,
            "expected 503 next_retry_after to be zero when transient cooldown is disabled (-1)"
        );
        assert!(
            !state.unavailable,
            "expected model not to be unavailable when transient cooldown is disabled"
        );
    }

    // Default (0): a 503 with a hint uses the hint.
    {
        let h = Harness::new(transient_settings(0));
        let id = "auth-test-503-hint";
        h.add(active(id, "claude"), &[]);

        h.manager.mark_result(&failure(
            id,
            "claude",
            model,
            503,
            "service unavailable",
            hint,
        ));

        let updated = h.get(id);
        let state = updated
            .model_states
            .get(model)
            .expect("expected model state to be present");
        let diff = until(&h, state.next_retry_after);
        assert!(
            in_range(diff, 15, 25),
            "expected 503 hint cooldown of ~20s, got {diff}"
        );
        assert!(
            state.unavailable,
            "expected model to be unavailable during cooldown"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_auth_level_cloudflare520_sets_transient_error() {
    let h = Harness::new(Settings::default());
    let id = "auth-test-520-authlevel";
    h.add(active(id, "codex"), &[]);

    h.manager
        .mark_result(&failure(id, "codex", "", 520, ORIGIN_520_LONG, None));

    let updated = h.get(id);
    assert!(
        !updated.quota.exceeded,
        "expected quota.exceeded to be false for 520 origin error"
    );
    assert_eq!(
        updated.quota.reason, "",
        "expected quota.reason to be empty for 520 origin error"
    );
    assert_eq!(updated.status_message, "transient upstream error");
    assert_eq!(
        updated.last_error.as_ref().map(|e| e.http_status),
        Some(520),
        "expected last_error to retain HTTP 520"
    );
    assert!(
        updated.unavailable,
        "expected auth to be marked unavailable during transient cooldown"
    );
    let diff = until(&h, updated.next_retry_after);
    assert!(
        in_range(diff, 45, 75),
        "expected default transient cooldown of ~60s, got {diff}"
    );
}

#[test]
fn is_cloudflare_challenge_result_error_excludes5xx() {
    // A 5xx is an origin failure even when it carries challenge markers.
    for status in [500, 502, 503, 504, 520, 521, 522, 523, 524, 525, 526] {
        let err = AuthError {
            http_status: status,
            message: "cf-mitigated: challenge but with 5xx status code".into(),
            ..AuthError::default()
        };
        assert!(
            !is_cloudflare_challenge_result_error(&err),
            "expected {status} status code to be excluded from cloudflare challenge"
        );
    }

    let err403 = AuthError {
        http_status: 403,
        message: "cf-mitigated: challenge".into(),
        ..AuthError::default()
    };
    assert!(
        is_cloudflare_challenge_result_error(&err403),
        "expected 403 challenge to be recognized"
    );
}

#[test]
fn is_cloudflare_challenge_error() {
    let err520 = AuthError {
        http_status: 520,
        message: "cf-error-details cf-error-520: Web server is returning an unknown error".into(),
        ..AuthError::default()
    };
    assert!(
        !classify::is_cloudflare_challenge_error(ErrView::Auth(&err520)),
        "expected 520 error not to be cloudflare challenge"
    );

    let err403 = AuthError {
        http_status: 403,
        message: "cf-mitigated: challenge".into(),
        ..AuthError::default()
    };
    assert!(
        classify::is_cloudflare_challenge_error(ErrView::Auth(&err403)),
        "expected 403 challenge error to be recognized"
    );

    let plain_challenge = ExecError::upstream(0, "challenge-platform orchestrate script blocked");
    assert!(
        classify::is_cloudflare_challenge_error(ErrView::Exec(&plain_challenge)),
        "expected plain challenge error to be recognized"
    );

    let plain_origin = ExecError::upstream(
        0,
        "Web server is returning an unknown error between Cloudflare and origin",
    );
    assert!(
        !classify::is_cloudflare_challenge_error(ErrView::Exec(&plain_origin)),
        "expected plain origin error not to be recognized as challenge"
    );
}
