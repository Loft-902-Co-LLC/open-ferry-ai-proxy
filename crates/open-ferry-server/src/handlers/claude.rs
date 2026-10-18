// Ported from ClaudeMessages, ClaudeCountTokens and their response handlers
// in CLIProxyAPI sdk/api/handlers/claude/code_handlers.go (v8.0.27, MIT).
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
            Peeked::Closed => sse_response(&headers, Body::empty()),
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
#[derive(Default)]
struct ClaudeWriter {
    saw_error_event: bool,
}

impl StreamWriter for ClaudeWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        self.saw_error_event |= chunk
            .windows(b"event: error\n".len())
            .any(|w| w == b"event: error\n")
            || chunk
                .windows(b"event: error\r\n".len())
                .any(|w| w == b"event: error\r\n");
        out.extend_from_slice(&chunk);
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        if self.saw_error_event {
            return;
        }
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(claude_error_json(error).as_bytes());
        out.extend_from_slice(b"\n\n");
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
            let chunk = Bytes::from(format!("event: error{newline}data: {{}}{newline}{newline}"));
            writer.write_chunk(chunk.clone(), &mut out);
            writer.write_terminal_error(&ErrorMessage::new(502, "failed"), &mut out);
            assert_eq!(&out[..], &chunk[..]);
        }
    }
}
