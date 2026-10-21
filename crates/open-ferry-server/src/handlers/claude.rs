// Ported from ClaudeMessages, ClaudeCountTokens and their response handlers
// in CLIProxyAPI sdk/api/handlers/claude/code_handlers.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1/messages` and `POST /v1/messages/count_tokens`.
//!
//! Upstream also turns model IDs it disguised in its model list back into
//! real ones. The disguise isn't ported, so neither is that.

use std::io::Read;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use open_ferry_core::exec::Format;
use serde_json::Value;

use super::{gjson_string, parse_body};
use crate::body;
use crate::errors::{ErrorMessage, claude_error_json, claude_error_response};
use crate::exec::{Call, ClientRequest, Started};
use crate::state::AppState;
use crate::stream::{
    Peeked, StreamWriter, forward, json_response, keep_alive, peek, pre_first_token,
    pre_first_token_interval, sse_response,
};

/// `POST /v1/messages`. The body goes on as it came, without decoding a
/// `Content-Encoding`.
pub(crate) async fn messages(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let limit = settings.config.body_limit;
    let nonstream_keepalive = settings.config.nonstream_keepalive;
    let keepalive = settings.config.streaming.keepalive;
    let pre_ft = pre_first_token_interval(&settings.config.streaming);
    drop(settings);
    let raw = match body::read_raw(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let parsed = parse_body(&raw);
    // Anything but a missing or `false` `stream` streams, `null` included.
    let streaming = !matches!(parsed.get("stream"), None | Some(Value::Bool(false)));
    let model = gjson_string(parsed.get("model"));

    if !streaming {
        let call = match Call::new(
            &state,
            &client,
            Format::CLAUDE,
            &model,
            raw,
            &client.alt,
            false,
        ) {
            Ok(call) => call,
            Err(error) => return claude_error_response(&error, passthrough),
        };
        return keep_alive(
            nonstream_keepalive,
            call.execute(),
            move |result| match result {
                Ok(reply) => json_response(&reply.headers, gunzip(reply.body)),
                Err(error) => claude_error_response(&error, passthrough),
            },
        )
        .await;
    }

    let call = Call::new(&state, &client, Format::CLAUDE, &model, raw, "", true);
    let commit_headers = crate::stream::CommitHeaders::default();
    let upstream_headers = commit_headers.clone();
    let first = async move {
        let started = match call {
            Ok(call) => call.stream().await,
            Err(error) => Started::failed(error),
        };
        upstream_headers.set(&started.headers);
        (started.headers, peek(started.items).await)
    };
    pre_first_token(
        pre_ft,
        commit_headers,
        first,
        move |(headers, peeked), committed| match peeked {
            Peeked::Failed(error) if committed => {
                let items = stream::iter([Err(error)]).boxed();
                sse_response(&headers, forward(items, ClaudeWriter::default(), None))
            }
            Peeked::Failed(error) => claude_error_response(&error, passthrough),
            Peeked::Closed => sse_response(
                &headers,
                forward(stream::empty().boxed(), ClaudeWriter::default(), None),
            ),
            Peeked::First(first, rest) => {
                let items = stream::iter([Ok(first)]).chain(rest).boxed();
                sse_response(&headers, forward(items, ClaudeWriter::default(), keepalive))
            }
        },
    )
    .await
}

/// `POST /v1/messages/count_tokens`.
pub(crate) async fn count_tokens(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let limit = settings.config.body_limit;
    drop(settings);
    let raw = match body::read_raw(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let model = gjson_string(parse_body(&raw).get("model"));
    let call = match Call::new(
        &state,
        &client,
        Format::CLAUDE,
        &model,
        raw,
        &client.alt,
        false,
    ) {
        Ok(call) => call,
        Err(error) => return claude_error_response(&error, passthrough),
    };
    match call.count_tokens().await {
        Ok(reply) => json_response(&reply.headers, reply.body),
        Err(error) => claude_error_response(&error, passthrough),
    }
}

/// A gzipped body unzipped, or as it is when it isn't gzip or won't unzip.
fn gunzip(body: Bytes) -> Bytes {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return body;
    }
    let mut unzipped = Vec::new();
    match flate2::read::MultiGzDecoder::new(&body[..]).read_to_end(&mut unzipped) {
        Ok(_) => Bytes::from(unzipped),
        Err(err) => {
            tracing::warn!("failed to decompress gzipped Claude response: {err}");
            body
        }
    }
}

/// Writes a Claude event stream: payloads as they are, and an `error`
/// event at the end of one that fails.
struct ClaudeWriter {
    saw_error_event: bool,
    saw_message_stop: bool,
    saw_bare_message: bool,
    trailing_newlines: usize,
    line: Vec<u8>,
    event: String,
    data: Vec<u8>,
}

impl Default for ClaudeWriter {
    fn default() -> Self {
        Self {
            saw_error_event: false,
            saw_message_stop: false,
            saw_bare_message: false,
            trailing_newlines: 2,
            line: Vec::new(),
            event: String::new(),
            data: Vec::new(),
        }
    }
}

impl ClaudeWriter {
    fn observe_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        let line = open_ferry_translate::go::trim_space(&line);
        if line.is_empty() {
            self.flush_event();
            self.event.clear();
        } else if let Some(event) = line.strip_prefix(b"event:") {
            self.event = String::from_utf8_lossy(event).trim().to_owned();
        } else if let Some(data) = line.strip_prefix(b"data:") {
            self.data
                .extend_from_slice(open_ferry_translate::go::trim_space(data));
            self.data.push(b'\n');
        } else {
            self.observe_json(line, false);
        }
    }

    fn observe_json(&mut self, data: &[u8], sse: bool) {
        let Ok(root) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        match root["type"].as_str().unwrap_or_default() {
            "message_stop" => self.saw_message_stop = true,
            "error" => self.saw_error_event = true,
            "message" if !sse && !gjson_string(root.get("stop_reason")).trim().is_empty() => {
                self.saw_message_stop = true;
                self.saw_bare_message = true;
            }
            _ if sse && self.event == "error" && root.get("error").is_some() => {
                self.saw_error_event = true
            }
            _ => {}
        }
    }

    fn flush_event(&mut self) {
        let data = std::mem::take(&mut self.data);
        if !data.is_empty() {
            self.observe_json(&data, true);
        }
    }

    fn flush_pending(&mut self) {
        if !self.line.is_empty() {
            self.observe_line();
        }
        self.flush_event();
    }

    fn boundary(&mut self, out: &mut BytesMut) {
        if !self.saw_bare_message {
            for _ in self.trailing_newlines..2 {
                out.extend_from_slice(b"\n");
            }
            self.trailing_newlines = 2;
        }
    }
}

impl StreamWriter for ClaudeWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        for &byte in &chunk {
            match byte {
                b'\r' => {}
                b'\n' => self.trailing_newlines = (self.trailing_newlines + 1).min(2),
                _ => self.trailing_newlines = 0,
            }
            if byte == b'\n' {
                self.observe_line();
            } else {
                self.line.push(byte);
            }
        }
        out.extend_from_slice(&chunk);
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        self.flush_pending();
        self.boundary(out);
        if self.saw_error_event {
            return;
        }
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(claude_error_json(error).as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    fn close_error(&mut self) -> Option<ErrorMessage> {
        self.flush_pending();
        (!self.saw_error_event && !self.saw_message_stop)
            .then(|| ErrorMessage::new(502, "upstream stream closed before message_stop"))
    }

    fn write_done(&mut self, out: &mut BytesMut) {
        self.flush_pending();
        if self.saw_error_event || self.saw_message_stop {
            self.boundary(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unzips_gzipped_bodies() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"{\"ok\":true}").unwrap();
        let zipped = Bytes::from(encoder.finish().unwrap());
        assert_eq!(&gunzip(zipped)[..], b"{\"ok\":true}");
        assert_eq!(&gunzip(Bytes::from_static(b"{}"))[..], b"{}");
        let broken = Bytes::from_static(&[0x1f, 0x8b, 0, 1, 2]);
        assert_eq!(gunzip(broken.clone()), broken);
    }
}

#[cfg(test)]
mod translated_error_tests {
    use super::*;

    // Ported from TestClaudeMessagesHandlerDeduplicatesTranslatedSSEErrorAndTerminalChunkError.
    #[test]
    fn suppresses_terminal_error_after_translated_error() {
        for newline in ["\n", "\r\n"] {
            let mut writer = ClaudeWriter::default();
            let mut out = BytesMut::new();
            let chunk = Bytes::from(format!(
                "event: error{newline}data: {{\"type\":\"error\"}}{newline}{newline}"
            ));
            writer.write_chunk(chunk.clone(), &mut out);
            writer.write_terminal_error(&ErrorMessage::new(502, "failed"), &mut out);
            assert_eq!(&out[..], &chunk[..]);
        }
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    // TestClaudeStream* completion, chunk splitting, error deduplication and framing.
    #[test]
    fn observes_terminal_events_across_every_chunk_boundary() {
        for (input, complete, errors, suffix) in [
            ("", false, 1, ""),
            ("event: message_stop", false, 1, ""),
            (
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"message_stop\"}}",
                false,
                1,
                "",
            ),
            ("data: {\"type\" : \"message_stop\"}", true, 0, "\n\n"),
            (
                "data: {\"type\":\n\ndata: \"message_stop\"}\n\n",
                false,
                1,
                "",
            ),
            (
                "event: message_stop\r\ndata: {\"type\":\r\ndata: \"message_stop\"}\r\n\r\n",
                true,
                0,
                "",
            ),
            (
                "event: error\ndata: {\"error\":\ndata: {\"message\":\"failed\"}}",
                true,
                1,
                "\n\n",
            ),
            ("event: error\n", false, 1, ""),
            (
                "{\"type\":\"message\",\"stop_reason\":\"end_turn\"}",
                true,
                0,
                "",
            ),
        ] {
            for split in 0..=input.len() {
                let mut writer = ClaudeWriter::default();
                let mut out = BytesMut::new();
                writer.write_chunk(Bytes::copy_from_slice(&input.as_bytes()[..split]), &mut out);
                writer.write_chunk(Bytes::copy_from_slice(&input.as_bytes()[split..]), &mut out);
                let error = writer.close_error();
                assert_eq!(error.is_none(), complete, "{input:?} split {split}");
                match error {
                    Some(error) => writer.write_terminal_error(&error, &mut out),
                    None => writer.write_done(&mut out),
                }
                let output = String::from_utf8(out.to_vec()).unwrap();
                assert_eq!(
                    output.matches("event: error\ndata:").count(),
                    errors,
                    "{output:?}"
                );
                if complete {
                    assert_eq!(output, format!("{input}{suffix}"));
                }
            }
        }
    }

    #[test]
    fn terminal_errors_have_an_event_boundary_and_are_deduplicated() {
        for ending in ["", "\n", "\r\n", "\n\n"] {
            let mut writer = ClaudeWriter::default();
            let mut out = BytesMut::new();
            writer.write_chunk(
                Bytes::from(format!(
                    "data: {{\"type\":\"content_block_delta\"}}{ending}"
                )),
                &mut out,
            );
            writer.write_terminal_error(&ErrorMessage::new(502, "failed"), &mut out);
            assert!(
                String::from_utf8(out.to_vec())
                    .unwrap()
                    .contains("\n\nevent: error\n")
            );
        }
        let mut writer = ClaudeWriter::default();
        let mut out = BytesMut::new();
        writer.write_chunk(
            Bytes::from_static(b"event: error\ndata: {\"type\":\"error\"}"),
            &mut out,
        );
        writer.write_terminal_error(&ErrorMessage::new(502, "failed"), &mut out);
        assert_eq!(&out[..], b"event: error\ndata: {\"type\":\"error\"}\n\n");
    }
}
