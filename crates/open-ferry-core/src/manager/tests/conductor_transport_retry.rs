// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_transport_retry_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A connection failure before any HTTP answer (TLS handshake, refused dial,
//! unexpected EOF) may start another retry round, and never cools the
//! credential down.
//!
//! Deviations from upstream:
//! - Upstream builds Go network error values (`url.Error`, `net.OpError`,
//!   `io.ErrUnexpectedEOF`); here they are status-0 errors with the same
//!   text, marked with the [`TransportFault`] an executor would set. The
//!   certificate error carries no fault, as it isn't a transport failure.
//! - `TestHomeExecuteRetriesPreHTTPTransportFailure` is dropped: the Home
//!   dispatcher isn't ported.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::support::*;
use crate::auth::AuthError;
use crate::exec::{Dispatcher, ExecError, TransportFault};
use crate::manager::classify::{ErrView, result_error_from_error};
use crate::manager::models::Resolver;
use crate::manager::retry::{RetryQuery, should_retry_after_error};
use crate::manager::select::Selection;
use crate::manager::{CallResult, Settings};

const TLS_HANDSHAKE_TEXT: &str = "Post \"https://chatgpt.com/backend-api/codex/responses\": tls: TLS handshake: read tcp 127.0.0.1:1->127.0.0.1:2: wsarecv: A connection attempt failed because the connected party did not properly respond after a period of time, or established connection failed because connected host has failed to respond.";

/// Upstream's `windowsCodexTLSHandshakeError`.
fn windows_codex_tls_handshake_error() -> ExecError {
    ExecError::upstream(0, TLS_HANDSHAKE_TEXT).with_transport(TransportFault::Transient)
}

/// Upstream's `dialRefusedError`.
fn dial_refused_error() -> ExecError {
    ExecError::upstream(
        0,
        "Post \"https://chatgpt.com/backend-api/codex/responses\": dial tcp: connection refused",
    )
    .with_transport(TransportFault::Transient)
}

/// Upstream's `assertNoCooldown`.
fn assert_no_cooldown(h: &Harness, auth_id: &str, model: &str) {
    let updated = h.get(auth_id);
    assert!(
        !updated.unavailable,
        "expected connection lifecycle error to keep auth available"
    );
    assert!(
        updated.next_retry_after.is_none(),
        "expected connection lifecycle error to keep auth cooldown unset, got {:?}",
        updated.next_retry_after
    );
    if let Some(state) = updated.model_states.get(model) {
        assert!(
            !state.unavailable && state.next_retry_after.is_none(),
            "expected no model cooldown, got {state:?}"
        );
    }
}

/// Upstream's `transportThenSuccessExecutor`: the first call fails with
/// `fail`, later ones answer `ok`.
fn transport_then_success(id: &str, fail: ExecError) -> (Arc<FakeExecutor>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let executor = FakeExecutor::with(id, move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::Err(fail.clone())
        } else {
            Reply::ok("ok")
        }
    });
    (executor, calls)
}

/// Upstream's `m.shouldRetryAfterError(err, attempt, providers, model, 0)`.
fn should_retry(h: &Harness, err: &ExecError, attempt: usize, model: &str) -> Option<Duration> {
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
        now: h.now(),
    };
    let provs = providers(&["codex"]);
    let attempted = HashSet::new();
    let query = RetryQuery {
        providers: &provs,
        model,
        pinned: "",
        attempt,
        default_retry: state.settings.request_retry,
        eligibility: Default::default(),
        attempted: &attempted,
    };
    should_retry_after_error(&selection, &query, err, Duration::ZERO)
}

#[tokio::test(start_paused = true)]
async fn manager_should_retry_after_error_retries_pre_http_transport_failure() {
    let h = Harness::new(Settings {
        request_retry: 1,
        ..Settings::default()
    });
    h.executor(&FakeExecutor::new("codex"));
    let model = "gpt-transport-retry";
    h.add(auth("transport-retry", "codex"), &[model]);

    let cases = [
        (
            "windows tls handshake",
            windows_codex_tls_handshake_error(),
            true,
        ),
        ("dial refused", dial_refused_error(), true),
        (
            "unexpected eof",
            ExecError::upstream(0, "unexpected EOF").with_transport(TransportFault::Lifecycle),
            true,
        ),
        (
            "unauthorized",
            ExecError::upstream(401, "unauthorized"),
            false,
        ),
        ("canceled", ExecError::canceled(), false),
        (
            "certificate",
            ExecError::upstream(
                0,
                "Post \"https://chatgpt.com/backend-api/codex/responses\": x509: certificate signed by unknown authority",
            ),
            false,
        ),
    ];
    for (name, err, want) in cases {
        let wait = should_retry(&h, &err, 0, model);
        assert_eq!(
            wait.is_some(),
            want,
            "{name}: shouldRetryAfterError() = {wait:?}, want retry {want}"
        );
        if want {
            assert_eq!(
                wait,
                Some(Duration::ZERO),
                "{name}: shouldRetryAfterError() wait"
            );
            assert!(
                should_retry(&h, &err, 1, model).is_none(),
                "{name}: transport retried after the configured additional round"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_pre_http_transport_failure_does_not_cooldown() {
    let model = "gpt-6-astra";
    let cases = [
        (
            "typed tls handshake",
            result_error_from_error(ErrView::Exec(&windows_codex_tls_handshake_error())),
        ),
        (
            "connection reset message",
            AuthError {
                message: "connection reset".into(),
                ..AuthError::default()
            },
        ),
    ];
    for (name, err) in cases {
        let h = Harness::new(Settings {
            transient_error_cooldown_seconds: 5,
            ..Settings::default()
        });
        let id = format!("auth-transport-{name}");
        h.add(auth(&id, "codex"), &[]);
        h.manager.mark_result(&CallResult {
            auth_id: id.clone(),
            provider: "codex".into(),
            model: model.into(),
            success: false,
            error: Some(err),
            ..CallResult::default()
        });
        assert_no_cooldown(&h, &id, model);
    }
}

#[tokio::test(start_paused = true)]
async fn execute_retries_pre_http_transport_failure_without_cooling() {
    let h = Harness::new(Settings {
        request_retry: 1,
        ..Settings::default()
    });
    let (executor, calls) = transport_then_success("codex", windows_codex_tls_handshake_error());
    h.executor(&executor);
    let model = "gpt-6-astra-transport";
    let auth_id = "codex-transport";
    h.add(auth(auth_id, "codex"), &[model]);

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(model), options())
        .await
        .expect("Execute() error, want success after transport retry");
    assert_eq!(&resp.payload[..], b"ok", "Execute() payload");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "executor calls");
    assert_no_cooldown(&h, auth_id, model);
}

#[tokio::test(start_paused = true)]
async fn execute_does_not_poison_credential_on_pre_http_transport_failure() {
    let h = Harness::new(Settings::default());
    let (executor, calls) = transport_then_success("codex", windows_codex_tls_handshake_error());
    h.executor(&executor);
    let model = "gpt-6-astra-poison";
    let auth_id = "codex-transport-poison";
    h.add(auth(auth_id, "codex"), &[model]);

    let first = h
        .manager
        .execute(&providers(&["codex"]), request(model), options())
        .await;
    match first {
        Ok(_) => panic!("first Execute() error = nil, want transport failure"),
        Err(err) => assert!(
            !err.kind.is_auth_selection(),
            "first Execute() = {err}, want raw transport error rather than auth_unavailable"
        ),
    }
    assert_no_cooldown(&h, auth_id, model);

    let resp = h
        .manager
        .execute(&providers(&["codex"]), request(model), options())
        .await
        .expect("second Execute() error, want success on the still-available credential");
    assert_eq!(&resp.payload[..], b"ok", "second Execute() payload");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "executor calls");
}
