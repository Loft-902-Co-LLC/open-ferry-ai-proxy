// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_transport_retry_test.go (v8.0.38, MIT).
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

use futures_util::StreamExt;

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
        // Ported from TestManager_ShouldRetryAfterError_RetriesPreHTTPTransportFailure's
        // HTTP/2 cases; the last is hyper's text for the same drop.
        (
            "http2 force closed",
            ExecError::upstream(
                0,
                "http2: client connection force closed via ClientConn.Close",
            ),
            true,
        ),
        (
            "http2 conn closed",
            ExecError::upstream(
                0,
                "Post \"https://example.com\": http2: client conn is closed",
            ),
            true,
        ),
        (
            "hyper connection closed",
            ExecError::upstream(0, "operation was canceled: connection closed"),
            true,
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

/// Upstream's TestConfiguredTLSHandshakeTimeoutIsRetryable: an executor's own
/// handshake deadline is a transient pre-HTTP failure even though it is a
/// deadline, while the caller's deadline or cancellation, or the same words
/// in a bare message, are not.
///
/// Deviation: upstream's typed `ConfiguredTLSHandshakeTimeoutError` becomes a
/// [`TransportFault::Transient`] fault on a deadline error; the cases that
/// join it with the caller's own cancellation or deadline can't be built, as
/// an error has one kind.
#[tokio::test(start_paused = true)]
async fn configured_tls_handshake_timeout_is_retryable() {
    use crate::exec::ErrorKind;
    use crate::manager::classify::{
        CODE_TRANSIENT_TRANSPORT, is_connection_lifecycle_result_error,
        is_transient_transport_error, is_transient_transport_result_error,
    };

    let h = Harness::new(Settings {
        request_retry: 1,
        ..Settings::default()
    });
    h.executor(&FakeExecutor::new("codex"));
    let model = "gpt-handshake-retry";
    h.add(auth("handshake-retry", "codex"), &[model]);

    let url = "Post \"https://api.anthropic.com/v1/messages\"";
    let cases = [
        (
            "configured handshake timeout",
            ExecError::new(
                ErrorKind::DeadlineExceeded,
                format!("{url}: tls handshake timeout: context deadline exceeded"),
            )
            .with_transport(TransportFault::Transient),
            true,
        ),
        (
            "caller deadline stays non retryable",
            ExecError::new(
                ErrorKind::DeadlineExceeded,
                format!("{url}: context deadline exceeded"),
            ),
            false,
        ),
        (
            "plaintext timeout phrase is not trusted",
            ExecError::new(
                ErrorKind::DeadlineExceeded,
                format!(
                    "{url}: external transport tls handshake timeout: context deadline exceeded"
                ),
            ),
            false,
        ),
        (
            "caller cancellation stays non retryable",
            ExecError::new(ErrorKind::Canceled, format!("{url}: context canceled")),
            false,
        ),
    ];
    for (name, err, want) in cases {
        let view = ErrView::Exec(&err);
        assert_eq!(is_transient_transport_error(view), want, "{name}");
        assert_eq!(should_retry(&h, &err, 0, model).is_some(), want, "{name}");
        let result = result_error_from_error(view);
        if want {
            assert_eq!(result.code, CODE_TRANSIENT_TRANSPORT, "{name}");
            assert!(is_transient_transport_result_error(&result), "{name}");
            assert!(!is_connection_lifecycle_result_error(&result), "{name}");
        } else {
            assert_ne!(result.code, CODE_TRANSIENT_TRANSPORT, "{name}");
            assert!(!is_transient_transport_result_error(&result), "{name}");
        }
    }
}

/// Not upstream's: a recorded message with the handshake-timeout words and no
/// transient code isn't promoted to a transient failure (the
/// `isTransientTransportResultError` guard), while the same words with the
/// code are, and other transient texts still are.
#[test]
fn recorded_handshake_timeout_text_needs_the_transient_code() {
    use crate::manager::classify::{CODE_TRANSIENT_TRANSPORT, is_transient_transport_result_error};

    let bare = AuthError {
        message: "Post \"https://api.anthropic.com/v1/messages\": tls handshake timeout".into(),
        ..AuthError::default()
    };
    assert!(!is_transient_transport_result_error(&bare));
    let coded = AuthError {
        code: CODE_TRANSIENT_TRANSPORT.into(),
        ..bare.clone()
    };
    assert!(is_transient_transport_result_error(&coded));
    let reset = AuthError {
        message: "connection reset".into(),
        ..AuthError::default()
    };
    assert!(is_transient_transport_result_error(&reset));
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

/// Upstream's TestManager_HTTP2ClosedConnectionRespectsRequestRetry: a closed
/// HTTP/2 connection never cools the credential, and a call gets its request
/// retry round on every path.
///
/// Deviation: besides upstream's two Go messages, the cases are hyper's
/// texts for the same failures and an opaque error an executor marked as a
/// lifecycle fault from the typed hyper error.
#[tokio::test(start_paused = true)]
async fn http2_closed_connection_respects_request_retry() {
    use super::support::Kind;

    let failures = [
        ExecError::upstream(
            0,
            "http2: client connection force closed via ClientConn.Close",
        ),
        ExecError::upstream(0, "http2: client conn is closed"),
        ExecError::upstream(0, "operation was canceled: connection closed"),
        ExecError::upstream(0, "connection closed before message completed"),
        ExecError::upstream(0, "error sending request: client error (Connect)")
            .with_transport(TransportFault::Lifecycle),
    ];
    for fail in failures {
        for kind in [Kind::Execute, Kind::Stream, Kind::Count] {
            for retries in [0, 1] {
                let name = format!("{}/{kind:?}/retries={retries}", fail.message);
                let h = Harness::new(Settings {
                    request_retry: retries,
                    ..Settings::default()
                });
                let (executor, calls) =
                    transport_then_success("openai-compatibility", fail.clone());
                h.executor(&executor);
                let model = "http2-retry";
                let auth_id = "http2-auth";
                h.add(auth(auth_id, "openai-compatibility"), &[model]);
                let provs = providers(&["openai-compatibility"]);
                let call = || async {
                    match kind {
                        Kind::Execute => h
                            .manager
                            .execute(&provs, request(model), options())
                            .await
                            .map(|_| ()),
                        Kind::Count => h
                            .manager
                            .count_tokens(&provs, request(model), options())
                            .await
                            .map(|_| ()),
                        Kind::Stream => {
                            let mut opts = options();
                            opts.stream = true;
                            let mut response = h
                                .manager
                                .execute_stream(&provs, request(model), opts)
                                .await?;
                            while let Some(chunk) = response.chunks.next().await {
                                chunk?;
                            }
                            Ok(())
                        }
                    }
                };
                let result = call().await;
                assert_no_cooldown(&h, auth_id, model);
                if retries == 0 {
                    let err = result.expect_err(&name);
                    assert_eq!(err.message, fail.message, "{name}");
                    assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
                    call()
                        .await
                        .unwrap_or_else(|e| panic!("{name}: next request: {e}"));
                } else {
                    result.unwrap_or_else(|e| panic!("{name}: no retry round used: {e}"));
                }
                assert_eq!(calls.load(Ordering::SeqCst), 2, "{name}");
                assert_no_cooldown(&h, auth_id, model);
            }
        }
    }
}

/// Upstream's TestExecuteRetriesConfiguredClaudeHandshakeTimeoutToConfiguredRounds:
/// request-retry 3 is the first attempt plus three more rounds for the
/// executor's own handshake deadline, and one attempt for the caller's.
///
/// Deviation: as in the other handshake tests, the typed configured-deadline
/// error is a deadline error marked as a transient transport fault.
#[tokio::test(start_paused = true)]
async fn execute_retries_configured_claude_handshake_timeout_to_configured_rounds() {
    use crate::exec::ErrorKind;

    let url = "Post \"https://api.anthropic.com/v1/messages\"";
    let cases = [
        (
            "configured handshake timeout",
            ExecError::new(
                ErrorKind::DeadlineExceeded,
                format!("{url}: claude tls: handshake upstream: tls handshake timeout: context deadline exceeded"),
            )
            .with_transport(TransportFault::Transient),
            4,
        ),
        (
            "caller deadline",
            ExecError::new(
                ErrorKind::DeadlineExceeded,
                format!("{url}: context deadline exceeded"),
            ),
            1,
        ),
    ];
    for (name, fail, want_calls) in cases {
        let h = Harness::new(Settings {
            request_retry: 3,
            ..Settings::default()
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let executor = FakeExecutor::with("claude", move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Reply::Err(fail.clone())
        });
        h.executor(&executor);
        let model = "claude-handshake-retry";
        let auth_id = "claude-handshake-auth";
        h.add(auth(auth_id, "claude"), &[model]);

        let result = h
            .manager
            .execute(&providers(&["claude"]), request(model), options())
            .await;
        assert!(result.is_err(), "{name}: want the final handshake failure");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            want_calls,
            "{name}: executor calls"
        );
        assert_no_cooldown(&h, auth_id, model);
    }
}
