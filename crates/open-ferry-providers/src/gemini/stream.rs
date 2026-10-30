// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go and
// gemini_vertex_executor.go (the stream readers of ExecuteStream,
// executeStreamWithServiceAccount and executeStreamWithAPIKey),
// helps/apply_patch.go (EndApplyPatchStream, StopApplyPatchStream) and
// helps/claude_input_tokens.go (TranslateStreamWithClaudeInputTokens)
// (v8.0.42, MIT).
// Buffered usage on read failure follows b12db8c3 (v8.0.42).
// https://github.com/router-for-me/CLIProxyAPI

//! A Gemini or Vertex AI SSE stream, read a line at a time and translated
//! to the client's format.
//!
//! From Gemini, each line has its running usage renamed unless it is the
//! last chunk's (see [`super::sse`]), and only the JSON object on a line is
//! translated; blank lines, event names and `[DONE]` are skipped. From
//! Vertex AI, each line goes to the translator as it is, except the Interactions
//! fallback receives normalized payloads and skips SSE control lines. When
//! the stream ends, the translator finishes and a final `[DONE]` is
//! translated. When the connection fails instead, the translator finishes
//! and the read error follows, with no `[DONE]`, so the client gets no
//! terminal event for an answer that was cut off.
//!
//! Deviations from upstream:
//! - Every line has the secrets the request sent (the API key or token
//!   among them) redacted before it is read, so none reaches the client in
//!   an error the stream carries, or anywhere else; see [`crate::redact`],
//!   whose `Policy::Client` leaves a secret shorter than eight bytes alone,
//!   as for every client error.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - After a translated Claude error, the stream ends once Gemini's trailing
//!   usage arrives, rather than waiting for EOF. This keeps the terminal error
//!   ready for the manager even when the client leaves and upstream stays open.
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
    body: crate::transport::ResponseBody,
    reader: LineReader,
    setup: StreamSetup,
    claude: claude_tokens::State,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
    translated_error: Option<String>,
    saw_finish_reason: bool,
}

/// Translates the provider's stream in `response` to the client's format.
pub(crate) fn translate(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let claude = claude_tokens::State::new(
        &setup.source_format,
        &Format::GEMINI,
        &setup.response_format,
    );
    let state = State {
        body: crate::transport::ResponseBody::new(&response),
        reader: LineReader::new(response),
        setup,
        claude,
        pending: VecDeque::new(),
        finished: false,
        translated_error: None,
        saw_finish_reason: false,
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
        if let Some(message) = json_payload(&line).and_then(super::prompt_block_error_message) {
            let error = ExecError::upstream(502, message).with_usage_kept();
            self.reader.report(&error);
            self.finished = true;
            self.pending.push_back(Err(error));
            return;
        }
        if (self.setup.response_format == Format::CLAUDE
            || self.setup.response_format == Format::OPENAI)
            && !self.saw_finish_reason
        {
            self.saw_finish_reason = json_payload(&line).is_some_and(has_finish_signal);
        }
        match self.setup.lines {
            Lines::Gemini => {
                let filtered = filter_sse_usage_metadata(&line);
                let Some(payload) = json_payload(&filtered) else {
                    return;
                };
                let payload = payload.to_vec();
                self.send(&payload).await;
            }
            Lines::Raw if self.setup.response_format == Format::INTERACTIONS => {
                let trimmed = open_ferry_translate::go::trim_space(&line);
                let payload = open_ferry_translate::go::trim_space(
                    trimmed.strip_prefix(b"data:").unwrap_or(trimmed),
                );
                if payload.is_empty()
                    || payload.starts_with(b":")
                    || [b"event:".as_slice(), b"id:", b"retry:"]
                        .iter()
                        .any(|prefix| payload.starts_with(prefix))
                {
                    return;
                }
                self.send(payload).await;
            }
            Lines::Raw => self.send(&line).await,
        }
        if self.translated_error.is_some()
            && json_payload(&line)
                .and_then(|payload| serde_json::from_slice::<serde_json::Value>(payload).ok())
                .is_some_and(|chunk| chunk.get("usageMetadata").is_some())
        {
            self.finished = true;
            let error = self.translated_error.take().unwrap();
            self.pending.push_back(Err(
                ExecError::from(StatusError::new(502, error)).with_usage_kept()
            ));
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
            if self.translated_error.is_none() && self.setup.response_format == Format::CLAUDE {
                // Retain the first translated error while reading trailing usage.
                self.translated_error = retained_error(&self.setup.response_format, &chunk);
                if let Some(error) = &self.translated_error {
                    self.reader.report(error);
                }
            }
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
    /// translator still holds, then the error or, at the end, a translated
    /// `[DONE]`.
    async fn end(&mut self, error: Option<LineError>) {
        self.finished = true;
        let chunks = self.setup.translator.finish();
        let tool_input_failed = self.setup.translator.tool_input_error().is_some();
        self.queue(chunks);
        if tool_input_failed {
            return self.fail();
        }
        if let Some(error) = self.translated_error.take() {
            self.pending.push_back(Err(
                ExecError::from(StatusError::new(502, error)).with_usage_kept()
            ));
            return;
        }
        if let Some(error) = error {
            tracing::debug!("{}: stream read failed: {error}", self.setup.name);
            self.reader.report(&error);
            self.pending.push_back(Err(self.body.classify(
                &error,
                ExecError::new(ErrorKind::Upstream, error.to_string()).with_usage_kept(),
            )));
            return;
        }
        if (self.setup.response_format == Format::CLAUDE
            || self.setup.response_format == Format::OPENAI)
            && !self.saw_finish_reason
        {
            let error = ExecError::stream_truncated("upstream stream closed before finishReason");
            self.reader.report(&error);
            self.pending.push_back(Err(error));
            return;
        }
        self.send(b"[DONE]").await;
        if let Some(error) = self.translated_error.take() {
            self.pending.push_back(Err(
                ExecError::from(StatusError::new(502, error)).with_usage_kept()
            ));
            return;
        }
        if self.setup.translator.tool_input_error().is_some() {
            self.fail();
        }
    }
}

fn has_finish_signal(payload: &[u8]) -> bool {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    [&root, &root["response"]].into_iter().any(|root| {
        root["candidates"].as_array().is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                !crate::json::str_at(candidate, "finishReason")
                    .trim()
                    .is_empty()
            })
        })
    })
}

/// Retains the JSON body from any SSE line form JSONPayload accepts.
fn retained_error(format: &Format, chunk: &[u8]) -> Option<String> {
    chunk
        .split(|b| *b == b'\n')
        .find_map(|line| super::translated_claude_error(format, json_payload(line)?))
}

#[cfg(test)]
mod retained_error_tests {
    use super::*;

    // Not upstream's: check JSONPayload forms at the error retention boundary.
    #[test]
    fn retains_trimmed_error_payloads() {
        let error = r#"{"type":"error","error":{"type":"api_error","message":"malformed"}}"#;
        for line in [
            error.to_owned(),
            format!("data: {error}"),
            format!("  data: {error}\r"),
            format!("event: error\ndata: {error}\n\n"),
        ] {
            assert_eq!(
                retained_error(&Format::CLAUDE, line.as_bytes()).as_deref(),
                Some(error)
            );
            assert!(retained_error(&Format::GEMINI, line.as_bytes()).is_none());
        }
    }
}
