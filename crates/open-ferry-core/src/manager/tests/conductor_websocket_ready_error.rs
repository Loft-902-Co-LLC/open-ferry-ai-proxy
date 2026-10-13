//! A usage-limit error the source of a stream already has when the client
//! leaves is still recorded: the credential's quota cooldown is not lost.
//!
//! Upstream has no test for this: its fix (`5dbd92a0`) makes the Responses
//! websocket's reader take a ready usage-limit error before it takes the
//! client's disconnect, and the manager half is this port's.
//!
//! Deviations from upstream: the whole file. The client leaves by dropping
//! its stream (a listed deviation of the manager).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::TimeDelta;
use futures_core::future::BoxFuture;
use futures_util::{StreamExt, stream};
use tokio::sync::oneshot;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{ChunkStream, Dispatcher, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::{Settings, lock};

const MODEL: &str = "ready-error-model";
const AUTH: &str = "codex-1";
const CREATED: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n";

/// A Codex executor whose one stream call answers with the chunks it was
/// given, and counts the stream calls it got.
struct Scripted {
    chunks: Mutex<Option<ChunkStream>>,
    streams: AtomicUsize,
}

impl ProviderExecutor for Scripted {
    fn id(&self) -> &str {
        "codex"
    }

    fn execute(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(ExecError::upstream(500, "not used")) })
    }

    fn execute_stream(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        let chunks = lock(&self.chunks).take().expect("one stream call");
        Box::pin(async move {
            Ok(StreamResponse {
                headers: Default::default(),
                chunks,
            })
        })
    }

    fn count_tokens(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async { Err(ExecError::upstream(500, "not used")) })
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }
}

/// The credential-scoped usage-limit error, with a 600 s retry hint.
fn usage_limit() -> ExecError {
    let mut err = ExecError::upstream(
        429,
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#,
    );
    err.retry_after = Some(Duration::from_secs(600));
    err.credential_scoped = true;
    err
}

/// Not upstream's: the client takes `response.created` and leaves in the
/// same instant the source's usage-limit error becomes ready, so the
/// manager's task finds both ready (as when the two run on different
/// threads); the error wins, and the credential is cooled down for the
/// provider's hint.
#[tokio::test]
async fn a_ready_usage_limit_error_wins_over_a_client_that_left() {
    let (release, gate) = oneshot::channel::<()>();
    let chunks = stream::once(async { Ok(Bytes::from_static(CREATED)) })
        .chain(stream::once(async move {
            let _ = gate.await;
            Err(usage_limit())
        }))
        .boxed();
    let executor = Arc::new(Scripted {
        chunks: Mutex::new(Some(chunks)),
        streams: AtomicUsize::new(0),
    });
    let h = Harness::new(Settings::default());
    h.manager.register_executor(executor.clone());
    h.add(auth(AUTH, "codex"), &[MODEL]);

    let before = h.now();
    let mut opts = options();
    opts.stream = true;
    let mut response = h
        .manager
        .execute_stream(&providers(&["codex"]), request(MODEL), opts)
        .await
        .expect("the stream");
    let first = response
        .chunks
        .next()
        .await
        .expect("a chunk")
        .expect("not an error");
    assert_eq!(first, CREATED);
    // The task is now waiting on the source.
    settle().await;

    // No await between the two: the task finds both ready.
    release.send(()).expect("the source is waiting");
    drop(response);

    let ten_minutes = TimeDelta::seconds(600);
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.get(AUTH).quota.reason == "credential_quota" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the usage-limit error was lost: quota={:?}",
        h.get(AUTH).quota
    );
    settle().await;

    let stored = h.get(AUTH);
    assert!(
        stored
            .quota
            .next_recover_at
            .is_some_and(|t| t >= before + ten_minutes),
        "credential cooldown lost the retry hint: quota={:?}",
        stored.quota
    );
    let state = stored.model_states.get(MODEL);
    assert!(
        state
            .and_then(|state| state.next_retry_after)
            .is_some_and(|t| t >= before + ten_minutes),
        "model cooldown lost the retry hint: state={state:?}"
    );
    // The failure stands: no success cleared it, and nothing replayed it.
    assert_eq!(stored.quota.reason, "credential_quota");
    assert_eq!(executor.streams.load(Ordering::SeqCst), 1);
}
