// Ported from CLIProxyAPI sdk/cliproxy/auth/codex_model_not_found_cooldown_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A structured Codex `model_not_found` error is not a caller fault: it keeps
//! its code and cools the model down for 12 hours, unless cooling is off. A
//! caller-input error and a generic 404 are not `model_not_found`.
//!
//! Deviations from upstream:
//! - The session affinity picks go through the `affinity` tests' pool,
//!   which picks as the manager does, where upstream calls its selector;
//!   the provider scope is an argument.
//! - Upstream's `statusBearingError` is `ExecError::upstream(status, body)`.

use super::affinity::{Sticky, session};
use super::support::*;

use chrono::TimeDelta;

use crate::auth::AuthError;
use crate::exec::ExecError;
use crate::manager::classify::{
    CODE_REQUEST_SCOPED, ErrView, is_request_invalid_error, is_request_scoped_error,
    result_error_from_error, should_skip_credential_cooldown,
};
use crate::manager::{CallResult, RoutingStrategy, Settings};

const MODEL: &str = "gpt-5.5";
const STRUCTURED_MODEL_NOT_FOUND: &str = r#"{"error":{"type":"invalid_request_error","code":"model_not_found","message":"The model gpt-5.5 does not exist or you do not have access to it."}}"#;

fn failure(auth_id: &str, error: AuthError) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: "codex".into(),
        model: MODEL.into(),
        success: false,
        error: Some(error),
        ..CallResult::default()
    }
}

#[tokio::test(start_paused = true)]
async fn codex_structured_model_not_found_classification_and_cooldown() {
    let cases = [
        (
            "status 400 with invalid_request_error and model_not_found code",
            400,
        ),
        (
            "status 404 with invalid_request_error and model_not_found code",
            404,
        ),
    ];
    for (name, status) in cases {
        let raw = ExecError::upstream(status, STRUCTURED_MODEL_NOT_FOUND);
        assert!(
            !is_request_invalid_error(ErrView::Exec(&raw)),
            "{name}: model_not_found is not a caller request fault"
        );

        let result_err = result_error_from_error(ErrView::Exec(&raw));
        assert_eq!(result_err.code, "model_not_found", "{name}");
        assert!(
            !is_request_scoped_error(ErrView::Auth(&result_err)),
            "{name}: result error must not be marked request-scoped"
        );
        assert!(
            !should_skip_credential_cooldown(Some(&result_err)),
            "{name}: should_skip_credential_cooldown({result_err:?}) = true, want false"
        );

        let h = Harness::new(Settings::default());
        let id = "auth-codex-1";
        h.add(auth(id, "codex"), &[]);
        h.manager.mark_result(&failure(id, result_err));

        let updated = h.get(id);
        let state = updated.model_states.get(MODEL);
        assert!(
            state.is_some_and(|s| s.unavailable),
            "{name}: expected model state to be unavailable, got {state:?}"
        );
        let state = state.expect("model state");
        assert_eq!(
            state.last_error.as_ref().map(|e| e.code.as_str()),
            Some("model_not_found"),
            "{name}"
        );
        let remaining = state.next_retry_after.expect("retry time") - h.now();
        assert!(
            remaining >= TimeDelta::hours(11) && remaining <= TimeDelta::hours(13),
            "{name}: expected ~12h cooldown, got remaining={remaining}"
        );
    }
}

#[test]
fn codex_model_not_found_caller_input_error_not_model_cooldown() {
    let raw = ExecError::upstream(
        400,
        r#"{"error":{"type":"invalid_request_error","message":"The model not found in request body"}}"#,
    );
    assert!(
        is_request_invalid_error(ErrView::Exec(&raw)),
        "is_request_invalid_error = false, want true for caller request fault"
    );
    let result_err = result_error_from_error(ErrView::Exec(&raw));
    assert_eq!(
        result_err.code, CODE_REQUEST_SCOPED,
        "result_err = {result_err:?}, want request_scoped code"
    );
    assert!(
        should_skip_credential_cooldown(Some(&result_err)),
        "should_skip_credential_cooldown({result_err:?}) = false, want true for request fault"
    );
}

#[tokio::test(start_paused = true)]
async fn codex_model_not_found_generic404_not_model_not_found() {
    let raw = ExecError::upstream(404, r#"{"error":{"message":"Not Found"}}"#);
    let result_err = result_error_from_error(ErrView::Exec(&raw));
    assert_ne!(
        result_err.code, "model_not_found",
        "want a generic non-model_not_found code"
    );

    let h = Harness::new(Settings::default());
    let id = "auth-codex-generic-404";
    h.add(auth(id, "codex"), &[]);
    h.manager.mark_result(&failure(id, result_err));

    let updated = h.get(id);
    let code = updated
        .model_states
        .get(MODEL)
        .and_then(|s| s.last_error.as_ref())
        .map(|e| e.code.as_str());
    assert_ne!(
        code,
        Some("model_not_found"),
        "generic 404 should not have model_not_found error code"
    );
}

/// A call's success on `auth_id`.
fn success(auth_id: &str) -> CallResult {
    CallResult {
        auth_id: auth_id.into(),
        provider: "codex".into(),
        model: MODEL.into(),
        success: true,
        ..CallResult::default()
    }
}

// TestCodexStructuredModelNotFound_SessionAffinityReleased.
#[test]
fn codex_structured_model_not_found_session_affinity_released() {
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &["auth-1", "auth-2"]);
    let session = session(&[("X-Session-Id", "session-model-not-found-test")], "");

    // The first pick binds the session to auth-1.
    let picked = pool.pick("codex", MODEL, &session, &["auth-1", "auth-2"]);
    assert_eq!(picked, "auth-1");
    pool.on_result(&session, &success(&picked), "codex");

    // A structured model_not_found failure.
    let raw = ExecError::upstream(400, STRUCTURED_MODEL_NOT_FOUND);
    let result_err = result_error_from_error(ErrView::Exec(&raw));
    pool.on_result(&session, &failure(&picked, result_err), "codex");

    // The binding is gone, so the next pick isn't auth-1.
    let next = pool.pick("codex", MODEL, &session, &["auth-2", "auth-1"]);
    assert_eq!(next, "auth-2", "affinity was not released");
}

#[tokio::test(start_paused = true)]
async fn codex_terminal_event_end_to_end_cooldown_and_affinity() {
    let ids = ["auth-e2e-1", "auth-e2e-2"];
    let mut pool = Sticky::of(RoutingStrategy::RoundRobin, &ids);
    let session = session(&[("X-Session-Id", "session-e2e-codex-model-not-found")], "");
    let picked = pool.pick("codex", MODEL, &session, &ids);
    assert_eq!(picked, "auth-e2e-1", "initial pick");
    pool.on_result(&session, &success(&picked), "codex");

    let h = Harness::new(Settings::default());
    h.add(auth("auth-e2e-1", "codex"), &[]);
    h.add(auth("auth-e2e-2", "codex"), &[]);

    let raw = ExecError::upstream(404, STRUCTURED_MODEL_NOT_FOUND);
    let result_err = result_error_from_error(ErrView::Exec(&raw));
    assert_eq!(
        result_err.code, "model_not_found",
        "expected preserved model_not_found code, got {result_err:?}"
    );

    let result = failure("auth-e2e-1", result_err);
    pool.on_result(&session, &result, "codex");
    h.manager.mark_result(&result);

    let updated = h.get("auth-e2e-1");
    let state = updated.model_states.get(MODEL);
    assert!(
        state.is_some_and(|s| s.unavailable),
        "expected model gpt-5.5 to be cooling down, got {state:?}"
    );
    assert_eq!(
        state
            .and_then(|s| s.last_error.as_ref())
            .map(|e| e.code.as_str()),
        Some("model_not_found")
    );

    let next = pool.pick("codex", MODEL, &session, &["auth-e2e-2", "auth-e2e-1"]);
    assert_eq!(next, "auth-e2e-2", "affinity was not released");
}

#[tokio::test(start_paused = true)]
async fn codex_structured_model_not_found_disable_cooling() {
    let h = Harness::new(Settings::default());
    let id = "auth-codex-disable-cooling";
    h.add(
        auth_with_metadata(id, "codex", serde_json::json!({"disable_cooling": true})),
        &[],
    );

    let raw = ExecError::upstream(400, STRUCTURED_MODEL_NOT_FOUND);
    let result_err = result_error_from_error(ErrView::Exec(&raw));
    h.manager.mark_result(&failure(id, result_err));

    let updated = h.get(id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model state to be present");
    assert_eq!(
        state.next_retry_after, None,
        "expected next_retry_after to be zero when disable_cooling=true"
    );
}
