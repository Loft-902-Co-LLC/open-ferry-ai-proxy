// Ported from ChatCompletions, Completions and their response handlers in
// CLIProxyAPI sdk/api/handlers/openai/openai_handlers.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1/chat/completions` and `POST /v1/completions`.

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
use open_ferry_translate::openai::responses::convert_openai_responses_request_to_openai_chat_completions;
use serde_json::Value;

use super::{gjson_string, parse_body};
use crate::body;
use crate::errors::{ErrorMessage, openai_body, openai_error_response};
use crate::exec::{Call, ClientRequest, HandlerStream};
use crate::state::AppState;
use crate::stream::{Peeked, StreamWriter, forward, json_response, keep_alive, peek, sse_response};

/// `POST /v1/chat/completions`. A body in the Responses format, with
/// `input` or `instructions` and no `messages`, is converted first.
pub(crate) async fn chat_completions(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let limit = state.settings().config.body_limit;
    let raw = match body::read_decoded(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let mut parsed = parse_body(&raw);
    let mut payload = raw;
    let mut stream = parsed.get("stream") == Some(&Value::Bool(true));
    if is_responses_format(&parsed) {
        let model = gjson_string(parsed.get("model"));
        parsed =
            convert_openai_responses_request_to_openai_chat_completions(&model, &parsed, stream);
        stream = parsed.get("stream").is_some_and(gjson_bool);
        payload = Bytes::from(parsed.to_string());
    }
    let model = gjson_string(parsed.get("model"));
    let alt = client.alt.clone();
    if stream {
        let started = match Call::new(&state, &client, Format::OPENAI, &model, payload, &alt, true)
        {
            Ok(call) => call.stream().await,
            Err(error) => crate::exec::Started::failed(error),
        };
        stream_chat(&state, started.headers, started.items, Some).await
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
    let limit = state.settings().config.body_limit;
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
        let started = match Call::new(&state, &client, Format::OPENAI, &model, payload, "", true) {
            Ok(call) => call.stream().await,
            Err(error) => crate::exec::Started::failed(error),
        };
        stream_chat(&state, started.headers, started.items, |chunk| {
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
    headers: http::HeaderMap,
    items: HandlerStream,
    rewrite: impl FnMut(Bytes) -> Option<Bytes> + Send + 'static,
) -> Response {
    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let keepalive = settings.config.streaming.keepalive;
    drop(settings);
    match peek(items).await {
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
            sse_response(&headers, forward(items, ChatWriter, keepalive))
        }
    }
}

/// Writes a Chat Completions event stream.
struct ChatWriter;

impl StreamWriter for ChatWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&chunk);
        out.extend_from_slice(b"\n\n");
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        let body = openai_body(error.http_status(), &error.text, false);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    fn write_done(&mut self, out: &mut BytesMut) {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
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
