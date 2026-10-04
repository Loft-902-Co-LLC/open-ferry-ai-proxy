// Ported from CLIProxyAPI sdk/cliproxy/auth/*_test.go (the fake executors,
// stores and registry setup the tests share) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Fakes for the manager tests.
//!
//! - [`FakeExecutor`] answers every call through a handler and records the
//!   calls, refreshes and closed sessions.
//! - [`FakeModels`] is the model registry: which models each credential
//!   serves, and the projections the manager published.
//! - [`FakeStore`] keeps saved credentials in memory, and can fail saves.
//! - [`TestClock`] is the manager's clock: a fixed base plus a hand-moved
//!   offset plus the Tokio time elapsed, so paused Tokio time moves it.
//!
//! Deviations from upstream:
//! - One configurable fake replaces upstream's many single-purpose executor
//!   types.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::{TimeDelta, TimeZone, Utc};
use futures_core::future::BoxFuture;
use futures_util::StreamExt;
use http::HeaderMap;

use crate::auth::{Auth, AuthStore, Timestamp};
use crate::exec::{Dispatcher, ExecError, Format, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use crate::manager::select::{ClientModels, ModelProjection};
use crate::manager::{Clock, Manager, Settings, lock};

/// The kind of call an executor got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Execute,
    Stream,
    Count,
}

/// One call an executor got.
#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub(crate) kind: Kind,
    pub(crate) auth_id: String,
    /// The request's model, as the executor saw it.
    pub(crate) model: String,
    pub(crate) payload: Bytes,
    pub(crate) options: Options,
    /// The credential as the executor got it.
    pub(crate) auth: Arc<Auth>,
}

/// What a fake executor answers.
pub(crate) enum Reply {
    /// A success with this body. A stream gets it as its one chunk.
    Ok(Response),
    /// A failure. A stream fails before it starts.
    Err(ExecError),
    /// A stream with these headers and chunks, in order. A unary call gets
    /// the chunks joined, or the first error among them.
    Stream {
        headers: HeaderMap,
        chunks: Vec<Result<Bytes, ExecError>>,
    },
}

impl Reply {
    /// A success with `body`.
    pub(crate) fn ok(body: impl Into<Bytes>) -> Self {
        Self::Ok(Response {
            payload: body.into(),
            headers: HeaderMap::new(),
        })
    }

    /// A provider failure with `status` and `body`.
    pub(crate) fn status(status: u16, body: &str) -> Self {
        Self::Err(ExecError::upstream(status, body))
    }

    /// A stream of these chunks.
    pub(crate) fn chunks(chunks: Vec<Result<Bytes, ExecError>>) -> Self {
        Self::Stream {
            headers: HeaderMap::new(),
            chunks,
        }
    }
}

type Handler = Arc<dyn Fn(&Call) -> Reply + Send + Sync>;
type RefreshHandler = Arc<dyn Fn(&Auth) -> Result<Auth, ExecError> + Send + Sync>;

/// A configurable executor.
pub(crate) struct FakeExecutor {
    id: String,
    handler: Mutex<Handler>,
    refresh: Mutex<RefreshHandler>,
    lead: Mutex<Option<Duration>>,
    delay: Mutex<Duration>,
    refresh_delay: Mutex<Duration>,
    calls: Mutex<Vec<Call>>,
    refreshes: Mutex<Vec<Arc<Auth>>>,
    closed: Mutex<Vec<String>>,
}

impl FakeExecutor {
    /// An executor for `id` that answers every call with `ok`, and returns
    /// the credential unchanged from a refresh.
    pub(crate) fn new(id: &str) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_owned(),
            handler: Mutex::new(Arc::new(|_: &Call| Reply::ok("ok"))),
            refresh: Mutex::new(Arc::new(|auth: &Auth| Ok(auth.clone()))),
            lead: Mutex::new(None),
            delay: Mutex::new(Duration::ZERO),
            refresh_delay: Mutex::new(Duration::ZERO),
            calls: Mutex::new(Vec::new()),
            refreshes: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        })
    }

    /// An executor for `id` that answers through `handler`.
    pub(crate) fn with(
        id: &str,
        handler: impl Fn(&Call) -> Reply + Send + Sync + 'static,
    ) -> Arc<Self> {
        let executor = Self::new(id);
        executor.set_handler(handler);
        executor
    }

    pub(crate) fn set_handler(&self, handler: impl Fn(&Call) -> Reply + Send + Sync + 'static) {
        *lock(&self.handler) = Arc::new(handler);
    }

    pub(crate) fn set_refresh(
        &self,
        refresh: impl Fn(&Auth) -> Result<Auth, ExecError> + Send + Sync + 'static,
    ) {
        *lock(&self.refresh) = Arc::new(refresh);
    }

    pub(crate) fn set_lead(&self, lead: Option<Duration>) {
        *lock(&self.lead) = lead;
    }

    /// Makes every call wait `delay` (Tokio time) before answering.
    pub(crate) fn set_delay(&self, delay: Duration) {
        *lock(&self.delay) = delay;
    }

    /// Makes every refresh wait `delay` (Tokio time) before answering.
    pub(crate) fn set_refresh_delay(&self, delay: Duration) {
        *lock(&self.refresh_delay) = delay;
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    /// The credentials called, in order, for calls of `kind`.
    pub(crate) fn ids(&self, kind: Kind) -> Vec<String> {
        lock(&self.calls)
            .iter()
            .filter(|call| call.kind == kind)
            .map(|call| call.auth_id.clone())
            .collect()
    }

    /// The models called, in order, for calls of `kind`.
    pub(crate) fn models(&self, kind: Kind) -> Vec<String> {
        lock(&self.calls)
            .iter()
            .filter(|call| call.kind == kind)
            .map(|call| call.model.clone())
            .collect()
    }

    /// The credentials refreshed, in order, as the executor got them.
    pub(crate) fn refreshes(&self) -> Vec<Arc<Auth>> {
        lock(&self.refreshes).clone()
    }

    pub(crate) fn refresh_count(&self) -> usize {
        lock(&self.refreshes).len()
    }

    pub(crate) fn closed_sessions(&self) -> Vec<String> {
        lock(&self.closed).clone()
    }

    async fn answer(
        &self,
        kind: Kind,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> Reply {
        let call = Call {
            kind,
            auth_id: auth.id.clone(),
            model: request.model.clone(),
            payload: request.payload.clone(),
            options,
            auth,
        };
        lock(&self.calls).push(call.clone());
        let delay = *lock(&self.delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let handler = lock(&self.handler).clone();
        handler(&call)
    }

    async fn unary(
        &self,
        kind: Kind,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> Result<Response, ExecError> {
        match self.answer(kind, auth, request, options).await {
            Reply::Ok(response) => Ok(response),
            Reply::Err(err) => Err(err),
            Reply::Stream { headers, chunks } => {
                let mut payload = Vec::new();
                for chunk in chunks {
                    payload.extend_from_slice(&chunk?);
                }
                Ok(Response {
                    payload: payload.into(),
                    headers,
                })
            }
        }
    }
}

impl ProviderExecutor for FakeExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(self.unary(Kind::Execute, auth, request, options))
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        Box::pin(async move {
            match self.answer(Kind::Stream, auth, request, options).await {
                Reply::Ok(response) => Ok(StreamResponse {
                    headers: response.headers,
                    chunks: futures_util::stream::iter([Ok(response.payload)]).boxed(),
                }),
                Reply::Err(err) => Err(err),
                Reply::Stream { headers, chunks } => Ok(StreamResponse {
                    headers,
                    chunks: futures_util::stream::iter(chunks).boxed(),
                }),
            }
        })
    }

    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(self.unary(Kind::Count, auth, request, options))
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move {
            lock(&self.refreshes).push(auth.clone());
            let delay = *lock(&self.refresh_delay);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let refresh = lock(&self.refresh).clone();
            refresh(&auth)
        })
    }

    fn refresh_lead(&self) -> Option<Duration> {
        *lock(&self.lead)
    }

    fn close_execution_session(&self, session_id: &str) {
        lock(&self.closed).push(session_id.to_owned());
    }
}

/// The projections one publish carried.
#[derive(Clone, Debug)]
pub(crate) struct Published {
    pub(crate) client_id: String,
    pub(crate) epoch: u64,
    pub(crate) generation: u64,
    pub(crate) projections: Vec<ModelProjection>,
}

/// The model registry: the models each credential serves.
#[derive(Default)]
pub(crate) struct FakeModels {
    clients: Mutex<HashMap<String, (Vec<String>, u64)>>,
    published: Mutex<Vec<Published>>,
}

impl FakeModels {
    /// Registers `models` for credential `client_id`, bumping its epoch
    /// (upstream's `RegisterClient`).
    pub(crate) fn register(&self, client_id: &str, models: &[&str]) {
        let mut clients = lock(&self.clients);
        let entry = clients.entry(client_id.to_owned()).or_default();
        entry.0 = models.iter().map(|m| (*m).to_owned()).collect();
        entry.1 += 1;
    }

    /// Forgets `client_id`'s models (upstream's `UnregisterClient`).
    pub(crate) fn unregister(&self, client_id: &str) {
        lock(&self.clients).remove(client_id);
    }

    /// Every publish so far.
    pub(crate) fn published(&self) -> Vec<Published> {
        lock(&self.published).clone()
    }

    /// The last projection published for `model` of `client_id`.
    pub(crate) fn projection(&self, client_id: &str, model: &str) -> Option<ModelProjection> {
        lock(&self.published)
            .iter()
            .rev()
            .filter(|p| p.client_id == client_id)
            .find_map(|p| {
                p.projections
                    .iter()
                    .find(|item| item.model_id == model)
                    .cloned()
            })
    }
}

impl ClientModels for FakeModels {
    fn models_for_client(&self, client_id: &str) -> Vec<String> {
        lock(&self.clients)
            .get(client_id)
            .map(|(models, _)| models.clone())
            .unwrap_or_default()
    }

    fn models_and_epoch_for_client(&self, client_id: &str) -> (Vec<String>, u64) {
        lock(&self.clients)
            .get(client_id)
            .cloned()
            .unwrap_or_default()
    }

    fn apply_client_model_projections(
        &self,
        client_id: &str,
        epoch: u64,
        generation: u64,
        projections: &[ModelProjection],
    ) -> bool {
        lock(&self.published).push(Published {
            client_id: client_id.to_owned(),
            epoch,
            generation,
            projections: projections.to_vec(),
        });
        true
    }
}

/// A store in memory.
#[derive(Default)]
pub(crate) struct FakeStore {
    items: Mutex<BTreeMap<String, Auth>>,
    saved: Mutex<Vec<Auth>>,
    deleted: Mutex<Vec<String>>,
    fail_saves: AtomicBool,
    fail_list: AtomicBool,
}

impl FakeStore {
    /// Puts `auth` in the store without recording a save.
    pub(crate) fn put(&self, auth: Auth) {
        lock(&self.items).insert(auth.id.clone(), auth);
    }

    /// Every save so far, in order.
    pub(crate) fn saved(&self) -> Vec<Auth> {
        lock(&self.saved).clone()
    }

    pub(crate) fn save_count(&self) -> usize {
        lock(&self.saved).len()
    }

    pub(crate) fn stored(&self, id: &str) -> Option<Auth> {
        lock(&self.items).get(id).cloned()
    }

    pub(crate) fn set_fail_saves(&self, fail: bool) {
        self.fail_saves.store(fail, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_list(&self, fail: bool) {
        self.fail_list.store(fail, Ordering::SeqCst);
    }
}

impl AuthStore for FakeStore {
    fn list(&self) -> io::Result<Vec<Auth>> {
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(io::Error::other("list failed"));
        }
        Ok(lock(&self.items).values().cloned().collect())
    }

    fn save(&self, auth: &Auth) -> io::Result<String> {
        if self.fail_saves.load(Ordering::SeqCst) {
            return Err(io::Error::other("save failed"));
        }
        lock(&self.saved).push(auth.clone());
        lock(&self.items).insert(auth.id.clone(), auth.clone());
        Ok(auth.id.clone())
    }

    fn delete(&self, id: &str) -> io::Result<()> {
        lock(&self.deleted).push(id.to_owned());
        lock(&self.items).remove(id);
        Ok(())
    }
}

/// The manager's clock in tests.
#[derive(Clone)]
pub(crate) struct TestClock {
    base: Timestamp,
    start: tokio::time::Instant,
    offset: Arc<Mutex<TimeDelta>>,
}

impl TestClock {
    /// A clock at 2026-06-01 00:00:00 UTC.
    pub(crate) fn new() -> Self {
        Self {
            base: Utc
                .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
                .single()
                .unwrap_or_default(),
            start: tokio::time::Instant::now(),
            offset: Arc::new(Mutex::new(TimeDelta::zero())),
        }
    }

    pub(crate) fn now(&self) -> Timestamp {
        let elapsed = TimeDelta::from_std(self.start.elapsed()).unwrap_or(TimeDelta::zero());
        self.base + *lock(&self.offset) + elapsed
    }

    /// Moves the clock forward by `d` without moving Tokio time.
    pub(crate) fn advance(&self, d: Duration) {
        let mut offset = lock(&self.offset);
        *offset += TimeDelta::from_std(d).unwrap_or(TimeDelta::zero());
    }

    /// Moves the clock back by `d`.
    pub(crate) fn rewind(&self, d: Duration) {
        let mut offset = lock(&self.offset);
        *offset -= TimeDelta::from_std(d).unwrap_or(TimeDelta::zero());
    }

    pub(crate) fn clock(&self) -> Clock {
        let this = self.clone();
        Arc::new(move || this.now())
    }
}

/// A manager wired to fakes.
pub(crate) struct Harness {
    pub(crate) manager: Manager,
    pub(crate) models: Arc<FakeModels>,
    pub(crate) store: Arc<FakeStore>,
    pub(crate) clock: TestClock,
}

impl Harness {
    /// A manager with `settings` and no store.
    pub(crate) fn new(settings: Settings) -> Self {
        Self::build(settings, false)
    }

    /// A manager with `settings` saving to a [`FakeStore`].
    pub(crate) fn with_store(settings: Settings) -> Self {
        Self::build(settings, true)
    }

    fn build(settings: Settings, with_store: bool) -> Self {
        let models = Arc::new(FakeModels::default());
        let store = Arc::new(FakeStore::default());
        let clock = TestClock::new();
        let store_arg: Option<Arc<dyn AuthStore>> = if with_store {
            Some(store.clone())
        } else {
            None
        };
        let manager = Manager::with_clock(settings, models.clone(), store_arg, clock.clock());
        Self {
            manager,
            models,
            store,
            clock,
        }
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Registers `executor`.
    pub(crate) fn executor(&self, executor: &Arc<FakeExecutor>) {
        self.manager.register_executor(executor.clone());
    }

    /// Registers `auth` with the registry serving `models`, as upstream's
    /// tests do with `RegisterClient` then `Register`.
    pub(crate) fn add(&self, auth: Auth, models: &[&str]) -> Arc<Auth> {
        if !models.is_empty() {
            self.models.register(&auth.id, models);
        }
        match self.manager.register(auth) {
            Ok(auth) => auth,
            Err(err) => panic!("register: {err}"),
        }
    }

    /// The credential with `id`.
    pub(crate) fn get(&self, id: &str) -> Arc<Auth> {
        match self.manager.get(id) {
            Some(auth) => auth,
            None => panic!("no auth {id}"),
        }
    }

    /// The (epoch, generation) of `id`.
    pub(crate) fn versions(&self, id: &str) -> (u64, u64) {
        let state = self.manager.lock();
        state
            .auths
            .get(id)
            .map(|entry| (entry.auth.registration_epoch, entry.auth.generation))
            .unwrap_or_default()
    }

    /// The `invalid_grant` failure count of `id`.
    pub(crate) fn refresh_failures(&self, id: &str) -> u32 {
        let state = self.manager.lock();
        state
            .auths
            .get(id)
            .map_or(0, |entry| entry.refresh_failures)
    }
}

/// A credential with `id` and `provider`.
pub(crate) fn auth(id: &str, provider: &str) -> Auth {
    Auth {
        id: id.to_owned(),
        provider: provider.to_owned(),
        ..Auth::default()
    }
}

/// A credential with `id`, `provider` and `metadata` (a JSON object).
pub(crate) fn auth_with_metadata(id: &str, provider: &str, metadata: serde_json::Value) -> Auth {
    let mut auth = auth(id, provider);
    if let serde_json::Value::Object(map) = metadata {
        auth.metadata = map;
    }
    auth
}

/// A request for `model`.
pub(crate) fn request(model: &str) -> Request {
    Request {
        model: model.to_owned(),
        payload: Bytes::new(),
    }
}

/// A request for `model` with a JSON `payload`.
pub(crate) fn request_with(model: &str, payload: &str) -> Request {
    Request {
        model: model.to_owned(),
        payload: Bytes::copy_from_slice(payload.as_bytes()),
    }
}

/// Default options, in the OpenAI format.
pub(crate) fn options() -> Options {
    Options::new(Format::from("openai"))
}

/// Options pinned to `auth_id`.
pub(crate) fn pinned(auth_id: &str) -> Options {
    let mut opts = options();
    opts.metadata.pinned_auth_id = Some(auth_id.to_owned());
    opts
}

/// Provider names as the dispatcher takes them.
pub(crate) fn providers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Runs a call for `model` over `provider`, and returns the credential the
/// executor got (upstream's `scheduler.pickSingle`, made through a call).
pub(crate) async fn pick_by_call(
    h: &Harness,
    executor: &FakeExecutor,
    provider: &str,
    model: &str,
) -> Result<String, ExecError> {
    let before = executor.ids(Kind::Execute).len();
    h.manager
        .execute(&providers(&[provider]), request(model), options())
        .await?;
    Ok(executor.ids(Kind::Execute)[before..]
        .last()
        .cloned()
        .expect("an executor call"))
}

/// Reads a stream to its end: the chunks as text, and the error that ended
/// it, if one did.
pub(crate) async fn collect(stream: StreamResponse) -> (Vec<String>, Option<ExecError>) {
    let mut chunks = Vec::new();
    let mut chunk_stream = stream.chunks;
    while let Some(item) = chunk_stream.next().await {
        match item {
            Ok(bytes) => chunks.push(String::from_utf8_lossy(&bytes).into_owned()),
            Err(err) => return (chunks, Some(err)),
        }
    }
    (chunks, None)
}

/// Lets spawned tasks run until they all wait (paused Tokio time).
pub(crate) async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::exec::Dispatcher;

    #[tokio::test(start_paused = true)]
    async fn harness_runs_a_call() {
        let h = Harness::new(Settings::default());
        let executor = FakeExecutor::new("codex");
        h.executor(&executor);
        h.add(auth("a", "codex"), &["gpt-5"]);
        let resp = h
            .manager
            .execute(&providers(&["codex"]), request("gpt-5"), options())
            .await;
        let resp = match resp {
            Ok(resp) => resp,
            Err(err) => panic!("execute: {err}"),
        };
        assert_eq!(&resp.payload[..], b"ok");
        assert_eq!(executor.ids(Kind::Execute), ["a"]);
    }

    #[tokio::test(start_paused = true)]
    async fn clock_follows_tokio_time() {
        let clock = TestClock::new();
        let before = clock.now();
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(clock.now() - before, TimeDelta::seconds(90));
        clock.advance(Duration::from_secs(10));
        assert_eq!(clock.now() - before, TimeDelta::seconds(100));
    }
}
