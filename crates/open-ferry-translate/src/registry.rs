// Ported from CLIProxyAPI sdk/translator (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The translator registry: which translators convert requests and responses
//! between two formats, and what happens when there are none.
//!
//! A pair is registered from the client's format to the provider's. Requests
//! are translated in that direction and responses the other way, so
//! [`Registry::response_stream`] and the other response methods take the
//! provider's format as `from` and the client's as `to`, as upstream's do.
//!
//! [`Registry::builtin`] holds the translators ported so far:
//!
//! | Client format | Provider format |
//! |---|---|
//! | `claude` | `codex` |
//! | `openai` | `codex` |
//! | `openai-response` | `codex` |
//! | `openai` | `claude` |
//! | `openai-response` | `claude` |
//! | `claude` | `openai` |
//! | `openai` | `openai` |
//! | `openai-response` | `openai` |
//! | `claude` | `gemini` |
//! | `gemini` | `gemini` |
//! | `openai` | `gemini` |
//!
//! Translating a request also carries over whether the client asked to see
//! reasoning summaries, in the provider's own terms. With no translator, the
//! request is passed on with only its `model` replaced.
//!
//! A request translator can refuse a request it can't send faithfully, such
//! as a user turn left empty by an attachment the provider can't take. The
//! refusal is [`RequestEnvelope::err`]; [`Registry::translate_request_checked`]
//! returns it, so executors can answer 400 without calling the provider.
//!
//! Deviations from upstream:
//! - Request bodies are parsed JSON. Response chunks and bodies are bytes, as
//!   upstream's are. So with no translator, a `model` that is an object or
//!   array is compared with the model as compact JSON, where upstream
//!   compares the text as the client wrote it: for the model `{}`, upstream
//!   replaces `{ }` with the string, and we leave it.
//! - A response stream is an object, [`ResponseStream`], made once per
//!   response. Upstream passes the same `*any` to every call, which the
//!   translator fills on the first one; its executors then read a failed
//!   `apply_patch` call from it. The stream's translator is looked up when it
//!   is made, not for each chunk.
//! - Empty chunks are left out. Upstream's Codex to Claude translator returns
//!   one, possibly empty, for each `data:` line, and its Chat Completions
//!   passthrough one for each line but `[DONE]`; its stream manager drops
//!   empty chunks before the handlers see them.
//! - A non-streaming translator returns `None` where upstream's returns `nil`
//!   or records a failed `apply_patch` call; both give `nil` from upstream's
//!   registry when the caller passes a parameter, as its executors do.
//! - Not ported: plugin hooks and the pipeline's middleware, which only
//!   plugins use; the envelope's `ModelInfo`, which comes from configured
//!   accounts; and `ConfigurationUpdatesChanged`, which only plugin hooks
//!   set. Models are looked up in the static catalog in use
//!   ([`ModelCatalog::current`]), taken for each request.

mod builtin;

use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use serde_json::Value;

pub use crate::common::parts::UnsupportedPartError;
use crate::json::{set_path, str_of};
use crate::models::ModelCatalog;
use crate::thinking::summary;

/// A request or response format, such as `openai` for Chat Completions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Format(Cow<'static, str>);

impl Format {
    /// OpenAI Chat Completions.
    pub const OPENAI: Self = Self::from_static("openai");
    /// OpenAI Responses.
    pub const OPENAI_RESPONSE: Self = Self::from_static("openai-response");
    /// Anthropic Messages.
    pub const CLAUDE: Self = Self::from_static("claude");
    /// Gemini `generateContent`.
    pub const GEMINI: Self = Self::from_static("gemini");
    /// Codex, a dialect of OpenAI Responses.
    pub const CODEX: Self = Self::from_static("codex");
    /// Antigravity, a dialect of Gemini.
    pub const ANTIGRAVITY: Self = Self::from_static("antigravity");
    /// Gemini Interactions.
    pub const INTERACTIONS: Self = Self::from_static("interactions");
    /// The OpenAI Images endpoints, `/v1/images/generations` and
    /// `/v1/images/edits`. A request may be JSON or a multipart form, and no
    /// translator takes it: the executors read it themselves.
    pub const OPENAI_IMAGE: Self = Self::from_static("openai-image");
    /// The OpenAI Videos endpoints. As with [`Format::OPENAI_IMAGE`], no
    /// translator takes a request in it.
    pub const OPENAI_VIDEO: Self = Self::from_static("openai-video");

    /// A format named by a string literal.
    pub const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// A format with any name.
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }

    /// The format's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&'static str> for Format {
    fn from(name: &'static str) -> Self {
        Self::from_static(name)
    }
}

impl From<String> for Format {
    fn from(name: String) -> Self {
        Self(Cow::Owned(name))
    }
}

/// A request on its way through translation.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestEnvelope {
    /// The format `body` is in.
    pub format: Format,
    /// The model the request is for, as resolved by the proxy.
    pub model: String,
    /// Whether the client asked for a streaming response.
    pub stream: bool,
    /// The request.
    pub body: Value,
    /// Why the request must not be sent, when a translator refused it
    /// (upstream's `Err`). `body` still holds what the translator produced.
    pub err: Option<UnsupportedPartError>,
}

/// Translates a request body, given the model and whether it streams. It
/// gives back the body, and a refusal when the request must not be sent.
pub type RequestTransform =
    Arc<dyn Fn(&str, Value, bool) -> (Value, Option<UnsupportedPartError>) + Send + Sync>;

impl RequestEnvelope {
    /// A request in `format` for `model`, not yet refused.
    pub fn new(format: &Format, model: &str, stream: bool, body: Value) -> Self {
        Self {
            format: format.clone(),
            model: model.to_owned(),
            stream,
            body,
            err: None,
        }
    }
}

/// Translates a whole [`RequestEnvelope`].
pub type RequestEnvelopeTransform = Arc<dyn Fn(RequestEnvelope) -> RequestEnvelope + Send + Sync>;

/// What a response translator knows about the request it answers.
#[derive(Clone, Copy, Debug)]
pub struct ResponseContext<'a> {
    /// The model the request was for.
    pub model: &'a str,
    /// The client's request.
    pub original_request: &'a Value,
    /// The request sent to the provider.
    pub request: &'a Value,
}

/// Translates one response stream, a chunk at a time. Make one per response:
/// translators keep state between chunks.
pub trait StreamTranslator: Send {
    /// Translates one chunk, usually one line, of the provider's stream into
    /// the chunks to send to the client.
    fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>>;

    /// Call when the provider's stream ends. Returns any chunks still to
    /// send; a translator can record a [`tool_input_error`] instead of
    /// letting a cut-off stream look complete.
    ///
    /// [`tool_input_error`]: StreamTranslator::tool_input_error
    fn finish(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// Why the stream failed, if a tool call's input from the provider was
    /// unusable. The response must then end as an error, after any chunks
    /// already returned.
    fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        None
    }

    /// Whether the provider's stream has said how the turn ended, so that a
    /// clean end without `[DONE]` can still be completed for an OpenAI
    /// Responses client by translating `data: [DONE]` (upstream's
    /// `CanFinalizeResponseStream`). By default it can't.
    fn can_finalize_response_stream(&self) -> bool {
        false
    }
}

/// Makes a [`StreamTranslator`] for one response.
pub type StreamTransform =
    Arc<dyn Fn(&ResponseContext<'_>) -> Box<dyn StreamTranslator> + Send + Sync>;

/// Translates a whole response. `None` means it failed, and there is nothing
/// to send.
pub type NonStreamTransform =
    Arc<dyn Fn(&ResponseContext<'_>, &[u8]) -> Option<Vec<u8>> + Send + Sync>;

/// Writes a token count in the client's format.
pub type TokenCountTransform = Arc<dyn Fn(i64) -> Vec<u8> + Send + Sync>;

/// The response translators for one pair of formats. Any of them may be
/// missing.
#[derive(Clone, Default)]
pub struct ResponseTransform {
    /// Translates streaming responses.
    pub stream: Option<StreamTransform>,
    /// Translates non-streaming responses.
    pub non_stream: Option<NonStreamTransform>,
    /// Translates token counts.
    pub token_count: Option<TokenCountTransform>,
}

impl ResponseTransform {
    fn is_empty(&self) -> bool {
        self.stream.is_none() && self.non_stream.is_none() && self.token_count.is_none()
    }
}

impl fmt::Debug for ResponseTransform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseTransform")
            .field("stream", &self.stream.is_some())
            .field("non_stream", &self.non_stream.is_some())
            .field("token_count", &self.token_count.is_some())
            .finish()
    }
}

type ByTarget<T> = HashMap<Format, HashMap<Format, T>>;

#[derive(Default)]
struct Tables {
    /// Client format → provider format → request translator.
    requests: ByTarget<RequestEnvelopeTransform>,
    /// Client format → provider format → response translators.
    responses: ByTarget<ResponseTransform>,
}

/// Translators between pairs of formats.
pub struct Registry {
    tables: RwLock<Tables>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tables = self.read();
        let mut pairs: Vec<(&Format, &Format)> = pairs(&tables.requests)
            .chain(pairs(&tables.responses))
            .collect();
        pairs.sort();
        pairs.dedup();
        f.debug_struct("Registry").field("pairs", &pairs).finish()
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            tables: RwLock::default(),
        }
    }

    /// A registry holding the built-in translators.
    pub fn builtin() -> Self {
        let registry = Self::new();
        builtin::register(&registry);
        registry
    }

    /// The shared registry, which starts with the built-in translators.
    pub fn global() -> &'static Self {
        static GLOBAL: LazyLock<Registry> = LazyLock::new(Registry::builtin);
        &GLOBAL
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Tables> {
        // Writes only insert or remove whole entries, so tables behind a
        // poisoned lock are still consistent.
        self.tables.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Tables> {
        self.tables.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the translators for requests from `from` to `to`, and for
    /// their responses back. Without a `request` translator, one already
    /// registered stays; `response` always replaces what was there.
    pub fn register(
        &self,
        from: Format,
        to: Format,
        request: Option<RequestTransform>,
        response: ResponseTransform,
    ) {
        let mut tables = self.write();
        if let Some(request) = request {
            let envelope: RequestEnvelopeTransform = Arc::new(move |mut req: RequestEnvelope| {
                let (body, err) = request(&req.model, req.body, req.stream);
                req.body = body;
                req.err = err;
                req
            });
            tables
                .requests
                .entry(from.clone())
                .or_default()
                .insert(to.clone(), envelope);
        }
        tables
            .responses
            .entry(from)
            .or_default()
            .insert(to, response);
    }

    /// Registers a request translator that sees the whole envelope.
    pub fn register_request_envelope(
        &self,
        from: Format,
        to: Format,
        request: RequestEnvelopeTransform,
    ) {
        self.write()
            .requests
            .entry(from)
            .or_default()
            .insert(to, request);
    }

    /// Removes the request and response translators from `from` to `to`.
    pub fn unregister(&self, from: &Format, to: &Format) {
        let mut tables = self.write();
        remove(&mut tables.requests, from, to);
        remove(&mut tables.responses, from, to);
    }

    /// Whether a request translator from `from` to `to` is registered.
    pub fn has_request_transformer(&self, from: &Format, to: &Format) -> bool {
        lookup(&self.read().requests, from, to).is_some()
    }

    /// Whether any response translator is registered for the pair `from`,
    /// `to` (the client's format and the provider's).
    pub fn has_response_transformer(&self, from: &Format, to: &Format) -> bool {
        lookup(&self.read().responses, from, to).is_some_and(|response| !response.is_empty())
    }

    /// Whether a streaming response translator is registered for the pair
    /// `from`, `to` (the client's format and the provider's).
    pub fn has_stream_response_transformer(&self, from: &Format, to: &Format) -> bool {
        lookup(&self.read().responses, from, to).is_some_and(|response| response.stream.is_some())
    }

    /// Whether a non-streaming response translator is registered for the
    /// pair `from`, `to` (the client's format and the provider's).
    pub fn has_non_stream_response_transformer(&self, from: &Format, to: &Format) -> bool {
        lookup(&self.read().responses, from, to)
            .is_some_and(|response| response.non_stream.is_some())
    }

    /// Translates a request body from `from` to `to`, ignoring a refusal.
    /// See [`translate_request_envelope`](Self::translate_request_envelope).
    pub fn translate_request(
        &self,
        from: &Format,
        to: &Format,
        model: &str,
        body: Value,
        stream: bool,
    ) -> Value {
        self.translate_request_envelope(from, to, RequestEnvelope::new(from, model, stream, body))
            .body
    }

    /// Translates a request body from `from` to `to`, or gives the reason
    /// the request must not be sent. Executors use this for the body they
    /// send, so a refused request never reaches the provider.
    pub fn translate_request_checked(
        &self,
        from: &Format,
        to: &Format,
        model: &str,
        body: Value,
        stream: bool,
    ) -> Result<Value, UnsupportedPartError> {
        let req = self.translate_request_envelope(
            from,
            to,
            RequestEnvelope::new(from, model, stream, body),
        );
        match req.err {
            Some(err) => Err(err),
            None => Ok(req.body),
        }
    }

    /// Translates a request from `from` to `to`, then asks it to show or hide
    /// reasoning summaries as the original did. With no translator, only the
    /// `model` changes, to `req.model` if that isn't empty, so that a prefix
    /// the client used to pick a provider isn't passed on.
    pub fn translate_request_envelope(
        &self,
        from: &Format,
        to: &Format,
        mut req: RequestEnvelope,
    ) -> RequestEnvelope {
        let transform = lookup(&self.read().requests, from, to).cloned();
        if let Some(transform) = transform {
            let summary = summary::extract_translated(&req.body, from.as_str(), to.as_str());
            req = transform(req);
            let models = ModelCatalog::current();
            summary::apply_for_model(&mut req.body, to.as_str(), &req.model, summary, &models);
        } else if !req.model.is_empty() && str_of(req.body.get("model")) != req.model {
            set_path(&mut req.body, "model", Value::String(req.model.clone()));
        }
        req.format = to.clone();
        req
    }

    /// Starts translating a streaming response from the provider's format
    /// `from` to the client's format `to`.
    pub fn response_stream(
        &self,
        from: &Format,
        to: &Format,
        context: &ResponseContext<'_>,
    ) -> ResponseStream {
        let stream =
            lookup(&self.read().responses, to, from).and_then(|response| response.stream.clone());
        ResponseStream {
            native: stream.map(|stream| stream(context)),
        }
    }

    /// Translates a whole response from the provider's format `from` to the
    /// client's format `to`. With no translator, `body` is returned as it is;
    /// `None` means the translator failed.
    pub fn translate_non_stream(
        &self,
        from: &Format,
        to: &Format,
        context: &ResponseContext<'_>,
        body: Vec<u8>,
    ) -> Option<Vec<u8>> {
        let transform = lookup(&self.read().responses, to, from)
            .and_then(|response| response.non_stream.clone());
        match transform {
            Some(transform) => transform(context, &body),
            None => Some(body),
        }
    }

    /// Writes a token count in the client's format `to`, for a request to the
    /// provider's format `from`. With no translator, returns `body`.
    pub fn translate_token_count(
        &self,
        from: &Format,
        to: &Format,
        count: i64,
        body: Vec<u8>,
    ) -> Vec<u8> {
        let transform = lookup(&self.read().responses, to, from)
            .and_then(|response| response.token_count.clone());
        match transform {
            Some(transform) => transform(count),
            None => body,
        }
    }
}

fn pairs<T>(table: &ByTarget<T>) -> impl Iterator<Item = (&Format, &Format)> {
    table
        .iter()
        .flat_map(|(from, by_target)| by_target.keys().map(move |to| (from, to)))
}

fn lookup<'t, T>(table: &'t ByTarget<T>, from: &Format, to: &Format) -> Option<&'t T> {
    table.get(from)?.get(to)
}

/// Removes one pair, and its source's map once that's empty.
fn remove<T>(table: &mut ByTarget<T>, from: &Format, to: &Format) {
    if let Some(by_target) = table.get_mut(from) {
        by_target.remove(to);
        if by_target.is_empty() {
            table.remove(from);
        }
    }
}

/// One response stream being translated. Without a translator for the pair,
/// each chunk other than an empty one is passed on as it is.
pub struct ResponseStream {
    native: Option<Box<dyn StreamTranslator>>,
}

impl fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseStream")
            .field("native", &self.native.is_some())
            .finish()
    }
}

impl ResponseStream {
    /// Whether a translator handles the stream.
    pub fn is_translated(&self) -> bool {
        self.native.is_some()
    }

    /// Translates one chunk of the provider's stream.
    pub fn translate(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        match &mut self.native {
            Some(native) => native.translate(chunk),
            None if chunk.is_empty() => Vec::new(),
            None => vec![chunk.to_vec()],
        }
    }

    /// Call when the provider's stream ends: see [`StreamTranslator::finish`].
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        self.native
            .as_mut()
            .map(|native| native.finish())
            .unwrap_or_default()
    }

    /// See [`StreamTranslator::tool_input_error`].
    pub fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.native.as_ref()?.tool_input_error()
    }

    /// See [`StreamTranslator::can_finalize_response_stream`]. A stream
    /// passed on as it is can't be finalized.
    pub fn can_finalize_response_stream(&self) -> bool {
        self.native
            .as_ref()
            .is_some_and(|native| native.can_finalize_response_stream())
    }
}

#[cfg(test)]
mod tests;
