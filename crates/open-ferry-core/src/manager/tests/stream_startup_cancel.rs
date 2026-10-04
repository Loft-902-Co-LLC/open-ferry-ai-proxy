//! A streaming call dropped while its executor is still starting the stream
//! reports itself canceled to its taps, once, for each of the executor calls
//! that start a stream: the first, and the two retries after a refresh.
//!
//! Upstream has no tests for these: its executors report from their own
//! goroutines, which a canceled context ends.
//!
//! Deviations from upstream: the whole file. The call is canceled by
//! dropping its future (a listed deviation of the manager), and the taps are
//! the port's (see [`crate::observe`]).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_core::future::BoxFuture;
use futures_util::StreamExt;
use http::{HeaderMap, Method};
use serde_json::{Value, json};

use super::support::*;
use crate::auth::Auth;
use crate::config::Config;
use crate::exec::{Dispatcher, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::{Settings, lock};
use crate::observe::usage::{Usage, reconfigure};
use crate::observe::{Observation, Outcome, RequestContext, Tap};

const MODEL: &str = "gpt-5.5";
const AUTH: &str = "codex-1";

/// What the first stream call of [`Scripted`] does. Every later one never
/// answers.
#[derive(Clone, Copy, Debug)]
enum First {
    /// Never answers.
    Stall,
    /// Fails with a 401 at once.
    Unauthorized,
    /// Answers with a stream whose first item is a 401.
    UnauthorizedInStream,
}

/// An executor whose stream calls follow a script, then never answer; its
/// refresh succeeds.
struct Scripted {
    first: First,
    calls: AtomicUsize,
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
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let first = self.first;
        Box::pin(async move {
            if call == 0 {
                match first {
                    First::Stall => {}
                    First::Unauthorized => return Err(ExecError::upstream(401, "token invalid")),
                    First::UnauthorizedInStream => {
                        return Ok(StreamResponse {
                            headers: HeaderMap::new(),
                            chunks: futures_util::stream::iter([Err(ExecError::upstream(
                                401,
                                "token invalid",
                            ))])
                            .boxed(),
                        });
                    }
                }
            }
            std::future::pending::<Result<StreamResponse, ExecError>>().await
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
        Box::pin(async move {
            let mut refreshed = (*auth).clone();
            refreshed
                .metadata
                .insert("access_token".into(), json!("fresh-access-token"));
            Ok(refreshed)
        })
    }
}

/// A tap that writes down how each executor call ended.
#[derive(Default)]
struct Finishes(Mutex<Vec<Outcome>>);

impl Tap for Finishes {
    fn finish(&self, outcome: Outcome) {
        lock(&self.0).push(outcome);
    }
}

/// How a streaming call that never gets past its executor ended, when the
/// executor does `first` and then never answers, and it is dropped after
/// five seconds: what a tap was told, and the usage records.
async fn cancel_while_starting(first: First) -> (Vec<Outcome>, Vec<Value>) {
    let h = Harness::new(Settings::default());
    h.manager.register_executor(Arc::new(Scripted {
        first,
        calls: AtomicUsize::new(0),
    }));
    h.add(
        auth_with_metadata(
            AUTH,
            "codex",
            json!({"access_token": "stale-access-token", "refresh_token": "refresh-token"}),
        ),
        &[MODEL],
    );

    let config = Config {
        usage_statistics_enabled: true,
        ..Config::default()
    };
    let usage = Usage::new(&config);
    reconfigure(&usage, None, &config, true);
    let context = Arc::new(RequestContext::new(
        Method::POST,
        "/v1/responses".to_owned(),
    ));
    let req = request(MODEL);
    let mut opts = options();
    opts.stream = true;
    let usage_tap = usage.tap(&context, &req, &opts).expect("the usage tap");
    let finishes = Arc::new(Finishes::default());
    opts.observation = Some(Arc::new(Observation::new(
        context,
        vec![finishes.clone(), usage_tap],
    )));

    let providers = providers(&["codex"]);
    let call = h.manager.execute_stream(&providers, req, opts);
    let waited = tokio::time::timeout(Duration::from_secs(5), call).await;
    assert!(waited.is_err(), "the call is still waiting for its stream");

    let finishes = lock(&finishes.0).clone();
    let records = usage
        .pop_oldest(usize::MAX)
        .iter()
        .map(|record| serde_json::from_slice(record).expect("a JSON record"))
        .collect();
    (finishes, records)
}

/// Not upstream's: a call dropped while its first executor call is
/// starting the stream tells the taps once that it was canceled, and
/// leaves one failed usage record, status 499, `context canceled`.
#[tokio::test(start_paused = true)]
async fn cancel_during_the_first_stream_call() {
    let (finishes, records) = cancel_while_starting(First::Stall).await;
    assert_eq!(finishes, [Outcome::Canceled]);
    assert_eq!(records.len(), 1, "records: {records:?}");
    assert_eq!(records[0]["failed"], json!(true));
    assert_eq!(records[0]["fail"]["status_code"], json!(499));
    assert_eq!(records[0]["fail"]["body"], json!("context canceled"));
}

/// Not upstream's: a call dropped while its retry after a refresh, made as
/// the first call failed with a 401, is starting the stream tells the taps
/// the first call failed and the retry was canceled, once each; the usage
/// records are the 401 and a canceled call.
#[tokio::test(start_paused = true)]
async fn cancel_during_the_retry_after_a_401() {
    let (finishes, records) = cancel_while_starting(First::Unauthorized).await;
    assert_eq!(finishes, [Outcome::Failed, Outcome::Canceled]);
    assert_eq!(records.len(), 2, "records: {records:?}");
    assert_eq!(records[0]["fail"]["status_code"], json!(401));
    assert_eq!(records[1]["failed"], json!(true));
    assert_eq!(records[1]["fail"]["status_code"], json!(499));
    assert_eq!(records[1]["fail"]["body"], json!("context canceled"));
}

/// Not upstream's: the same, when the 401 is the first item of the stream
/// the first call returned.
#[tokio::test(start_paused = true)]
async fn cancel_during_the_retry_after_a_401_in_the_stream() {
    let (finishes, records) = cancel_while_starting(First::UnauthorizedInStream).await;
    assert_eq!(finishes, [Outcome::Failed, Outcome::Canceled]);
    assert_eq!(records.len(), 2, "records: {records:?}");
    assert_eq!(records[0]["fail"]["status_code"], json!(401));
    assert_eq!(records[1]["failed"], json!(true));
    assert_eq!(records[1]["fail"]["status_code"], json!(499));
    assert_eq!(records[1]["fail"]["body"], json!("context canceled"));
}
