// Ported from CLIProxyAPI sdk/cliproxy/auth/connection_lifecycle_cooldown_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Connection lifecycle failures (a closed WebSocket, a cancelled call, an
//! EOF) never cool a credential or model down, while the same text with an
//! HTTP status, or any other failure, still does.
//!
//! Deviations from upstream:
//! - Go's typed errors become [`ExecError`]s: `context.Canceled` and
//!   `context.DeadlineExceeded` (bare or inside a `url.Error`) are the
//!   `Canceled` and `DeadlineExceeded` kinds, with the `url.Error` text;
//!   `io.EOF`, `io.ErrUnexpectedEOF` and `websocket.CloseError` (bare or
//!   wrapped) are status-less errors with their Go text and a
//!   [`TransportFault::Lifecycle`] fault, as the classifiers' docs map
//!   them; `errors.New` texts are status-less errors with no fault. The
//!   status-bearing errors are upstream errors with that status.

use super::support::*;
use crate::auth::AuthError;
use crate::exec::{ErrorKind, ExecError, TransportFault};
use crate::manager::classify::{
    CODE_CONNECTION_LIFECYCLE, ErrView, is_connection_lifecycle_error, is_request_invalid_error,
    is_request_scoped_error, result_error_from_error, should_skip_credential_cooldown,
};
use crate::manager::{CallResult, Settings};

const MODEL: &str = "gpt-5.6-sol";

fn settings() -> Settings {
    Settings {
        transient_error_cooldown_seconds: 5,
        ..Settings::default()
    }
}

fn message(text: &str) -> AuthError {
    AuthError {
        message: text.into(),
        ..AuthError::default()
    }
}

/// A Go typed connection error: no status, its Go text, a lifecycle fault.
fn lifecycle_fault(text: &str) -> ExecError {
    ExecError::upstream(0, text).with_transport(TransportFault::Lifecycle)
}

fn deadline_exceeded(text: &str) -> ExecError {
    ExecError::new(ErrorKind::DeadlineExceeded, text)
}

fn canceled(text: &str) -> ExecError {
    ExecError::new(ErrorKind::Canceled, text)
}

fn assert_no_cooldown(h: &Harness, auth_id: &str, model: &str) {
    let updated = h.get(auth_id);
    assert!(
        !updated.unavailable,
        "{auth_id}: expected connection lifecycle error to keep auth available"
    );
    assert_eq!(
        updated.next_retry_after, None,
        "{auth_id}: expected connection lifecycle error to keep auth cooldown unset"
    );
    if let Some(state) = updated.model_states.get(model) {
        assert!(
            !state.unavailable && state.next_retry_after.is_none(),
            "{auth_id}: expected no model cooldown, got {state:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_connection_lifecycle_does_not_cooldown() {
    let cases: Vec<(&str, AuthError)> = vec![
        ("websocket 1000", message("websocket: close 1000 (normal)")),
        (
            "websocket 1001",
            message("websocket: close 1001 (going away)"),
        ),
        (
            "websocket 1006",
            message("websocket: close 1006 (abnormal closure): unexpected EOF"),
        ),
        ("context canceled", message("context canceled")),
        (
            "context deadline exceeded",
            message("context deadline exceeded"),
        ),
        ("unexpected EOF", message("unexpected EOF")),
        ("plain EOF", message("EOF")),
        (
            "wrapped unexpected EOF",
            message("read tcp 127.0.0.1:1->127.0.0.1:2: unexpected EOF"),
        ),
        (
            "typed canceled",
            result_error_from_error(ErrView::Exec(&ExecError::canceled())),
        ),
        (
            "typed deadline",
            result_error_from_error(ErrView::Exec(&deadline_exceeded(
                "context deadline exceeded",
            ))),
        ),
        (
            "url canceled",
            result_error_from_error(ErrView::Exec(&canceled(
                "Post \"https://example.com\": context canceled",
            ))),
        ),
        (
            "url deadline",
            result_error_from_error(ErrView::Exec(&deadline_exceeded(
                "Post \"https://example.com\": context deadline exceeded",
            ))),
        ),
    ];

    for (name, err) in cases {
        let h = Harness::new(settings());
        let auth = h.add(auth(&format!("auth-lifecycle-{name}"), "codex"), &[]);
        h.manager.mark_result(&CallResult {
            auth_id: auth.id.clone(),
            provider: auth.provider.clone(),
            model: MODEL.into(),
            success: false,
            error: Some(err),
            ..CallResult::default()
        });
        assert_no_cooldown(&h, &auth.id, MODEL);
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_connection_lifecycle_auth_level_does_not_cooldown() {
    let h = Harness::new(settings());
    let auth = h.add(auth("auth-lifecycle-auth-level", "codex"), &[]);

    // No model: the auth-level failure path.
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        success: false,
        error: Some(message(
            "websocket: close 1006 (abnormal closure): unexpected EOF",
        )),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    assert!(
        !updated.unavailable,
        "expected auth-level lifecycle error to keep auth available"
    );
    assert_eq!(
        updated.next_retry_after, None,
        "expected auth-level lifecycle error to keep auth cooldown unset"
    );
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_http_status_with_lifecycle_text_still_cooldowns() {
    // (name, status, message, whether the model must be unavailable)
    let cases = [
        ("401 unexpected EOF", 401, "unexpected EOF", true),
        ("429 context canceled", 429, "context canceled", true),
        ("500 unexpected EOF", 500, "unexpected EOF", false),
        (
            "500 websocket 1006 text",
            500,
            "websocket: close 1006 (abnormal closure): unexpected EOF",
            false,
        ),
    ];

    for (name, http_status, text, want_auth) in cases {
        let h = Harness::new(settings());
        let auth = h.add(auth(&format!("auth-status-{name}"), "codex"), &[]);
        let before = h.now();
        h.manager.mark_result(&CallResult {
            auth_id: auth.id.clone(),
            provider: auth.provider.clone(),
            model: MODEL.into(),
            success: false,
            error: Some(AuthError {
                http_status,
                message: text.into(),
                ..AuthError::default()
            }),
            ..CallResult::default()
        });

        let updated = h.get(&auth.id);
        let state = updated
            .model_states
            .get(MODEL)
            .unwrap_or_else(|| panic!("{name}: expected model cooldown state"));
        let next_retry_after = state.next_retry_after.unwrap_or_else(|| {
            panic!("{name}: expected HTTP status {http_status} with lifecycle text to still cool")
        });
        if http_status == 500 {
            assert!(
                next_retry_after >= before + chrono::TimeDelta::seconds(4),
                "{name}: expected ~5s transient cooldown, got {next_retry_after}"
            );
        }
        if want_auth {
            assert!(
                state.unavailable,
                "{name}: expected auth-class status to mark model unavailable"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_non_lifecycle_still_cooldowns() {
    let h = Harness::new(settings());
    let auth = h.add(auth("auth-still-cools", "codex"), &[]);

    let before = h.now();
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        model: MODEL.into(),
        success: false,
        error: Some(AuthError {
            http_status: 500,
            message: "upstream internal failure".into(),
            retryable: true,
            ..AuthError::default()
        }),
        ..CallResult::default()
    });

    let updated = h.get(&auth.id);
    let state = updated
        .model_states
        .get(MODEL)
        .expect("expected model cooldown state");
    assert!(
        state.unavailable,
        "expected non-lifecycle 500 to mark model unavailable"
    );
    assert!(
        state
            .next_retry_after
            .is_some_and(|t| t >= before + chrono::TimeDelta::seconds(4)),
        "expected ~5s transient cooldown, got {:?}",
        state.next_retry_after
    );
}

#[test]
fn result_error_from_error_connection_lifecycle_does_not_become_request_scoped() {
    let cases = [
        ExecError::canceled(),
        deadline_exceeded("context deadline exceeded"),
        lifecycle_fault("EOF"),
        lifecycle_fault("unexpected EOF"),
        canceled("Post \"https://example.com\": context canceled"),
        deadline_exceeded("Post \"https://example.com\": context deadline exceeded"),
        lifecycle_fault("websocket: close 1000 (normal): normal"),
        lifecycle_fault("websocket: close 1001 (going away): bye"),
        lifecycle_fault("websocket: close 1006 (abnormal closure): unexpected EOF"),
        lifecycle_fault("upstream read: websocket: close 1006 (abnormal closure): unexpected EOF"),
        lifecycle_fault("wrap: unexpected EOF"),
        ExecError::upstream(0, "websocket: close 1000 (normal)"),
        ExecError::upstream(
            0,
            "websocket: close 1006 (abnormal closure): unexpected EOF",
        ),
        ExecError::upstream(0, "context deadline exceeded"),
        ExecError::upstream(0, "unexpected EOF"),
    ];
    for err in &cases {
        let view = ErrView::Exec(err);
        assert!(
            is_connection_lifecycle_error(view),
            "is_connection_lifecycle_error({err}) = false, want true"
        );
        let got = result_error_from_error(view);
        assert!(
            !is_request_scoped_error(ErrView::Auth(&got)),
            "result_error_from_error({err}) code={:?}, want non-request-scoped lifecycle error",
            got.code
        );
        assert_eq!(
            got.code, CODE_CONNECTION_LIFECYCLE,
            "result_error_from_error({err})"
        );
        assert!(
            !is_request_invalid_error(view),
            "is_request_invalid_error({err}) = true, lifecycle must not stop credential fallback"
        );
        assert!(
            should_skip_credential_cooldown(Some(&got)),
            "should_skip_credential_cooldown({got:?}) = false, want true"
        );
    }
}

#[test]
fn is_connection_lifecycle_error_status_bearing_errors_stay_coolable() {
    let cases = [
        ExecError::upstream(401, "unexpected EOF"),
        ExecError::upstream(429, "context canceled"),
        ExecError::upstream(500, "unexpected EOF"),
        ExecError::upstream(
            502,
            "websocket: close 1006 (abnormal closure): unexpected EOF",
        ),
    ];
    for err in &cases {
        let view = ErrView::Exec(err);
        assert!(
            !is_connection_lifecycle_error(view),
            "is_connection_lifecycle_error({err}) = true, want false for status-bearing errors"
        );
        let got = result_error_from_error(view);
        assert!(
            !should_skip_credential_cooldown(Some(&got)),
            "should_skip_credential_cooldown({got:?}) = true, want false"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn is_connection_lifecycle_error_typed_close_wins() {
    // A typed WebSocket close is a lifecycle failure even with a status.
    let err = ExecError::upstream(
        502,
        "websocket: close 1006 (abnormal closure): unexpected EOF",
    )
    .with_transport(TransportFault::Lifecycle);
    let view = ErrView::Exec(&err);
    assert!(
        is_connection_lifecycle_error(view),
        "typed close should be lifecycle even with outer status"
    );
    let got = result_error_from_error(view);
    assert_eq!(got.code, CODE_CONNECTION_LIFECYCLE);
    assert!(
        should_skip_credential_cooldown(Some(&got)),
        "should_skip_credential_cooldown({got:?}) = false, want true"
    );

    let h = Harness::new(Settings::default());
    let auth = h.add(auth("auth-typed-close", "codex"), &[]);
    h.manager.mark_result(&CallResult {
        auth_id: auth.id.clone(),
        provider: auth.provider.clone(),
        model: MODEL.into(),
        success: false,
        error: Some(got),
        ..CallResult::default()
    });
    assert_no_cooldown(&h, &auth.id, MODEL);
}
