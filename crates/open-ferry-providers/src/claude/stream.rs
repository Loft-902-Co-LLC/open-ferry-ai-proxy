// Ported from CLIProxyAPI internal/runtime/executor/claude_executor_stream.go
// (the stream loops and validateClaudeStreamingResponse),
// claude_executor_diagnostics.go (observeClaudeStreamLine) and
// helps/apply_patch.go (the stream failure helpers) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude's SSE stream, read a line at a time.
//!
//! A Claude client gets whole events, each with its blank line, as Claude
//! sent them. Any other client gets each line translated to its format. A
//! `message_stop` event ends the stream; a connection that fails before it
//! ends the stream with an error, while a clean end without it just ends.
//!
//! A non-streaming call that Claude answered with a stream is checked first
//! ([`validate`]): it must hold a `message_start` with an ID and model and a
//! `message_delta`, and no `error` event.
//!
//! Deviations from upstream:
//! - An event that grows past 50 MiB goes out in pieces rather than whole;
//!   upstream buffers it however large it gets.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting, request logging, response model restoring, OAuth tool
//!   name restoring, continuity tracking and the thinking replay cache
//!   aren't ported.

use std::collections::VecDeque;
use std::fmt;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ExecError};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;
use serde_json::Value;

use super::client::error_chain;
use super::ratelimit::{plain_error, wrap_fast};
use super::usage::ensure_responses_usage_details;
use crate::json::str_at;

/// The longest line read, as upstream's scanner allows.
pub(crate) const MAX_LINE: usize = 52_428_800;

/// The error for a tool call whose input couldn't be translated
/// (`ApplyPatchUpstreamErrorMessage`).
pub(crate) const APPLY_PATCH_ERROR_MESSAGE: &str =
    "Invalid apply_patch tool arguments received from upstream.";

/// The 502 for a tool call whose input couldn't be translated.
pub(crate) fn apply_patch_error() -> ExecError {
    ExecError::upstream(502, APPLY_PATCH_ERROR_MESSAGE)
}

/// Why a line couldn't be read.
#[derive(Debug)]
pub(crate) enum LineError {
    /// The connection failed.
    Read(reqwest::Error),
    /// A line was longer than [`MAX_LINE`].
    TooLong,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => f.write_str(&error_chain(error)),
            Self::TooLong => f.write_str("bufio.Scanner: token too long"),
        }
    }
}

#[derive(Debug)]
enum ReaderState {
    Reading,
    Failed(reqwest::Error),
    Done,
}

/// Reads a body a line at a time, as Go's `bufio.Scanner` with `ScanLines`
/// does: a line loses its `\n` and one trailing `\r`, and the last line
/// needs no `\n`. When the connection fails, what was read of the last line
/// comes first, then the error.
pub(crate) struct LineReader {
    response: reqwest::Response,
    buffer: Vec<u8>,
    /// Where the unread data starts.
    start: usize,
    /// How much of the unread data is known to hold no `\n`.
    scanned: usize,
    state: ReaderState,
}

impl LineReader {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            start: 0,
            scanned: 0,
            state: ReaderState::Reading,
        }
    }

    /// The next line, or `None` at the end.
    pub(crate) async fn next_line(&mut self) -> Option<Result<Vec<u8>, LineError>> {
        loop {
            let unread = self.buffer.get(self.start..).unwrap_or_default();
            if let Some(offset) = unread
                .get(self.scanned..)
                .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
            {
                let end = self.scanned + offset;
                let line = drop_cr(unread.get(..end).unwrap_or_default()).to_vec();
                self.start += end + 1;
                self.scanned = 0;
                return Some(Ok(line));
            }
            self.scanned = unread.len();
            match std::mem::replace(&mut self.state, ReaderState::Done) {
                ReaderState::Reading => self.state = ReaderState::Reading,
                ReaderState::Failed(error) => {
                    if let Some(line) = self.take_rest() {
                        self.state = ReaderState::Failed(error);
                        return Some(Ok(line));
                    }
                    return Some(Err(LineError::Read(error)));
                }
                ReaderState::Done => return self.take_rest().map(Ok),
            }
            if self.scanned > MAX_LINE {
                self.state = ReaderState::Done;
                self.buffer = Vec::new();
                self.start = 0;
                self.scanned = 0;
                return Some(Err(LineError::TooLong));
            }
            if self.start > 0 {
                self.buffer.drain(..self.start);
                self.start = 0;
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.buffer.extend_from_slice(&chunk),
                Ok(None) => self.state = ReaderState::Done,
                Err(error) => self.state = ReaderState::Failed(error.without_url()),
            }
        }
    }

    /// The unread rest as a last line, if there is any.
    fn take_rest(&mut self) -> Option<Vec<u8>> {
        let rest = self.buffer.get(self.start..).unwrap_or_default();
        let line = (!rest.is_empty()).then(|| drop_cr(rest).to_vec());
        self.buffer = Vec::new();
        self.start = 0;
        self.scanned = 0;
        line
    }
}

fn drop_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// The JSON of a `data:` line, if it has valid JSON.
fn data_event(line: &[u8]) -> Option<Value> {
    let data = trim_space(line).strip_prefix(b"data:")?;
    serde_json::from_slice(trim_space(data)).ok()
}

/// Whether a line is the `message_stop` event that ends a reply
/// (`observeClaudeStreamLine`).
fn is_message_stop(line: &[u8]) -> bool {
    data_event(line).is_some_and(|event| str_at(&event, "type") == "message_stop")
}

/// A 502 for a stream that doesn't hold a complete reply.
fn bad_stream(message: impl Into<String>) -> ExecError {
    ExecError::upstream(502, message)
}

/// Checks that a stream Claude sent for a non-streaming call holds a whole
/// reply (`validateClaudeStreamingResponse`).
pub(crate) fn validate(data: &[u8]) -> Result<(), ExecError> {
    let mut has_data = false;
    let mut has_message_start = false;
    let mut has_message_delta = false;
    for line in data.split(|&b| b == b'\n') {
        let Some(payload) = trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let payload = trim_space(payload);
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }
        has_data = true;
        let Ok(root) = serde_json::from_slice::<Value>(payload) else {
            return Err(bad_stream(
                "claude executor: upstream returned malformed stream data",
            ));
        };
        match str_at(&root, "type").as_str() {
            "error" => {
                let mut message = str_at(&root, "error.message").trim().to_owned();
                if message.is_empty() {
                    message = str_at(&root, "error.type").trim().to_owned();
                }
                if message.is_empty() {
                    message = "unknown upstream error".to_owned();
                }
                return Err(bad_stream(format!(
                    "claude executor: upstream returned error event: {message}"
                )));
            }
            "message_start" => {
                if str_at(&root, "message.id").trim().is_empty()
                    || str_at(&root, "message.model").trim().is_empty()
                {
                    return Err(bad_stream(
                        "claude executor: upstream stream message_start is missing id or model",
                    ));
                }
                has_message_start = true;
            }
            "message_delta" => has_message_delta = true,
            _ => {}
        }
    }
    if !has_data {
        return Err(bad_stream(
            "claude executor: upstream returned empty stream response",
        ));
    }
    if !has_message_start {
        return Err(bad_stream(
            "claude executor: upstream stream response is missing message_start",
        ));
    }
    if !has_message_delta {
        return Err(bad_stream(
            "claude executor: upstream stream response ended before message completion",
        ));
    }
    Ok(())
}

/// What a streaming call needs to pass Claude's events on.
pub(crate) struct StreamSetup {
    /// Translates Claude's lines to the client's format; `None` for a Claude
    /// client, which gets the events as they are.
    pub(crate) translator: Option<ResponseStream>,
    /// Whether the client gets OpenAI Responses events.
    pub(crate) responses: bool,
    /// Whether the request asked for fast mode.
    pub(crate) fast: bool,
    /// Claude's status, for errors.
    pub(crate) status: u16,
}

/// The state of one stream.
struct State {
    reader: LineReader,
    setup: StreamSetup,
    /// The event read so far, for a Claude client.
    event: Vec<u8>,
    completed: bool,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
}

/// Passes Claude's stream in `response` on to the client.
pub(crate) fn forward(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let state = State {
        reader: LineReader::new(response),
        setup,
        event: Vec::new(),
        completed: false,
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
    /// Reads and passes on one line.
    async fn step(&mut self) {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => line,
            Some(Err(error)) => return self.end(Some(error)),
            None => return self.end(None),
        };
        if is_message_stop(&line) {
            self.completed = true;
        }
        if self.setup.translator.is_none() {
            self.forward_line(&line);
        } else {
            self.translate_line(&line);
        }
    }

    /// Adds a line to the event, and sends the event at its blank line.
    fn forward_line(&mut self, line: &[u8]) {
        if !self.event.is_empty() && self.event.len() + line.len() >= MAX_LINE {
            self.flush();
        }
        self.event.extend_from_slice(line);
        self.event.push(b'\n');
        if trim_space(line).is_empty() {
            self.flush();
            if self.completed {
                self.end(None);
            }
        }
    }

    fn flush(&mut self) {
        if !self.event.is_empty() {
            let event = std::mem::take(&mut self.event);
            self.pending.push_back(Ok(Bytes::from(event)));
        }
    }

    /// Translates a line and queues what it gives. A tool call that fails to
    /// translate ends the stream with a 502.
    fn translate_line(&mut self, line: &[u8]) {
        let Some(translator) = self.setup.translator.as_mut() else {
            return;
        };
        let mut chunks = translator.translate(line);
        let failed = translator.tool_input_error().is_some();
        if self.setup.responses && !failed {
            chunks = chunks
                .into_iter()
                .map(ensure_responses_usage_details)
                .collect();
        }
        self.queue(chunks);
        if failed {
            self.pending.push_back(Err(apply_patch_error()));
            self.finished = true;
            return;
        }
        if self.completed {
            self.end(None);
        }
    }

    fn queue(&mut self, chunks: Vec<Vec<u8>>) {
        for chunk in chunks {
            if !chunk.is_empty() {
                self.pending.push_back(Ok(Bytes::from(chunk)));
            }
        }
    }

    /// Ends the stream: sends what is left, then an error if the connection
    /// failed before `message_stop`.
    fn end(&mut self, error: Option<LineError>) {
        self.finished = true;
        match self.setup.translator.as_mut() {
            None => self.flush(),
            Some(translator) => {
                let chunks = translator.finish();
                let failed = translator.tool_input_error().is_some();
                self.queue(chunks);
                if failed {
                    self.pending.push_back(Err(apply_patch_error()));
                    return;
                }
            }
        }
        if self.completed {
            return;
        }
        if let Some(error) = error {
            tracing::debug!("claude: stream read failed: {error}");
            let error = wrap_fast(
                self.setup.fast,
                self.setup.status,
                plain_error(error.to_string()),
            );
            self.pending.push_back(Err(error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TestValidateClaudeStreamingResponse*.
    #[test]
    fn validates_streamed_replies() {
        let complete = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-5\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n",
        );
        assert!(validate(complete.as_bytes()).is_ok());

        for (body, want) in [
            ("", "upstream returned empty stream response"),
            (
                "event: ping\ndata: [DONE]\n\n",
                "upstream returned empty stream response",
            ),
            ("data: {nope\n", "upstream returned malformed stream data"),
            (
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\" Overloaded \"}}\n",
                "upstream returned error event: Overloaded",
            ),
            (
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n",
                "upstream returned error event: overloaded_error",
            ),
            (
                "data: {\"type\":\"error\"}\n",
                "upstream returned error event: unknown upstream error",
            ),
            (
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\" \",\"model\":\"m\"}}\n",
                "upstream stream message_start is missing id or model",
            ),
            (
                "data: {\"type\":\"message_delta\"}\n",
                "upstream stream response is missing message_start",
            ),
            (
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"1\",\"model\":\"m\"}}\n",
                "upstream stream response ended before message completion",
            ),
        ] {
            let error = validate(body.as_bytes()).unwrap_err();
            assert_eq!(error.status, 502, "{body}");
            assert_eq!(error.message, format!("claude executor: {want}"), "{body}");
        }
    }

    #[test]
    fn spots_message_stop() {
        assert!(is_message_stop(b" data: {\"type\":\"message_stop\"} "));
        assert!(is_message_stop(b"data:{\"type\":\"message_stop\"}"));
        assert!(!is_message_stop(b"event: message_stop"));
        assert!(!is_message_stop(b"data: {\"type\":\"message_delta\"}"));
        assert!(!is_message_stop(b"data: message_stop"));
    }
}
