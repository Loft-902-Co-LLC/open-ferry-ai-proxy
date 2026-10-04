// Ported from the hooks of CLIProxyAPI internal/runtime/executor/helps/
// logging_helpers.go (RecordAPIRequest, RecordAPIResponseMetadata,
// RecordAPIResponseError, AppendAPIResponseChunk) and usage_helpers.go
// (UsageReporter's observe and publish calls) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The [`Tap`]s that see a call's upstream traffic, and the [`Observation`]
//! that carries them with the request's context.
//!
//! For each executor call a tap sees, in order: for each upstream attempt
//! the executor makes, the request ([`Tap::attempt_request`]), then, for an
//! attempt that has to connect first, that the request went out
//! ([`Tap::request_sent`]), then once an answer comes its head
//! ([`Tap::response_head`]) and its body as it is read ([`Tap::chunk`]), and
//! what failed if reading it did ([`Tap::attempt_error`]); then, when the
//! call failed, its error ([`Tap::error`]);
//! and last how it ended ([`Tap::finish`]), once. The executor reports the
//! attempts, and the manager the error and the end, so a call the manager
//! retries on another credential or model gives the taps another such
//! sequence. The manager makes its report before it awaits the executor, so
//! a call dropped while the executor is still connecting ends with a
//! [`Tap::finish`] of [`Outcome::Canceled`] all the same.
//!
//! Deviations from upstream:
//! - Upstream's executors write each event into the gin context and the
//!   usage reporter themselves; here they report to the taps, and each tap
//!   keeps what it wants.
//! - The error is the executor call's [`ExecError`], where upstream records
//!   the transport error and the usage failure apart. An attempt's failure
//!   after its head is told apart, as text, where upstream's executors
//!   record it.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};

use super::RequestContext;
use super::redact::Secrets;
use crate::auth::Auth;
use crate::exec::{ExecError, Format};

/// What sees a call's upstream traffic. Every method runs on the request's
/// task and must return at once; none may block or wait.
pub trait Tap: Send + Sync {
    /// An upstream attempt is about to be sent, with its credential's
    /// headers set.
    fn attempt_request(&self, _request: &AttemptRequest<'_>) {}

    /// The attempt's request is about to go out on a connection that is up.
    /// Only an attempt that connects after it is announced tells this, as a
    /// message on an upstream WebSocket does once its handshake is done; an
    /// HTTP attempt is announced as it goes out and tells nothing more. A
    /// WebSocket attempt that is retried on a new connection tells it again.
    fn request_sent(&self) {}

    /// The attempt's answer came with `status` and `headers`.
    fn response_head(&self, _status: u16, _headers: &HeaderMap) {}

    /// The next part of the attempt's answer body, as it came off the wire.
    fn chunk(&self, _chunk: &Bytes) {}

    /// The attempt failed after its answer's head came, as the executor
    /// saw it: its body couldn't be read, its stream broke off or failed,
    /// or an upstream WebSocket's turn ended empty. The executor tells this
    /// where upstream's executors record
    /// the error (`RecordAPIResponseError`); the call's error comes after
    /// through [`Tap::error`] all the same.
    fn attempt_error(&self, _message: &str) {}

    /// The executor call failed with `error`.
    fn error(&self, _error: &ExecError) {}

    /// The executor call ended.
    fn finish(&self, _outcome: Outcome) {}
}

/// How an executor call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It gave its answer, or its stream ended.
    Completed,
    /// It failed, or its stream ended with an error.
    Failed,
    /// It was dropped before it ended, as when the client went away.
    Canceled,
}

/// What an upstream attempt is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    /// A non-streaming call.
    Execute,
    /// A streaming call.
    Stream,
    /// A token count.
    CountTokens,
    /// A plain HTTP call made with a credential, such as Codex Alpha
    /// Search.
    Http,
    /// A message on an upstream WebSocket. Its request is the handshake's
    /// URL and headers, with `GET` (upstream's log says `WEBSOCKET`), and
    /// the message as its body; it has no answer head, and each chunk is a
    /// message read.
    Websocket,
}

/// An upstream attempt, as it is sent.
///
/// The URL, the headers and the body may hold secrets: the credential's
/// token in a header or the URL's query, and the [`Self::secrets`] the
/// executor sent anywhere. A tap that keeps them must mask or redact them
/// (see [`super::mask`] and [`super::redact`]). `Debug` leaves them out.
pub struct AttemptRequest<'a> {
    /// What the attempt is for.
    pub kind: AttemptKind,
    /// The method; `GET` for a WebSocket message.
    pub method: &'a Method,
    /// The URL, whose query may hold a key.
    pub url: &'a str,
    /// The headers, the credential's included.
    pub headers: &'a HeaderMap,
    /// The body, or the WebSocket message.
    pub body: &'a Bytes,
    /// The executor's provider, such as `codex` or an OpenAI-compatible
    /// provider's key.
    pub provider: &'a str,
    /// The model sent upstream.
    pub model: &'a str,
    /// The format of the body, upstream's own.
    pub format: &'a Format,
    /// The credential.
    pub auth: &'a Auth,
    /// The secrets the attempt sends, gathered from its headers, its URL,
    /// its proxy and its credential (see [`Secrets`]), to scrub from
    /// anything kept.
    pub secrets: &'a Secrets,
}

impl fmt::Debug for AttemptRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttemptRequest")
            .field("kind", &self.kind)
            .field("method", &self.method)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("format", &self.format)
            .field("auth", &self.auth.id)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// A call's view for its observers: the request's context, and the taps
/// that see its upstream traffic. The server gives every call one; it may
/// have no taps.
#[derive(Clone)]
pub struct Observation {
    context: Arc<RequestContext>,
    taps: Box<[Arc<dyn Tap>]>,
}

impl Observation {
    /// An observation of a call in the request of `context`, seen by
    /// `taps`.
    pub fn new(context: Arc<RequestContext>, taps: Vec<Arc<dyn Tap>>) -> Self {
        Self {
            context,
            taps: taps.into_boxed_slice(),
        }
    }

    /// The request's context.
    pub fn context(&self) -> &Arc<RequestContext> {
        &self.context
    }

    /// Whether any tap sees the call.
    pub fn is_tapped(&self) -> bool {
        !self.taps.is_empty()
    }

    /// Tells every tap an attempt is about to be sent.
    pub fn attempt_request(&self, request: &AttemptRequest<'_>) {
        for tap in &self.taps {
            tap.attempt_request(request);
        }
    }

    /// Tells every tap the attempt's request is about to go out on a
    /// connection that is up.
    pub fn request_sent(&self) {
        for tap in &self.taps {
            tap.request_sent();
        }
    }

    /// Tells every tap the attempt's answer came.
    pub fn response_head(&self, status: u16, headers: &HeaderMap) {
        for tap in &self.taps {
            tap.response_head(status, headers);
        }
    }

    /// Gives every tap the next part of the answer's body.
    pub fn chunk(&self, chunk: &Bytes) {
        for tap in &self.taps {
            tap.chunk(chunk);
        }
    }

    /// Tells every tap the attempt failed after its answer's head came.
    pub fn attempt_error(&self, message: &str) {
        for tap in &self.taps {
            tap.attempt_error(message);
        }
    }

    /// Tells every tap the call failed.
    pub fn error(&self, error: &ExecError) {
        for tap in &self.taps {
            tap.error(error);
        }
    }

    /// Tells every tap the call ended.
    pub fn finish(&self, outcome: Outcome) {
        for tap in &self.taps {
            tap.finish(outcome);
        }
    }
}

impl fmt::Debug for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Observation")
            .field("request_id", &self.context.id)
            .field("taps", &self.taps.len())
            .finish()
    }
}
