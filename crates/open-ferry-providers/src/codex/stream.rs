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
//! Chunks go out as they come, unless the config's
//! `codex.stream-bootstrap-buffering` is on. Then [`translate_buffered`]
//! holds back the lines before generation starts (the handshake,
//! keepalives, and items announced with no content yet), up to 48 lines,
//! 1 MiB and the config's `codex.stream-bootstrap-timeout`. An overload
//! among them fails the call before it starts, with a 503 or 429, so that
//! the credential manager can try another credential; any other failure
//! comes after the held chunks, as it would have without the buffering.
//!
//! Deviations from upstream:
//! - A Grok Build client is told by the call's own headers alone. Upstream
//!   also reads the user agent from the request's Gin context
//!   (`IsGrokClientContext`), which has the same headers: the Codex
//!   executor is handed them as the call's options.
//! - A rewritten terminal event is written by `serde_json`.
//! - Dropping the stream, or the call while lines are held back, stops
//!   reading, where upstream watches its context.
//! - Usage reporting and request logging are left to the call's taps,
//!   which see each chunk as it is read (see the crate's `observe_send`
//!   module). Image tool usage isn't ported.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderMap;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError, Format};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;
use serde_json::Value;

use super::claude_tokens;
use super::client::error_chain;
use super::ext::{self, Turn};
use super::terminal::{
    MAX_BOOTSTRAP_BYTES, MAX_BOOTSTRAP_FRAMES, OutputItems, bootstrap_overload_error,
    empty_incomplete_stream_error, has_meaningful_output_delta, incomplete_stream_error,
    is_bootstrap_bufferable_event, is_overload_bootstrap_failure, is_terminal_empty_incomplete,
    normalize_completion, terminal_failure,
};
use super::usage::ensure_responses_usage_details;
use crate::json::{get, str_at, str_of};
use crate::observe_send::{self, BodyTap};

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
    /// What sees the body as it is read, when the call's taps do.
    tap: Option<BodyTap>,
    buffer: Vec<u8>,
    /// Where the unread data starts.
    start: usize,
    /// How much of the unread data is known to hold no `\n`.
    scanned: usize,
    state: ReaderState,
}

impl LineReader {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        let tap = BodyTap::of(&response);
        Self {
            response,
            tap,
            buffer: Vec::new(),
            start: 0,
            scanned: 0,
            state: ReaderState::Reading,
        }
    }

    /// Tells the call's taps, if any see it, the stream failed with `error`
    /// (see [`BodyTap::error`]).
    pub(crate) fn report(&self, error: &dyn fmt::Display) {
        observe_send::attempt_error(self.tap.as_ref(), error);
    }

    /// The next line, or `None` at the end. A line that wouldn't fit in
    /// [`MAX_LINE`] bytes with its `\n` is an error, as it is for Go's
    /// scanner, and ends the reading.
    pub(crate) async fn next_line(&mut self) -> Option<Result<Vec<u8>, LineError>> {
        loop {
            let unread = self.buffer.get(self.start..).unwrap_or_default();
            if let Some(offset) = unread
                .get(self.scanned..)
                .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
            {
                let end = self.scanned + offset;
                if end >= MAX_LINE {
                    return Some(Err(self.too_long()));
                }
                let line = drop_cr(unread.get(..end).unwrap_or_default()).to_vec();
                self.start += end + 1;
                self.scanned = 0;
                return Some(Ok(line));
            }
            self.scanned = unread.len();
            if self.scanned > MAX_LINE {
                return Some(Err(self.too_long()));
            }
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
            if self.scanned == MAX_LINE {
                return Some(Err(self.too_long()));
            }
            if self.start > 0 {
                self.buffer.drain(..self.start);
                self.start = 0;
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => {
                    if let Some(tap) = &self.tap {
                        tap.chunk(&chunk);
                    }
                    self.buffer.extend_from_slice(&chunk);
                }
                Ok(None) => self.state = ReaderState::Done,
                Err(error) => self.state = ReaderState::Failed(error.without_url()),
            }
        }
    }

    /// Drops what is left and ends the reading, for a line over the limit.
    fn too_long(&mut self) -> LineError {
        self.state = ReaderState::Done;
        self.buffer = Vec::new();
        self.start = 0;
        self.scanned = 0;
        LineError::TooLong
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
    /// The secrets the request sent, redacted from the errors made from
    /// Codex's events.
    pub(crate) secrets: crate::redact::Secrets,
    /// Whether a usage limit cools only the model
    /// (`codex.model-level-cooling`).
    pub(crate) model_level_cooling: bool,
    /// What the request's hooks noted ([`ext`]).
    pub(crate) turn: Turn,
}

/// The clock bootstrap buffering's time limit is measured on
/// (upstream's `nowCodexBootstrap`).
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// How long [`translate_buffered`] may hold lines back.
pub(crate) struct Bootstrap {
    /// The config's `codex.stream-bootstrap-timeout`; zero for no limit.
    pub(crate) timeout: Duration,
    /// The time now.
    pub(crate) now: Clock,
}

/// One line of Codex's stream, checked and translated.
struct Frame {
    /// The chunks for the client.
    chunks: Vec<Vec<u8>>,
    /// Whether the line may be held back while the stream starts: it isn't
    /// a `data:` line, or its event is one [`is_bootstrap_bufferable_event`]
    /// allows.
    handshake: bool,
    /// Whether it was the terminal success, which ends the stream.
    terminal: bool,
}

/// A line that ends the stream with an error.
struct Failure {
    /// The error, with the request's secrets redacted.
    error: ExecError,
    /// A terminal failure event's error body, which says whether it was an
    /// overload; `None` for an empty `response.incomplete`.
    body: Option<String>,
}

/// What upstream records for a stream that ended before its first chunk,
/// which the caller sees as an empty stream.
const CLOSED_BEFORE_FIRST_PAYLOAD: &str = "upstream stream closed before first payload";

/// The state of one translated stream.
struct State {
    reader: LineReader,
    setup: StreamSetup,
    claude: claude_tokens::State,
    items: OutputItems,
    saw_output_delta: bool,
    emitted: usize,
    pending: VecDeque<Bytes>,
    /// An error to end the stream with once `pending` is sent.
    failure: Option<ExecError>,
    finished: bool,
}

/// Translates Codex's stream in `response` to the client's format.
pub(crate) fn translate(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    State::new(response, setup).into_stream()
}

/// Translates Codex's stream in `response` to the client's format, holding
/// back the lines before generation starts (`codex.stream-bootstrap-buffering`).
///
/// A line is held while it is a handshake line and the lines held so far
/// stay within [`MAX_BOOTSTRAP_FRAMES`] lines, [`MAX_BOOTSTRAP_BYTES`]
/// bytes (each line with its chunks) and the time limit. The first line
/// that isn't held starts the stream, after the held chunks. Before then:
/// - an overload ([`is_overload_bootstrap_failure`]) within the time limit
///   is the call's error, a 503 or 429;
/// - another failure, or an overload after the time limit, ends the stream
///   after the held chunks;
/// - a read error is the call's error;
/// - the end of the stream gives an empty stream if nothing was held, else
///   a 408 for the call.
pub(crate) async fn translate_buffered(
    response: reqwest::Response,
    setup: StreamSetup,
    bootstrap: Bootstrap,
) -> Result<ChunkStream, ExecError> {
    let mut state = State::new(response, setup);
    state.bootstrap(&bootstrap).await?;
    Ok(state.into_stream())
}

impl State {
    fn new(response: reqwest::Response, setup: StreamSetup) -> Self {
        let claude =
            claude_tokens::State::new(&setup.source_format, &Format::CODEX, &setup.response_format);
        Self {
            reader: LineReader::new(response),
            setup,
            claude,
            items: OutputItems::default(),
            saw_output_delta: false,
            emitted: 0,
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

    /// Reads and translates one line. An error ends the stream.
    async fn step(&mut self) -> Result<(), ExecError> {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                tracing::debug!("codex: stream read failed: {error}");
                self.reader.report(&error);
                return self.end_early();
            }
            None => return self.end_early(),
        };
        let frame = self.process(line).await.map_err(|failure| failure.error)?;
        self.send(frame.chunks);
        if frame.terminal {
            self.finished = true;
        }
        Ok(())
    }

    /// Holds back the lines before generation starts; see
    /// [`translate_buffered`]. Returns the call's error, or leaves the
    /// stream to go on from where it started.
    async fn bootstrap(&mut self, bootstrap: &Bootstrap) -> Result<(), ExecError> {
        let start = (bootstrap.now)();
        let timed_out = || {
            !bootstrap.timeout.is_zero()
                && (bootstrap.now)().saturating_duration_since(start) >= bootstrap.timeout
        };
        let mut held = Vec::new();
        let mut held_any = false;
        let mut frames = 0;
        let mut bytes = 0;
        while let Some(line) = self.reader.next_line().await {
            let line = line.map_err(|error| {
                tracing::debug!("codex: stream read failed: {error}");
                self.reader.report(&error);
                ExecError::new(ErrorKind::Upstream, error.to_string())
            })?;
            let line_len = line.len();
            let gives_chunk = self.upstream_gives_chunk(&line);
            let frame = match self.process(line).await {
                Ok(frame) => frame,
                Err(failure) => {
                    if let Some(body) = &failure.body
                        && is_overload_bootstrap_failure(body.as_bytes())
                    {
                        if !timed_out() {
                            tracing::debug!(
                                "codex executor: bootstrap overload rejection after {frames} buffered lines, failing over"
                            );
                            let error = bootstrap_overload_error(body.as_bytes());
                            return Err(error.redacted(&self.setup.secrets).into());
                        }
                        tracing::debug!(
                            "codex executor: bootstrap overload rejection after {frames} lines, time budget exhausted; delivering in-stream"
                        );
                    }
                    self.send(held);
                    self.failure = Some(failure.error);
                    return Ok(());
                }
            };
            if frame.handshake && !frame.terminal {
                let frame_bytes = line_len + frame.chunks.iter().map(Vec::len).sum::<usize>();
                let timed_out = timed_out();
                if !timed_out
                    && frames < MAX_BOOTSTRAP_FRAMES
                    && bytes + frame_bytes <= MAX_BOOTSTRAP_BYTES
                {
                    frames += 1;
                    bytes += frame_bytes;
                    held_any |= !frame.chunks.is_empty() || gives_chunk;
                    held.extend(frame.chunks);
                    continue;
                }
                let exhausted = if timed_out {
                    "time budget"
                } else if frames < MAX_BOOTSTRAP_FRAMES {
                    "byte budget"
                } else {
                    "frame budget"
                };
                tracing::debug!(
                    "codex executor: bootstrap {exhausted} exhausted after {frames} lines / {bytes} bytes, releasing stream without overload probing"
                );
            }
            self.send(held);
            self.send(frame.chunks);
            if frame.terminal {
                self.finished = true;
            }
            return Ok(());
        }
        if held_any {
            let error: ExecError = incomplete_stream_error().into();
            self.reader.report(&error);
            return Err(error);
        }
        tracing::debug!("codex: upstream stream closed before first payload");
        self.reader.report(&CLOSED_BEFORE_FIRST_PAYLOAD);
        self.finished = true;
        Ok(())
    }

    /// Whether upstream's translation of `line` gives a chunk even when it
    /// is empty, which ours leaves out; bootstrap buffering counts the line
    /// as held all the same, as upstream does. Every line does when the
    /// client speaks Codex's own format or OpenAI Responses, as each line
    /// becomes a chunk as it is. Every `data:` line does for a Claude client,
    /// whose translator gives one chunk per `data:` line, empty for an event
    /// such as `response.in_progress`; it only holds an event back while a
    /// tool call is open, which can't happen before generation starts.
    fn upstream_gives_chunk(&self, line: &[u8]) -> bool {
        if !self.setup.translator.is_translated()
            || self.setup.response_format == Format::OPENAI_RESPONSE
        {
            return true;
        }
        self.setup.response_format == Format::CLAUDE && line.starts_with(b"data:")
    }

    /// Checks and translates one line.
    async fn process(&mut self, line: Vec<u8>) -> Result<Frame, Failure> {
        let mut terminal = false;
        let mut handshake = true;
        let translated_line = if self.setup.grok && is_keepalive_line(&line) {
            KEEPALIVE_COMMENT.to_vec()
        } else if let Some(rest) = line.strip_prefix(b"data:") {
            let data = ext::restore(&self.setup.turn, trim_space(rest));
            let mut event: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
            if let Some((error, body)) = terminal_failure(&event, self.setup.model_level_cooling) {
                ext::on_failure(&self.setup.turn, error.status, body.as_bytes());
                let error: ExecError = error.redacted(&self.setup.secrets).into();
                self.reader.report(&error);
                return Err(Failure {
                    error,
                    body: Some(body),
                });
            }
            if has_meaningful_output_delta(&event) {
                self.saw_output_delta = true;
            }
            if is_terminal_empty_incomplete(&event, self.items.len(), self.saw_output_delta) {
                let error: ExecError = empty_incomplete_stream_error().into();
                self.reader.report(&error);
                return Err(Failure { error, body: None });
            }
            let event_type = str_at(&event, "type");
            handshake = is_bootstrap_bufferable_event(&event_type, &data, &event);
            let mut rewritten = None;
            match event_type.as_str() {
                "response.output_item.done" => self.items.collect(&event),
                "response.completed" | "response.incomplete" | "response.done" => {
                    terminal = true;
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
        Ok(Frame {
            chunks,
            handshake,
            terminal,
        })
    }

    /// Queues the non-empty chunks for the client.
    fn send(&mut self, chunks: Vec<Vec<u8>>) {
        for chunk in chunks {
            if !chunk.is_empty() {
                self.emitted += 1;
                self.pending.push_back(Bytes::from(chunk));
            }
        }
    }

    /// The stream ended before a terminal event: silently when nothing was
    /// sent (the caller sees an empty stream), else with a 408.
    fn end_early(&mut self) -> Result<(), ExecError> {
        self.finished = true;
        if self.emitted == 0 {
            tracing::debug!("codex: upstream stream closed before first payload");
            self.reader.report(&CLOSED_BEFORE_FIRST_PAYLOAD);
            return Ok(());
        }
        let error: ExecError = incomplete_stream_error().into();
        self.reader.report(&error);
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    // Not upstream's: a line must fit in the scanner's buffer with its
    // `\n`, as Go's `bufio.Scanner` with a 50 MiB limit needs.
    #[tokio::test]
    async fn lines_must_fit_the_scanner_limit() {
        for (len, newline, fits) in [
            (MAX_LINE - 1, true, true),
            (MAX_LINE, true, false),
            (MAX_LINE + 1, false, false),
        ] {
            let mut body = vec![b'x'; len];
            if newline {
                body.push(b'\n');
            }
            let mut reader = LineReader::new(reqwest::Response::from(http::Response::new(body)));
            match reader.next_line().await {
                Some(Ok(line)) => assert!(fits && line.len() == len, "{len} bytes read"),
                Some(Err(LineError::TooLong)) => assert!(!fits, "{len} bytes refused"),
                other => panic!("{len} bytes: {other:?}"),
            }
            if !fits {
                assert!(reader.next_line().await.is_none(), "reading goes on");
            }
        }
    }

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

    // TestIsKeepaliveSSELine, and TestIsKeepalivePayload, whose payloads are
    // the `data:` lines here: `IsKeepalivePayload` isn't a function of its
    // own.
    #[test]
    fn detects_keepalive_lines() {
        for (line, want) in [
            ("event: keepalive", true),
            ("event: keepalive\n", true),
            ("  event: keepalive  ", true),
            ("  event:keepalive  ", true),
            (r#"data: {"type":"keepalive"}"#, true),
            (r#"data: {"type":"keepalive","sequence_number":3}"#, true),
            (r#"data:{"type":"keepalive","sequence_number":3}"#, true),
            ("event: response.created", false),
            ("event: keepalive-other", false),
            (r#"data: {"type":"response.created"}"#, false),
            (r#"data: {"type":"response.reasoning.delta"}"#, false),
            ("data: [DONE]", false),
            ("data: ", false),
            (": comment", false),
            ("", false),
        ] {
            assert_eq!(is_keepalive_line(line.as_bytes()), want, "{line}");
        }
    }
}
