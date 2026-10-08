// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_stream_overload_failover_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A stream whose credentials reject it as overloaded or at capacity before
//! it starts moves on to the next credential, within the
//! `max_retry_credentials` budget.
//!
//! Deviations from upstream:
//! - `TestExecuteStream_BootstrapOverload_StopsAtCredentialBudget` waits 30
//!   seconds of paused Tokio time, where upstream waits 30 wall-clock
//!   seconds.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, header};

use super::support::*;
use crate::auth::Status;
use crate::exec::{Dispatcher, ExecError};
use crate::manager::Settings;

pub(super) const OVERLOAD_MODEL: &str = "gpt-5.6-terra";

/// Upstream's `registerOverloadAuths`: `n` active codex credentials with
/// descending priority, returned in pick order.
pub(super) fn register_overload_auths(h: &Harness, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = format!("auth-overload-{}", i + 1);
        let mut credential = auth(&id, "codex");
        credential.status = Status::Active;
        credential
            .attributes
            .insert("priority".into(), (100 - i).to_string());
        h.add(credential, &[OVERLOAD_MODEL]);
        ids.push(id);
    }
    ids
}

/// Upstream's `overloadStatusError`.
pub(super) fn overload_status_error() -> ExecError {
    ExecError::upstream(
        503,
        r#"{"error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","param":null}}"#,
    )
}

/// Upstream's `capacityStatusError`.
fn capacity_status_error() -> ExecError {
    ExecError::upstream(
        429,
        r#"{"error":{"message":"Selected model is at capacity. Please try a different model."}}"#,
    )
}

pub(super) fn event_stream_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers
}

/// Upstream's `successStreamResult`.
fn success_stream_result() -> Reply {
    Reply::Stream {
        headers: event_stream_headers(),
        chunks: vec![
            Ok(Bytes::from_static(
                br#"data: {"type":"response.output_item.added"}"#,
            )),
            Ok(Bytes::from_static(
                br#"data: {"type":"response.completed"}"#,
            )),
        ],
    }
}

/// Three failing credentials, then one that serves the stream.
async fn skips_consecutive_failing_credentials(failure: fn() -> ExecError, what: &str) {
    let h = Harness::new(Settings {
        request_retry: 5,
        max_retry_credentials: 6,
        ..Settings::default()
    });
    let ids = register_overload_auths(&h, 6);
    let failing: HashSet<String> = ids[..3].iter().cloned().collect();
    let handler_failing = failing.clone();
    let executor: Arc<FakeExecutor> = FakeExecutor::with("codex", move |call| {
        if handler_failing.contains(&call.auth_id) {
            Reply::Err(failure())
        } else {
            success_stream_result()
        }
    });
    h.executor(&executor);

    let result = h
        .manager
        .execute_stream(&providers(&["codex"]), request(OVERLOAD_MODEL), options())
        .await;
    let stream = match result {
        Ok(stream) => stream,
        Err(err) => panic!("expected the request to survive three {what} credentials: {err}"),
    };
    let (_, chunk_err) = collect(stream).await;
    assert!(chunk_err.is_none(), "unexpected chunk error: {chunk_err:?}");

    let order = executor.ids(Kind::Stream);
    assert_eq!(
        order.len(),
        4,
        "attempted {} credentials ({order:?}), want exactly 4",
        order.len()
    );
    for (i, id) in order[..3].iter().enumerate() {
        assert!(
            failing.contains(id),
            "attempt {} used {id}, expected one of the {what} credentials",
            i + 1
        );
    }
    assert!(
        !failing.contains(&order[3]),
        "final attempt used {what} credential {}",
        order[3]
    );
}

#[tokio::test(start_paused = true)]
async fn execute_stream_bootstrap_overload_skips_consecutive_overloaded_credentials() {
    skips_consecutive_failing_credentials(overload_status_error, "overloaded").await;
}

#[tokio::test(start_paused = true)]
async fn execute_stream_bootstrap_capacity_skips_consecutive_overloaded_credentials() {
    skips_consecutive_failing_credentials(capacity_status_error, "at-capacity").await;
}

#[tokio::test(start_paused = true)]
async fn execute_stream_bootstrap_overload_stops_at_credential_budget() {
    // Six credentials exist and only four may be attempted in one round.
    let h = Harness::new(Settings {
        request_retry: 5,
        max_retry_credentials: 4,
        ..Settings::default()
    });
    register_overload_auths(&h, 6);
    let executor = FakeExecutor::with("codex", |_| Reply::Err(overload_status_error()));
    h.executor(&executor);

    let provs = providers(&["codex"]);
    let call = h
        .manager
        .execute_stream(&provs, request(OVERLOAD_MODEL), options());
    if tokio::time::timeout(Duration::from_secs(30), call)
        .await
        .is_err()
    {
        panic!("ExecuteStream did not terminate within the credential budget");
    }

    let attempts = executor.ids(Kind::Stream).len();
    assert!(attempts != 0, "expected at least one attempt");
    // A no-wait retry round may consume the remaining two credentials after
    // the first four-credential sweep, but it must not exceed the available
    // set.
    assert!(
        (4..=6).contains(&attempts),
        "attempts = {attempts}, want between 4 and 6"
    );
}
