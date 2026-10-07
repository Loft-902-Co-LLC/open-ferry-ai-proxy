// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_images_handlers.go
// (ImagesGenerations, ImagesEdits, imagesEditsFromMultipart,
// imagesEditsFromJSON, imagesModelParts, imagesModelBase,
// isSupportedImagesModel, isCodexImagesToolModel, isOpenAICompatImagesModel,
// rejectUnsupportedImagesModel, multipartFileToDataURL,
// buildOpenAICompatImagesJSONRequest, buildOpenAICompatImagesMultipartRequest,
// parseIntField, parseBoolField, setImagesSSEHeaders,
// waitImagesStreamExecution, writeImagesStreamKeepAlive,
// writeImagesStreamErrorEvent, handleRoutedImages, collectRoutedImages,
// streamRoutedImages, forwardRawImageStream, handleOpenAICompatImages,
// streamOpenAICompatImages, handleXAIImages, collectXAIImages,
// collectImagesWithModel, streamXAIImages and streamImagesWithModel)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1/images/generations` and `POST /v1/images/edits`.
//!
//! The model a request names, `gpt-image-2` when it names none, picks its
//! way:
//! - A Codex image tool model goes to the providers that serve it as the
//!   client sent it, with the model set and `stream` set or taken out, and
//!   never on a Codex credential on the free plan. Its answer, or its
//!   stream, goes back as it came.
//! - An xAI image model becomes an xAI image request, made without a stream
//!   even when the client asked for one. Its answer is written as the
//!   OpenAI images API's, or as `image_generation.completed` or
//!   `image_edit.completed` events.
//! - A model an `openai-compatibility` provider lists as an image model goes
//!   to it as the client sent it, with the model and `stream` set as for
//!   Codex. Its answer is read as an xAI one; its stream goes back as it
//!   came.
//! - Any other model is turned away with a 400.
//!
//! An edit is JSON or a `multipart/form-data` form. A form is read in
//! memory, never written to disk, and is held to the body limit; for Codex
//! and `openai-compatibility` it is written again with its model and
//! `stream` set, and for xAI its images become data URLs. Nothing here logs
//! a prompt or an image.
//!
//! `disable-image-generation: true` takes both endpoints away: they answer
//! 404 with no body.
//!
//! A stream writes an SSE comment every `streaming.keepalive-seconds` while
//! it waits for its first payload, which starts the response; an error
//! after that is written as an `error` event, with secrets taken out.
//!
//! Deviations from upstream:
//! - Upstream would make a request whose model none of the three serve a
//!   Responses call with the `image_generation` tool, but turns such a
//!   request away first, so that fallback can never run. It isn't ported.
//! - A form is held to the body limit, a 413 past it, and kept in memory;
//!   upstream has no limit and writes files past 32 MiB to temporary files.
//! - A form written again has its fields, then its files, in name order;
//!   upstream's maps give them in random order.
//! - An error in the request's query isn't reported once a form has been
//!   read; upstream's `ParseMultipartForm` answers 400 with it.
//! - While a Codex or `openai-compatibility` stream waits for its first
//!   payload, one ticker paces the keep-alives, and the response they start
//!   has none of the provider's headers. Upstream starts a new ticker once
//!   the provider has answered, and a keep-alive that starts the response
//!   after that has the provider's headers; this port's calls give a
//!   stream's headers only with its first payload.

mod xai;

#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use axum::body::{Body, BodyDataStream};
use axum::extract::State;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use open_ferry_core::config::DisableImageGeneration;
use open_ferry_core::exec::Format;
use open_ferry_core::multipart::{
    FileHeader, Form, MAX_FORM_MEMORY, Reader, Writer, file_content_disposition, lossy,
    parse_media_type,
};
use open_ferry_core::registry::StaticCatalog;
use open_ferry_core::registry::registration::OPENAI_IMAGE_MODEL_TYPE;
use open_ferry_translate::go;
use tokio::time::Interval;

use super::responses::stream_error::{sanitize_error, stream_error_text};
use crate::body;
use crate::errors::{
    ErrorMessage, invalid_request, local_error, openai_body, openai_error_response,
};
use crate::exec::{Call, ClientRequest};
use crate::json;
use crate::state::AppState;
use crate::stream::{
    Peeked, StreamWriter, forward, json_response, keep_alive, peek, sse_response, tick, ticker,
};

/// The Codex image tool's models (upstream's `gptImage15Model`,
/// `defaultImagesToolModel`, `gptImage25FlareModel`,
/// `gptImage25SunburstModel` and `gptImage25Model`).
const CODEX_TOOL_MODELS: [&str; 5] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
];

/// The model of a request that names none.
const DEFAULT_MODEL: &str = "gpt-image-2";

/// What a stream comment that keeps the connection alive writes.
const KEEP_ALIVE: &[u8] = b": keep-alive\n\n";

/// Go's `http.ErrNotMultipart`.
const NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";

/// Go's `http.ErrMissingBoundary`.
const MISSING_BOUNDARY: &str = "no multipart boundary param in Content-Type";

/// `POST /v1/images/generations` (upstream's `ImagesGenerations`).
pub(crate) async fn generations(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    if disabled(&state) {
        return not_found();
    }
    let request = match read_json(&state, &client, body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let JsonRequest {
        raw,
        model,
        format,
        stream,
        ..
    } = request;
    if is_codex_tool_model(&model) {
        let payload = compat_json_request(&raw, &model, stream);
        return routed(&state, &client, &model, payload, stream).await;
    }
    if xai::is_model(&model) {
        let request = xai::generations_request(&raw, &model, &format);
        return xai_images(&state, &client, request, format, "image_generation", stream).await;
    }
    let payload = compat_json_request(&raw, &model, stream);
    compat(&state, &client, &model, payload, format, stream).await
}

/// `POST /v1/images/edits` (upstream's `ImagesEdits`): JSON, or a form when
/// it says so or names no content type.
pub(crate) async fn edits(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    if disabled(&state) {
        return not_found();
    }
    let content_type = client
        .headers
        .get(header::CONTENT_TYPE)
        .map(|value| lossy(value.as_bytes()))
        .unwrap_or_default();
    let content_type = go::to_lower(content_type.trim());
    if content_type.starts_with("application/json") {
        return edits_from_json(&state, &client, body).await;
    }
    if content_type.starts_with("multipart/form-data") || content_type.is_empty() {
        return edits_from_form(&state, &client, body).await;
    }
    invalid_request(
        400,
        &format!("unsupported Content-Type {}", go::quote(&content_type)),
    )
}

/// A JSON edit (upstream's `imagesEditsFromJSON`).
async fn edits_from_json(state: &AppState, client: &ClientRequest, body: Body) -> Response {
    let request = match read_json(state, client, body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let JsonRequest {
        raw,
        model,
        prompt,
        format,
        stream,
    } = request;
    if is_codex_tool_model(&model) {
        let payload = compat_json_request(&raw, &model, stream);
        return routed(state, client, &model, payload, stream).await;
    }
    if xai::is_model(&model) {
        let images = xai::images_from_json(&raw);
        if images.is_empty() {
            return invalid_request(400, "image is required");
        }
        let options = xai::edit_options_from_json(&raw);
        let request = xai::edit_request(&model, &prompt, &images, &format, &options);
        return xai_images(state, client, request, format, "image_edit", stream).await;
    }
    let payload = compat_json_request(&raw, &model, stream);
    compat(state, client, &model, payload, format, stream).await
}

/// A form edit (upstream's `imagesEditsFromMultipart`).
async fn edits_from_form(state: &AppState, client: &ClientRequest, body: Body) -> Response {
    let boundary = match form_boundary(&client.headers) {
        Ok(boundary) => boundary,
        Err(text) => return invalid_request(400, text),
    };
    let limit = state.settings().config.body_limit;
    let raw = match body::read_raw(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    let form = match Reader::new(raw, &boundary).read_form(MAX_FORM_MEMORY) {
        Ok(form) => form,
        Err(error) => return invalid_request(400, &error.to_string()),
    };
    let model = model_or_default(&form_value(&form, "model"));
    if let Some(response) = reject(state, &model) {
        return response;
    }
    let prompt = form_value(&form, "prompt").trim().to_owned();
    if prompt.is_empty() {
        return invalid_request(400, "prompt is required");
    }
    let files = match form.files_of("image[]") {
        [] => form.files_of("image"),
        files => files,
    };
    if files.is_empty() {
        return invalid_request(400, "image is required");
    }
    let format = response_format(&form_value(&form, "response_format"));
    let stream = parse_bool(&form_value(&form, "stream"), false);

    // A Codex image tool model's base is never an xAI one.
    if xai::is_model(&model) {
        let images: Vec<String> = files.iter().map(FileHeader::data_url).collect();
        let size = form_value(&form, "size");
        let aspect = xai::aspect_ratio(&form_value(&form, "aspect_ratio"), "");
        let options = xai::Options {
            aspect_ratio: xai::aspect_ratio_from_size(&size, aspect).to_owned(),
            resolution: xai::resolution(&form_value(&form, "resolution"), &size, ""),
            quality: form_value(&form, "quality").trim().to_owned(),
            n: parse_int(&form_value(&form, "n"), 0),
        };
        let request = xai::edit_request(&model, &prompt, &images, &format, &options);
        return xai_images(state, client, request, format, "image_edit", stream).await;
    }
    let (payload, content_type) = compat_form_request(&form, &model, stream);
    let mut client = client.clone();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        client.headers.insert(header::CONTENT_TYPE, value);
    }
    if is_codex_tool_model(&model) {
        return routed(state, &client, &model, payload, stream).await;
    }
    compat(state, &client, &model, payload, format, stream).await
}

/// What a JSON request asks for.
struct JsonRequest {
    /// The body, its `Content-Encoding` decoded.
    raw: Bytes,
    /// The trimmed model, or the default one.
    model: String,
    /// The trimmed prompt, which isn't empty.
    prompt: String,
    /// The trimmed `response_format`, or `b64_json`.
    format: String,
    /// Whether `stream` is true as gjson reads it.
    stream: bool,
}

/// Reads a JSON request, or answers 400 when it isn't JSON, names a model
/// none of the three serve, or has no prompt.
async fn read_json(
    state: &AppState,
    client: &ClientRequest,
    body: Body,
) -> Result<JsonRequest, Response> {
    let limit = state.settings().config.body_limit;
    let raw = body::read_decoded(&client.headers, body, limit).await?;
    if !json::valid(&raw) {
        return Err(invalid_request(400, "body must be valid JSON"));
    }
    let model = model_or_default(&json::str_at(&raw, "model"));
    if let Some(response) = reject(state, &model) {
        return Err(response);
    }
    let prompt = json::str_at(&raw, "prompt").trim().to_owned();
    if prompt.is_empty() {
        return Err(invalid_request(400, "prompt is required"));
    }
    let format = response_format(&json::str_at(&raw, "response_format"));
    let stream = json::get(&raw, "stream").is_some_and(|value| value.bool());
    Ok(JsonRequest {
        raw,
        model,
        prompt,
        format,
        stream,
    })
}

/// Whether `disable-image-generation: true` takes the endpoints away.
fn disabled(state: &AppState) -> bool {
    state.settings().config.disable_image_generation == DisableImageGeneration::All
}

/// A 404 with no body (gin's `AbortWithStatus`).
fn not_found() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// The trimmed `model`, or the default one.
fn model_or_default(model: &str) -> String {
    match model.trim() {
        "" => DEFAULT_MODEL.to_owned(),
        model => model.to_owned(),
    }
}

/// The trimmed `format`, or `b64_json`.
fn response_format(format: &str) -> String {
    match format.trim() {
        "" => "b64_json".to_owned(),
        format => format.to_owned(),
    }
}

/// `model`, trimmed, as its prefix and base: around its last `/` when
/// something follows it, else no prefix (upstream's `imagesModelParts`).
pub(super) fn model_parts(model: &str) -> (&str, &str) {
    let model = model.trim();
    match model.rsplit_once('/') {
        Some((prefix, base)) if !base.is_empty() => (prefix.trim(), base.trim()),
        _ => ("", model),
    }
}

/// `model`'s base, lowercased (upstream's `imagesModelBase`).
fn model_base(model: &str) -> String {
    go::to_lower(model_parts(model).1.trim())
}

/// Whether `model`'s base is a Codex image tool model, whatever its prefix
/// (upstream's `isCodexImagesToolModel`).
fn is_codex_tool_model(model: &str) -> bool {
    CODEX_TOOL_MODELS.contains(&model_base(model).as_str())
}

/// Whether `model` is registered as an image model, as an
/// `openai-compatibility` provider lists one (upstream's
/// `isOpenAICompatImagesModel` and `LookupModelInfo`).
fn is_compat_model(state: &AppState, model: &str) -> bool {
    let model = model.trim();
    if model.is_empty() {
        return false;
    }
    state
        .catalog()
        .model_info(model, "")
        .or_else(|| StaticCatalog::current().lookup(model))
        .is_some_and(|info| info.model_type == OPENAI_IMAGE_MODEL_TYPE)
}

/// A 400 for a model none of the three serve, or `None` (upstream's
/// `rejectUnsupportedImagesModel`).
fn reject(state: &AppState, model: &str) -> Option<Response> {
    if is_codex_tool_model(model) || xai::is_model(model) || is_compat_model(state, model) {
        return None;
    }
    let message = format!(
        "Model {model} is not supported on /v1/images/generations or /v1/images/edits. Use {}, \
         {}, or a configured openai-compatibility image model.",
        CODEX_TOOL_MODELS.join(", "),
        xai::MODELS.join(", "),
    );
    Some(local_error(400, &message, "invalid_request_error"))
}

/// The boundary of a form request, or Go's error for a request that isn't
/// one (Go's `multipartReader`).
fn form_boundary(headers: &HeaderMap) -> Result<Vec<u8>, &'static str> {
    let value = headers
        .get(header::CONTENT_TYPE)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    if value.is_empty() {
        return Err(NOT_MULTIPART);
    }
    match parse_media_type(value) {
        Ok((kind, params)) if kind == "multipart/form-data" => {
            params.get("boundary").cloned().ok_or(MISSING_BOUNDARY)
        }
        _ => Err(NOT_MULTIPART),
    }
}

/// The first value of form field `name` as text, empty when there is none
/// (gin's `PostForm`).
fn form_value(form: &Form, name: &str) -> String {
    form.value(name)
        .map(|value| lossy(value))
        .unwrap_or_default()
}

/// `raw` as a whole number, or `fallback` (upstream's `parseIntField`).
fn parse_int(raw: &str, fallback: i64) -> i64 {
    match raw.trim() {
        "" => fallback,
        raw => raw.parse().unwrap_or(fallback),
    }
}

/// `raw` as a yes or no, or `fallback` (upstream's `parseBoolField`).
fn parse_bool(raw: &str, fallback: bool) -> bool {
    match go::to_lower(raw).trim() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

/// A JSON request as a Codex or `openai-compatibility` provider takes it,
/// with `model` set and `stream` set or taken out (upstream's
/// `buildOpenAICompatImagesJSONRequest`).
fn compat_json_request(raw: &[u8], model: &str, stream: bool) -> Bytes {
    let mut payload = raw.to_vec();
    let model = model.trim();
    if !model.is_empty() {
        payload = json::set_str(&payload, "model", model);
    }
    payload = if stream {
        json::set_bool(&payload, "stream", true)
    } else {
        json::delete(&payload, "stream")
    };
    Bytes::from(payload)
}

/// A form written again for a Codex or `openai-compatibility` provider, and
/// its content type: `model`, then `stream` when streaming, then the other
/// fields, then the files, each with a content type (upstream's
/// `buildOpenAICompatImagesMultipartRequest`).
fn compat_form_request(form: &Form, model: &str, stream: bool) -> (Bytes, String) {
    let mut writer = Writer::new();
    writer.write_field("model", model.as_bytes());
    if stream {
        writer.write_field("stream", b"true");
    }
    for (name, values) in form.values() {
        if name == "model" || name == "stream" {
            continue;
        }
        for value in values {
            writer.write_field(name, value);
        }
    }
    for (name, files) in form.files() {
        for file in files {
            let mut header = file.header.clone();
            header.set(
                "Content-Disposition",
                file_content_disposition(name, &file.filename),
            );
            if header.get("Content-Type").is_none_or(<[u8]>::is_empty) {
                header.set("Content-Type", "application/octet-stream");
            }
            writer.write_part(&header, &file.data);
        }
    }
    let content_type = writer.form_data_content_type();
    (Bytes::from(writer.finish()), content_type)
}

/// The settings a call's answer is written with.
#[derive(Clone, Copy)]
struct Timing {
    /// `passthrough-headers`.
    passthrough: bool,
    /// The keep-alive interval of a call made without a stream.
    nonstream: Option<Duration>,
    /// The keep-alive interval of a stream.
    keepalive: Option<Duration>,
}

impl Timing {
    fn of(state: &AppState) -> Self {
        let settings = state.settings();
        Self {
            passthrough: settings.config.passthrough_headers,
            nonstream: settings.config.nonstream_keepalive,
            keepalive: settings.config.streaming.keepalive,
        }
    }
}

/// A Codex image tool call, its answer or stream sent back as it came
/// (upstream's `handleRoutedImages`, `collectRoutedImages` and
/// `streamRoutedImages`). It never uses a Codex credential on the free plan.
async fn routed(
    state: &AppState,
    client: &ClientRequest,
    model: &str,
    payload: Bytes,
    stream: bool,
) -> Response {
    let timing = Timing::of(state);
    let mut call = match Call::image(state, client, Format::OPENAI_IMAGE, model, payload, stream) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, timing.passthrough),
    };
    call.options.metadata.disallow_free_auth = true;
    if stream {
        return stream_raw(call, timing, false).await;
    }
    keep_alive(
        timing.nonstream,
        call.execute(),
        move |result| match result {
            Ok(reply) => json_response(&reply.headers, reply.body),
            Err(error) => openai_error_response(&error, timing.passthrough),
        },
    )
    .await
}

/// An `openai-compatibility` image call, its answer read as xAI's, its
/// stream sent back as it came (upstream's `handleOpenAICompatImages`).
async fn compat(
    state: &AppState,
    client: &ClientRequest,
    model: &str,
    payload: Bytes,
    format: String,
    stream: bool,
) -> Response {
    let timing = Timing::of(state);
    let call = match Call::image(state, client, Format::OPENAI_IMAGE, model, payload, stream) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, timing.passthrough),
    };
    if stream {
        return stream_raw(call, timing, true).await;
    }
    collect(call, timing, format).await
}

/// An xAI image call, made without a stream, its answer written as the
/// OpenAI images API's, or as `<prefix>.completed` events (upstream's
/// `handleXAIImages`).
async fn xai_images(
    state: &AppState,
    client: &ClientRequest,
    request: Vec<u8>,
    format: String,
    prefix: &'static str,
    stream: bool,
) -> Response {
    let timing = Timing::of(state);
    let model = json::str_at(&request, "model").trim().to_owned();
    let payload = Bytes::from(request);
    let call = match Call::image(state, client, Format::OPENAI_IMAGE, &model, payload, false) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, timing.passthrough),
    };
    if stream {
        return stream_xai(call, timing, format, prefix).await;
    }
    collect(call, timing, format).await
}

/// Makes `call` and answers with its answer read as xAI's, in the OpenAI
/// images API's shape, or a 502 when there are no images in it (upstream's
/// `collectImagesWithModel`).
async fn collect(call: Call, timing: Timing, format: String) -> Response {
    keep_alive(timing.nonstream, call.execute(), move |result| {
        let result =
            result.and_then(
                |reply| match xai::images_api_response(&reply.body, &format) {
                    Ok(body) => Ok((body, reply.headers)),
                    Err(text) => Err(ErrorMessage::new(502, text)),
                },
            );
        match result {
            Ok((body, headers)) => json_response(&headers, body),
            Err(error) => openai_error_response(&error, timing.passthrough),
        }
    })
    .await
}

/// Streams `call`, its payloads written as they come (upstream's
/// `streamRoutedImages` and `streamOpenAICompatImages`). A stream that ends
/// with no payload writes a newline, or nothing for `compat`.
async fn stream_raw(call: Call, timing: Timing, compat: bool) -> Response {
    let keepalive = timing.keepalive.filter(|interval| !interval.is_zero());
    let mut waiting = keepalive.map(ticker);
    match until_tick(&mut waiting, Box::pin(first_payload(call))).await {
        Waited::Done((_, Peeked::Failed(error))) => {
            openai_error_response(&error, timing.passthrough)
        }
        Waited::Done((headers, peeked)) => {
            sse_response(&headers, raw_body(peeked, keepalive, compat))
        }
        Waited::Due(rest) => {
            let late = async move { raw_body(rest.await.1, keepalive, compat) };
            sse_response(&HeaderMap::new(), keep_alive_body(waiting, late))
        }
    }
}

/// Starts `call`'s stream, and gives its headers and how it started.
async fn first_payload(call: Call) -> (HeaderMap, Peeked) {
    let started = call.stream().await;
    (started.headers, peek(started.items).await)
}

/// The body of a raw stream once it has started: its payloads, or the
/// error that stopped it as an event.
fn raw_body(peeked: Peeked, keepalive: Option<Duration>, compat: bool) -> Body {
    match peeked {
        Peeked::Failed(error) => Body::from(error_event(error)),
        Peeked::Closed if compat => Body::empty(),
        Peeked::Closed => Body::from("\n"),
        Peeked::First(first, rest) => {
            let items = stream::iter([Ok(first)]).chain(rest).boxed();
            forward(items, RawWriter { compat }, keepalive)
        }
    }
}

/// Makes `call`, made without a stream, and writes its answer as
/// `<prefix>.completed` events (upstream's `streamImagesWithModel`).
async fn stream_xai(call: Call, timing: Timing, format: String, prefix: &'static str) -> Response {
    let mut waiting = timing
        .keepalive
        .filter(|interval| !interval.is_zero())
        .map(ticker);
    match until_tick(&mut waiting, Box::pin(call.execute())).await {
        Waited::Done(Err(error)) => openai_error_response(&error, timing.passthrough),
        Waited::Done(Ok(reply)) => match xai::completed_events(&reply.body, &format, prefix) {
            Ok(events) => sse_response(&reply.headers, Body::from(events)),
            Err(text) => openai_error_response(&ErrorMessage::new(502, text), timing.passthrough),
        },
        Waited::Due(rest) => {
            let late = async move {
                let events = rest.await.and_then(|reply| {
                    xai::completed_events(&reply.body, &format, prefix)
                        .map_err(|text| ErrorMessage::new(502, text))
                });
                match events {
                    Ok(events) => Body::from(events),
                    Err(error) => Body::from(error_event(error)),
                }
            };
            sse_response(&HeaderMap::new(), keep_alive_body(waiting, late))
        }
    }
}

/// What came first while a call ran.
enum Waited<F: Future> {
    /// The call's output.
    Done(F::Output),
    /// A keep-alive tick; the call still runs.
    Due(Pin<Box<F>>),
}

/// Runs `call` until it is done or `ticker` ticks, whichever comes first
/// (upstream's `waitImagesStreamExecution`, and its handlers' wait for a
/// first payload).
async fn until_tick<F: Future>(ticker: &mut Option<Interval>, mut call: Pin<Box<F>>) -> Waited<F> {
    tokio::select! {
        biased;
        output = &mut call => Waited::Done(output),
        () = tick(ticker) => Waited::Due(call),
    }
}

/// The body of a stream a keep-alive has started while `rest` runs: that
/// keep-alive, another each time `ticker` ticks, then the body `rest`
/// gives (upstream's `writeImagesStreamKeepAlive`).
fn keep_alive_body<F>(ticker: Option<Interval>, rest: F) -> Body
where
    F: Future<Output = Body> + Send + 'static,
{
    let phase = Phase::Due(ticker, Box::pin(rest));
    Body::from_stream(stream::unfold(phase, |phase| async move {
        match phase {
            Phase::Due(ticker, rest) => Some((
                Ok(Bytes::from_static(KEEP_ALIVE)),
                Phase::Waiting(ticker, rest),
            )),
            Phase::Waiting(mut ticker, mut rest) => {
                tokio::select! {
                    biased;
                    body = &mut rest => {
                        let mut body = body.into_data_stream();
                        let item = body.next().await?;
                        Some((item, Phase::Body(body)))
                    }
                    () = tick(&mut ticker) => Some((
                        Ok(Bytes::from_static(KEEP_ALIVE)),
                        Phase::Waiting(ticker, rest),
                    )),
                }
            }
            Phase::Body(mut body) => {
                let item = body.next().await?;
                Some((item, Phase::Body(body)))
            }
        }
    }))
}

/// Where a body [`keep_alive_body`] gives is.
enum Phase<F> {
    /// A keep-alive is due.
    Due(Option<Interval>, Pin<Box<F>>),
    /// The call runs.
    Waiting(Option<Interval>, Pin<Box<F>>),
    /// The call is done, and its body is being written.
    Body(BodyDataStream),
}

/// Writes a raw image stream: each payload as it is, and an error as an
/// `error` event (upstream's `forwardRawImageStream`, and the
/// `StreamForwardOptions` of `streamOpenAICompatImages`).
struct RawWriter {
    /// Whether this is an `openai-compatibility` stream, whose errors are
    /// sanitized before they are written too.
    compat: bool,
}

impl StreamWriter for RawWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        out.extend_from_slice(&chunk);
    }

    fn normalize_terminal_error(&mut self, error: ErrorMessage) -> ErrorMessage {
        if self.compat {
            sanitize_error(error)
        } else {
            error
        }
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        out.extend_from_slice(&error_event(error.clone()));
    }
}

/// An `error` event for `error`, with secrets taken out of its text
/// (upstream's `writeImagesStreamErrorEvent`).
fn error_event(error: ErrorMessage) -> Bytes {
    let error = sanitize_error(error);
    let text = stream_error_text(&error, error.status);
    let body = openai_body(error.status, &text, false);
    Bytes::from(format!("event: error\ndata: {body}\n\n"))
}
