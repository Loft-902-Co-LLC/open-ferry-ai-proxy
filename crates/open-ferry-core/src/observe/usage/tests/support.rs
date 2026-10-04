//! What the usage tests share: a clock they move by hand, and a harness
//! that drives the usage tap as the executors' reports do and reads the
//! records it queues.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde_json::Value;

use super::super::Usage;
use super::super::response_model::Clock;
use crate::auth::Auth;
use crate::exec::{ExecError, Format, Options, Request};
use crate::observe::{AttemptKind, AttemptRequest, Outcome, RequestContext, Tap};

/// A clock that moves only when told to.
#[derive(Clone)]
pub(super) struct ManualClock(Arc<Mutex<Instant>>);

impl ManualClock {
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    pub(super) fn now(&self) -> Instant {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The clock as the usage statistics read it.
    pub(super) fn clock(&self) -> Clock {
        let clock = self.clone();
        Arc::new(move || clock.now())
    }

    pub(super) fn advance(&self, by: Duration) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }
}

/// Usage statistics with the queue on, records on and a manual clock.
pub(super) struct Harness {
    pub(super) usage: Usage,
    pub(super) clock: ManualClock,
}

impl Harness {
    pub(super) fn new() -> Self {
        let clock = ManualClock::new();
        let usage = Usage::with_clock(clock.clock());
        usage.inner.queue.set_enabled(true);
        usage.inner.queue.set_usage_statistics_enabled(true);
        Self { usage, clock }
    }

    pub(super) fn advance_ms(&self, millis: u64) {
        self.clock.advance(Duration::from_millis(millis));
    }

    /// The records queued, oldest first, as JSON.
    pub(super) fn records(&self) -> Vec<Value> {
        self.usage
            .pop_oldest(usize::MAX)
            .iter()
            .map(|record| serde_json::from_slice(record).expect("record is JSON"))
            .collect()
    }

    /// The one record queued.
    #[track_caller]
    pub(super) fn record(&self) -> Value {
        let mut records = self.records();
        assert_eq!(records.len(), 1, "records: {records:?}");
        records.remove(0)
    }
}

/// A client's call, before its tap is made.
pub(super) struct ClientCall {
    pub(super) context: RequestContext,
    pub(super) request: Request,
    pub(super) options: Options,
}

impl ClientCall {
    /// A non-streaming Chat Completions call for `model`.
    pub(super) fn new(model: &str) -> Self {
        let path = "/v1/chat/completions";
        let mut context = RequestContext::new(Method::POST, path.to_owned());
        context.endpoint = format!("POST {path}");
        let mut options = Options::new(Format::OPENAI);
        options.metadata.requested_model = model.to_owned();
        options.metadata.request_path = path.to_owned();
        let request = Request {
            model: model.to_owned(),
            payload: Bytes::new(),
        };
        Self {
            context,
            request,
            options,
        }
    }

    /// The client's body.
    pub(super) fn body(mut self, body: &str) -> Self {
        self.options.original_request = Bytes::copy_from_slice(body.as_bytes());
        self.request.payload = self.options.original_request.clone();
        self
    }

    /// A header the client sent.
    pub(super) fn header(mut self, name: &str, value: &str) -> Self {
        self.options.headers.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
        self
    }

    /// The client asked for a stream.
    pub(super) fn stream(mut self) -> Self {
        self.options.stream = true;
        self
    }

    /// The tap for the call, which `harness` must keep.
    #[track_caller]
    pub(super) fn tap(self, harness: &Harness) -> Driver {
        let context = Arc::new(self.context);
        let tap = harness
            .usage
            .tap(&context, &self.request, &self.options)
            .expect("the usage statistics tap the call");
        Driver { tap, context }
    }
}

/// Reports an executor's traffic to a tap.
pub(super) struct Driver {
    pub(super) tap: Arc<dyn Tap>,
    pub(super) context: Arc<RequestContext>,
}

/// The format a provider's executor sends.
fn format_of(provider: &str) -> Format {
    match provider {
        "codex" => Format::CODEX,
        "claude" => Format::CLAUDE,
        "gemini" | "vertex" => Format::GEMINI,
        _ => Format::OPENAI,
    }
}

impl Driver {
    /// An attempt of `kind` to `provider` for `model` with `auth`.
    pub(super) fn attempt(&self, kind: AttemptKind, provider: &str, model: &str, auth: &Auth) {
        self.attempt_with(kind, provider, model, &format_of(provider), auth, &[], "{}");
    }

    /// An attempt with all its parts given.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn attempt_with(
        &self,
        kind: AttemptKind,
        provider: &str,
        model: &str,
        format: &Format,
        auth: &Auth,
        secrets: &[&str],
        body: &str,
    ) {
        let body = Bytes::copy_from_slice(body.as_bytes());
        let headers = HeaderMap::new();
        self.tap.attempt_request(&AttemptRequest {
            kind,
            method: &Method::POST,
            url: "http://127.0.0.1:1/v1/test",
            headers: &headers,
            body: &body,
            provider,
            model,
            format,
            auth,
            secrets,
        });
    }

    /// The answer's head.
    pub(super) fn head(&self, status: u16, headers: &[(&str, &str)]) {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        self.tap.response_head(status, &map);
    }

    /// A part of the answer.
    pub(super) fn chunk(&self, chunk: &str) {
        self.tap.chunk(&Bytes::copy_from_slice(chunk.as_bytes()));
    }

    /// The executor call failed with `error`.
    pub(super) fn fail(&self, error: &ExecError) {
        self.tap.error(error);
        self.tap.finish(Outcome::Failed);
    }

    pub(super) fn finish(&self, outcome: Outcome) {
        self.tap.finish(outcome);
    }
}

/// A credential with `id`, `index` and `provider`.
pub(super) fn auth(id: &str, index: &str, provider: &str) -> Auth {
    Auth {
        id: id.to_owned(),
        index: index.to_owned(),
        provider: provider.to_owned(),
        ..Auth::default()
    }
}

/// `record`'s string field `key`.
#[track_caller]
pub(super) fn str_field<'a>(record: &'a Value, key: &str) -> &'a str {
    record
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no string {key:?} in {record}"))
}

/// `record`'s integer at `pointer`, such as `/tokens/total_tokens`.
#[track_caller]
pub(super) fn int_at(record: &Value, pointer: &str) -> i64 {
    record
        .pointer(pointer)
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("no integer {pointer:?} in {record}"))
}

/// `record`'s boolean at `pointer`.
#[track_caller]
pub(super) fn bool_at(record: &Value, pointer: &str) -> bool {
    record
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("no boolean {pointer:?} in {record}"))
}

/// The warnings logged on this thread while it lives, and on any thread
/// given its [`Warnings::dispatch`].
pub(super) struct Warnings {
    lines: Arc<Mutex<Vec<String>>>,
    dispatch: tracing::Dispatch,
    _guard: tracing::dispatcher::DefaultGuard,
}

impl Warnings {
    pub(super) fn capture() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let dispatch = tracing::Dispatch::new(Capture(Arc::clone(&lines)));
        let guard = tracing::dispatcher::set_default(&dispatch);
        Self {
            lines,
            dispatch,
            _guard: guard,
        }
    }

    /// What another thread must set to have its warnings captured too.
    pub(super) fn dispatch(&self) -> &tracing::Dispatch {
        &self.dispatch
    }

    /// The substitution warnings logged so far.
    pub(super) fn substitutions(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|line| line.contains("upstream served model"))
            .cloned()
            .collect()
    }
}

/// A subscriber that keeps the message of each warning.
struct Capture(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for Capture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.0);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// An event's `message` field.
struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}
