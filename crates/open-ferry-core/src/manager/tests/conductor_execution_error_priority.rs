// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_execution_error_priority_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! When a retry round ends in a cancellation or a passed deadline, the call
//! reports that, not the earlier round's upstream error.
//!
//! Deviations from upstream:
//! - `TestManagerRetryPreservesTerminalContextErrorAfterUpstreamFailure`:
//!   there is no context to cancel, so the "canceled" case only has the
//!   executor return [`ExecError::canceled`], as an executor does when its
//!   call is canceled; "the earlier error isn't reported" is checked as not
//!   the 503 or its message.
//! - `TestHomeRetryPreservesTerminalContextErrorAfterUpstreamFailure` and
//!   `TestHomePreferredUpstreamErrorPreservesCurrentRetryAfter` are dropped:
//!   the Home dispatcher isn't ported.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::support::*;
use crate::exec::{Dispatcher, ErrorKind, ExecError};
use crate::manager::Settings;

#[derive(Clone, Copy, Debug)]
enum Path {
    Execute,
    Count,
    Stream,
}

impl Path {
    fn name(self) -> &'static str {
        match self {
            Path::Execute => "execute",
            Path::Count => "count-tokens",
            Path::Stream => "stream",
        }
    }
}

async fn invoke(h: &Harness, path: Path, provider: &str, model: &str) -> Result<(), ExecError> {
    let provs = providers(&[provider]);
    match path {
        Path::Execute => h
            .manager
            .execute(&provs, request(model), options())
            .await
            .map(|_| ()),
        Path::Count => h
            .manager
            .count_tokens(&provs, request(model), options())
            .await
            .map(|_| ()),
        Path::Stream => {
            let mut opts = options();
            opts.stream = true;
            h.manager
                .execute_stream(&provs, request(model), opts)
                .await
                .map(|_| ())
        }
    }
}

#[tokio::test(start_paused = true)]
async fn manager_retry_preserves_terminal_context_error_after_upstream_failure() {
    let terminal_cases = [
        ("canceled", ErrorKind::Canceled),
        ("deadline-exceeded", ErrorKind::DeadlineExceeded),
    ];
    for path in [Path::Execute, Path::Count, Path::Stream] {
        for (case, terminal_kind) in terminal_cases {
            let provider = format!("retry-terminal-priority-{}-{case}", path.name());
            let model = format!("{provider}-model");
            let auth_id = format!("{provider}-auth");
            let upstream_message = "first retry round reached upstream";

            let h = Harness::new(Settings {
                request_retry: 1,
                ..Settings::default()
            });
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let executor = FakeExecutor::with(&provider, move |_| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Reply::status(503, upstream_message);
                }
                Reply::Err(match terminal_kind {
                    ErrorKind::Canceled => ExecError::canceled(),
                    _ => ExecError::new(ErrorKind::DeadlineExceeded, "context deadline exceeded"),
                })
            });
            h.executor(&executor);
            h.add(
                auth_with_metadata(
                    &auth_id,
                    &provider,
                    json!({"request_retry": 1, "disable_cooling": true}),
                ),
                &[&model],
            );

            let err = invoke(&h, path, &provider, &model)
                .await
                .expect_err("execution error = nil");
            let label = format!("{}/{case}", path.name());
            assert_eq!(
                err.kind, terminal_kind,
                "{label}: execution error = {err}, want {terminal_kind:?}"
            );
            assert!(
                err.status != 503 && err.message != upstream_message,
                "{label}: execution error = {err}, earlier upstream error took priority"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "{label}: executor calls, want one upstream failure and one terminal retry"
            );
        }
    }
}
