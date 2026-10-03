// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_claude_cancellation_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A canceled call never cools its credential down, whether it is canceled
//! during a refresh after a 401 or in a stream's tail; a real upstream
//! failure still does.
//!
//! Deviations from upstream:
//! - Cancellation here is dropping the call's future or stream, where
//!   upstream cancels a context (a listed deviation of the manager).
//! - `TestManagerClaudeRefreshCancellationStopsWithoutCooldown`: the call is
//!   a task aborted while the refresh is in flight, in place of a refresh
//!   that cancels the context; the "error is context.Canceled" check becomes
//!   "the task was cancelled".
//! - `TestManagerClaudeStreamTailCancellationIsAvailabilityNeutral`: hooks
//!   aren't ported, so "one failed result" is checked as one generation bump
//!   (each recorded result bumps it once), and its request-scoped code and
//!   status 0 through `result_error_from_error`, which builds the recorded
//!   error. A second test covers a client that drops the stream before the
//!   tail: nothing is recorded (the manager's listed stream deviation).
//! - `TestManagerClaudePrepareCancellationStopsWithoutCooldown` is dropped:
//!   request preparation isn't ported.
//! - `TestClaudeRequestCancellationDoesNotChangeOtherProviders` is dropped:
//!   it tests `claudeOAuthRequestCancellation`, a context check the port
//!   doesn't need (no contexts).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::StreamExt;
use http::HeaderMap;
use serde_json::json;
use tokio::sync::mpsc;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{Dispatcher, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::classify::{CODE_REQUEST_SCOPED, ErrView, result_error_from_error};
use crate::manager::{Settings, lock};

pub(super) const CLAUDE_CANCEL_AUTH: &str = "claude-cancel-auth";
pub(super) const CLAUDE_CANCEL_MODEL: &str = "claude-cancel-model";

/// Upstream's `newClaudeCancellationTestManager`: a Claude OAuth credential
/// with a refresh token, no retry rounds, and `executor` registered.
pub(super) fn new_claude_cancellation_harness(executor: Arc<dyn ProviderExecutor>) -> Harness {
    let h = Harness::new(Settings::default());
    h.manager.register_executor(executor);
    let mut credential = auth_with_metadata(
        CLAUDE_CANCEL_AUTH,
        "claude",
        json!({
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "request_retry": 0.0,
        }),
    );
    credential
        .attributes
        .insert("auth_kind".into(), "oauth".into());
    h.add(credential, &[CLAUDE_CANCEL_MODEL]);
    h
}

/// Upstream's `requireClaudeCancellationNeutral`.
pub(super) fn require_claude_cancellation_neutral(h: &Harness, auth_id: &str, model: &str) {
    let auth = h.get(auth_id);
    assert!(
        !auth.unavailable && auth.next_retry_after.is_none(),
        "auth was cooled: unavailable={} next={:?}",
        auth.unavailable,
        auth.next_retry_after
    );
    if let Some(state) = auth.model_states.get(model) {
        assert!(
            !state.unavailable && state.next_retry_after.is_none() && !state.quota.exceeded,
            "model was cooled: {state:?}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum Path {
    Execute,
    Count,
    Stream,
}

async fn invoke(
    manager: &crate::manager::Manager,
    path: Path,
    model: &str,
) -> Result<(), ExecError> {
    let provs = providers(&["claude"]);
    match path {
        Path::Execute => manager
            .execute(&provs, request(model), options())
            .await
            .map(|_| ()),
        Path::Count => manager
            .count_tokens(&provs, request(model), options())
            .await
            .map(|_| ()),
        Path::Stream => {
            let mut opts = options();
            opts.stream = true;
            manager
                .execute_stream(&provs, request(model), opts)
                .await
                .map(|_| ())
        }
    }
}

#[tokio::test(start_paused = true)]
async fn manager_claude_refresh_cancellation_stops_without_cooldown() {
    for path in [Path::Execute, Path::Count, Path::Stream] {
        let executor = FakeExecutor::with("claude", |_| Reply::status(401, "unauthorized"));
        executor.set_refresh_delay(Duration::from_secs(10));
        let h = new_claude_cancellation_harness(executor.clone());

        let manager = h.manager.clone();
        let task = tokio::spawn(async move { invoke(&manager, path, CLAUDE_CANCEL_MODEL).await });
        settle().await;
        assert_eq!(executor.refresh_count(), 1, "{path:?}: refresh in flight");
        task.abort();
        let joined = task.await;
        assert!(
            joined.as_ref().is_err_and(|err| err.is_cancelled()),
            "{path:?}: call = {joined:?}, want canceled"
        );
        settle().await;

        assert_eq!(executor.refresh_count(), 1, "{path:?}: Refresh calls");
        assert_eq!(executor.calls().len(), 1, "{path:?}: upstream calls");
        require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);
    }
}

/// A Claude executor whose stream gives `first`, then whatever the test
/// sends on its source channel.
struct TailExecutor {
    source: Mutex<Option<mpsc::UnboundedReceiver<Result<Bytes, ExecError>>>>,
}

impl TailExecutor {
    fn new() -> (Arc<Self>, mpsc::UnboundedSender<Result<Bytes, ExecError>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let executor = Arc::new(Self {
            source: Mutex::new(Some(rx)),
        });
        (executor, tx)
    }
}

fn ok_response() -> Result<Response, ExecError> {
    Ok(Response {
        payload: Bytes::from_static(b"ok"),
        headers: HeaderMap::new(),
    })
}

impl ProviderExecutor for TailExecutor {
    fn id(&self) -> &str {
        "claude"
    }

    fn execute(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { ok_response() })
    }

    fn execute_stream(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        let source = lock(&self.source).take();
        Box::pin(async move {
            let source = source.expect("one stream per test");
            let tail = futures_util::stream::unfold(source, |mut source| async move {
                source.recv().await.map(|item| (item, source))
            });
            let chunks = futures_util::stream::iter([Ok(Bytes::from_static(b"first"))]).chain(tail);
            Ok(StreamResponse {
                headers: HeaderMap::new(),
                chunks: chunks.boxed(),
            })
        })
    }

    fn count_tokens(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { ok_response() })
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }
}

/// Upstream's `claudeRequestScopedCancellation`.
fn request_scoped_cancellation() -> ExecError {
    ExecError::canceled().with_request_scoped()
}

async fn open_tail_stream(h: &Harness) -> StreamResponse {
    let mut opts = options();
    opts.stream = true;
    let mut stream = h
        .manager
        .execute_stream(&providers(&["claude"]), request(CLAUDE_CANCEL_MODEL), opts)
        .await
        .expect("ExecuteStream() error");
    let first = stream.chunks.next().await;
    assert!(
        matches!(&first, Some(Ok(chunk)) if &chunk[..] == b"first"),
        "first chunk = {first:?}"
    );
    stream
}

#[tokio::test(start_paused = true)]
async fn manager_claude_stream_tail_cancellation_is_availability_neutral() {
    let (executor, source) = TailExecutor::new();
    let h = new_claude_cancellation_harness(executor);
    let mut stream = open_tail_stream(&h).await;
    let (_, before) = h.versions(CLAUDE_CANCEL_AUTH);

    source
        .send(Err(request_scoped_cancellation()))
        .expect("tail send");
    drop(source);
    let mut tail_errors = Vec::new();
    while let Some(item) = stream.chunks.next().await {
        if let Err(err) = item {
            tail_errors.push(err);
        }
    }
    settle().await;

    let (_, after) = h.versions(CLAUDE_CANCEL_AUTH);
    assert_eq!(
        after - before,
        1,
        "results = {}, want one failed cancellation result",
        after - before
    );
    assert_eq!(tail_errors.len(), 1, "tail errors = {tail_errors:?}");
    let recorded = result_error_from_error(ErrView::Exec(&tail_errors[0]));
    assert!(
        recorded.code == CODE_REQUEST_SCOPED && recorded.http_status == 0,
        "cancellation result = {recorded:?}, want request-scoped status 0"
    );
    require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);
}

#[tokio::test(start_paused = true)]
async fn manager_claude_stream_tail_after_client_drop_records_nothing() {
    let (executor, source) = TailExecutor::new();
    let h = new_claude_cancellation_harness(executor);
    let stream = open_tail_stream(&h).await;
    let before = h.versions(CLAUDE_CANCEL_AUTH);

    drop(stream);
    settle().await;
    // The stream task has stopped reading and dropped the source.
    assert!(
        source.send(Err(request_scoped_cancellation())).is_err(),
        "source still read after the client dropped the stream"
    );
    settle().await;

    assert_eq!(
        h.versions(CLAUDE_CANCEL_AUTH),
        before,
        "a result was recorded"
    );
    require_claude_cancellation_neutral(&h, CLAUDE_CANCEL_AUTH, CLAUDE_CANCEL_MODEL);
}

#[tokio::test(start_paused = true)]
async fn manager_claude_upstream_failure_still_cools_credential() {
    let executor = FakeExecutor::with("claude", |_| Reply::status(500, "upstream failure"));
    let h = new_claude_cancellation_harness(executor);

    let err = h
        .manager
        .execute(
            &providers(&["claude"]),
            request(CLAUDE_CANCEL_MODEL),
            options(),
        )
        .await
        .expect_err("Execute() error = nil, want HTTP 500");
    assert_eq!(
        err.http_status(),
        500,
        "Execute() error = {err}, want HTTP 500"
    );
    let got = h.get(CLAUDE_CANCEL_AUTH);
    let state = got.model_states.get(CLAUDE_CANCEL_MODEL);
    assert!(
        state.is_some_and(|s| s.unavailable && s.next_retry_after.is_some()),
        "upstream failure did not cool model: {state:?}"
    );
}
