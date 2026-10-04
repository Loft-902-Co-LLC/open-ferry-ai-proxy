// Ported from CLIProxyAPI internal/runtime/executor/openai_compat_executor.go
// (the stream reader of ExecuteStream), helps/apply_patch.go
// (EndApplyPatchStream) and helps/claude_input_tokens.go
// (TranslateStreamWithClaudeInputTokens) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An OpenAI-compatible provider's SSE stream, read a frame at a time and
//! translated to the client's format.
//!
//! A frame's `data:` lines are joined with newlines and checked before the
//! frame is translated: data that isn't JSON, `[DONE]` among other data
//! lines, `[DONE]` or no data under an error event (`error`,
//! `response.error`, `response.failed`), and an error payload each end the
//! stream with an error. `data: [DONE]` ends the stream, and whatever
//! follows it is dropped. Comments, `id:` and `retry:` lines are skipped,
//! and a line of bare JSON ends the stream with that JSON as a 502.
//!
//! A stream that closes without `[DONE]` is finished as if it had sent one,
//! except for an OpenAI Responses client, which needs a terminal event and
//! gets a 502 instead.
//!
//! Deviations from upstream:
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting and request logging are left to the call's taps,
//!   which see each chunk as it is read (see the crate's `observe_send`
//!   module).
//! - Data that Go's `json.Valid` accepts but `serde_json` can't read
//!   (invalid UTF-8, very deep nesting) is checked for an error as if it had
//!   no fields; see [`super::status`].
//! - A frame's data, joined, may hold at most 50 MiB, as one line may; a
//!   bigger frame ends the stream with a 502. Upstream holds any amount.
//! - An error that quotes a secret the request sent has it redacted if it is
//!   of eight bytes or more, as every client error is; see
//!   [`crate::redact`] and its `Policy::Client`.

use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError, Format};
use open_ferry_translate::go::{json_valid, trim_space};
use open_ferry_translate::registry::ResponseStream;

use super::status::{is_error_event, stream_data_error};
use crate::codex::claude_tokens;
use crate::codex::stream::{LineError, LineReader, MAX_LINE};
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, StatusError};
use crate::codex::usage::ensure_responses_usage_details;

/// The most data a frame may hold, joined: as much as one line may.
const MAX_FRAME: usize = MAX_LINE;

/// What the call's taps are told of an error that holds the provider's
/// payload (`publishStreamError`'s `containsPayload`).
const ERROR_PAYLOAD: &str = "upstream stream returned an error payload";

/// What a streaming call needs to translate the provider's events.
pub(crate) struct StreamSetup {
    /// Translates the provider's OpenAI chunks to the client's format.
    pub(crate) translator: ResponseStream,
    /// The format the client gets.
    pub(crate) response_format: Format,
    /// The client's format.
    pub(crate) source_format: Format,
    /// The client's request as it came, for the Claude input estimate.
    pub(crate) original: Bytes,
    /// The secrets the request sent, redacted from the errors made from the
    /// provider's events.
    pub(crate) secrets: crate::redact::Secrets,
}

/// The state of one translated stream.
struct State {
    reader: LineReader,
    setup: StreamSetup,
    claude: claude_tokens::State,
    /// The current frame's event name.
    event: String,
    /// The current frame's data lines, trimmed and joined with newlines.
    data: Vec<u8>,
    /// How many data lines the current frame has.
    data_lines: usize,
    /// Whether one of them is `[DONE]`.
    data_done: bool,
    seen_done: bool,
    failed: bool,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
}

/// Translates the provider's stream in `response` to the client's format.
pub(crate) fn translate(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let claude = claude_tokens::State::new(
        &setup.source_format,
        &Format::OPENAI,
        &setup.response_format,
    );
    let state = State {
        reader: LineReader::new(response),
        setup,
        claude,
        event: String::new(),
        data: Vec::new(),
        data_lines: 0,
        data_done: false,
        seen_done: false,
        failed: false,
        pending: VecDeque::new(),
        finished: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }
            if state.finished {
                return None;
            }
            state.step().await;
        }
    })
    .boxed()
}

impl State {
    /// Reads one line and acts on it.
    async fn step(&mut self) {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => line,
            Some(Err(error)) => return self.end(Some(error)).await,
            None => return self.end(None).await,
        };
        let trimmed = trim_space(&line);
        if trimmed.is_empty() {
            if self.process_frame().await {
                self.end(None).await;
            }
        } else if let Some(rest) = trimmed.strip_prefix(b"data:") {
            let rest = trim_space(rest);
            if self.data.len() + 1 + rest.len() > MAX_FRAME {
                self.fail(StatusError::new(
                    502,
                    "upstream SSE data frame is too large",
                ));
                return self.end(None).await;
            }
            if self.data_lines > 0 {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(rest);
            self.data_lines += 1;
            self.data_done |= rest == b"[DONE]";
        } else if let Some(rest) = trimmed.strip_prefix(b"event:") {
            self.event = String::from_utf8_lossy(trim_space(rest)).into_owned();
        } else if trimmed.starts_with(b":")
            || trimmed.starts_with(b"id:")
            || trimmed.starts_with(b"retry:")
        {
            // Comments and fields that don't concern us.
        } else if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
            self.fail_with_payload(StatusError::new(502, String::from_utf8_lossy(trimmed)));
            self.end(None).await;
        }
    }

    /// Checks and sends the frame read so far. Returns whether the stream
    /// is over: it failed or sent `[DONE]`.
    async fn process_frame(&mut self) -> bool {
        let event = std::mem::take(&mut self.event);
        let joined = std::mem::take(&mut self.data);
        let lines = std::mem::take(&mut self.data_lines);
        let has_done = std::mem::take(&mut self.data_done);
        if lines == 0 {
            if is_error_event(&event) {
                self.fail(StatusError::new(
                    502,
                    "upstream error event ended without data",
                ));
                return true;
            }
            return false;
        }
        if lines > 1 && has_done {
            self.fail(StatusError::new(
                502,
                "upstream stream ended with incomplete data before [DONE]",
            ));
            return true;
        }
        let payload = trim_space(&joined);
        let done = payload == b"[DONE]";
        if done && is_error_event(&event) {
            self.fail(StatusError::new(
                502,
                "upstream error event ended before [DONE]",
            ));
            return true;
        }
        if !done {
            if !json_valid(payload) {
                self.fail(StatusError::new(
                    502,
                    "upstream stream ended with incomplete SSE data frame",
                ));
                return true;
            }
            if let Some(error) = stream_data_error(payload, &event) {
                self.fail_with_payload(error);
                return true;
            }
        }

        let line = [b"data: ".as_slice(), payload].concat();
        self.send(&line).await;
        if self.setup.translator.tool_input_error().is_some() {
            self.fail(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE));
            return true;
        }
        if done {
            self.seen_done = true;
            return true;
        }
        false
    }

    /// Translates one line and queues what comes out
    /// (`TranslateStreamWithClaudeInputTokens`).
    async fn send(&mut self, line: &[u8]) {
        let mut chunks = self.setup.translator.translate(line);
        if self.setup.translator.tool_input_error().is_none() {
            if self.setup.response_format == Format::OPENAI_RESPONSE {
                chunks = chunks
                    .into_iter()
                    .map(ensure_responses_usage_details)
                    .collect();
            }
            if let Some(patch) = self.claude.find(&chunks) {
                let original = self.setup.original.clone();
                let estimate =
                    tokio::task::spawn_blocking(move || claude_tokens::estimate(&original))
                        .await
                        .unwrap_or_else(|_| Err("the estimate stopped".to_owned()));
                patch.apply(&mut chunks, estimate);
            }
        }
        self.queue(chunks);
    }

    fn queue(&mut self, chunks: Vec<Vec<u8>>) {
        for chunk in chunks {
            if !chunk.is_empty() {
                self.pending.push_back(Ok(Bytes::from(chunk)));
            }
        }
    }

    /// Queues an error, after which nothing more is sent, and tells the
    /// call's taps of it (`publishStreamError`).
    fn fail(&mut self, error: StatusError) {
        let error: ExecError = error.redacted(&self.setup.secrets).into();
        self.reader.report(&error);
        self.stop(error);
    }

    /// [`Self::fail`] for an error that holds the provider's payload, which
    /// the taps are told only as [`ERROR_PAYLOAD`], as upstream records it.
    fn fail_with_payload(&mut self, error: StatusError) {
        self.reader.report(&ERROR_PAYLOAD);
        self.stop(error.redacted(&self.setup.secrets).into());
    }

    /// Queues `error`, after which nothing more is sent, without telling
    /// the taps.
    fn stop(&mut self, error: ExecError) {
        self.failed = true;
        self.pending.push_back(Err(error));
    }

    /// Ends the stream, after a read `error` or once it is over: sends a
    /// frame left without its blank line and what the translator still
    /// holds, then the read error, or for a stream without `[DONE]` either a
    /// final `[DONE]` or, for an OpenAI Responses client, an error.
    async fn end(&mut self, error: Option<LineError>) {
        self.finished = true;
        if error.is_none() && !self.seen_done && !self.failed && self.data_lines > 0 {
            self.process_frame().await;
        }
        if self.failed {
            return;
        }
        let chunks = self.setup.translator.finish();
        let tool_input_failed = self.setup.translator.tool_input_error().is_some();
        self.queue(chunks);
        if tool_input_failed {
            // Upstream's `EndApplyPatchStream` doesn't record it.
            self.stop(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE).into());
            return;
        }
        if let Some(error) = error {
            tracing::debug!("openai compat executor: stream read failed: {error}");
            self.reader.report(&error);
            self.pending
                .push_back(Err(ExecError::new(ErrorKind::Upstream, error.to_string())));
        } else if !self.seen_done {
            if self.setup.response_format == Format::OPENAI_RESPONSE {
                self.fail(StatusError::new(
                    502,
                    "upstream stream closed before [DONE]",
                ));
                return;
            }
            // Other clients keep working with providers that send no [DONE].
            self.send(b"data: [DONE]").await;
        }
    }
}
