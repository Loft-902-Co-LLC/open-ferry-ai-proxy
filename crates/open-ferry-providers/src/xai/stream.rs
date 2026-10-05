// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_stream.go
// (ExecuteStream's reading goroutine) and
// helps/claude_input_tokens.go (TranslateStreamWithClaudeInputTokens)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI's SSE stream, read a line at a time and translated to the client's
//! format.
//!
//! An `event:` line is held until its `data:` line comes. Each `data:`
//! event is undone first: reasoning text becomes a reasoning summary (a
//! `response.reasoning_text.done` becoming two events), namespace tools get
//! their namespaces back, a client's `web_search` function its name, and X
//! search's own tool calls are dropped, with their `event:` line. The held
//! line is renamed as its event was, and an event split in two gets a line
//! of its own.
//!
//! Every line then passes through the `apply_patch` bridge (see
//! [`crate::apply_patch_responses`]). `response.output_item.done` items are
//! kept, and a terminal success (`response.completed`,
//! `response.incomplete`) gets them as its `output` when it came without.
//! At the end of the stream, the bridge fails a response that never ended.
//! A bridge or translation failure ends the stream with a 502.
//!
//! Deviations from upstream:
//! - A rewritten event is written by `serde_json`.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting and request logging are left to the call's taps,
//!   which see each chunk as it is read (see the crate's `observe_send`
//!   module).
//! - Each line has the secrets the request sent redacted before it is
//!   read, if they are of eight bytes or more, as every client error is (see
//!   [`crate::redact`] and its `Policy::Client`), and so has the text of a
//!   read error. So an error that quotes one, a `response.failed` event
//!   among them, and what a model echoes back in its output, which upstream
//!   passes on as it is, reach the client without it. The call's taps see
//!   each line as xAI sent it.

use std::borrow::Cow;
use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError, Format};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;
use serde_json::Value;

use super::reasoning::{
    normalize_summary_data, normalize_summary_data_events, normalize_summary_event_line,
};
use super::replay;
use super::request::Prepared;
use super::response::{
    NamespaceRestorer, XSearchFilter, patch_completed_output, restore_client_web_search_name,
};
use crate::codex::claude_tokens;
use crate::codex::stream::LineReader;
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError};
use crate::json::str_at;
use crate::redact::{Policy, Secrets};

/// The tag of an SSE data line (`xaiDataTag`).
const DATA_TAG: &[u8] = b"data:";

/// The tag of an SSE event line (`xaiEventTag`).
const EVENT_TAG: &[u8] = b"event:";

/// What a streaming call needs to translate xAI's events.
pub(crate) struct StreamSetup {
    /// Translates Codex-format lines to the client's format.
    pub(crate) translator: ResponseStream,
    /// The client's format.
    pub(crate) source_format: Format,
    /// The prepared request.
    pub(crate) prepared: Prepared,
    /// The secrets the request sent, redacted from each line xAI sends.
    pub(crate) secrets: Secrets,
}

/// The state of one translated stream.
struct State {
    reader: LineReader,
    translator: ResponseStream,
    prepared: Prepared,
    claude: claude_tokens::State,
    items: OutputItems,
    filter: XSearchFilter,
    restorer: NamespaceRestorer,
    /// The secrets the request sent.
    secrets: Secrets,
    /// The `event:` line waiting for its data.
    pending_event_line: Option<Vec<u8>>,
    pending: VecDeque<Bytes>,
    /// An error to end the stream with once `pending` is sent.
    failure: Option<ExecError>,
    finished: bool,
}

/// Translates xAI's stream in `response` to the client's format.
pub(crate) fn translate(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let StreamSetup {
        translator,
        source_format,
        mut prepared,
        secrets,
    } = setup;
    let claude = claude_tokens::State::new(&source_format, &prepared.to, &prepared.response_format);
    let filter = XSearchFilter::new(
        prepared.filter_internal_x_search,
        std::mem::take(&mut prepared.client_declared_tools),
    );
    let restorer = NamespaceRestorer::new(std::mem::take(&mut prepared.namespace_tools));
    let state = State {
        reader: LineReader::new(response),
        translator,
        prepared,
        claude,
        items: OutputItems::default(),
        filter,
        restorer,
        secrets,
        pending_event_line: None,
        pending: VecDeque::new(),
        failure: None,
        finished: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
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
                state.finished = true;
                state.failure = Some(error);
            }
        }
    })
    .boxed()
}

/// The error a bridge or translation failure ends the stream with.
fn apply_patch_failure() -> ExecError {
    StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE).into()
}

impl State {
    /// Reads and translates one line. An error ends the stream.
    async fn step(&mut self) -> Result<(), ExecError> {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                tracing::debug!("xai: stream read failed: {error}");
                self.reader.report(&error);
                self.finish().await?;
                return Err(ExecError::new(
                    ErrorKind::Upstream,
                    self.secrets.text(error.to_string(), Policy::Client),
                ));
            }
            None => return self.finish().await,
        };
        // Each line, as it is read; the taps read it as it came.
        let line = match self.secrets.bytes(&line, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => line,
        };
        if line.starts_with(EVENT_TAG) {
            if let Some(pending) = self.pending_event_line.replace(line) {
                self.emit(normalize_summary_event_line(&pending, ""))
                    .await?;
            }
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix(DATA_TAG) {
            let events = normalize_summary_data_events(trim_space(rest).to_vec());
            let had_event_line = self.pending_event_line.is_some();
            for (index, data) in events.into_iter().enumerate() {
                let Some(data) = self.undo(data) else {
                    if had_event_line && index == 0 {
                        self.pending_event_line = None;
                    }
                    continue;
                };
                let event_type = str_at(
                    &serde_json::from_slice(&data).unwrap_or(Value::Null),
                    "type",
                );
                if had_event_line {
                    let event_line = match self.pending_event_line.take() {
                        Some(pending) if index == 0 => {
                            normalize_summary_event_line(&pending, &event_type)
                        }
                        _ => format!("event: {event_type}").into_bytes(),
                    };
                    self.emit(event_line).await?;
                }
                self.emit([b"data: ".as_slice(), &data].concat()).await?;
            }
            return Ok(());
        }
        if let Some(pending) = self.pending_event_line.take() {
            self.emit(normalize_summary_event_line(&pending, ""))
                .await?;
        }
        self.emit(line).await
    }

    /// Undoes xAI's reshaping of one event; `None` if it is dropped.
    fn undo(&mut self, data: Vec<u8>) -> Option<Vec<u8>> {
        self.prepared.apply_patch.remember_dispatcher_event(&data);
        let mut data = self.restorer.restore(data);
        if !self.prepared.web_search_alias.is_empty() {
            data = restore_client_web_search_name(data, &self.prepared.web_search_alias);
        }
        self.filter.apply(data).filter(|data| !data.is_empty())
    }

    /// Passes one line through the `apply_patch` bridge, completes a
    /// terminal success, and queues its translation (upstream's
    /// `emitTranslatedLine`). A bridge or translation failure ends the
    /// stream after what was translated.
    async fn emit(&mut self, line: Vec<u8>) -> Result<(), ExecError> {
        let (lines, bridge_error) = self.prepared.apply_patch.stream(&line);
        let mut chunks = Vec::new();
        for mut line in lines {
            if let Some(rest) = line.strip_prefix(DATA_TAG) {
                let data = trim_space(rest).to_vec();
                let parsed: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
                match str_at(&parsed, "type").as_str() {
                    "response.output_item.done" => self.items.collect(&parsed),
                    "response.completed" | "response.incomplete" => {
                        // Only after the bridge has restored dispatcher
                        // children.
                        let data = patch_completed_output(data, &self.items);
                        let data = normalize_summary_data(data);
                        if str_at(
                            &serde_json::from_slice(&data).unwrap_or(Value::Null),
                            "type",
                        ) == "response.completed"
                        {
                            replay::cache_completed(&self.prepared.replay, &data);
                        }
                        let ending_at = line
                            .iter()
                            .rposition(|&b| b != b'\r' && b != b'\n')
                            .map_or(0, |at| at + 1);
                        let ending = line.get(ending_at..).unwrap_or_default().to_vec();
                        line = [b"data: ".as_slice(), &data, &ending].concat();
                    }
                    _ => {}
                }
            }
            chunks.extend(self.translate(&line).await);
        }
        self.send(chunks);
        if self.translator.tool_input_error().is_some() || bridge_error.is_some() {
            return Err(apply_patch_failure());
        }
        Ok(())
    }

    /// Translates one line, with a Claude client's input tokens estimated
    /// (`TranslateStreamWithClaudeInputTokens`).
    async fn translate(&mut self, line: &[u8]) -> Vec<Vec<u8>> {
        let mut chunks = self.translator.translate(line);
        if self.translator.tool_input_error().is_none()
            && let Some(patch) = self.claude.find(&chunks)
        {
            let original = self.prepared.original_payload.clone();
            let estimate = tokio::task::spawn_blocking(move || claude_tokens::estimate(&original))
                .await
                .unwrap_or_else(|_| Err("the estimate stopped".to_owned()));
            patch.apply(&mut chunks, estimate);
        }
        chunks
    }

    /// Queues the non-empty chunks for the client.
    fn send(&mut self, chunks: Vec<Vec<u8>>) {
        self.pending.extend(
            chunks
                .into_iter()
                .filter(|chunk| !chunk.is_empty())
                .map(Bytes::from),
        );
    }

    /// Ends the stream: sends the held `event:` line, then whatever the
    /// bridge has to say about a response that never ended.
    async fn finish(&mut self) -> Result<(), ExecError> {
        self.finished = true;
        if let Some(pending) = self.pending_event_line.take() {
            self.emit(normalize_summary_event_line(&pending, ""))
                .await?;
        }
        let (events, error) = self.prepared.apply_patch.finish_stream();
        let mut chunks = Vec::new();
        for event in events {
            chunks.extend(self.translate(&event).await);
        }
        self.send(chunks);
        match error {
            Some(_) => Err(apply_patch_failure()),
            None => Ok(()),
        }
    }
}
