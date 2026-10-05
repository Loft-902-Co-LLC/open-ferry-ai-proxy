// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go
// (executeInteractionsStream's reader, geminiInteractionsSSEPayload,
// geminiInteractionsSSEDone), helps/apply_patch.go
// (InitializeApplyPatchStream, EndApplyPatchStream, StopApplyPatchStream)
// and helps/claude_input_tokens.go
// (TranslateStreamWithClaudeInputTokens) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! An Interactions SSE stream, read a frame at a time and translated to the
//! client's format.
//!
//! A frame is the lines up to a blank one. An Interactions client gets each
//! frame as it came, ended by a blank line. Any other client gets the
//! frame's `data:` lines, joined, translated: a frame whose data is only
//! `[DONE]`, or that is an `event: done` frame, is translated as `[DONE]`,
//! and a frame that is bare JSON as it is. When the stream ends, or the
//! connection fails, the last frame is sent and the translator finishes,
//! then the read error, if any, follows; no `[DONE]` is added. For an
//! OpenAI Responses client the translator is started before the first
//! frame, so that a request with the `apply_patch` tool whose stream ends
//! before its terminal event, even with nothing at all, fails with one
//! `response.failed` and the `apply_patch` 502.
//!
//! Deviations from upstream:
//! - Every line has the secrets the request sent (the API key among them)
//!   redacted before it is read, so none reaches the client in an error the
//!   stream carries, or anywhere else; see [`crate::redact`], whose
//!   `Policy::Client` leaves a secret shorter than eight bytes alone, as for
//!   every client error.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting and request logging are left to the call's taps,
//!   which see each chunk as it is read (see the crate's `observe_send`
//!   module).

use std::borrow::Cow;
use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError, Format};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;

use crate::codex::claude_tokens;
use crate::codex::stream::{LineError, LineReader};
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::redact::{Policy, Secrets};

/// What a streaming call needs to translate the upstream's frames.
pub(super) struct StreamSetup {
    /// Translates Interactions events to the client's format.
    pub(super) translator: ResponseStream,
    /// The format the client gets.
    pub(super) response_format: Format,
    /// The client's format.
    pub(super) source_format: Format,
    /// The client's request as it came, for the Claude input estimate.
    pub(super) original: Bytes,
    /// The secrets the request sent, redacted from each line.
    pub(super) secrets: Secrets,
    /// How log lines name the executor.
    pub(super) name: &'static str,
}

/// The state of one translated stream.
struct State {
    reader: LineReader,
    setup: StreamSetup,
    claude: claude_tokens::State,
    /// The lines of the frame being read, joined by `\n`.
    frame: Vec<u8>,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
}

/// Translates the Interactions stream in `response` to the client's format.
pub(super) fn translate(response: reqwest::Response, mut setup: StreamSetup) -> ChunkStream {
    if setup.response_format == Format::OPENAI_RESPONSE {
        // `InitializeApplyPatchStream`: an empty chunk starts the
        // translator, so it fails a patch request at the end of even an
        // empty stream. Upstream also checks the request has the patch
        // tool; the translator only fails a stream for one that has.
        setup.translator.translate(b"");
    }
    let claude = claude_tokens::State::new(
        &setup.source_format,
        &Format::INTERACTIONS,
        &setup.response_format,
    );
    let state = State {
        reader: LineReader::new(response),
        setup,
        claude,
        frame: Vec::new(),
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
    /// Reads one line: a blank one sends the frame, any other joins it.
    async fn step(&mut self) {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => self.redact(line),
            Some(Err(error)) => return self.end(Some(error)).await,
            None => return self.end(None).await,
        };
        if trim_space(&line).is_empty() {
            self.emit_frame().await;
            return;
        }
        if !self.frame.is_empty() {
            self.frame.push(b'\n');
        }
        self.frame.extend_from_slice(&line);
    }

    /// `line` without the secrets the request was sent with.
    fn redact(&self, line: Vec<u8>) -> Vec<u8> {
        match self.setup.secrets.bytes(&line, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => line,
        }
    }

    /// Sends the frame read so far (upstream's `emitFrame`).
    async fn emit_frame(&mut self) {
        let frame = std::mem::take(&mut self.frame);
        let trimmed = trim_space(&frame);
        if trimmed.is_empty() {
            return;
        }
        if self.setup.response_format == Format::INTERACTIONS {
            let mut visible = trim_end_newlines(&frame).to_vec();
            visible.extend_from_slice(b"\n\n");
            self.pending.push_back(Ok(Bytes::from(visible)));
            return;
        }
        let payload = match sse_payload(&frame) {
            Some(payload) => payload,
            None if sse_done(&frame) => b"[DONE]".to_vec(),
            None if trimmed.first() == Some(&b'{') => trimmed.to_vec(),
            None => return,
        };
        self.send(&payload).await;
        if self.setup.translator.tool_input_error().is_some() {
            self.fail();
        }
    }

    /// Translates one event and queues what comes out
    /// (`TranslateStreamWithClaudeInputTokens`).
    async fn send(&mut self, payload: &[u8]) {
        let mut chunks = self.setup.translator.translate(payload);
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

    /// Ends the stream with the 502 of an `apply_patch` call that couldn't
    /// be translated.
    fn fail(&mut self) {
        self.finished = true;
        self.pending
            .push_back(Err(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE).into()));
    }

    /// Ends the stream, after a read `error` or at its end: sends the last
    /// frame and what the translator still holds, then the error.
    async fn end(&mut self, error: Option<LineError>) {
        self.emit_frame().await;
        if self.finished {
            return;
        }
        self.finished = true;
        let chunks = self.setup.translator.finish();
        let tool_input_failed = self.setup.translator.tool_input_error().is_some();
        self.queue(chunks);
        if tool_input_failed {
            return self.fail();
        }
        if let Some(error) = error {
            tracing::debug!("{}: stream read failed: {error}", self.setup.name);
            self.reader.report(&error);
            self.pending
                .push_back(Err(ExecError::new(ErrorKind::Upstream, error.to_string())));
        }
    }
}

/// `frame` without the `\r` and `\n` it ends with (Go's
/// `bytes.TrimRight(frame, "\r\n")`).
fn trim_end_newlines(frame: &[u8]) -> &[u8] {
    let end = frame
        .iter()
        .rposition(|&b| b != b'\r' && b != b'\n')
        .map_or(0, |last| last + 1);
    frame.get(..end).unwrap_or_default()
}

/// `geminiInteractionsSSEPayload`: the event a frame carries. A frame that
/// is bare JSON is the event as it is; otherwise its `data:` lines, each
/// trimmed and joined by `\n`, without empty ones and `[DONE]`. `None` when
/// there is no such data.
pub(super) fn sse_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let trimmed = trim_space(frame);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with(b"{") {
        return Some(trimmed.to_vec());
    }
    let mut payload = Vec::new();
    for line in frame.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !trim_space(line).starts_with(b"data:") {
            continue;
        }
        let Some(data) = after_data(line) else {
            continue;
        };
        let data = trim_space(data);
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    (!payload.is_empty()).then_some(payload)
}

/// What follows the first `data:` in `line`.
fn after_data(line: &[u8]) -> Option<&[u8]> {
    let start = line.windows(5).position(|window| window == b"data:")?;
    line.get(start + 5..)
}

/// `geminiInteractionsSSEDone`: whether a frame ends the stream: it is
/// `[DONE]`, has a `data: [DONE]` line, or is an `event: done` frame.
pub(super) fn sse_done(frame: &[u8]) -> bool {
    if trim_space(frame) == b"[DONE]" {
        return true;
    }
    let mut done_event = false;
    for line in frame.split(|&b| b == b'\n') {
        let line = trim_space(line.strip_suffix(b"\r").unwrap_or(line));
        if line.eq_ignore_ascii_case(b"event: done") {
            done_event = true;
            continue;
        }
        if let Some(data) = line.strip_prefix(b"data:")
            && trim_space(data) == b"[DONE]"
        {
            return true;
        }
    }
    done_event
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the event of a frame of `data:` lines, of bare JSON,
    // or of nothing but `[DONE]`.
    #[test]
    fn reads_a_frames_event() {
        let frame =
            b"event: step.delta\r\ndata: {\"a\":1}\n data:  {\"b\":2} \ndata:\ndata: [DONE]";
        assert_eq!(
            sse_payload(frame).as_deref(),
            Some(&b"{\"a\":1}\n{\"b\":2}"[..])
        );
        assert_eq!(
            sse_payload(b"  {\"a\":1}\n").as_deref(),
            Some(&b"{\"a\":1}"[..])
        );
        assert_eq!(sse_payload(b"event: done\ndata: [DONE]"), None);
        assert_eq!(sse_payload(b" \n "), None);
        assert_eq!(sse_payload(b"id: 1"), None);
    }

    // Not upstream's: what ends a stream.
    #[test]
    fn knows_a_done_frame() {
        assert!(sse_done(b" [DONE] "));
        assert!(sse_done(b"event: step.stop\ndata:  [DONE]\r"));
        assert!(sse_done(b"EVENT: DONE"));
        assert!(!sse_done(b"event: interaction.completed\ndata: {}"));
        assert!(!sse_done(b"data: [DONE]x"));
    }

    // Not upstream's: Go's `bytes.TrimRight(frame, "\r\n")`.
    #[test]
    fn trims_trailing_newlines() {
        assert_eq!(trim_end_newlines(b"a\r\n\n"), b"a");
        assert_eq!(trim_end_newlines(b"\r\n"), b"");
        assert_eq!(trim_end_newlines(b"a\nb"), b"a\nb");
    }
}
