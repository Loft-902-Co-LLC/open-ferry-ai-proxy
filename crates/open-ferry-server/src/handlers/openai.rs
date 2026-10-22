// Ported from ChatCompletions, Completions, their response handlers and
// chunkHasFinishReason in CLIProxyAPI
// sdk/api/handlers/openai/openai_handlers.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1/chat/completions` and `POST /v1/completions`.
//!
//! A stream that ends without any chunk carrying a `finish_reason` was cut
//! short: it ends with an error, not `[DONE]`, so the client can tell.

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use open_ferry_core::exec::Format;
use open_ferry_translate::completions::{
    convert_chat_completions_response_to_completions,
    convert_chat_completions_stream_chunk_to_completions,
    convert_completions_request_to_chat_completions,
};
use open_ferry_translate::go;
use open_ferry_translate::openai::responses::convert_openai_responses_request_to_openai_chat_completions;
use serde_json::Value;

use super::{gjson_string, parse_body};
use crate::body;
use crate::errors::{ErrorMessage, openai_body, openai_error_response};
use crate::exec::{Call, ClientRequest, Started};
use crate::json;
use crate::state::AppState;
use crate::stream::{Peeked, StreamWriter, forward, json_response, keep_alive, peek, sse_response};

/// `POST /v1/chat/completions`. A body in the Responses format, with
/// `input` or `instructions` and no `messages`, is converted first.
pub(crate) async fn chat_completions(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let limit = crate::ingress::body_limit(state.settings().config.body_limit);
    let raw = match body::read_decoded(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let mut parsed = parse_body(&raw);
    let mut payload = raw;
    let mut stream = parsed.get("stream") == Some(&Value::Bool(true));
    if is_responses_format(&parsed) {
        let model = gjson_string(parsed.get("model"));
        // As upstream, a refusal is not acted on here: the Chat Completions
        // body goes on, and the provider's translator judges it.
        (parsed, _) =
            convert_openai_responses_request_to_openai_chat_completions(&model, &parsed, stream);
        stream = parsed.get("stream").is_some_and(gjson_bool);
        payload = Bytes::from(parsed.to_string());
    }
    let model = gjson_string(parsed.get("model"));
    let alt = client.alt.clone();
    if stream {
        let call = Call::new(&state, &client, Format::OPENAI, &model, payload, &alt, true);
        let start = async move {
            match call {
                Ok(call) => call.stream().await,
                Err(error) => Started::failed(error),
            }
        };
        stream_chat(&state, start, Some).await
    } else {
        respond_once(&state, &client, &model, payload, &alt, Ok).await
    }
}

/// `POST /v1/completions`: converted to and from Chat Completions.
pub(crate) async fn completions(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let limit = crate::ingress::body_limit(state.settings().config.body_limit);
    let raw = match body::read_decoded(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    // The call below carries the converted body, which has nothing of the
    // depth the client's had.
    if let Err(error) = body::check_depth(&raw) {
        return openai_error_response(&error, false);
    }
    let parsed = parse_body(&raw);
    let stream = parsed.get("stream") == Some(&Value::Bool(true));
    let chat = convert_completions_request_to_chat_completions(&parsed);
    let model = gjson_string(chat.get("model"));
    let payload = Bytes::from(chat.to_string());
    if stream {
        let call = Call::new(&state, &client, Format::OPENAI, &model, payload, "", true);
        let start = async move {
            match call {
                Ok(call) => call.stream().await,
                Err(error) => Started::failed(error),
            }
        };
        stream_chat(&state, start, |chunk| {
            convert_chat_completions_stream_chunk_to_completions(&chunk)
                .map(|converted| Bytes::from(converted.to_string()))
        })
        .await
    } else {
        respond_once(&state, &client, &model, payload, "", |body| {
            Ok(Bytes::from(
                convert_chat_completions_response_to_completions(&body).to_string(),
            ))
        })
        .await
    }
}

/// Whether a Chat Completions body is really a Responses request
/// (`shouldTreatAsResponsesFormat`).
fn is_responses_format(body: &Value) -> bool {
    if body.get("messages").is_some() {
        return false;
    }
    body.get("input").is_some() || body.get("instructions").is_some()
}

/// gjson's `Bool()`: `true`, a string `strconv.ParseBool` reads as true, or
/// a number other than zero.
fn gjson_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// Makes a non-streaming call and answers with its body, rewritten by
/// `rewrite`, keeping the connection alive as configured.
async fn respond_once(
    state: &AppState,
    client: &ClientRequest,
    model: &str,
    payload: Bytes,
    alt: &str,
    rewrite: impl FnOnce(Bytes) -> Result<Bytes, ErrorMessage> + Send + 'static,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let interval = settings.config.nonstream_keepalive;
    drop(settings);
    let call = match Call::new(state, client, Format::OPENAI, model, payload, alt, false) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, passthrough),
    };
    keep_alive(interval, call.execute(), move |result| {
        match result.and_then(|reply| Ok((rewrite(reply.body)?, reply.headers))) {
            Ok((body, headers)) => json_response(&headers, body),
            Err(error) => openai_error_response(&error, passthrough),
        }
    })
    .await
}

/// Answers with a Chat Completions event stream, each payload rewritten by
/// `rewrite`, which drops a payload by giving `None`.
async fn stream_chat(
    state: &AppState,
    start: impl std::future::Future<Output = Started> + Send + 'static,
    rewrite: impl FnMut(Bytes) -> Option<Bytes> + Send + 'static,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let keepalive = settings.config.streaming.keepalive;
    let pre_ft = crate::stream::pre_first_token_interval(&settings.config.streaming);
    drop(settings);
    let commit_headers = crate::stream::CommitHeaders::default();
    let upstream_headers = commit_headers.clone();
    let first = async move {
        let started = start.await;
        upstream_headers.set(&started.headers);
        (started.headers, peek(started.items).await)
    };
    crate::stream::pre_first_token(
        pre_ft,
        commit_headers,
        first,
        move |(headers, peeked), committed| match peeked {
            Peeked::Failed(error) if committed => {
                let items = stream::iter([Err(error)]).boxed();
                sse_response(&headers, forward(items, ChatWriter::default(), None))
            }
            Peeked::Failed(error) => openai_error_response(&error, passthrough),
            Peeked::Closed => sse_response(&headers, Body::from("data: [DONE]\n\n")),
            Peeked::First(first, rest) => {
                let mut rewrite = rewrite;
                let items = stream::iter([Ok(first)])
                    .chain(rest)
                    .filter_map(move |item| {
                        let item = match item {
                            Ok(chunk) => rewrite(chunk).map(Ok),
                            Err(error) => Some(Err(error)),
                        };
                        std::future::ready(item)
                    })
                    .boxed();
                let writer = ChatWriter::default();
                sse_response(&headers, forward(items, writer, keepalive))
            }
        },
    )
    .await
}

/// Writes a Chat Completions event stream.
#[derive(Default)]
struct ChatWriter {
    /// Whether a chunk written has carried a `finish_reason`.
    saw_finish_reason: bool,
    terminal_chunk_error: Option<ErrorMessage>,
}

impl StreamWriter for ChatWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        if let Some(error) = chunk_terminal_error(&chunk) {
            self.terminal_chunk_error = Some(error);
            self.saw_finish_reason = true;
        } else if !self.saw_finish_reason && chunk_has_finish_reason(&chunk) {
            self.saw_finish_reason = true;
        }
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&chunk);
        out.extend_from_slice(b"\n\n");
    }

    fn chunk_error(&mut self) -> Option<ErrorMessage> {
        self.terminal_chunk_error.clone()
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        if self.terminal_chunk_error.is_some() {
            return;
        }
        let body = openai_body(error.http_status(), &error.text, false);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    fn close_error(&mut self) -> Option<ErrorMessage> {
        (!self.saw_finish_reason).then(|| {
            ErrorMessage::new(
                502,
                "upstream stream closed before any chunk carried finish_reason",
            )
        })
    }

    fn write_done(&mut self, out: &mut BytesMut) {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
}

fn chunk_terminal_error(chunk: &[u8]) -> Option<ErrorMessage> {
    let mut chunk = go::trim_space(chunk);
    if let Some(rest) = chunk.strip_prefix(b"data:") {
        chunk = go::trim_space(rest);
    }
    if json::get(chunk, "choices").is_some()
        || !json::get(chunk, "error").is_some_and(|error| error.is_object())
    {
        return None;
    }
    let message = json::get(chunk, "error.message")
        .map(|value| value.str())
        .unwrap_or_default();
    let message = if message.is_empty() {
        String::from_utf8_lossy(chunk).into_owned()
    } else {
        message
    };
    Some(ErrorMessage::new(400, message))
}

/// Whether a chunk, with or without its `data:` prefix, has a choice whose
/// `finish_reason` is there and neither `null` nor empty
/// (`chunkHasFinishReason`).
fn chunk_has_finish_reason(chunk: &[u8]) -> bool {
    if chunk_terminal_error(chunk).is_some() {
        return true;
    }
    let mut chunk = go::trim_space(chunk);
    if let Some(rest) = chunk.strip_prefix(b"data:") {
        chunk = go::trim_space(rest);
    }
    json::get(chunk, "choices").is_some_and(|choices| {
        choices.array().iter().any(|choice| {
            choice
                .get("finish_reason")
                .is_some_and(|reason| !reason.is_null() && !reason.str().is_empty())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spots_responses_bodies() {
        assert!(is_responses_format(&json!({"input": "hi"})));
        assert!(is_responses_format(&json!({"instructions": null})));
        assert!(!is_responses_format(
            &json!({"input": "hi", "messages": []})
        ));
        assert!(!is_responses_format(&json!({"model": "m"})));
        assert!(!is_responses_format(&Value::Null));
    }

    // Not upstream's: chunkHasFinishReason on its own.
    #[test]
    fn spots_a_finish_reason() {
        for chunk in [
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
            r#" data: {"choices":[{"index":0},{"finish_reason":"length"}]} "#,
            r#"{"choices":{"finish_reason":"stop"}}"#,
            r#"{"choices":[{"finish_reason":false}]}"#,
        ] {
            assert!(chunk_has_finish_reason(chunk.as_bytes()), "{chunk}");
        }
        for chunk in [
            r#"{"choices":[{"finish_reason":null}]}"#,
            r#"{"choices":[{"finish_reason":""}]}"#,
            r#"{"choices":[]}"#,
            r#"{"finish_reason":"stop"}"#,
            "data: [DONE]",
            "",
        ] {
            assert!(!chunk_has_finish_reason(chunk.as_bytes()), "{chunk}");
        }
    }

    #[test]
    fn reads_booleans_as_gjson_does() {
        for value in [json!(true), json!("t"), json!("1"), json!(2), json!(-0.5)] {
            assert!(gjson_bool(&value), "{value}");
        }
        for value in [json!(false), json!("yes"), json!(0), json!(null), json!({})] {
            assert!(!gjson_bool(&value), "{value}");
        }
    }
}

#[cfg(test)]
mod stop_reason_tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn terminal_error_first_or_after_content_stops_without_duplicate_error_or_done() {
        for content in [false, true] {
            let mut items = Vec::new();
            if content {
                items.push(Ok(Bytes::from_static(
                    br#"{"choices":[{"delta":{"content":"Partial"},"finish_reason":null}]}"#,
                )));
            }
            items.push(Ok(Bytes::from_static(br#"{"error":{"message":"context window filled","type":"invalid_request_error","code":"context_length_exceeded"}}"#)));
            items.push(Err(ErrorMessage::new(
                502,
                "upstream stream closed before message_stop",
            )));
            let body = forward(stream::iter(items).boxed(), ChatWriter::default(), None)
                .collect()
                .await
                .unwrap()
                .to_bytes();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(body.matches("\"error\":").count(), 1);
            assert!(body.contains("context_length_exceeded"));
            assert!(!body.contains("[DONE]"));
            assert!(!body.contains("upstream stream closed"));
        }
        assert!(chunk_has_finish_reason(
            br#"data: {"error":{"message":"context"}}"#
        ));
        assert!(chunk_terminal_error(br#"{"choices":[],"error":{"message":"context"}}"#).is_none());
    }
}
