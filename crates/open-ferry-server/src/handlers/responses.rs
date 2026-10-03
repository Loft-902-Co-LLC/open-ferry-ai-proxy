// Ported from Responses, Compact, handleNonStreamingResponse,
// handleStreamingResponse, forwardResponsesStream,
// isCodexResponsesClientRequest and logResponsesStreamError in CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_handlers.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1/responses` and `POST /v1/responses/compact`, also served under
//! `/backend-api/codex/`.
//!
//! A stream is held back until its first data event, so a failure before
//! then is a JSON error with its status. After that, the events go through
//! a framer that keeps them whole and ends the stream with one terminal
//! event: the provider's, or a failure the client can read.
//!
//! Deviations from upstream:
//! - Codex multi-agent v2 tools (`prepareCodexMultiAgentV2Tools`) and
//!   orphan delegation (`prepareCodexOrphanDelegation`) aren't ported: the
//!   body goes to the provider as the client sent it.
//! - Errors aren't kept for the request log or usage records
//!   (`LoggingAPIResponseError`). A stream's errors are logged with
//!   `tracing` at debug level, as upstream words them for its request log.
//! - Plugins can't answer for the provider, so an error before the first
//!   event is always sanitized (there is no `DirectResponse`).
//! - The Responses model list (`OpenAIResponsesModels`) isn't routed.
//! - Besides `<`, `>` and `&`, JSON written here doesn't escape U+2028 and
//!   U+2029. Invalid UTF-8 in an event or error becomes U+FFFD as Rust
//!   replaces it, where Go replaces each bad byte.
//! - JSON with an escaped lone surrogate, which Go reads as U+FFFD, is
//!   taken for text that isn't JSON. An error's text that is such an object,
//!   or one nested more than 128 deep, is reported as its status's text
//!   alone, as it can't be redacted field by field.
//! - A `sequence_number` in an error's text that isn't JSON is read only
//!   from a well-formed object after the first `{`; gjson reads what it can
//!   from a malformed one too.

mod framer;
mod stream_error;
#[cfg(test)]
mod tests;

use std::future::ready;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::HeaderMap;
use open_ferry_core::exec::Format;
use open_ferry_translate::go;
use serde_json::Value;

use self::framer::Framer;
use self::stream_error::{error_chunk, failed_chunk, sanitize_error, stream_error_text};
use super::{gjson_string, parse_body};
use crate::body;
use crate::errors::{ErrorMessage, local_error, openai_error_response};
use crate::exec::{Call, ClientRequest, Started};
use crate::json;
use crate::state::AppState;
use crate::stream::{StreamWriter, forward, json_response, keep_alive, sse_response};

/// `POST /v1/responses` (`Responses`): a stream when `stream` is `true`.
pub(crate) async fn responses(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let limit = state.settings().config.body_limit;
    let raw = match body::read_decoded(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let parsed = parse_body(&raw);
    let model = gjson_string(parsed.get("model"));
    if parsed.get("stream") == Some(&Value::Bool(true)) {
        respond_streaming(&state, &client, &model, raw).await
    } else {
        respond_once(&state, &client, &model, raw, "").await
    }
}

/// `POST /v1/responses/compact` (`Compact`): never a stream. A `stream`
/// field that isn't `true` is taken out of the body.
pub(crate) async fn compact(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let limit = state.settings().config.body_limit;
    let mut raw = match body::read_decoded(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let parsed = parse_body(&raw);
    let stream = parsed.get("stream");
    if stream == Some(&Value::Bool(true)) {
        return local_error(
            400,
            "Streaming not supported for compact responses",
            "invalid_request_error",
        );
    }
    if stream.is_some()
        && let Some(updated) = json::try_delete(&raw, "stream")
    {
        raw = Bytes::from(updated);
    }
    let model = gjson_string(parsed.get("model"));
    respond_once(&state, &client, &model, raw, "responses/compact").await
}

/// Makes a non-streaming call and answers with its body, keeping the
/// connection alive as configured (`handleNonStreamingResponse`).
async fn respond_once(
    state: &AppState,
    client: &ClientRequest,
    model: &str,
    payload: Bytes,
    alt: &str,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let interval = settings.config.nonstream_keepalive;
    drop(settings);
    let call = match Call::new(
        state,
        client,
        Format::OPENAI_RESPONSE,
        model,
        payload,
        alt,
        false,
    ) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, passthrough),
    };
    keep_alive(interval, call.execute(), move |result| match result {
        Ok(reply) => json_response(&reply.headers, reply.body),
        Err(error) => openai_error_response(&error, passthrough),
    })
    .await
}

/// Makes a streaming call and answers with its events, or with a JSON error
/// when it fails before its first data event (`handleStreamingResponse`).
async fn respond_streaming(
    state: &AppState,
    client: &ClientRequest,
    model: &str,
    payload: Bytes,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let keepalive = settings.config.streaming.keepalive;
    drop(settings);
    let Started { headers, mut items } = match Call::new(
        state,
        client,
        Format::OPENAI_RESPONSE,
        model,
        payload,
        "",
        true,
    ) {
        Ok(call) => call.stream().await,
        Err(error) => Started::failed(error),
    };
    let mut framer = Framer::new(is_codex_client(&client.headers));
    let mut initial = BytesMut::new();
    loop {
        match items.next().await {
            Some(Ok(chunk)) => {
                framer.write_chunk(&mut initial, &chunk);
                if framer.data_frames == 0 {
                    continue;
                }
                if let Some(error) = &framer.terminal_error {
                    log_stream_error(&framer, error);
                    return sse_response(&headers, Body::from(initial.freeze()));
                }
                let rest = forward(items, ResponsesWriter::new(framer), keepalive);
                return sse_response(&headers, prepend(initial.freeze(), rest));
            }
            Some(Err(error)) => {
                framer.flush(&mut initial);
                let error = sanitize_error(error);
                if framer.data_frames == 0 {
                    return openai_error_response(&error, passthrough);
                }
                let mut writer = ResponsesWriter::new(framer);
                let error = writer.normalize_terminal_error(error);
                writer.write_terminal_error(&error, &mut initial);
                return sse_response(&headers, Body::from(initial.freeze()));
            }
            None => {
                framer.flush(&mut initial);
                if framer.data_frames == 0 {
                    let error = sanitize_error(ErrorMessage::new(
                        502,
                        "upstream stream closed before first payload",
                    ));
                    return openai_error_response(&error, passthrough);
                }
                if let Some(error) = &framer.terminal_error {
                    log_stream_error(&framer, error);
                } else if framer.terminal_event.is_empty() {
                    let error = sanitize_error(ErrorMessage::new(
                        502,
                        "upstream stream closed before a terminal event",
                    ));
                    let mut writer = ResponsesWriter::new(framer);
                    let error = writer.normalize_terminal_error(error);
                    writer.write_terminal_error(&error, &mut initial);
                }
                return sse_response(&headers, Body::from(initial.freeze()));
            }
        }
    }
}

/// `first`, then `rest`.
fn prepend(first: Bytes, rest: Body) -> Body {
    Body::from_stream(
        stream::once(ready(Ok::<_, axum::Error>(first))).chain(rest.into_data_stream()),
    )
}

/// Whether the request comes from an official Codex client, which gets
/// `response.failed` rather than `error` for a failure
/// (`isCodexResponsesClientRequest`).
fn is_codex_client(headers: &HeaderMap) -> bool {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .unwrap_or_default()
    };
    let user_agent = header("user-agent");
    let user_agent = user_agent.trim();
    if [
        "Codex Desktop/",
        "codex-tui/",
        "codex_cli_rs/",
        "codex_exec/",
    ]
    .iter()
    .any(|prefix| user_agent.starts_with(prefix))
        || user_agent == "codex_cli_rs"
    {
        return true;
    }
    let originator = go::to_lower(header("originator").trim());
    matches!(
        originator.as_str(),
        "codex desktop" | "codex-tui" | "codex_cli_rs"
    ) || ["codex desktop/", "codex-tui/", "codex_cli_rs/"]
        .iter()
        .any(|prefix| originator.starts_with(prefix))
}

/// The status and text a stream error is logged with
/// (`logResponsesStreamError`).
fn stream_error_diagnostic(framer: &Framer, error: &ErrorMessage) -> (u16, String) {
    let status = match error.status {
        status @ 400..=599 => status,
        _ => 500,
    };
    let last_event = match framer.last_event.as_str() {
        "" => "none",
        last_event => last_event,
    };
    let text = stream_error_text(error, status);
    (
        status,
        format!("responses stream terminated after {last_event}: {text}"),
    )
}

/// Logs a stream error at debug level.
fn log_stream_error(framer: &Framer, error: &ErrorMessage) {
    let (status, text) = stream_error_diagnostic(framer, error);
    tracing::debug!(status, "{text}");
}

/// Writes a Responses stream once it has started (the options
/// `forwardResponsesStream` gives `ForwardStream`).
struct ResponsesWriter {
    framer: Framer,
    /// What [`StreamWriter::close_error`] flushed, written next.
    flushed: BytesMut,
}

impl ResponsesWriter {
    fn new(framer: Framer) -> Self {
        Self {
            framer,
            flushed: BytesMut::new(),
        }
    }

    /// Writes what was flushed, then flushes the framer.
    fn flush(&mut self, out: &mut BytesMut) {
        out.extend_from_slice(&self.flushed.split());
        self.framer.flush(out);
    }
}

impl StreamWriter for ResponsesWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        self.framer.write_chunk(out, &chunk);
    }

    fn chunk_error(&mut self) -> Option<ErrorMessage> {
        let error = self.framer.terminal_error.clone()?;
        log_stream_error(&self.framer, &error);
        Some(error)
    }

    fn normalize_terminal_error(&mut self, error: ErrorMessage) -> ErrorMessage {
        sanitize_error(error)
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        self.flush(out);
        let status = error.http_status();
        let err_text = stream_error_text(error, status);
        log_stream_error(&self.framer, error);
        if !self.framer.terminal_event.is_empty() {
            return;
        }
        let sequence = json::find(err_text.as_bytes(), "sequence_number")
            .map_or(self.framer.data_frames, |sequence| sequence.int());
        let event = if self.framer.is_codex_client {
            let chunk = failed_chunk(status, &err_text, sequence);
            format!("\nevent: response.failed\ndata: {chunk}\n\n")
        } else {
            let chunk = error_chunk(status, &err_text, sequence);
            format!("\nevent: error\ndata: {chunk}\n\n")
        };
        out.extend_from_slice(event.as_bytes());
    }

    fn close_error(&mut self) -> Option<ErrorMessage> {
        self.framer.flush(&mut self.flushed);
        if let Some(error) = &self.framer.terminal_error {
            return Some(error.clone());
        }
        if !self.framer.terminal_event.is_empty() {
            return None;
        }
        let last_event = match self.framer.last_event.as_str() {
            "" => "none",
            last_event => last_event,
        };
        Some(ErrorMessage::new(
            502,
            format!("upstream stream closed before a terminal event (last event: {last_event})"),
        ))
    }

    fn write_done(&mut self, out: &mut BytesMut) {
        self.flush(out);
        out.extend_from_slice(b"\n");
    }
}
