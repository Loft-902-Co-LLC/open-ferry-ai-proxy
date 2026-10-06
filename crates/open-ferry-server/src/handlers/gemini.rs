// Ported from GeminiModels, GeminiGetHandler, GeminiHandler,
// handleStreamGenerateContent, handleCountTokens, handleGenerateContent and
// forwardGeminiStream in CLIProxyAPI sdk/api/handlers/gemini/gemini_handlers.go,
// and the "gemini" case of convertModelToMap in
// internal/registry/model_registry.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini API: `GET /v1beta/models`, `GET /v1beta/models/<name>` and
//! `POST /v1beta/models/<model>:<method>`.
//!
//! The action after `/v1beta/models/` is read from the path percent-decoded,
//! as gin reads it. A POST action splits on `:` into the model and the
//! method, which is `generateContent`, `streamGenerateContent` or
//! `countTokens`; any other method gets an empty 200, since upstream writes
//! nothing for one. Bodies go on as they came, and errors before a response
//! starts are OpenAI error bodies, as for the other routes.
//!
//! A stream is SSE, a `data: ` line per payload, unless the client asks for
//! another `alt`. Then the payloads are written as they come, with no
//! keep-alives, and with the `Content-Type` Go's server sniffs from the
//! first one, since upstream sets none. A failure partway through ends the
//! stream with its OpenAI error body, after `event: error` in SSE.
//!
//! JSON this module builds escapes `<`, `>`, `&`, U+2028 and U+2029 as Go's
//! encoder does, and sorts object keys as Go does for a map.
//!
//! Deviations from upstream:
//! - The model list is sorted by ID, and where two models have the same
//!   name, `GET /v1beta/models/<name>` finds the first by ID. Upstream's
//!   order varies from call to call.
//! - `POST /v1beta/models` gets 404, where gin redirects it to
//!   `/v1beta/models/` with a 307.
//! - A `%` that doesn't start a valid escape is read as it is. Go's server
//!   answers a path holding one with 400.
//! - A model name that isn't valid UTF-8 once decoded has each bad sequence
//!   replaced with U+FFFD.
//! - A body over the configured limit gets 413, and one that fails to read
//!   gets 400. Upstream reads any size and ignores read errors.
//! - The 400 upstream gives when the route has no action isn't ported: the
//!   router always has one.
//! - Home mode's model list (`handleHomeGeminiModels`) isn't ported.
//!   `POST /v1beta/interactions` is served by [`super::interactions`].

pub(crate) mod sniff;

#[cfg(test)]
mod tests;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, Uri, header};
use open_ferry_core::exec::Format;
use open_ferry_core::models::ModelInfo;
use serde_json::{Map, Value};

use super::json_utf8;
use crate::body;
use crate::errors::{
    ErrorMessage, JSON_UTF8, error_response, local_error, openai_body, openai_error_response,
};
use crate::exec::{Call, ClientRequest, Started};
use crate::headers::write_upstream_headers;
use crate::json::json_string;
use crate::state::AppState;
use crate::stream::{Peeked, StreamWriter, forward, json_response, keep_alive, peek, sse_response};

/// The path the model routes are under.
const MODELS_PATH: &str = "/v1beta/models";

/// The route a call's metadata names, as gin's `FullPath` gives it.
const ACTION_ROUTE: &str = "/v1beta/models/*action";

/// The prefix of a Gemini model name.
const NAME_PREFIX: &str = "models/";

/// `GET /v1beta/models`: the available models in Gemini's format, each
/// named `models/<name>`, with its name as the display name and description
/// when it has none, and `generateContent` as its method when it lists none.
pub(crate) async fn models(State(state): State<AppState>) -> Response {
    let models: Vec<Value> = sorted_models(&state)
        .iter()
        .map(|model| Value::Object(normalized(gemini_map(model))))
        .collect();
    let mut list = Map::new();
    list.insert("models".into(), Value::Array(models));
    json_utf8(marshal(&Value::Object(list)))
}

/// `GET /v1beta/models/<name>`: the model named `<name>` or
/// `models/<name>`, with its name given the `models/` prefix, or a 404.
pub(crate) async fn model(State(state): State<AppState>, uri: Uri) -> Response {
    let path = decode_path(uri.path());
    let action = action_of(&path);
    for model in sorted_models(&state) {
        let mut entry = gemini_map(&model);
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let bare = name.strip_prefix(NAME_PREFIX);
        if name.as_bytes() != action && bare.map(str::as_bytes) != Some(action) {
            continue;
        }
        if !name.is_empty() && bare.is_none() {
            entry.insert("name".into(), format!("{NAME_PREFIX}{name}").into());
        }
        return json_utf8(marshal(&Value::Object(entry)));
    }
    local_error(404, "Not Found", "not_found")
}

/// What a POST action asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Generate,
    Stream,
    CountTokens,
}

/// `POST /v1beta/models/<model>:<method>`.
pub(crate) async fn action(
    State(state): State<AppState>,
    mut client: ClientRequest,
    uri: Uri,
    body: Body,
) -> Response {
    let path = decode_path(uri.path());
    let parts: Vec<&[u8]> = action_of(&path).split(|&b| b == b':').collect();
    let [model, method] = parts[..] else {
        return unknown_action(&path);
    };
    let method = match method {
        b"generateContent" => Method::Generate,
        b"streamGenerateContent" => Method::Stream,
        b"countTokens" => Method::CountTokens,
        // Upstream writes nothing, which gin sends as an empty 200.
        _ => return Response::new(Body::empty()),
    };
    let model = String::from_utf8_lossy(model).into_owned();

    let settings = state.settings();
    let passthrough = settings.config.passthrough_headers;
    let limit = settings.config.body_limit;
    let nonstream_keepalive = settings.config.nonstream_keepalive;
    let keepalive = settings.config.streaming.keepalive;
    drop(settings);
    let raw = match body::read_raw(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    client.path = ACTION_ROUTE.to_owned();
    let alt = client.alt.clone();

    if method == Method::Stream {
        let started = match Call::new(&state, &client, Format::GEMINI, &model, raw, &alt, true) {
            Ok(call) => call.stream().await,
            Err(error) => Started::failed(error),
        };
        let sse = alt.is_empty();
        return match peek(started.items).await {
            Peeked::Failed(error) => openai_error_response(&error, passthrough),
            Peeked::Closed if sse => sse_response(&started.headers, Body::empty()),
            Peeked::Closed => raw_response(&started.headers, None, Body::empty()),
            Peeked::First(first, rest) if sse => {
                let items = stream::iter([Ok(first)]).chain(rest).boxed();
                let body = forward(items, GeminiWriter { sse }, keepalive);
                sse_response(&started.headers, body)
            }
            Peeked::First(first, rest) => {
                let sniffed = sniff::detect_content_type(&first);
                let items = stream::iter([Ok(first)]).chain(rest).boxed();
                let body = forward(items, GeminiWriter { sse }, None);
                raw_response(&started.headers, Some(sniffed), body)
            }
        };
    }

    let call = match Call::new(&state, &client, Format::GEMINI, &model, raw, &alt, false) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, passthrough),
    };
    if method == Method::CountTokens {
        return match call.count_tokens().await {
            Ok(reply) => json_response(&reply.headers, reply.body),
            Err(error) => openai_error_response(&error, passthrough),
        };
    }
    keep_alive(
        nonstream_keepalive,
        call.execute(),
        move |result| match result {
            Ok(reply) => json_response(&reply.headers, reply.body),
            Err(error) => openai_error_response(&error, passthrough),
        },
    )
    .await
}

/// The available models, sorted by ID.
fn sorted_models(state: &AppState) -> Vec<ModelInfo> {
    let mut models = state.catalog().available_models();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// A model as upstream's registry describes it for Gemini clients: its name
/// (or ID when it has none), and the other Gemini fields it has.
fn gemini_map(model: &ModelInfo) -> Map<String, Value> {
    let mut entry = Map::new();
    let name = if model.name.is_empty() {
        &model.id
    } else {
        &model.name
    };
    entry.insert("name".into(), name.clone().into());
    for (key, value) in [
        ("version", &model.version),
        ("displayName", &model.display_name),
        ("description", &model.description),
    ] {
        if !value.is_empty() {
            entry.insert(key.into(), value.clone().into());
        }
    }
    for (key, value) in [
        ("inputTokenLimit", model.input_token_limit),
        ("outputTokenLimit", model.output_token_limit),
    ] {
        if value > 0 {
            entry.insert(key.into(), value.into());
        }
    }
    for (key, value) in [
        (
            "supportedGenerationMethods",
            &model.supported_generation_methods,
        ),
        (
            "supportedInputModalities",
            &model.supported_input_modalities,
        ),
        (
            "supportedOutputModalities",
            &model.supported_output_modalities,
        ),
    ] {
        if !value.is_empty() {
            entry.insert(key.into(), value.clone().into());
        }
    }
    entry
}

/// A model list entry as `GeminiModels` fills it in.
fn normalized(mut entry: Map<String, Value>) -> Map<String, Value> {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !name.is_empty() {
        if !name.starts_with(NAME_PREFIX) {
            entry.insert("name".into(), format!("{NAME_PREFIX}{name}").into());
        }
        for key in ["displayName", "description"] {
            if entry.get(key).and_then(Value::as_str).unwrap_or_default() == "" {
                entry.insert(key.into(), name.clone().into());
            }
        }
    }
    if !entry.contains_key("supportedGenerationMethods") {
        entry.insert(
            "supportedGenerationMethods".into(),
            Value::Array(vec!["generateContent".into()]),
        );
    }
    entry
}

/// `value` as Go's `json.Marshal` writes it: object keys sorted, as for a
/// map, and strings escaped as Go escapes them.
fn marshal(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Object(fields) => {
            let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (n, (key, value)) in entries.into_iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(key));
                out.push(':');
                write_value(out, value);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::String(s) => out.push_str(&json_string(s)),
        other => out.push_str(&other.to_string()),
    }
}

/// Go's `json.Marshal` of a string holding `bytes`, which writes each byte
/// that isn't part of valid UTF-8 as an escaped U+FFFD.
fn go_string(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    let mut rest = bytes;
    loop {
        let (valid, bad) = match std::str::from_utf8(rest) {
            Ok(valid) => (valid, None),
            Err(err) => {
                let (valid, bad) = rest.split_at(err.valid_up_to());
                (std::str::from_utf8(valid).unwrap_or_default(), Some(bad))
            }
        };
        let quoted = json_string(valid);
        out.push_str(&quoted[1..quoted.len() - 1]);
        let Some(bad) = bad else { break };
        out.push('\\');
        out.push_str("ufffd");
        rest = &bad[1..];
    }
    out.push('"');
    out
}

/// A request path percent-decoded, as Go's `url.URL.Path` holds it. A `%`
/// that doesn't start a valid escape is kept as it is.
fn decode_path(path: &str) -> Vec<u8> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The value of a hex digit.
fn hex(b: u8) -> Option<u8> {
    char::from(b)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// The action in a decoded path: what follows `/v1beta/models/`.
fn action_of(path: &[u8]) -> &[u8] {
    let rest = path.strip_prefix(MODELS_PATH.as_bytes()).unwrap_or(path);
    rest.strip_prefix(b"/").unwrap_or(rest)
}

/// Upstream's 404 for an action that isn't `<model>:<method>`, naming the
/// decoded path.
fn unknown_action(path: &[u8]) -> Response {
    let mut message = path.to_vec();
    message.extend_from_slice(b" not found.");
    let body = format!(
        r#"{{"error":{{"message":{},"type":"invalid_request_error"}}}}"#,
        go_string(&message)
    );
    error_response(404, HeaderMap::new(), Bytes::from(body), JSON_UTF8)
}

/// A 200 stream written as it comes: the provider's headers, then, when
/// they have no `Content-Type`, the `sniffed` one, which Go's server gives
/// a response from its first bytes.
fn raw_response(upstream: &HeaderMap, sniffed: Option<&'static str>, body: Body) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    write_upstream_headers(headers, upstream);
    if let Some(content_type) = sniffed
        && !headers.contains_key(header::CONTENT_TYPE)
    {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    response
}

/// Writes a Gemini stream: in SSE, each payload on a `data: ` line and a
/// failure as an `error` event; otherwise the payloads and the failure's
/// body as they are.
struct GeminiWriter {
    sse: bool,
}

impl StreamWriter for GeminiWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        if self.sse {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(&chunk);
            out.extend_from_slice(b"\n\n");
        } else {
            out.extend_from_slice(&chunk);
        }
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        let body = openai_body(error.http_status(), &error.text, false);
        if self.sse {
            out.extend_from_slice(b"event: error\ndata: ");
            out.extend_from_slice(body.as_bytes());
            out.extend_from_slice(b"\n\n");
        } else {
            out.extend_from_slice(body.as_bytes());
        }
    }
}
