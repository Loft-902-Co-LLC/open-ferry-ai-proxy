// Ported from CLIProxyAPI internal/runtime/executor/claude_executor_stream.go
// (the stream loops and validateClaudeStreamingResponse),
// claude_executor_diagnostics.go (observeClaudeStreamLine) and
// helps/apply_patch.go (the stream failure helpers) (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude's SSE stream, read a line at a time.
//!
//! A Claude client gets whole events, each with its blank line, as Claude
//! sent them. Any other client gets each line translated to its format. A
//! `message_stop` event ends the stream; a connection that fails before it
//! ends the stream with an error, including a clean premature EOF.
//!
//! A non-streaming call that Claude answered with a stream is checked first
//! ([`validate`]): it must hold a `message_start` with an ID and model and a
//! `message_delta`, and no `error` event.
//!
//! Deviations from upstream:
//! - Each line has the secrets the request sent (the API key or token among
//!   them) redacted before it is passed on or translated, so none reaches
//!   the client in an error the stream carries, nor in what a model echoes
//!   back in its output, which upstream passes on as it is. Only those of
//!   eight bytes or more are redacted, as every client error is (see
//!   `Policy::Client` in [`crate::redact`]). The call's taps see each line
//!   as Claude sent it.
//! - An event that grows past 50 MiB goes out in pieces rather than whole;
//!   upstream buffers it however large it gets.
//! - Dropping the stream stops reading, where upstream watches its context.
//! - Usage reporting and request logging are left to the call's taps,
//!   which see each chunk as it is read (see the crate's `observe_send`
//!   module).
//! - Response model restoring, OAuth tool name restoring, continuity
//!   tracking and the thinking replay cache aren't ported.

use std::borrow::Cow;
use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::exec::{ChunkStream, ExecError};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::ResponseStream;
use serde_json::Value;

use super::ratelimit::{plain_error, wrap_fast};
use super::usage::ensure_responses_usage_details;
pub(crate) use crate::codex::stream::MAX_LINE;
use crate::codex::stream::{LineError, LineReader};
use crate::json::str_at;
use crate::redact::{Policy, Secrets};

/// The error for a tool call whose input couldn't be translated
/// (`ApplyPatchUpstreamErrorMessage`).
pub(crate) const APPLY_PATCH_ERROR_MESSAGE: &str =
    "Invalid apply_patch tool arguments received from upstream.";

/// The 502 for a tool call whose input couldn't be translated.
pub(crate) fn apply_patch_error() -> ExecError {
    ExecError::upstream(502, APPLY_PATCH_ERROR_MESSAGE)
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
    /// The secrets the request sent, redacted from each error event.
    pub(crate) secrets: Secrets,
    pub(crate) web_search: Option<super::web_search::Scope>,
}

// Keep in sync with open-ferry-server/src/handlers/claude.rs. The private
// stream modules cannot share this policy without moving it outside their scope.
const OBSERVATION_REUSE_LIMIT: usize = 8 << 10;

/// Joins nonempty data lines inside the observation's own storage. Writes
/// always trail reads, so no temporary line list, payload, or bare copy is
/// needed. These bytes are observation-only; forwarded bytes are untouched.
fn event_payload(raw: &mut [u8]) -> (&[u8], bool) {
    let mut start = 0;
    let mut written = 0;
    let mut found = false;
    while start < raw.len() {
        let end = raw[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(raw.len(), |n| start + n);
        let range = trim_space(&raw[start..end])
            .strip_prefix(b"data:")
            .map(trim_space)
            .filter(|data| !data.is_empty())
            .map(|data| {
                // trim_space can return an unrelated empty slice. Filtering
                // empties above ensures this nonempty subslice lies in raw.
                let offset = data.as_ptr() as usize - raw.as_ptr() as usize;
                offset..offset + data.len()
            });
        if let Some(range) = range {
            if found {
                raw[written] = b'\n';
                written += 1;
            }
            let len = range.len();
            raw.copy_within(range, written);
            written += len;
            found = true;
        }
        start = end + 1;
    }
    if found {
        (&raw[..written], false)
    } else {
        (raw, true)
    }
}

/// The state of one stream.
struct State {
    body: crate::transport::ResponseBody,
    reader: LineReader,
    setup: StreamSetup,
    /// The event read so far, for a Claude client.
    event: Vec<u8>,
    completed: bool,
    observation: Vec<u8>,
    observation_overflow: bool,
    stream_error: Option<String>,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
}

/// Passes Claude's stream in `response` on to the client.
pub(crate) fn forward(response: reqwest::Response, setup: StreamSetup) -> ChunkStream {
    let state = State {
        body: crate::transport::ResponseBody::new(&response),
        reader: LineReader::new(response),
        setup,
        event: Vec::new(),
        completed: false,
        observation: Vec::new(),
        observation_overflow: false,
        stream_error: None,
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
        let line = self.redact(line);
        if let Some(scope) = &self.setup.web_search {
            scope.record(&line);
        }
        if !self.observation_overflow && self.observation.len() + line.len() < MAX_LINE {
            self.observation.extend_from_slice(&line);
            self.observation.push(b'\n');
        } else {
            self.observation_overflow = true;
        }
        if trim_space(&line).is_empty() {
            self.observe_event();
        }
        if self.setup.translator.is_none() {
            self.forward_line(&line);
        } else {
            self.translate_line(&line);
        }
        if self.stream_error.is_some() && !self.finished {
            self.end(None);
        }
    }

    fn observe_event(&mut self) {
        let mut raw = std::mem::take(&mut self.observation);
        if !std::mem::take(&mut self.observation_overflow) {
            self.observe_raw(&mut raw);
        }
        raw.clear();
        // Reuse ordinary events, but don't retain a large event's peak.
        if raw.capacity() <= OBSERVATION_REUSE_LIMIT {
            self.observation = raw;
        }
    }

    fn observe_raw(&mut self, raw: &mut [u8]) {
        let (payload, bare) = event_payload(raw);
        let Ok(event) = serde_json::from_slice::<Value>(trim_space(payload)) else {
            return;
        };
        match str_at(&event, "type").as_str() {
            "message_stop" => self.completed = true,
            "message" if bare && !str_at(&event, "stop_reason").trim().is_empty() => {
                self.completed = true
            }
            "error" if !bare && self.stream_error.is_none() => {
                self.stream_error = Some(String::from_utf8_lossy(trim_space(payload)).into_owned());
            }
            _ => {}
        }
    }

    /// `line` without the secrets the request was sent with.
    fn redact(&self, line: Vec<u8>) -> Vec<u8> {
        match self.setup.secrets.bytes(&line, Policy::Client) {
            Cow::Owned(redacted) => redacted,
            Cow::Borrowed(_) => line,
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
        self.observe_event();
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
        let error = match error {
            Some(error) => self.body.classify(&error, plain_error(error.to_string())),
            None => ExecError::upstream(
                502,
                self.stream_error
                    .take()
                    .unwrap_or_else(|| "upstream stream closed before message_stop".to_owned()),
            ),
        };
        let error = wrap_fast(self.setup.fast, self.setup.status, error);
        self.reader.report(&error);
        self.pending.push_back(Err(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_releases_large_event_capacity() {
        let response = reqwest::Response::from(http::Response::new(Vec::<u8>::new()));
        let mut state = State {
            body: crate::transport::ResponseBody::new(&response),
            reader: LineReader::new(response),
            setup: StreamSetup {
                translator: None,
                responses: false,
                fast: false,
                status: 200,
                secrets: Secrets::new(),
                web_search: None,
            },
            event: Vec::new(),
            completed: false,
            observation: format!(
                "data: {{\"type\":\"content_block_delta\",\"text\":\"{}\"}}\n\n",
                "x".repeat(2 * OBSERVATION_REUSE_LIMIT)
            )
            .into_bytes(),
            observation_overflow: false,
            stream_error: None,
            pending: VecDeque::new(),
            finished: false,
        };
        assert!(state.observation.len() > OBSERVATION_REUSE_LIMIT);
        assert!(state.observation.capacity() > OBSERVATION_REUSE_LIMIT);
        state.observe_event();
        assert!(!state.completed);
        assert!(state.observation.is_empty());
        assert_eq!(state.observation.capacity(), 0);

        state
            .observation
            .extend_from_slice(b"data: {\"type\":\"message_stop\"}\n\n");
        state.observe_event();
        assert!(state.completed);
        assert!(state.observation.is_empty());
        assert!(state.observation.capacity() <= OBSERVATION_REUSE_LIMIT);
    }

    /// The original event extraction, kept independent of the in-place pass.
    fn reference_payload(raw: &[u8]) -> (Vec<u8>, bool) {
        let data: Vec<_> = raw
            .split(|b| *b == b'\n')
            .filter_map(|line| trim_space(line).strip_prefix(b"data:"))
            .map(trim_space)
            .filter(|data| !data.is_empty())
            .collect();
        let bare = data.is_empty();
        (
            if bare {
                raw.to_vec()
            } else {
                data.join(&b'\n')
            },
            bare,
        )
    }

    #[test]
    fn in_place_observation_matches_original_event_payloads() {
        let mut corpus = vec![
            "".to_owned(),
            "data:".to_owned(),
            "data: \r\n\n".to_owned(),
            "data:{\"type\":\"message_stop\"}\r\n\r\n".to_owned(),
            "event: message_stop\ndata: {\"type\":\ndata: \"message_stop\"}\n\n".to_owned(),
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"busy\"}}\n\n"
                .to_owned(),
            "event: ping\ndata: {\"type\":\"ping\"}\n\n".to_owned(),
            "{\"type\":\"message\",\"stop_reason\":\"end_turn\"}\n".to_owned(),
            "data: {truncated\n\ndata: \n".to_owned(),
            "\u{3000}data: {}\u{a0}\r\ndata:\ndata: []\n".to_owned(),
            format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"text\":\"{}\"}}\n\n",
                "x".repeat(2 << 20)
            ),
        ];
        let tokens = [
            "data:",
            "data: ",
            "event: ping",
            "{}",
            "\n",
            "\r\n",
            " ",
            "null",
            "data: []",
            "\u{a0}",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            let mut event = String::new();
            for _ in 0..20 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                event.push_str(tokens[seed as usize % tokens.len()]);
            }
            corpus.push(event);
        }
        for event in corpus {
            let (want, bare) = reference_payload(event.as_bytes());
            let mut raw = event.into_bytes();
            let (got, got_bare) = event_payload(&mut raw);
            assert_eq!(got_bare, bare);
            assert_eq!(got, want);
        }
    }

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

    async fn forward_chunks(chunks: &[&'static [u8]]) -> Vec<Result<Bytes, ExecError>> {
        let chunks = chunks.to_vec();
        let app = axum::Router::new().fallback(move || {
            let chunks = chunks.clone();
            async move {
                axum::body::Body::from_stream(futures_util::stream::iter(
                    chunks
                        .into_iter()
                        .map(|chunk| Ok::<_, std::io::Error>(Bytes::from_static(chunk))),
                ))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let chunks = forward(
            response,
            StreamSetup {
                translator: None,
                web_search: None,
                responses: false,
                fast: false,
                status: 200,
                secrets: Secrets::new(),
            },
        )
        .collect()
        .await;
        server.abort();
        chunks
    }

    #[tokio::test]
    async fn spots_message_stop() {
        for (body, complete) in [
            (
                &[b" data: {\"type\":\"message_stop\"} ".as_slice()][..],
                true,
            ),
            (&[b"data:{\"type\":\"message_stop\"}".as_slice()][..], true),
            (&[b"event: message_stop".as_slice()][..], false),
            (
                &[b"data: {\"type\":\"message_delta\"}".as_slice()][..],
                false,
            ),
            (&[b"data: message_stop".as_slice()][..], false),
            (
                &[
                    b"event: message_stop\n".as_slice(),
                    b"data: {\"type\":\"message_stop\"}\n".as_slice(),
                    b"\n".as_slice(),
                ][..],
                true,
            ),
        ] {
            let chunks = forward_chunks(body).await;
            assert_eq!(chunks.iter().all(Result::is_ok), complete, "{body:?}");
        }
    }

    #[tokio::test]
    async fn spots_error_events() {
        for (body, is_error) in [
            (b"data: {\"type\":\"error\",\"error\":{}}".as_slice(), true),
            (b"event: error".as_slice(), false),
            (b"data: {nope".as_slice(), false),
            (b"data: {}".as_slice(), false),
        ] {
            let chunks = forward_chunks(&[body]).await;
            let error = chunks
                .iter()
                .find_map(|chunk| chunk.as_ref().err())
                .unwrap();
            assert_eq!(error.status, 502);
            assert_eq!(
                error.message.contains("before message_stop"),
                !is_error,
                "{body:?}"
            );
        }
    }
}
