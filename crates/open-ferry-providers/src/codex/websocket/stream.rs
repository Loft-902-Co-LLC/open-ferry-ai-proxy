// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_stream.go
// (ExecuteStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The streaming call over the WebSocket.
//!
//! Each of Codex's events reaches the client as a chunk, as Codex sent it,
//! with the usage details an OpenAI Responses client expects. The
//! completed response (`response.completed`, `response.done` or
//! `response.incomplete`) goes as `response.completed`, its output filled
//! in from the items streamed before unless a native client sent the
//! request. The stream ends after it; an error event, a terminal failure
//! or a broken connection ends it with an error.
//!
//! With `codex.stream-bootstrap-buffering` on, the events before generation
//! starts are held back, as for HTTP: within [`MAX_BOOTSTRAP_FRAMES`]
//! messages read, [`MAX_BOOTSTRAP_BYTES`] bytes and the time limit, so that
//! an overload fails the call over to another credential. The time limit
//! is checked as each message is read, as upstream does.
//!
//! The call ends (the next call of the session may go, and a call's own
//! connection is closed) before its last chunk is handed on.
//!
//! Deviations from upstream:
//! - Chunks are Codex's events, for a client on the Responses WebSocket;
//!   upstream's SSE translation for other clients isn't ported, as only
//!   those clients take this route.
//! - The response steering duplex isn't ported.

use std::collections::VecDeque;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ChunkStream, ExecError, Options, Request, StreamResponse};
use serde_json::Value;

use super::errors;
use super::execute::{Call, Opened, open};
use super::request::prepare;
use super::session::{Hold, is_terminal_event};
use crate::codex::executor::CodexExecutor;
use crate::codex::ext::{self, Turn};
use crate::codex::request::Kind;
use crate::codex::stream::Bootstrap;
use crate::codex::terminal::{
    MAX_BOOTSTRAP_BYTES, MAX_BOOTSTRAP_FRAMES, OutputItems, bootstrap_overload_error,
    empty_incomplete_stream_error, has_meaningful_output_delta, is_bootstrap_bufferable_event,
    is_overload_bootstrap_failure, is_terminal_empty_incomplete, normalize_completion,
    terminal_failure,
};
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::str_at;

/// A streaming call over the WebSocket (`ExecuteStream`).
pub(in crate::codex) async fn execute_stream(
    executor: &CodexExecutor,
    auth: &Auth,
    request: Request,
    options: Options,
) -> Result<StreamResponse, ExecError> {
    let mut prepared = prepare(
        Kind::Stream,
        executor.context(auth),
        auth,
        executor.base_url(),
        &request,
        &options,
    )?;
    let Call {
        hold,
        headers,
        secrets,
    } = match open(executor, auth, &mut prepared, &options).await? {
        Opened::Ws(call) => call,
        Opened::Fallback => return executor.execute_stream_inner(auth, request, options).await,
    };
    let mut state = State {
        hold,
        turn: prepared.turn,
        native: prepared.native,
        secrets,
        model_level_cooling: executor.model_level_cooling(),
        items: OutputItems::default(),
        saw_output_delta: false,
        pending: VecDeque::new(),
        failure: None,
        finished: false,
    };
    if let Some(bootstrap) = executor.bootstrap() {
        state.bootstrap(&bootstrap).await?;
    }
    Ok(StreamResponse {
        headers: headers.unwrap_or_default(),
        chunks: state.into_stream(),
    })
}

/// An event of Codex's, ready for the client.
struct Event {
    /// The chunk for the client.
    chunk: Vec<u8>,
    /// The event's text, with the collaboration namespace named back.
    data: Vec<u8>,
    /// Its `type`.
    event_type: String,
    /// Its JSON.
    event: Value,
}

/// An event or failure that ends the call with an error.
enum Fault {
    /// The connection failed.
    Read(ExecError),
    /// An error event.
    Upstream(ExecError),
    /// A terminal failure event, with its error body.
    Terminal(ExecError, String),
    /// A `response.incomplete` with nothing in it.
    EmptyIncomplete(ExecError),
}

impl Fault {
    fn into_error(self) -> ExecError {
        match self {
            Self::Read(error)
            | Self::Upstream(error)
            | Self::Terminal(error, _)
            | Self::EmptyIncomplete(error) => error,
        }
    }
}

/// The state of one streaming call.
struct State {
    hold: Hold,
    turn: Turn,
    /// Whether a native client sent the request, so the completed response
    /// is kept as Codex sent it.
    native: bool,
    /// The secrets the call sent, redacted from its errors.
    secrets: crate::redact::Secrets,
    model_level_cooling: bool,
    items: OutputItems,
    saw_output_delta: bool,
    pending: VecDeque<Bytes>,
    /// An error to end the stream with once `pending` is sent.
    failure: Option<ExecError>,
    finished: bool,
}

impl State {
    /// Reads and checks Codex's next message; `None` for an empty one.
    async fn next(&mut self) -> Result<Option<Event>, Fault> {
        let payload = self
            .hold
            .recv()
            .await
            .map_err(|failure| Fault::Read(errors::error(&failure)))?;
        if payload.is_empty() {
            return Ok(None);
        }
        let data = ext::restore(&self.turn, payload.as_bytes()).into_owned();
        let event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
        if let Some((error, status, raw)) = errors::parse_ws_error(
            &event,
            self.model_level_cooling,
            &self.secrets,
            SystemTime::now(),
        ) {
            self.hold.invalidate("upstream_error");
            ext::on_failure(&self.turn, status, raw.as_bytes());
            return Err(Fault::Upstream(error));
        }
        if let Some((error, body)) = terminal_failure(&event, self.model_level_cooling) {
            self.hold.unlock();
            self.hold.invalidate("terminal_failure");
            ext::on_failure(&self.turn, error.status, body.as_bytes());
            return Err(Fault::Terminal(error.redacted(&self.secrets).into(), body));
        }

        let event_type = str_at(&event, "type");
        if has_meaningful_output_delta(&event) {
            self.saw_output_delta = true;
        }
        if is_terminal_empty_incomplete(&event, self.items.len(), self.saw_output_delta) {
            self.hold.invalidate("terminal_empty_incomplete");
            self.hold.unlock();
            let error: ExecError = empty_incomplete_stream_error().into();
            self.hold.report(&error);
            return Err(Fault::EmptyIncomplete(error));
        }
        if event_type == "response.output_item.done" {
            self.items.collect(&event);
        }
        let chunk = if matches!(
            event_type.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            let mut completed = event.clone();
            let normalized = normalize_completion(&mut completed);
            let patched = !self.native && self.items.patch(&mut completed);
            if event_type != "response.incomplete" {
                ext::on_completed(&self.turn, &completed);
            }
            if normalized || patched {
                completed.to_string().into_bytes()
            } else {
                data.clone()
            }
        } else {
            data.clone()
        };
        Ok(Some(Event {
            chunk: ensure_responses_usage_details(chunk),
            data,
            event_type,
            event,
        }))
    }

    /// Hands one event on, or ends the stream.
    async fn step(&mut self) {
        match self.next().await {
            Ok(None) => {}
            Ok(Some(event)) => {
                if is_terminal_event(&event.event_type) {
                    self.end();
                }
                self.pending.push_back(Bytes::from(event.chunk));
            }
            Err(fault) => {
                self.end();
                self.failure = Some(fault.into_error());
            }
        }
    }

    /// Ends the call: the stream ends once what is pending is sent.
    fn end(&mut self) {
        self.hold.release();
        self.finished = true;
    }

    /// Queues `chunks` for the client.
    fn send(&mut self, chunks: Vec<Vec<u8>>) {
        self.pending.extend(chunks.into_iter().map(Bytes::from));
    }

    /// Holds back the events before generation starts. Returns the call's
    /// error, or leaves the stream to go on from where it started.
    ///
    /// Each message read counts towards the limit, empty or not. Before
    /// the stream starts:
    /// - a broken connection is the call's error;
    /// - an error event within the time limit is the call's error, and
    ///   after it ends the stream after the held events;
    /// - an overload ([`is_overload_bootstrap_failure`]) within the time
    ///   limit is the call's error, a 503 or 429; another terminal failure,
    ///   or an overload after the time limit, ends the stream after the held
    ///   events, as does an empty `response.incomplete`.
    async fn bootstrap(&mut self, bootstrap: &Bootstrap) -> Result<(), ExecError> {
        let start = (bootstrap.now)();
        let mut held = Vec::new();
        let mut frames = 0;
        let mut bytes = 0;
        let mut logged = false;
        loop {
            let next = self.next().await;
            frames += 1;
            let elapsed = (bootstrap.now)().saturating_duration_since(start);
            let timed_out = !bootstrap.timeout.is_zero() && elapsed >= bootstrap.timeout;
            let window_open = frames <= MAX_BOOTSTRAP_FRAMES && !timed_out;
            if !window_open && !logged {
                logged = true;
                let exhausted = if timed_out {
                    "time budget"
                } else {
                    "frame budget"
                };
                tracing::debug!(
                    "codex websockets executor: bootstrap {exhausted} exhausted after {frames} messages read / {elapsed:?}; this message will be released"
                );
            }
            let event = match next {
                Ok(Some(event)) => event,
                Ok(None) if window_open => continue,
                Ok(None) => break,
                Err(Fault::Read(error)) => {
                    self.end();
                    return Err(error);
                }
                Err(Fault::Upstream(error)) => {
                    self.end();
                    if !timed_out {
                        return Err(error);
                    }
                    tracing::debug!(
                        "codex websockets executor: bootstrap error after {frames} messages read / {elapsed:?}, time budget exhausted; delivering in-stream"
                    );
                    self.send(held);
                    self.failure = Some(error);
                    return Ok(());
                }
                Err(Fault::Terminal(error, body)) => {
                    self.end();
                    if is_overload_bootstrap_failure(body.as_bytes()) {
                        if !timed_out {
                            tracing::debug!(
                                "codex websockets executor: bootstrap overload rejection after {frames} messages read, failing over"
                            );
                            let error = bootstrap_overload_error(body.as_bytes());
                            return Err(error.redacted(&self.secrets).into());
                        }
                        tracing::debug!(
                            "codex websockets executor: bootstrap overload rejection after {frames} messages read / {elapsed:?}, time budget exhausted; delivering in-stream"
                        );
                    }
                    self.send(held);
                    self.failure = Some(error);
                    return Ok(());
                }
                Err(Fault::EmptyIncomplete(error)) => {
                    self.end();
                    self.send(held);
                    self.failure = Some(error);
                    return Ok(());
                }
            };
            let terminal = is_terminal_event(&event.event_type);
            if window_open
                && is_bootstrap_bufferable_event(&event.event_type, &event.data, &event.event)
                && !terminal
            {
                let frame_bytes = event.data.len() + event.chunk.len();
                if bytes + frame_bytes <= MAX_BOOTSTRAP_BYTES {
                    bytes += frame_bytes;
                    held.push(event.chunk);
                    continue;
                }
                tracing::debug!(
                    "codex websockets executor: bootstrap byte limit reached after {frames} messages / {bytes} bytes, releasing stream without overload probing"
                );
            }
            self.send(held);
            if terminal {
                self.end();
            }
            self.pending.push_back(Bytes::from(event.chunk));
            return Ok(());
        }
        self.send(held);
        Ok(())
    }

    fn into_stream(self) -> ChunkStream {
        futures_util::stream::unfold(self, |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((Ok(chunk), state));
                }
                if let Some(error) = state.failure.take() {
                    return Some((Err(error), state));
                }
                if state.finished {
                    return None;
                }
                state.step().await;
            }
        })
        .boxed()
    }
}
