//! Test doubles for the catalog and the dispatcher.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, Stream, StreamExt, stream};
use http::HeaderMap;
use open_ferry_core::exec::{
    Dispatcher, ExecError, Options, ProviderId, Request, Response, StreamResponse, WebsocketSupport,
};
use open_ferry_core::models::{ModelCatalog, ModelInfo};

use crate::config::ServerConfig;
use crate::state::AppState;

/// A catalog that serves what it is told to.
#[derive(Clone, Debug, Default)]
pub(crate) struct FakeCatalog {
    providers: HashMap<String, Vec<ProviderId>>,
    first: Option<String>,
    models: Vec<ModelInfo>,
}

impl FakeCatalog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Has `providers` serve `model`, exactly as named.
    pub(crate) fn serve(mut self, model: &str, providers: &[&str]) -> Self {
        let providers = providers.iter().map(|&p| p.to_owned()).collect();
        self.providers.insert(model.to_owned(), providers);
        self
    }

    /// Has `auto` stand for `model`.
    pub(crate) fn first(mut self, model: &str) -> Self {
        self.first = Some(model.to_owned());
        self
    }

    /// Has the model list hold `models`.
    pub(crate) fn models(mut self, models: Vec<ModelInfo>) -> Self {
        self.models = models;
        self
    }
}

impl ModelCatalog for FakeCatalog {
    fn model_providers(&self, model: &str) -> Vec<ProviderId> {
        self.providers.get(model).cloned().unwrap_or_default()
    }

    fn first_available_model(&self) -> Option<String> {
        self.first.clone()
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
}

/// What a [`FakeDispatcher`] gives for a call.
pub(crate) enum Outcome {
    /// A non-streaming result.
    Reply(Response),
    /// A non-streaming result, after a wait.
    Slow(Duration, Response),
    /// A stream of these chunks, with these headers.
    Stream(HeaderMap, Vec<Result<Bytes, ExecError>>),
    /// A stream that gives these chunks, then never ends.
    Hang(HeaderMap, Vec<Result<Bytes, ExecError>>),
    /// An error.
    Fail(ExecError),
    /// This outcome, after telling the call it was given these credentials
    /// in turn.
    Via(Vec<String>, Box<Outcome>),
}

impl Outcome {
    /// A non-streaming result with `body` and no headers.
    pub(crate) fn reply(body: &str) -> Self {
        Self::Reply(Response {
            payload: Bytes::copy_from_slice(body.as_bytes()),
            headers: HeaderMap::new(),
        })
    }

    /// A stream of `chunks`, with no headers and no errors.
    pub(crate) fn chunks(chunks: &[&str]) -> Self {
        Self::Stream(
            HeaderMap::new(),
            chunks
                .iter()
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk.as_bytes())))
                .collect(),
        )
    }

    /// `outcome`, after the call is given `auths` in turn.
    pub(crate) fn via(auths: &[&str], outcome: Self) -> Self {
        Self::Via(
            auths.iter().map(|&auth| auth.to_owned()).collect(),
            Box::new(outcome),
        )
    }
}

/// A call a [`FakeDispatcher`] was given.
#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    /// `execute`, `count_tokens` or `execute_stream`.
    pub(crate) method: &'static str,
    pub(crate) providers: Vec<ProviderId>,
    pub(crate) request: Request,
    pub(crate) options: Options,
}

/// What a [`FakeDispatcher`] answers [`Dispatcher::websocket_support`] with.
type SupportFn = dyn Fn(&[ProviderId], &str, Option<&str>) -> WebsocketSupport + Send + Sync;

/// A dispatcher that gives scripted outcomes, in order, and records calls.
#[derive(Default)]
pub(crate) struct FakeDispatcher {
    outcomes: Mutex<VecDeque<Outcome>>,
    calls: Mutex<Vec<Recorded>>,
    support: Mutex<Option<Arc<SupportFn>>>,
    closed: Mutex<Vec<String>>,
    /// Held by each stream this has given, until it is dropped.
    live: Arc<()>,
}

impl FakeDispatcher {
    pub(crate) fn new(outcomes: impl IntoIterator<Item = Outcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            ..Self::default()
        })
    }

    /// Has [`Dispatcher::websocket_support`] answer with `support`.
    pub(crate) fn websocket(
        &self,
        support: impl Fn(&[ProviderId], &str, Option<&str>) -> WebsocketSupport + Send + Sync + 'static,
    ) {
        *self.support.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(support));
    }

    /// The WebSocket sessions closed so far.
    pub(crate) fn closed_sessions(&self) -> Vec<String> {
        self.closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many of the streams this has given are still held: those whose
    /// calls haven't been cancelled.
    pub(crate) fn live_streams(&self) -> usize {
        Arc::strong_count(&self.live) - 1
    }

    /// `chunks`, counted in [`FakeDispatcher::live_streams`] until dropped.
    fn held<S>(&self, chunks: S) -> BoxStream<'static, S::Item>
    where
        S: Stream + Send + 'static,
    {
        let live = Arc::clone(&self.live);
        chunks
            .map(move |chunk| {
                let _live = &live;
                chunk
            })
            .boxed()
    }

    /// The calls made so far.
    pub(crate) fn calls(&self) -> Vec<Recorded> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn take(
        &self,
        method: &'static str,
        providers: &[ProviderId],
        request: Request,
        options: Options,
    ) -> Outcome {
        let selected = options.metadata.selected_auth.clone();
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Recorded {
                method,
                providers: providers.to_vec(),
                request,
                options,
            });
        let mut outcome = self
            .outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| panic!("no outcome left for {method}"));
        while let Outcome::Via(auths, inner) = outcome {
            for auth in &auths {
                if let Some(selected) = &selected {
                    selected(auth);
                }
            }
            outcome = *inner;
        }
        outcome
    }

    fn once(
        &self,
        method: &'static str,
        providers: &[ProviderId],
        request: Request,
        options: Options,
    ) -> (Option<Duration>, Result<Response, ExecError>) {
        match self.take(method, providers, request, options) {
            Outcome::Reply(response) => (None, Ok(response)),
            Outcome::Slow(delay, response) => (Some(delay), Ok(response)),
            Outcome::Fail(error) => (None, Err(error)),
            Outcome::Stream(..) | Outcome::Hang(..) => panic!("{method} was given a stream"),
            Outcome::Via(..) => unreachable!("take unwraps credentials"),
        }
    }
}

impl Dispatcher for FakeDispatcher {
    fn execute<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        let (delay, result) = self.once("execute", providers, request, options);
        slow(delay, result).boxed()
    }

    fn count_tokens<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        let (delay, result) = self.once("count_tokens", providers, request, options);
        slow(delay, result).boxed()
    }

    fn execute_stream<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        let result = match self.take("execute_stream", providers, request, options) {
            Outcome::Stream(headers, chunks) => Ok(StreamResponse {
                headers,
                chunks: self.held(stream::iter(chunks)),
            }),
            Outcome::Hang(headers, chunks) => Ok(StreamResponse {
                headers,
                chunks: self.held(stream::iter(chunks).chain(stream::pending())),
            }),
            Outcome::Fail(error) => Err(error),
            Outcome::Reply(_) | Outcome::Slow(..) => panic!("execute_stream was given a reply"),
            Outcome::Via(..) => unreachable!("take unwraps credentials"),
        };
        async move { result }.boxed()
    }

    fn close_execution_session(&self, session_id: &str) {
        self.closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(session_id.to_owned());
    }

    fn websocket_support(
        &self,
        providers: &[ProviderId],
        model: &str,
        auth_id: Option<&str>,
    ) -> WebsocketSupport {
        let support = self
            .support
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        support.map_or_else(WebsocketSupport::default, |support| {
            support(providers, model, auth_id)
        })
    }
}

/// `result`, after `delay` if there is one.
async fn slow<T>(delay: Option<Duration>, result: T) -> T {
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    result
}

/// State with `config`, `catalog` and `dispatcher`.
pub(crate) fn state(
    config: ServerConfig,
    catalog: FakeCatalog,
    dispatcher: &Arc<FakeDispatcher>,
) -> AppState {
    let dispatcher: Arc<dyn Dispatcher> = dispatcher.clone();
    AppState::new(config, dispatcher, Arc::new(catalog))
}
