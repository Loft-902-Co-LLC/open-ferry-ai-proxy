// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_stream_overload_status_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! How an overload rejection reaches a streaming caller: as a 503 error when
//! every credential rejects the stream before it starts, and inside the
//! stream once a chunk has been handed on or when the rejection is the
//! stream's first item.
//!
//! Deviations from upstream:
//! - `TestExecuteStream_ErrorAsFirstChunk_IsDowngradedToCommittedStream`:
//!   upstream accepts either an error or a stream carrying the error, and
//!   only logs which; both Go and the port return the stream, so that is
//!   what is asserted.

use bytes::Bytes;

use super::conductor_stream_overload_failover::{
    OVERLOAD_MODEL, event_stream_headers, overload_status_error, register_overload_auths,
};
use super::support::*;
use crate::exec::Dispatcher;
use crate::manager::Settings;

fn overload_harness() -> Harness {
    let h = Harness::new(Settings {
        request_retry: 5,
        max_retry_credentials: 3,
        ..Settings::default()
    });
    register_overload_auths(&h, 3);
    h
}

#[tokio::test(start_paused = true)]
async fn execute_stream_all_credentials_overloaded_returns_status_error() {
    let h = overload_harness();
    // The rejection comes back before any chunk, as from a buffering
    // executor.
    h.executor(&FakeExecutor::with("codex", |_| {
        Reply::Err(overload_status_error())
    }));

    let result = h
        .manager
        .execute_stream(&providers(&["codex"]), request(OVERLOAD_MODEL), options())
        .await;
    let err = match result {
        Ok(_) => panic!("expected a hard error once every credential is overloaded"),
        Err(err) => err,
    };
    assert_eq!(err.http_status(), 503, "status code of {err}");
}

#[tokio::test(start_paused = true)]
async fn execute_stream_unbuffered_overload_stays_committed_stream() {
    let h = overload_harness();
    h.executor(&FakeExecutor::with("codex", |_| Reply::Stream {
        headers: event_stream_headers(),
        chunks: vec![
            Ok(Bytes::from_static(br#"data: {"type":"response.created"}"#)),
            Err(overload_status_error()),
        ],
    }));

    let result = h
        .manager
        .execute_stream(&providers(&["codex"]), request(OVERLOAD_MODEL), options())
        .await;
    let stream = match result {
        Ok(stream) => stream,
        Err(err) => panic!("unbuffered path should hand back a committed stream, got error: {err}"),
    };
    let (_, chunk_err) = collect(stream).await;
    assert!(
        chunk_err.is_some(),
        "expected the overload rejection to arrive in-stream"
    );
}

#[tokio::test(start_paused = true)]
async fn execute_stream_error_as_first_chunk_is_downgraded_to_committed_stream() {
    let h = overload_harness();
    h.executor(&FakeExecutor::with("codex", |_| Reply::Stream {
        headers: event_stream_headers(),
        chunks: vec![Err(overload_status_error())],
    }));

    let result = h
        .manager
        .execute_stream(&providers(&["codex"]), request(OVERLOAD_MODEL), options())
        .await;
    let stream = match result {
        Ok(stream) => stream,
        Err(err) => panic!("first-chunk error surfaced as a hard error: {err}"),
    };
    let (_, chunk_err) = collect(stream).await;
    assert!(
        chunk_err.is_some(),
        "expected the rejection to be delivered in-stream after the downgrade"
    );
}
