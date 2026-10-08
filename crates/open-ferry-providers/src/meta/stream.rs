// Ported from CLIProxyAPI internal/runtime/executor/meta_executor_stream.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Translates Meta's event stream to the client's format, a line at a time.
//!
//! A line that isn't a `data:` line goes on as it is. A `data:` line is read
//! first: an `error` or `response.failed` event ends the stream with an
//! error, `output_item.done` events are kept, and a completed event has its
//! output filled in from them. Each line then goes through the `apply_patch`
//! bridge, which may change it, hold it, or fail the stream, and through the
//! response translator.
//!
//! The terminal events don't end the reading: the stream goes on until
//! Meta's ends. A stream that ends with no terminal event isn't an error
//! (unless the bridge needs one): the client sees what it was sent.
//!
//! Deviations from upstream:
//! - A line that isn't JSON is read as having no fields, where gjson reads
//!   what it can from it.
//! - Usage, the served model and the response log are the call's taps' work
//!   (see the crate's `observe_send` module), not the reporter's.
//! - A dropped stream stops at once; upstream checks its context.
//! - Each line has the secrets the request sent redacted before it is read,
//!   if they are of eight bytes or more, as every client error is (see
//!   `Policy::Client` in the crate's `redact` module); a model can echo one
//!   back in its output, which upstream passes on as it is. The call's taps
//!   see each line as Meta sent it.

use std::borrow::Cow;
use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError, Format};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;
use serde_json::Value;

use super::error::stream_event_error;
use crate::apply_patch_responses;
use crate::codex::claude_tokens;
use crate::codex::stream::{LineError, LineReader};
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::str_at;
use crate::redact::{Policy, Secrets};

/// What a stream is translated with.
pub(super) struct Setup {
    /// Translates Meta's events to the client's format.
    pub(super) translator: ResponseStream,
    /// The `apply_patch` bridge for the response.
    pub(super) apply_patch: apply_patch_responses::State,
    /// The format of the client's request.
    pub(super) source_format: Format,
    /// The format the client gets.
    pub(super) response_format: Format,
    /// The client's request as it came, for the Claude input estimate.
    pub(super) original: Bytes,
    /// The secrets the request sent, redacted from the errors.
    pub(super) secrets: Secrets,
}

struct State {
    reader: LineReader,
    translator: ResponseStream,
    apply_patch: apply_patch_responses::State,
    claude: claude_tokens::State,
    items: OutputItems,
    response_format: Format,
    original: Bytes,
    secrets: Secrets,
    pending: VecDeque<Bytes>,
    /// An error to end the stream with once `pending` is sent.
    failure: Option<ExecError>,
    finished: bool,
}

/// Translates Meta's stream in `response` to the client's format.
pub(super) fn translate(response: reqwest::Response, setup: Setup) -> ChunkStream {
    State::new(response, setup).into_stream()
}

/// The error for a stream whose `apply_patch` call couldn't be carried over.
/// It keeps the usage the stream read (v8.0.20's
/// `StopApplyPatchStreamWithUsage`).
fn apply_patch_failure() -> ExecError {
    ExecError::from(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE)).with_usage_kept()
}

impl State {
    fn new(response: reqwest::Response, setup: Setup) -> Self {
        let claude =
            claude_tokens::State::new(&setup.source_format, &Format::CODEX, &setup.response_format);
        Self {
            reader: LineReader::new(response),
            translator: setup.translator,
            apply_patch: setup.apply_patch,
            claude,
            items: OutputItems::default(),
            response_format: setup.response_format,
            original: setup.original,
            secrets: setup.secrets,
            pending: VecDeque::new(),
            failure: None,
            finished: false,
        }
    }

    fn into_stream(self) -> ChunkStream {
        futures_util::stream::unfold(self, |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((Ok(chunk), state));
                }
                if let Some(error) = state.failure.take() {
                    state.finished = true;
                    return Some((Err(error), state));
                }
                if state.finished {
                    return None;
                }
                if let Err(error) = state.step().await {
                    state.failure = Some(error);
                }
            }
        })
        .boxed()
    }

    /// Reads and translates one line. An error ends the stream, after the
    /// chunks already queued.
    async fn step(&mut self) -> Result<(), ExecError> {
        match self.reader.next_line().await {
            Some(Ok(line)) => {
                let line = self.redact(line);
                self.line(line).await
            }
            Some(Err(error)) => self.end(Some(error)).await,
            None => self.end(None).await,
        }
    }

    /// `line` without the secrets the request was sent with, as the client
    /// gets it: a model can echo one back in its output.
    fn redact(&self, line: Vec<u8>) -> Vec<u8> {
        match self.secrets.bytes(&line, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => line,
        }
    }

    /// Handles one line of Meta's stream.
    async fn line(&mut self, line: Vec<u8>) -> Result<(), ExecError> {
        let Some(rest) = line.strip_prefix(b"data:") else {
            return self.emit(&line).await;
        };
        let mut data = trim_space(rest).to_vec();
        let mut event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
        if let Some(error) = stream_event_error(&event, &data) {
            let error: ExecError = error.redacted(&self.secrets).into();
            tracing::debug!(status = error.status, "meta: stream error event");
            self.reader.report(&error);
            return Err(error);
        }
        match str_at(&event, "type").as_str() {
            "response.output_item.done" => self.items.collect(&event),
            // The kept items fill in the completed event's output.
            "response.completed" | "response.incomplete" if self.items.patch(&mut event) => {
                data = event.to_string().into_bytes();
            }
            _ => {}
        }
        self.emit(&[b"data: ".as_slice(), &data].concat()).await
    }

    /// Passes `line` through the `apply_patch` bridge and the translator and
    /// queues what comes out. Ends the stream if an `apply_patch` call
    /// couldn't be carried over.
    async fn emit(&mut self, line: &[u8]) -> Result<(), ExecError> {
        let (lines, bridge_error) = self.apply_patch.stream(line);
        let mut chunks = Vec::new();
        for line in lines {
            chunks.extend(self.translate_line(&line).await);
        }
        self.send(chunks);
        if self.translator.tool_input_error().is_some() || bridge_error.is_some() {
            let error = apply_patch_failure();
            tracing::debug!("meta: stream's apply_patch call failed");
            self.reader.report(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Translates one line of Meta's stream.
    async fn translate_line(&mut self, line: &[u8]) -> Vec<Vec<u8>> {
        let mut chunks = self.translator.translate(line);
        if self.translator.tool_input_error().is_none() {
            if self.response_format == Format::OPENAI_RESPONSE {
                chunks = chunks
                    .into_iter()
                    .map(ensure_responses_usage_details)
                    .collect();
            }
            if let Some(patch) = self.claude.find(&chunks) {
                let original = self.original.clone();
                let estimate =
                    tokio::task::spawn_blocking(move || claude_tokens::estimate(&original))
                        .await
                        .unwrap_or_else(|_| Err("the estimate stopped".to_owned()));
                patch.apply(&mut chunks, estimate);
            }
        }
        chunks
    }

    /// Queues the non-empty chunks for the client.
    fn send(&mut self, chunks: Vec<Vec<u8>>) {
        for chunk in chunks {
            if !chunk.is_empty() {
                self.pending.push_back(Bytes::from(chunk));
            }
        }
    }

    /// Meta's stream ended, or its reading failed with `read_error`. The
    /// bridge fails the stream if its response never ended; else a read error
    /// is the stream's last chunk.
    async fn end(&mut self, read_error: Option<LineError>) -> Result<(), ExecError> {
        self.finished = true;
        let (frames, bridge_error) = self.apply_patch.finish_stream();
        let mut chunks = Vec::new();
        for frame in frames {
            chunks.extend(self.translate_line(&frame).await);
        }
        self.send(chunks);
        if bridge_error.is_some() {
            let error = apply_patch_failure();
            self.reader.report(&error);
            return Err(error);
        }
        if let Some(error) = read_error {
            tracing::debug!("meta: stream read failed: {error}");
            self.reader.report(&error);
            let text = self.secrets.text(error.to_string(), Policy::Client);
            return Err(ExecError::new(ErrorKind::Upstream, text));
        }
        Ok(())
    }
}
