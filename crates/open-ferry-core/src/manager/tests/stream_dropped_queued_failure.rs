//! A streaming call whose executor has already queued its failure when the
//! client leaves records that failure: here the 502 of an `apply_patch`
//! call the executor couldn't translate, which it queues right behind the
//! `response.failed` frame.
//!
//! Upstream has no tests for these: its executor records the patch failure
//! before it sends anything (`RecordApplyPatchStreamFailure`), which
//! `TestApplyPatchCanceledEOFStillRecordsFailure` checks with a canceled
//! context.
//!
//! Deviations from upstream: the whole file. The client leaves by dropping
//! its stream (a listed deviation of the manager), and the taps are the
//! port's (see [`crate::observe`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::{StreamExt, stream};
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};

use super::support::*;
use crate::auth::Auth;
use crate::config::Config;
use crate::exec::{
    ChunkStream, Dispatcher, ExecError, Format, Options, Request, Response, StreamResponse,
};
use crate::executor::ProviderExecutor;
use crate::manager::{Settings, lock};
use crate::observe::redact::Secrets;
use crate::observe::usage::{Usage, reconfigure};
use crate::observe::{AttemptKind, AttemptRequest, Observation, RequestContext};

const MODEL: &str = "gemini-2.5-pro";
const AUTH: &str = "gemini-1";

/// The line the executor read, as the taps see it.
const LINE: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n";
const CREATED: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n";
const FAILED: &[u8] = b"event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n";

/// The failure the executor queues behind the `response.failed` frame.
fn patch_failure() -> ExecError {
    ExecError::upstream(
        502,
        "Invalid apply_patch tool arguments received from upstream.",
    )
}

/// A Gemini executor whose one stream call tells the taps it read [`LINE`]
/// and answers with the chunks it was given.
struct Scripted {
    chunks: Mutex<Option<ChunkStream>>,
}

impl ProviderExecutor for Scripted {
    fn id(&self) -> &str {
        "gemini"
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
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        let chunks = lock(&self.chunks).take().expect("one stream call");
        Box::pin(async move {
            let observation = options.observation.as_ref().expect("a tapped call");
            observation.attempt_request(&AttemptRequest {
                kind: AttemptKind::Stream,
                method: &Method::POST,
                url: "http://127.0.0.1:1/v1beta/models/gemini-2.5-pro:streamGenerateContent",
                headers: &HeaderMap::new(),
                body: &request.payload,
                provider: "gemini",
                model: &request.model,
                format: &Format::GEMINI,
                auth: &auth,
                secrets: &Secrets::default(),
            });
            let mut head = HeaderMap::new();
            head.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            observation.response_head(200, &head);
            observation.chunk(&Bytes::from_static(LINE));
            Ok(StreamResponse {
                headers: HeaderMap::new(),
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

/// Starts a streaming call, seen by the usage tap, whose executor answers
/// with `chunks`: the harness, the usage records' queue and the client's
/// stream.
async fn start(chunks: ChunkStream) -> (Harness, Usage, StreamResponse) {
    let h = Harness::new(Settings::default());
    h.manager.register_executor(Arc::new(Scripted {
        chunks: Mutex::new(Some(chunks)),
    }));
    h.add(auth(AUTH, "gemini"), &[MODEL]);

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
    opts.observation = Some(Arc::new(Observation::new(context, vec![usage_tap])));

    let response = h
        .manager
        .execute_stream(&providers(&["gemini"]), req, opts)
        .await
        .expect("the stream");
    (h, usage, response)
}

/// The usage records published so far.
fn records(usage: &Usage) -> Vec<Value> {
    usage
        .pop_oldest(usize::MAX)
        .iter()
        .map(|record| serde_json::from_slice(record).expect("a JSON record"))
        .collect()
}

/// Checks `records` is the one record of the patch's 502.
fn assert_patch_failure(records: &[Value]) {
    assert_eq!(records.len(), 1, "records: {records:?}");
    assert_eq!(records[0]["failed"], json!(true));
    assert_eq!(records[0]["fail"]["status_code"], json!(502));
}

/// The next chunk the client takes.
async fn next_chunk(response: &mut StreamResponse) -> Bytes {
    response
        .chunks
        .next()
        .await
        .expect("a chunk")
        .expect("not an error")
}

/// Not upstream's: the client takes the first chunk and leaves while the
/// `response.failed` frame and the 502 are still queued behind it, the
/// frame already in the manager's task, which stops there; the usage record
/// is the 502.
#[tokio::test]
async fn a_client_that_leaves_before_the_failure_frame() {
    let (_h, usage, mut response) = start(
        stream::iter([
            Ok(Bytes::from_static(CREATED)),
            Ok(Bytes::from_static(FAILED)),
            Err(patch_failure()),
        ])
        .boxed(),
    )
    .await;
    assert_eq!(next_chunk(&mut response).await, CREATED);
    drop(response);
    settle().await;
    assert_patch_failure(&records(&usage));
}

/// Opens a [`gated`] stream.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        if let Some(waker) = lock(&self.waker).take() {
            waker.wake();
        }
    }
}

/// The `response.failed` frame, then the 502, which is ready only once
/// `gate` opens.
fn gated(gate: Arc<Gate>) -> ChunkStream {
    let mut items = VecDeque::from([Ok(Bytes::from_static(FAILED)), Err(patch_failure())]);
    stream::poll_fn(move |cx| {
        if items.len() == 1 && !gate.open.load(Ordering::SeqCst) {
            *lock(&gate.waker) = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(items.pop_front())
    })
    .boxed()
}

/// Not upstream's: the client takes the `response.failed` frame and leaves
/// just as the executor queues the 502, before the manager's task, which
/// then stops for the client that left, reads it (as when the two run on
/// different threads); the usage record is the 502.
#[tokio::test]
async fn a_client_that_leaves_after_the_failure_frame() {
    let gate = Arc::new(Gate::default());
    let (_h, usage, mut response) = start(gated(Arc::clone(&gate))).await;
    assert_eq!(next_chunk(&mut response).await, FAILED);
    gate.open();
    drop(response);
    settle().await;
    assert_patch_failure(&records(&usage));
}
