// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_stream.go,
// helps/claude_input_tokens.go (TranslateStreamWithClaudeInputTokens) and
// internal/client/grokbuild/keepalive.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's SSE stream, read a line at a time and translated to the
//! client's format.
//!
//! Each `data:` event is checked first: a terminal failure (`error`,
//! `response.failed`) or an empty `response.incomplete` ends the stream with
//! an error, `response.output_item.done` items are kept, and a terminal
//! success (`response.completed`, `response.incomplete`, `response.done`)
//! gets the kept items as its `output` when it came without, then ends the
//! stream. A stream that breaks off after sending something ends with a 408.
//!
//! Grok Build clients (`grok-pager`, `grok-shell`) get Codex's `keepalive`
//! events as SSE comments, which they expect.
//!
//! Deviations from upstream:
//! - Bootstrap buffering, a config option that holds the first events to
//!   catch an overload, isn't ported; chunks go out as they come.
//! - A rewritten terminal event is written by `serde_json`.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting, request logging, multi-agent v2 and image tool usage
//!   aren't ported.

use std::collections::VecDeque;
use std::fmt;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderMap;
use open_ferry_core::exec::{ChunkStream, ExecError, Format};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;
use serde_json::Value;

use super::claude_tokens;
use super::client::error_chain;
use super::ext::{self, Turn};
use super::terminal::{
    OutputItems, empty_incomplete_stream_error, has_meaningful_output_delta,
    incomplete_stream_error, is_terminal_empty_incomplete, normalize_completion, terminal_failure,
};
use super::usage::ensure_responses_usage_details;
use crate::json::{get, str_at, str_of};

/// The longest line read, as upstream's scanner allows.
pub(crate) const MAX_LINE: usize = 52_428_800;

/// What a Grok Build client gets for a `keepalive` event.
const KEEPALIVE_COMMENT: &[u8] = b": keepalive\n\n";

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

/// Whether the client is a Grok Build one, by its user agent
/// (`grokbuild.IsGrokClientHeaders`).
pub(crate) fn is_grok_client(headers: &HeaderMap) -> bool {
    headers
        .get_all(http::header::USER_AGENT)
        .iter()
        .any(|value| {
            let agent =
                open_ferry_translate::go::to_lower(&String::from_utf8_lossy(value.as_bytes()));
            agent.contains("grok-pager") || agent.contains("grok-shell")
        })
}

/// Whether an SSE line is a `keepalive` event or data frame
/// (`grokbuild.IsKeepaliveSSELine`).
fn is_keepalive_line(line: &[u8]) -> bool {
    let trimmed = trim_space(line);
    if let Some(name) = trimmed.strip_prefix(b"event:") {
        return trim_space(name) == b"keepalive";
    }
    if let Some(data) = trimmed.strip_prefix(b"data:") {
        return serde_json::from_slice::<Value>(trim_space(data))
            .is_ok_and(|payload| str_of(get(&payload, "type")) == "keepalive");
    }
    false
}

/// What a streaming call needs to translate Codex's events.
pub(crate) struct StreamSetup {
    /// Translates Codex's lines to the client's format.
    pub(crate) translator: ResponseStream,
    /// The format the client gets.
    pub(crate) response_format: Format,
    /// The client's format.
    pub(crate) source_format: Format,
    /// The client's request as it came, for the Claude input estimate.
    pub(crate) original: Bytes,
    /// Whether a native client gets Codex's terminal event as it is.
    pub(crate) preserve_native: bool,
    /// Whether the client is a Grok Build one.
    pub(crate) grok: bool,
    /// The credential's token, redacted from the errors made from Codex's
    /// events.
    pub(crate) secret: String,
    /// Whether a usage limit cools only the model
    /// (`codex.model-level-cooling`).
    pub(crate) model_level_cooling: bool,
    /// What the request's hooks noted ([`ext`]).
    pub(crate) turn: Turn,
}

/// The state of one translated stream.
struct State {
    reader: LineReader,
    setup: StreamSetup,
    claude: claude_tokens::State,
    items: OutputItems,
    saw_output_delta: bool,
    emitted: usize,
    pending: VecDeque<Bytes>,
    finished: bool,
}

/// Translates Codex's stream in `response` to the client's format.
pub(crate) fn translate(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let claude =
        claude_tokens::State::new(&setup.source_format, &Format::CODEX, &setup.response_format);
    let state = State {
        reader: LineReader::new(response),
        setup,
        claude,
        items: OutputItems::default(),
        saw_output_delta: false,
        emitted: 0,
        pending: VecDeque::new(),
        finished: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(chunk) = state.pending.pop_front() {
                return Some((Ok(chunk), state));
            }
            if state.finished {
                return None;
            }
            if let Err(error) = state.step().await {
                state.finished = true;
                return Some((Err(error), state));
            }
        }
    })
    .boxed()
}

impl State {
    /// Reads and translates one line. An error ends the stream.
    async fn step(&mut self) -> Result<(), ExecError> {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                tracing::debug!("codex: stream read failed: {error}");
                return self.end_early();
            }
            None => return self.end_early(),
        };

        let mut terminal_success = false;
        let translated_line = if self.setup.grok && is_keepalive_line(&line) {
            KEEPALIVE_COMMENT.to_vec()
        } else if let Some(rest) = line.strip_prefix(b"data:") {
            let data = ext::restore(&self.setup.turn, trim_space(rest));
            let mut event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            if let Some((error, body)) = terminal_failure(&event, self.setup.model_level_cooling) {
                ext::on_failure(&self.setup.turn, error.status, body.as_bytes());
                return Err(error.redacted(&self.setup.secret).into());
            }
            if has_meaningful_output_delta(&event) {
                self.saw_output_delta = true;
            }
            if is_terminal_empty_incomplete(&event, self.items.len(), self.saw_output_delta) {
                return Err(empty_incomplete_stream_error().into());
            }
            let mut rewritten = None;
            match str_at(&event, "type").as_str() {
                "response.output_item.done" => self.items.collect(&event),
                "response.completed" | "response.incomplete" | "response.done" => {
                    terminal_success = true;
                    let mut changed = normalize_completion(&mut event);
                    if !self.setup.preserve_native {
                        changed |= self.items.patch(&mut event);
                    }
                    ext::on_completed(&self.setup.turn, &event);
                    if changed {
                        rewritten = Some(event.to_string());
                    }
                }
                _ => {}
            }
            let data = rewritten.as_ref().map_or(&*data, |text| text.as_bytes());
            [b"data: ".as_slice(), data].concat()
        } else {
            line
        };

        let mut chunks = self.setup.translator.translate(&translated_line);
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
        for chunk in chunks {
            if !chunk.is_empty() {
                self.emitted += 1;
                self.pending.push_back(Bytes::from(chunk));
            }
        }
        if terminal_success {
            self.finished = true;
        }
        Ok(())
    }

    /// The stream ended before a terminal event: silently when nothing was
    /// sent (the caller sees an empty stream), else with a 408.
    fn end_early(&mut self) -> Result<(), ExecError> {
        self.finished = true;
        if self.emitted == 0 {
            tracing::debug!("codex: upstream stream closed before first payload");
            return Ok(());
        }
        Err(incomplete_stream_error().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    // TestIsGrokClientUserAgent and TestIsGrokClientHeaders.
    #[test]
    fn detects_grok_clients() {
        for (agent, want) in [
            ("grok-shell/0.2.119 (macos; aarch64)", true),
            ("grok-pager/1.0.5 grok-shell/1.0.5 (linux; x86_64)", true),
            ("grok-pager/1.0.5", true),
            ("GROK-PAGER/1.0", true),
            ("GROK-SHELL/1.0", true),
            ("curl/8.7.1", false),
            ("openai-python/1.0.0", false),
            ("", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::USER_AGENT, HeaderValue::from_static(agent));
            assert_eq!(is_grok_client(&headers), want, "{agent}");
        }
        assert!(!is_grok_client(&HeaderMap::new()));
        let mut headers = HeaderMap::new();
        headers.append(http::header::USER_AGENT, HeaderValue::from_static("curl/8"));
        headers.append(
            http::header::USER_AGENT,
            HeaderValue::from_static("grok-shell/0.2"),
        );
        assert!(is_grok_client(&headers));
    }

    // TestIsKeepaliveSSELine.
    #[test]
    fn detects_keepalive_lines() {
        for (line, want) in [
            ("event: keepalive", true),
            ("  event:keepalive  ", true),
            (r#"data: {"type":"keepalive"}"#, true),
            (r#"data:{"type":"keepalive","sequence_number":3}"#, true),
            ("event: response.created", false),
            (r#"data: {"type":"response.created"}"#, false),
            ("data: [DONE]", false),
            (": comment", false),
            ("", false),
        ] {
            assert_eq!(is_keepalive_line(line.as_bytes()), want, "{line}");
        }
    }
}
