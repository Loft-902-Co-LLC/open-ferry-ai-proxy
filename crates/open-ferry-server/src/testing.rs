//! Test doubles for the catalog and the dispatcher.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, StreamExt, stream};
use http::HeaderMap;
use open_ferry_core::exec::{
    Dispatcher, ExecError, Options, ProviderId, Request, Response, StreamResponse,
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
    /// A stream of these chunks, with these headers.
    Stream(HeaderMap, Vec<Result<Bytes, ExecError>>),
    /// A stream that gives these chunks, then never ends.
    Hang(HeaderMap, Vec<Result<Bytes, ExecError>>),
    /// An error.
    Fail(ExecError),
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

/// A dispatcher that gives scripted outcomes, in order, and records calls.
#[derive(Default)]
pub(crate) struct FakeDispatcher {
    outcomes: Mutex<VecDeque<Outcome>>,
    calls: Mutex<Vec<Recorded>>,
}

impl FakeDispatcher {
    pub(crate) fn new(outcomes: impl IntoIterator<Item = Outcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            calls: Mutex::default(),
        })
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
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Recorded {
                method,
                providers: providers.to_vec(),
                request,
                options,
            });
        self.outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| panic!("no outcome left for {method}"))
    }

    fn once(
        &self,
        method: &'static str,
        providers: &[ProviderId],
        request: Request,
        options: Options,
    ) -> Result<Response, ExecError> {
        match self.take(method, providers, request, options) {
            Outcome::Reply(response) => Ok(response),
            Outcome::Fail(error) => Err(error),
            Outcome::Stream(..) | Outcome::Hang(..) => panic!("{method} was given a stream"),
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
        let result = self.once("execute", providers, request, options);
        async move { result }.boxed()
    }

    fn count_tokens<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        let result = self.once("count_tokens", providers, request, options);
        async move { result }.boxed()
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
                chunks: stream::iter(chunks).boxed(),
            }),
            Outcome::Hang(headers, chunks) => Ok(StreamResponse {
                headers,
                chunks: stream::iter(chunks).chain(stream::pending()).boxed(),
            }),
            Outcome::Fail(error) => Err(error),
            Outcome::Reply(_) => panic!("execute_stream was given a reply"),
        };
        async move { result }.boxed()
    }
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
