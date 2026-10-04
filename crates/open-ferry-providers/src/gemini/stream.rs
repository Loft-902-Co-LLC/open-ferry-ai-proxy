// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go and
// gemini_vertex_executor.go (the stream readers of ExecuteStream,
// executeStreamWithServiceAccount and executeStreamWithAPIKey),
// helps/apply_patch.go (EndApplyPatchStream, StopApplyPatchStream) and
// helps/claude_input_tokens.go (TranslateStreamWithClaudeInputTokens)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A Gemini or Vertex AI SSE stream, read a line at a time and translated
//! to the client's format.
//!
//! From Gemini, each line has its running usage renamed unless it is the
//! last chunk's (see [`super::sse`]), and only the JSON object on a line is
//! translated; blank lines, event names and `[DONE]` are skipped. From
//! Vertex AI, each line goes to the translator as it is. Either way, when
//! the stream ends, or the connection fails, the translator finishes and a
//! final `[DONE]` is translated, then the read error, if any, follows.
//!
//! Deviations from upstream:
//! - Every line has the secrets the request sent (the API key or token
//!   among them) redacted before it is read, so none reaches the client in
//!   an error the stream carries, or anywhere else; see [`crate::redact`],
//!   whose `Policy::Client` leaves a secret shorter than eight bytes alone,
//!   as for every client error.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting and request logging are left to the call's taps,
//!   which see each chunk as it is read (see the crate's `observe_send`
//!   module).

use std::borrow::Cow;
use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ErrorKind, ExecError, Format};
use open_ferry_translate::registry::ResponseStream;

use super::sse::{filter_sse_usage_metadata, json_payload};
use crate::codex::claude_tokens;
use crate::codex::stream::{LineError, LineReader};
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, StatusError};
use crate::codex::usage::ensure_responses_usage_details;
use crate::redact::{Policy, Secrets};

/// How the lines of a stream reach the translator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lines {
    /// Gemini's: usage filtered, only the JSON object.
    Gemini,
    /// Vertex AI's: each line as it is.
    Raw,
}

/// What a streaming call needs to translate the provider's events.
pub(crate) struct StreamSetup {
    /// Translates Gemini chunks to the client's format.
    pub(crate) translator: ResponseStream,
    /// The format the client gets.
    pub(crate) response_format: Format,
    /// The client's format.
    pub(crate) source_format: Format,
    /// The client's request as it came, for the Claude input estimate.
    pub(crate) original: Bytes,
    pub(crate) lines: Lines,
    /// The secrets the request sent, redacted from each line.
    pub(crate) secrets: Secrets,
    /// How log lines name the executor.
    pub(crate) name: &'static str,
}

/// The state of one translated stream.
struct State {
    reader: LineReader,
    setup: StreamSetup,
    claude: claude_tokens::State,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
}

/// Translates the provider's stream in `response` to the client's format.
pub(crate) fn translate(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let claude = claude_tokens::State::new(
        &setup.source_format,
        &Format::GEMINI,
        &setup.response_format,
    );
    let state = State {
        reader: LineReader::new(response),
        setup,
        claude,
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
    /// Reads one line and translates it.
    async fn step(&mut self) {
        let line = match self.reader.next_line().await {
            Some(Ok(line)) => self.redact(line),
            Some(Err(error)) => return self.end(Some(error)).await,
            None => return self.end(None).await,
        };
        match self.setup.lines {
            Lines::Gemini => {
                let filtered = filter_sse_usage_metadata(&line);
                let Some(payload) = json_payload(&filtered) else {
                    return;
                };
                let payload = payload.to_vec();
                self.send(&payload).await;
            }
            Lines::Raw => self.send(&line).await,
        }
        if self.setup.translator.tool_input_error().is_some() {
            self.fail();
        }
    }

    /// `line` without the secrets the request was sent with.
    fn redact(&self, line: Vec<u8>) -> Vec<u8> {
        match self.setup.secrets.bytes(&line, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => line,
        }
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

    /// Ends the stream with the 502 of an `apply_patch` call that couldn't
    /// be translated.
    fn fail(&mut self) {
        self.finished = true;
        self.pending
            .push_back(Err(StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE).into()));
    }

    /// Ends the stream, after a read `error` or at its end: sends what the
    /// translator still holds and a translated `[DONE]`, then the error.
    async fn end(&mut self, error: Option<LineError>) {
        self.finished = true;
        let chunks = self.setup.translator.finish();
        let tool_input_failed = self.setup.translator.tool_input_error().is_some();
        self.queue(chunks);
        if tool_input_failed {
            return self.fail();
        }
        self.send(b"[DONE]").await;
        if self.setup.translator.tool_input_error().is_some() {
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
