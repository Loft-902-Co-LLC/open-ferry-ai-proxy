// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_fast_error_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A request-scoped error from a Fast call, local or carrying the
//! provider's response, ends the call at once: no refresh, no retry, no
//! cooldown, and the next call on the credential succeeds.
//!
//! Deviations from upstream:
//! - `TestManagerFastDirectErrorDoesNotRefreshRetryOrCoolCredential`:
//!   `RequestTerminatedError` (a direct HTTP response) isn't ported. The
//!   executor returns the provider's 401 and body as a request-scoped
//!   [`ExecError`], and "the error is a direct response with status 401" is
//!   checked as the caller getting that 401 and body unchanged.
//! - `TestManagerFastLocalErrorDoesNotRefreshRetryOrCoolCredential`: "not a
//!   direct response" is checked as the error having no HTTP status.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::{HeaderValue, header};

use super::conductor_claude_cancellation::{
    CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL, new_claude_cancellation_harness,
    require_claude_cancellation_neutral,
};
use super::support::*;
use crate::exec::{Dispatcher, ErrorKind, ExecError};

const FAST_DENIED: &str = r#"{"type":"error","error":{"message":"Fast denied"}}"#;

#[derive(Clone, Copy, Debug)]
enum Path {
    NonStream,
    Stream,
}

async fn run(h: &Harness, path: Path) -> Result<(), ExecError> {
    let provs = providers(&["claude"]);
    match path {
        Path::NonStream => h
            .manager
            .execute(&provs, request(CLAUDE_CANCEL_MODEL), options())
            .await
            .map(|_| ()),
        Path::Stream => {
            let mut opts = options();
            opts.stream = true;
            let stream = h
                .manager
                .execute_stream(&provs, request(CLAUDE_CANCEL_MODEL), opts)
                .await?;
            // Upstream drains the stream and ignores its chunks.
            collect(stream).await;
            Ok(())
        }
    }
}

/// An executor that fails its first call with `first` and then succeeds.
fn first_fails(first: ExecError) -> (Arc<FakeExecutor>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let executor = FakeExecutor::with("claude", move |call| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return Reply::Err(first.clone());
        }
        match call.kind {
            Kind::Stream => Reply::ok("ok"),
            _ => Reply::ok(r#"{"type":"message","content":[]}"#),
        }
    });
    (executor, calls)
}

#[tokio::test(start_paused = true)]
async fn manager_fast_local_error_does_not_refresh_retry_or_cool_credential() {
    let cases = [
        (Path::NonStream, "decode Fast response"),
        (Path::Stream, "decode Fast stream response"),
    ];
    for (path, message) in cases {
        let (executor, calls) = first_fails(ExecError::upstream(0, message).with_request_scoped());
        let h = new_claude_cancellation_harness(executor.clone());

        let err = run(&h, path)
            .await
            .expect_err("first Fast request error = nil");
        assert_eq!(
            err.http_status(),
            0,
            "{path:?}: local Fast error unexpectedly became a direct HTTP response: {err}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{path:?}: first request upstream calls"
        );
        assert_eq!(executor.refresh_count(), 0, "{path:?}: refresh calls");
        require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);

        if let Err(err) = run(&h, path).await {
            panic!("{path:?}: follow-up request error = {err}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "{path:?}: total upstream calls"
        );
        require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);
    }
}

#[tokio::test(start_paused = true)]
async fn manager_fast_direct_error_does_not_refresh_retry_or_cool_credential() {
    for path in [Path::NonStream, Path::Stream] {
        let mut direct = ExecError::upstream(401, FAST_DENIED).with_request_scoped();
        direct.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let (executor, calls) = first_fails(direct);
        let h = new_claude_cancellation_harness(executor.clone());

        let err = run(&h, path)
            .await
            .expect_err("first Fast request error = nil");
        assert!(
            err.kind == ErrorKind::Upstream && err.message == FAST_DENIED,
            "{path:?}: first error = {err:?}, want direct response"
        );
        assert_eq!(err.http_status(), 401, "{path:?}: direct status");
        assert_eq!(
            err.headers.get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json")),
            "{path:?}: direct headers"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{path:?}: first request upstream calls"
        );
        assert_eq!(executor.refresh_count(), 0, "{path:?}: refresh calls");
        require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);

        if let Err(err) = run(&h, path).await {
            panic!("{path:?}: follow-up request error = {err}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "{path:?}: total upstream calls"
        );
        require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);
    }
}
