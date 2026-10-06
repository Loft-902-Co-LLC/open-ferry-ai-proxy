// Ported from CLIProxyAPI internal/runtime/executor/codex_openai_images.go
// (isCodexOpenAIImageRequest, codexIsImagesEndpointPath,
// resolveGPTImage2BaseModel, executeOpenAIImage, executeOpenAIImageStream,
// executeDirectOpenAIImage, executeDirectOpenAIImageStream,
// codexDirectOpenAIImageEndpoint, codexPrepareDirectOpenAIImageBody,
// codexPrepareDirectOpenAIImagePayload,
// codexPrepareDirectOpenAIImageEditPayload,
// codexRewriteOpenAIImageEditMultipartToJSON,
// codexSetOpenAIImageEditFormValues, codexOpenAIImageEditFormJSONValue,
// codexOpenAIImageEditFormJSONPath, codexDirectOpenAIImageModel,
// codexOpenAIImageBaseModel, codexIsDirectOpenAIImageModel,
// prepareCodexOpenAIImageBody, codexPrepareOpenAIImageRequest,
// codexPrepareOpenAIImageGenerationJSON, codexPrepareOpenAIImageEditJSON,
// codexPrepareOpenAIImageEditMultipart, codexNormalizeImageResponseFormat,
// codexOpenAIImageToolModel, codexBuildOpenAIImageTool,
// codexBuildImagesResponsesRequest, codexMultipartImageFiles,
// codexExtractImageResults, codexBuildImagesAPIResponse,
// codexBuildImagePartialFrame, codexBuildImageCompletedFrame,
// codexBuildSSEFrame and codexMimeTypeFromOutputFormat), the image dispatch
// of codex_executor_execute.go and codex_executor_stream.go, and
// applyCodexDirectImageHeaders in codex_executor_request.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OpenAI Images endpoints served through Codex: a call whose client
//! format is `openai-image` and whose route ends in `/v1/images/generations`
//! or `/v1/images/edits` (upstream's `executeOpenAIImage`).
//!
//! A call for one of the Image API's models (`gpt-image-1.5`,
//! `gpt-image-2`, and `gpt-image-2.5` with its `-flare` and `-sunburst`
//! variants, named by the body's `model` or else the call's, read without
//! a thinking suffix or a `provider/` prefix and in any case) goes to
//! `<base>/images/generations` or `<base>/images/edits` with the body as
//! the client sent it: the model set, `"stream": true` for a stream and no
//! `stream` otherwise. A form edit goes as JSON, its images and mask as
//! data URLs. Codex's answer, or its stream, goes back as it came.
//!
//! Any other call goes to `<base>/responses` as a Responses request for
//! `gpt-5.4-mini`, or the config's `gpt-image-2-base-model` when that
//! starts with `gpt-`, with the `image_generation` tool forced: the prompt
//! is its input text and the images to edit its input images. The images
//! the tool makes are answered as the Images API answers, as `b64_json`
//! or, for `response_format: url`, as data URLs. A stream gives an
//! `image_generation.partial_image` event (`image_edit.` for an edit) for
//! each partial image, then an `image_generation.completed` one for each
//! image.
//!
//! Nothing here logs a prompt or an image, and nothing goes to disk; the
//! call's taps see the body sent as they see every body.
//!
//! Deviations from upstream:
//! - No `prompt_cache_key` or `Session-Id` is made up from the call's
//!   session, either way (see the module docs of [`super::super`]).
//! - A call to the Image API sends `User-Agent: open-ferry/<version>` and
//!   the client's own `Originator`, if it sent one. Upstream drops the
//!   client's `User-Agent` too, but then its cloaking, on by default, makes
//!   the call pass for `codex-tui`; that isn't ported, nor are models'
//!   `override_header`.
//! - A JSON body that changes is written again whole and compact. A form
//!   edit's fields are read in name order (Go's map gives them in random
//!   order), and a field's name is its JSON path as written, dots and all,
//!   without sjson's other path syntax. Its images replace an `images`
//!   field that isn't an array, where sjson adds them to an object under
//!   the key `-1`. A body Go reads as JSON but serde doesn't (a string with
//!   invalid UTF-8) reads as having no fields.
//! - A stream from the Image API is passed on a line at a time with the
//!   request's secrets redacted (see the crate's `images` module), and its
//!   answer is read up to 50 MiB. Every answer and error is redacted as the
//!   executor's others are.
//! - A tool call's answer is read a line at a time, up to 50 MiB a line,
//!   where upstream reads it whole first: a completed event read before the
//!   connection fails is answered, where upstream fails the read.
//! - Usage comes from the call's taps: an Image API call's answer is read
//!   as an OpenAI answer, whole or as a stream, and a tool call's as any
//!   Codex call's.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::{HeaderValue, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    ChunkStream, ErrorKind, ExecError, Format, Options, Request, Response, StreamResponse,
};
use open_ferry_core::multipart::{FileHeader, Form, lossy, parse_media_type};
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::go::{json_valid, quote, quote_bytes, to_lower, trim_space};
use open_ferry_translate::json::exact;
use serde_json::{Map, Value, json};

use super::{CodexExecutor, MAX_ERROR_BODY};
use crate::codex::client::{error_chain, read_body, read_body_prefix};
use crate::codex::input_ids::sanitize_input_item_ids;
use crate::codex::request::{
    base_model, build_headers, credentials, endpoint, format_is, normalize_instructions,
    set_bool_if_different, set_string_if_different,
};
use crate::codex::stream::{LineReader, MAX_LINE};
use crate::codex::terminal::{OutputItems, StatusError, status_error_with_cooling};
use crate::codex::thinking;
use crate::images;
use crate::json::{self, delete, eq_fold, get, int_at, int_of, set, str_at};
use crate::observe_send::{self, Attempt, BodyTap};
use crate::payload::{self, MediaTarget, Target};
use crate::redact::{Policy, Secrets};
use crate::thinking::Route;

/// The model of a tool call when the config names none
/// (`codexOpenAIImagesMainModel`).
const MAIN_MODEL: &str = "gpt-5.4-mini";
/// The tool's model when neither the request nor the call names one
/// (`codexDefaultImageToolModel`).
const DEFAULT_TOOL_MODEL: &str = "gpt-image-2";
const GENERATIONS: &str = "/v1/images/generations";
const EDITS: &str = "/v1/images/edits";
const DIRECT_GENERATIONS: &str = "/images/generations";
const DIRECT_EDITS: &str = "/images/edits";
/// The models Codex's Image API serves (`codexIsDirectOpenAIImageModel`).
const DIRECT_MODELS: [&str; 5] = [
    "gpt-image-1.5",
    DEFAULT_TOOL_MODEL,
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
];
/// The fields of a request copied into the tool as strings.
const GENERATION_STRINGS: [&str; 5] = [
    "size",
    "quality",
    "background",
    "output_format",
    "moderation",
];
const EDIT_STRINGS: [&str; 6] = [
    "size",
    "quality",
    "background",
    "output_format",
    "input_fidelity",
    "moderation",
];
/// The fields of a request copied into the tool as integers.
const NUMBERS: [&str; 2] = ["output_compression", "partial_images"];

/// Whether the call is one from the OpenAI Images endpoints
/// (`isCodexOpenAIImageRequest`).
pub(super) fn is_image_request(options: &Options) -> bool {
    if !format_is(&options.source_format, &Format::OPENAI_IMAGE) {
        return false;
    }
    let path = options.metadata.request_path.trim();
    path.ends_with(GENERATIONS) || path.ends_with(EDITS)
}

/// The main model of a tool call (`resolveGPTImage2BaseModel`).
fn main_model(config: Option<&Config>) -> String {
    let model = config.map_or("", |config| config.gpt_image_2_base_model.trim());
    if !model.is_empty() && to_lower(model).starts_with("gpt-") {
        model.to_owned()
    } else {
        MAIN_MODEL.to_owned()
    }
}

/// `model` without a thinking suffix or a `provider/` prefix, in lower case
/// (`codexOpenAIImageBaseModel`).
fn image_base_model(model: &str) -> String {
    let mut model = base_model(model).trim();
    if let Some(slash) = model.rfind('/')
        && let Some(rest) = model.get(slash + 1..)
        && !rest.is_empty()
    {
        model = rest.trim();
    }
    to_lower(model.trim())
}

/// The Image API model and endpoint of a call for one
/// (`codexDirectOpenAIImageModel` and `codexDirectOpenAIImageEndpoint`).
fn direct(request: &Request, options: &Options) -> Option<(String, &'static str)> {
    let named = exact::from_slice(&request.payload)
        .map(|payload| str_at(&payload, "model"))
        .unwrap_or_default();
    let model = [named.as_str(), request.model.as_str()]
        .into_iter()
        .map(image_base_model)
        .find(|model| DIRECT_MODELS.contains(&model.as_str()))?;
    let path = options.metadata.request_path.trim();
    let endpoint = if path.ends_with(GENERATIONS) {
        DIRECT_GENERATIONS
    } else if path.ends_with(EDITS) {
        DIRECT_EDITS
    } else {
        return None;
    };
    Some((model, endpoint))
}

/// An error of the request itself, which upstream returns as a plain error.
fn failure(message: impl Into<String>) -> ExecError {
    ExecError::new(ErrorKind::Upstream, message)
}

/// The client's `Content-Type`, as it sent it.
fn client_content_type(options: &Options) -> &[u8] {
    options
        .headers
        .get(header::CONTENT_TYPE)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default()
}

/// The body of an Image API call and its content type
/// (`codexPrepareDirectOpenAIImagePayload`).
fn direct_body(
    request: &Request,
    options: &Options,
    model: &str,
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    let content_type = client_content_type(options);
    if options.metadata.request_path.trim().ends_with(EDITS) {
        return direct_edit_body(&request.payload, model, content_type, stream);
    }
    images::prepare_payload(&request.payload, model, &lossy(content_type), stream)
}

/// The body of an Image API edit: JSON as the client sent it, a form as
/// JSON (`codexPrepareDirectOpenAIImageEditPayload`).
fn direct_edit_body(
    payload: &Bytes,
    model: &str,
    content_type: &[u8],
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    if json_valid(payload) {
        return images::prepare_payload(payload, model, &lossy(content_type), stream);
    }
    let unsupported = || {
        failure(format!(
            "unsupported OpenAI image edit Content-Type {}",
            quote_bytes(content_type)
        ))
    };
    let Ok((kind, params)) = parse_media_type(trim_space(content_type)) else {
        return Err(unsupported());
    };
    if !to_lower(kind.trim()).starts_with("multipart/") {
        return Err(unsupported());
    }
    let boundary = trim_space(
        params
            .get("boundary")
            .map(Vec::as_slice)
            .unwrap_or_default(),
    );
    if boundary.is_empty() {
        return Err(failure("multipart boundary is missing"));
    }
    edit_form_to_json(payload, model, boundary, stream)
}

/// A form edit as a JSON one, its images and mask as data URLs
/// (`codexRewriteOpenAIImageEditMultipartToJSON`).
fn edit_form_to_json(
    payload: &Bytes,
    model: &str,
    boundary: &[u8],
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    let form = images::read_form(payload, boundary)
        .map_err(|error| failure(format!("read multipart form failed: {error}")))?;
    let mut out = json!({ "model": model });
    if stream {
        set(&mut out, "stream", Value::Bool(true));
    }
    for (key, values) in form.values() {
        let key = key.trim();
        if key.is_empty() || key == "model" || key == "stream" {
            continue;
        }
        set_form_values(&mut out, key, values);
    }
    if let Some(mask) = form.files_of("mask").first() {
        set(&mut out, "mask.image_url", Value::String(mask.data_url()));
    }
    let files = image_files(&form);
    if !files.is_empty() {
        // Added to an array, replacing anything else (see the module docs).
        let mut images = match get(&out, "images") {
            Some(Value::Array(images)) => images.clone(),
            _ => Vec::new(),
        };
        images.extend(
            files
                .iter()
                .map(|file| json!({ "image_url": file.data_url() })),
        );
        set(&mut out, "images", Value::Array(images));
    }
    Ok((Bytes::from(out.to_string()), "application/json".to_owned()))
}

/// Sets a form field's values at its JSON path, an array when it was sent
/// more than once (`codexSetOpenAIImageEditFormValues`).
fn set_form_values(out: &mut Value, key: &str, values: &[Bytes]) {
    let path = match key {
        "mask[file_id]" => "mask.file_id",
        "mask[image_url]" => "mask.image_url",
        key => key,
    };
    match values {
        [] => {}
        [value] => {
            set(out, path, form_json_value(path, value));
        }
        values => {
            let items = values
                .iter()
                .map(|value| form_json_value(key, value))
                .collect();
            set(out, path, Value::Array(items));
        }
    }
}

/// A form value, trimmed, as JSON: an integer for the fields that take one
/// when it reads as one, else a string
/// (`codexOpenAIImageEditFormJSONValue`).
fn form_json_value(key: &str, value: &[u8]) -> Value {
    let value = lossy(value);
    let value = value.trim();
    if matches!(
        json::lower_trim(key).as_str(),
        "n" | "output_compression" | "partial_images"
    ) && let Ok(number) = value.parse::<i64>()
    {
        return Value::from(number);
    }
    Value::from(value)
}

/// The images of a form edit: its `image[]` files, else its `image` ones
/// (`codexMultipartImageFiles`).
fn image_files(form: &Form) -> &[FileHeader] {
    match form.files_of("image[]") {
        [] => form.files_of("image"),
        files => files,
    }
}

/// A tool call's Responses request, how its images are answered and its
/// stream events' prefix (`codexOpenAIImagePreparedRequest`).
struct Prepared {
    body: Value,
    response_format: &'static str,
    prefix: &'static str,
}

/// `codexPrepareOpenAIImageRequest`.
fn prepare_tool_request(request: &Request, options: &Options) -> Result<Prepared, ExecError> {
    let path = options.metadata.request_path.as_str();
    if path.ends_with(GENERATIONS) {
        return generation(&request.payload, &request.model);
    }
    if !path.ends_with(EDITS) {
        return Err(failure(format!(
            "unsupported OpenAI image endpoint path {}",
            quote(path)
        )));
    }
    let content_type = trim_space(client_content_type(options));
    let media_type = match parse_media_type(content_type) {
        Ok((kind, _)) => kind,
        Err(error) => error.media_type().to_owned(),
    };
    if to_lower(&media_type).starts_with("multipart/") {
        multipart_edit(&request.payload, &request.model, content_type)
    } else {
        json_edit(&request.payload, &request.model)
    }
}

/// `payload` as JSON, `null` when serde can't read it.
fn parse(payload: &[u8]) -> Value {
    exact::from_slice(payload).unwrap_or(Value::Null)
}

/// `codexPrepareOpenAIImageGenerationJSON`.
fn generation(payload: &[u8], route_model: &str) -> Result<Prepared, ExecError> {
    if !json_valid(payload) {
        return Err(failure("invalid OpenAI image generation request JSON"));
    }
    let payload = parse(payload);
    let prompt = str_at(&payload, "prompt");
    let tool = build_tool(&payload, route_model, "generate", &GENERATION_STRINGS);
    Ok(Prepared {
        body: responses_request(prompt.trim(), &[], tool),
        response_format: response_format(&str_at(&payload, "response_format")),
        prefix: "image_generation",
    })
}

/// `codexPrepareOpenAIImageEditJSON`.
fn json_edit(payload: &[u8], route_model: &str) -> Result<Prepared, ExecError> {
    if !json_valid(payload) {
        return Err(failure("invalid OpenAI image edit request JSON"));
    }
    let payload = parse(payload);
    let prompt = str_at(&payload, "prompt");
    let images: Vec<String> = match get(&payload, "images") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| str_at(item, "image_url").trim().to_owned())
            .filter(|url| !url.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let mut tool = build_tool(&payload, route_model, "edit", &EDIT_STRINGS);
    let mask = str_at(&payload, "mask.image_url");
    if !mask.trim().is_empty() {
        set(
            &mut tool,
            "input_image_mask.image_url",
            Value::from(mask.trim()),
        );
    }
    Ok(Prepared {
        body: responses_request(prompt.trim(), &images, tool),
        response_format: response_format(&str_at(&payload, "response_format")),
        prefix: "image_edit",
    })
}

/// `codexPrepareOpenAIImageEditMultipart`.
fn multipart_edit(
    payload: &Bytes,
    route_model: &str,
    content_type: &[u8],
) -> Result<Prepared, ExecError> {
    let (_, params) = parse_media_type(content_type)
        .map_err(|error| failure(format!("parse multipart content type failed: {error}")))?;
    let boundary = trim_space(
        params
            .get("boundary")
            .map(Vec::as_slice)
            .unwrap_or_default(),
    );
    if boundary.is_empty() {
        return Err(failure("multipart boundary is required"));
    }
    let form = images::read_form(payload, boundary)
        .map_err(|error| failure(format!("parse multipart form failed: {error}")))?;
    // `codexFormValue`.
    let field = |name: &str| {
        form.value(name)
            .map(|value| lossy(value).trim().to_owned())
            .unwrap_or_default()
    };
    let mut tool = json!({
        "type": "image_generation",
        "action": "edit",
        "model": tool_model(&field("model"), route_model),
    });
    for name in EDIT_STRINGS {
        let value = field(name);
        if !value.is_empty() {
            set(&mut tool, name, Value::from(value));
        }
    }
    for name in NUMBERS {
        if let Ok(number) = field(name).parse::<i64>() {
            set(&mut tool, name, Value::from(number));
        }
    }
    let images: Vec<String> = image_files(&form)
        .iter()
        .map(FileHeader::data_url)
        .collect();
    if let Some(mask) = form.files_of("mask").first() {
        set(
            &mut tool,
            "input_image_mask.image_url",
            Value::String(mask.data_url()),
        );
    }
    Ok(Prepared {
        body: responses_request(&field("prompt"), &images, tool),
        response_format: response_format(&field("response_format")),
        prefix: "image_edit",
    })
}

/// `url` when the client asked for URLs, else `b64_json`
/// (`codexNormalizeImageResponseFormat`).
fn response_format(format: &str) -> &'static str {
    if eq_fold(format.trim(), "url") {
        "url"
    } else {
        "b64_json"
    }
}

/// The request's model, else the call's, else `gpt-image-2`
/// (`codexOpenAIImageToolModel`).
fn tool_model(request_model: &str, route_model: &str) -> String {
    [request_model.trim(), route_model.trim()]
        .into_iter()
        .find(|model| !model.is_empty())
        .unwrap_or(DEFAULT_TOOL_MODEL)
        .to_owned()
}

/// The `image_generation` tool for a JSON request
/// (`codexBuildOpenAIImageTool`).
fn build_tool(payload: &Value, route_model: &str, action: &str, strings: &[&str]) -> Value {
    let mut tool = json!({
        "type": "image_generation",
        "action": action,
        "model": tool_model(&str_at(payload, "model"), route_model),
    });
    for field in strings {
        let value = str_at(payload, field);
        if !value.trim().is_empty() {
            set(&mut tool, field, Value::from(value.trim()));
        }
    }
    for field in NUMBERS {
        if let Some(number @ Value::Number(_)) = get(payload, field) {
            set(&mut tool, field, Value::from(int_of(Some(number))));
        }
    }
    tool
}

/// The Responses request that has the tool make the images
/// (`codexBuildImagesResponsesRequest`).
fn responses_request(prompt: &str, images: &[String], tool: Value) -> Value {
    let mut content = vec![json!({ "type": "input_text", "text": prompt })];
    content.extend(
        images
            .iter()
            .filter(|image| !image.trim().is_empty())
            .map(|image| json!({ "type": "input_image", "image_url": image })),
    );
    json!({
        "instructions": "",
        "stream": true,
        "reasoning": { "effort": "medium", "summary": "auto" },
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "model": MAIN_MODEL,
        "store": false,
        "tool_choice": { "type": "image_generation" },
        "tools": [tool],
        "input": [{ "type": "message", "role": "user", "content": content }],
    })
}

/// An image the tool made (`codexImageCallResult`).
#[derive(Debug, PartialEq)]
struct ImageCall {
    result: String,
    revised_prompt: String,
    output_format: String,
    size: String,
    background: String,
    quality: String,
}

/// What a completed event gives (`codexExtractImageResults`).
#[derive(Debug)]
struct Extracted {
    calls: Vec<ImageCall>,
    created: i64,
    usage: Option<Value>,
}

/// The images of a completed event: those of its `response.output`, else
/// those of the items collected from `response.output_item.done` events
/// (`codexExtractImageResults`).
fn extract(completed: &Value, items: &OutputItems) -> Result<Extracted, ExecError> {
    if str_at(completed, "type") != "response.completed" {
        return Err(failure("unexpected event type"));
    }
    let mut created = int_at(completed, "response.created_at");
    if created <= 0 {
        created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
    }
    let calls = match get(completed, "response.output") {
        Some(Value::Array(output)) if !output.is_empty() => {
            output.iter().filter_map(image_call).collect()
        }
        _ => items.items().filter_map(image_call).collect(),
    };
    let usage = get(completed, "response.tool_usage.image_gen")
        .filter(|usage| usage.is_object())
        .cloned();
    Ok(Extracted {
        calls,
        created,
        usage,
    })
}

/// An `image_generation_call` item with a result.
fn image_call(item: &Value) -> Option<ImageCall> {
    if str_at(item, "type") != "image_generation_call" {
        return None;
    }
    let field = |name: &str| str_at(item, name).trim().to_owned();
    let result = field("result");
    if result.is_empty() {
        return None;
    }
    Some(ImageCall {
        result,
        revised_prompt: field("revised_prompt"),
        output_format: field("output_format"),
        size: field("size"),
        background: field("background"),
        quality: field("quality"),
    })
}

/// The Images API's answer (`codexBuildImagesAPIResponse`).
fn build_response(extracted: &Extracted, format: &str) -> Value {
    let mut out = json!({ "created": extracted.created, "data": [] });
    if let Some(first) = extracted.calls.first() {
        for (name, value) in [
            ("background", &first.background),
            ("output_format", &first.output_format),
            ("quality", &first.quality),
            ("size", &first.size),
        ] {
            if !value.is_empty() {
                set(&mut out, name, Value::from(value.as_str()));
            }
        }
    }
    if let Some(usage) = &extracted.usage {
        set(&mut out, "usage", usage.clone());
    }
    let data = extracted
        .calls
        .iter()
        .map(|call| {
            let mut item = Map::new();
            if !call.revised_prompt.is_empty() {
                item.insert(
                    "revised_prompt".to_owned(),
                    Value::from(call.revised_prompt.as_str()),
                );
            }
            image_field(&mut item, format, &call.output_format, &call.result);
            Value::Object(item)
        })
        .collect();
    set(&mut out, "data", Value::Array(data));
    out
}

/// Adds an image as a data URL or as base64, as the client asked.
fn image_field(item: &mut Map<String, Value>, format: &str, output_format: &str, b64: &str) {
    if response_format(format) == "url" {
        item.insert(
            "url".to_owned(),
            Value::from(format!("data:{};base64,{b64}", mime_type(output_format))),
        );
    } else {
        item.insert("b64_json".to_owned(), Value::from(b64));
    }
}

/// `codexMimeTypeFromOutputFormat`.
fn mime_type(output_format: &str) -> &'static str {
    match json::lower_trim(output_format).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// The event for a partial image, if it has one
/// (`codexBuildImagePartialFrame`).
fn partial_frame(event: &Value, format: &str, prefix: &str) -> Option<Bytes> {
    let b64 = str_at(event, "partial_image_b64");
    let b64 = b64.trim();
    if b64.is_empty() {
        return None;
    }
    let name = format!("{}.partial_image", prefix.trim());
    let mut data = Map::new();
    data.insert("type".to_owned(), Value::from(name.as_str()));
    data.insert(
        "partial_image_index".to_owned(),
        Value::from(int_at(event, "partial_image_index")),
    );
    image_field(
        &mut data,
        format,
        str_at(event, "output_format").trim(),
        b64,
    );
    Some(sse_frame(&name, &Value::Object(data)))
}

/// The event for an image (`codexBuildImageCompletedFrame`).
fn completed_frame(call: &ImageCall, usage: Option<&Value>, format: &str, prefix: &str) -> Bytes {
    let name = format!("{}.completed", prefix.trim());
    let mut data = Map::new();
    data.insert("type".to_owned(), Value::from(name.as_str()));
    if let Some(usage) = usage {
        data.insert("usage".to_owned(), usage.clone());
    }
    image_field(&mut data, format, &call.output_format, &call.result);
    sse_frame(&name, &Value::Object(data))
}

/// `codexBuildSSEFrame`.
fn sse_frame(name: &str, data: &Value) -> Bytes {
    let mut frame = String::new();
    if !name.trim().is_empty() {
        frame.push_str("event: ");
        frame.push_str(name);
        frame.push('\n');
    }
    frame.push_str("data: ");
    frame.push_str(&data.to_string());
    frame.push_str("\n\n");
    Bytes::from(frame)
}

/// The tool call that made no image.
fn no_output() -> ExecError {
    StatusError::new(502, "upstream did not return image output").into()
}

/// `line` with the request's secrets redacted.
fn redact(secrets: &Secrets, line: Vec<u8>) -> Vec<u8> {
    match secrets.bytes(&line, Policy::Client) {
        std::borrow::Cow::Owned(redacted) => redacted,
        std::borrow::Cow::Borrowed(_) => line,
    }
}

/// The event of a `data:` line.
fn event_of(line: &[u8]) -> Option<Value> {
    let rest = line.strip_prefix(b"data:")?;
    Some(exact::from_slice(trim_space(rest)).unwrap_or(Value::Null))
}

impl CodexExecutor {
    /// `Execute` for the OpenAI Images endpoints (`executeOpenAIImage`).
    pub(super) async fn execute_image(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        if let Some((model, path)) = direct(request, options) {
            let (response, secrets) = self
                .send_direct(auth, request, options, &model, path, false)
                .await?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(self.status_failure(response, status, &secrets).await);
            }
            let headers = response.headers().clone();
            let data = read_body(response, MAX_LINE).await.map_err(|error| {
                ExecError::new(
                    ErrorKind::Upstream,
                    secrets.text(error.to_string(), Policy::Client),
                )
            })?;
            return Ok(Response {
                payload: Bytes::from(redact(&secrets, data)),
                headers,
            });
        }

        let prepared = prepare_tool_request(request, options)?;
        let (response, secrets) = self
            .send_tool(auth, request, options, &prepared, AttemptKind::Execute)
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.status_failure(response, status, &secrets).await);
        }
        let headers = response.headers().clone();
        let mut reader = LineReader::new(response);
        let mut items = OutputItems::default();
        while let Some(line) = reader.next_line().await {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    reader.report(&error);
                    return Err(ExecError::new(
                        ErrorKind::Upstream,
                        secrets.text(error.to_string(), Policy::Client),
                    ));
                }
            };
            let Some(event) = event_of(&redact(&secrets, line)) else {
                continue;
            };
            match str_at(&event, "type").as_str() {
                "response.output_item.done" => items.collect(&event),
                "response.completed" => {
                    let extracted = extract(&event, &items)?;
                    if extracted.calls.is_empty() {
                        return Err(no_output());
                    }
                    let out = build_response(&extracted, prepared.response_format);
                    return Ok(Response {
                        payload: Bytes::from(out.to_string()),
                        headers,
                    });
                }
                _ => {}
            }
        }
        Err(StatusError::new(504, "stream error: stream disconnected before completion").into())
    }

    /// `ExecuteStream` for the OpenAI Images endpoints
    /// (`executeOpenAIImageStream`).
    pub(super) async fn execute_image_stream(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        if let Some((model, path)) = direct(request, options) {
            let (response, secrets) = self
                .send_direct(auth, request, options, &model, path, true)
                .await?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(self.status_failure(response, status, &secrets).await);
            }
            return Ok(StreamResponse {
                headers: response.headers().clone(),
                chunks: images::raw_stream(response, secrets),
            });
        }

        let prepared = prepare_tool_request(request, options)?;
        let (response, secrets) = self
            .send_tool(auth, request, options, &prepared, AttemptKind::Stream)
            .await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.status_failure(response, status, &secrets).await);
        }
        let headers = response.headers().clone();
        let state = ToolStream {
            reader: LineReader::new(response),
            secrets,
            items: OutputItems::default(),
            format: prepared.response_format,
            prefix: prepared.prefix,
            pending: VecDeque::new(),
            done: false,
        };
        Ok(StreamResponse {
            headers,
            chunks: tool_stream(state),
        })
    }

    /// Sends an Image API call (`executeDirectOpenAIImage` up to the call).
    async fn send_direct(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        model: &str,
        path: &str,
        stream: bool,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        let (body, content_type) = direct_body(request, options, model, stream)?;
        let source = Options {
            source_format: Format::OPENAI_IMAGE,
            ..options.clone()
        };
        let (body, content_type) = payload::apply_media(
            self.config.as_deref(),
            &MediaTarget {
                executor: "codex",
                model,
                protocol: &Format::OPENAI,
            },
            request,
            &source,
            body,
            &content_type,
        )
        .map_err(|error| failure(error.to_string()))?;
        // `applyCodexDirectImageHeaders`: the client's headers without its
        // `User-Agent`.
        let mut client = options.headers.clone();
        client.remove(header::USER_AGENT);
        let mut headers = build_headers(auth, &client, stream)?;
        if !content_type.is_empty()
            && let Ok(value) = HeaderValue::from_bytes(content_type.as_bytes())
        {
            headers.insert(header::CONTENT_TYPE, value);
        }
        let (_, base) = credentials(auth);
        let base = if base.is_empty() {
            self.base_url.as_str()
        } else {
            base
        };
        let url = format!("{}{path}", base.strip_suffix('/').unwrap_or(base));
        let kind = if stream {
            AttemptKind::Stream
        } else {
            AttemptKind::Execute
        };
        self.send_bytes(
            auth,
            &url,
            headers,
            body,
            Attempt::new(options, kind, "codex", model, &Format::OPENAI_IMAGE, auth),
        )
        .await
    }

    /// Sends a tool call (`prepareCodexOpenAIImageBody` and the call).
    async fn send_tool(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        prepared: &Prepared,
        kind: AttemptKind,
    ) -> Result<(reqwest::Response, Secrets), ExecError> {
        let main = main_model(self.config.as_deref());
        let mut body = prepared.body.clone();
        thinking::apply_request(
            &mut body,
            Route {
                model: &main,
                from: Format::OPENAI_IMAGE.as_str(),
                to: Format::CODEX.as_str(),
                provider: "codex",
            },
            &json::Body::Json(prepared.body.clone()),
            &json::Body::Json(prepared.body.clone()),
            self.models.as_deref(),
        )?;
        set_string_if_different(&mut body, "model", &main);
        set_bool_if_different(&mut body, "stream", true);
        for field in [
            "previous_response_id",
            "prompt_cache_retention",
            "safety_identifier",
            "stream_options",
        ] {
            delete(&mut body, field);
        }
        normalize_instructions(&mut body, false);
        sanitize_input_item_ids(&mut body);
        // The rules' defaults check the request as the tool call has it.
        let source = Options {
            source_format: Format::OPENAI_IMAGE,
            ..options.clone()
        };
        let translate = |_: Value| prepared.body.clone();
        payload::apply(
            self.config.as_deref(),
            &Target {
                executor: "codex",
                protocol: &Format::CODEX,
                model: &main,
                root: "",
                stream: true,
                tracked: &[],
                translate: Some(&translate),
            },
            request,
            &source,
            &mut body,
        );
        let headers = build_headers(auth, &options.headers, true)?;
        let url = endpoint(auth, &self.base_url, false);
        self.send(
            auth,
            &url,
            headers,
            &body,
            Attempt::new(options, kind, "codex", &main, &Format::CODEX, auth),
        )
        .await
    }

    /// The error for an answer of `status`, not 2xx: its body, redacted,
    /// with the cooling the config asks for, or the error reading it.
    async fn status_failure(
        &self,
        response: reqwest::Response,
        status: u16,
        secrets: &Secrets,
    ) -> ExecError {
        let tap = BodyTap::of(&response);
        let (body, error) = read_body_prefix(response, MAX_ERROR_BODY).await;
        if let Some(error) = error {
            let error = ExecError::new(
                ErrorKind::Upstream,
                secrets.text(error_chain(&error.without_url()), Policy::Client),
            );
            observe_send::attempt_error(tap.as_ref(), &error);
            return error;
        }
        tracing::debug!(status, "codex: image request error");
        let body = secrets.bytes(&body, Policy::Client);
        status_error_with_cooling(status, &body, self.model_level_cooling()).into()
    }
}

/// Where a tool call's stream is.
struct ToolStream {
    reader: LineReader,
    secrets: Secrets,
    items: OutputItems,
    format: &'static str,
    prefix: &'static str,
    /// What is ready to pass on.
    pending: VecDeque<Result<Bytes, ExecError>>,
    /// Whether the completed event has been read.
    done: bool,
}

impl ToolStream {
    /// Reads a line of the stream.
    fn line(&mut self, line: Vec<u8>) {
        let Some(event) = event_of(&redact(&self.secrets, line)) else {
            return;
        };
        match str_at(&event, "type").as_str() {
            "response.output_item.done" => self.items.collect(&event),
            "response.image_generation_call.partial_image" => {
                if let Some(frame) = partial_frame(&event, self.format, self.prefix) {
                    self.pending.push_back(Ok(frame));
                }
            }
            "response.completed" => {
                self.done = true;
                match extract(&event, &self.items) {
                    Err(error) => self.pending.push_back(Err(error)),
                    Ok(extracted) if extracted.calls.is_empty() => {
                        self.pending.push_back(Err(no_output()));
                    }
                    Ok(extracted) => {
                        for call in &extracted.calls {
                            self.pending.push_back(Ok(completed_frame(
                                call,
                                extracted.usage.as_ref(),
                                self.format,
                                self.prefix,
                            )));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// A tool call's stream as Images API events: each partial image as it
/// comes, then each image when the call completes. It ends there, or when
/// the stream does.
fn tool_stream(state: ToolStream) -> ChunkStream {
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(next) = state.pending.pop_front() {
                return Some((next, state));
            }
            if state.done {
                return None;
            }
            match state.reader.next_line().await {
                None => return None,
                Some(Ok(line)) => state.line(line),
                Some(Err(error)) => {
                    state.reader.report(&error);
                    state.done = true;
                    let error = ExecError::new(
                        ErrorKind::Upstream,
                        state.secrets.text(error.to_string(), Policy::Client),
                    );
                    return Some((Err(error), state));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `imageGenItem`: a minimal `image_generation_call` item.
    fn image_gen_item(result: &str, format: &str) -> Value {
        json!({ "type": "image_generation_call", "result": result, "output_format": format })
    }

    /// Items collected from `response.output_item.done` events, each at its
    /// `output_index` if it has one.
    fn collected(items: &[(Option<i64>, Value)]) -> OutputItems {
        let mut collected = OutputItems::default();
        for (index, item) in items {
            let mut event = json!({ "type": "response.output_item.done", "item": item });
            if let Some(index) = index {
                set(&mut event, "output_index", Value::from(*index));
            }
            collected.collect(&event);
        }
        collected
    }

    fn results(extracted: &Extracted) -> Vec<&str> {
        extracted
            .calls
            .iter()
            .map(|call| call.result.as_str())
            .collect()
    }

    // Ported from upstream's
    // TestCodexExtractImageResults_FromCompletedOutput.
    #[test]
    fn extracts_from_completed_output() {
        let completed = json!({
            "type": "response.completed",
            "response": { "created_at": 111, "output": [image_gen_item("AAA", "png")] },
        });
        let extracted = extract(&completed, &OutputItems::default()).unwrap();
        assert_eq!(extracted.created, 111);
        assert_eq!(results(&extracted), ["AAA"]);
        assert_eq!(extracted.calls[0].output_format, "png");
    }

    // Ported from upstream's
    // TestCodexExtractImageResults_FallbackToCollectedItemsOrdered.
    #[test]
    fn falls_back_to_the_collected_items_in_order() {
        let completed = json!({
            "type": "response.completed",
            "response": { "created_at": 222, "output": [] },
        });
        let items = collected(&[
            (Some(2), image_gen_item("SECOND", "png")),
            (Some(0), image_gen_item("FIRST", "jpg")),
        ]);
        let extracted = extract(&completed, &items).unwrap();
        assert_eq!(extracted.created, 222);
        assert_eq!(results(&extracted), ["FIRST", "SECOND"]);
    }

    // Ported from upstream's
    // TestCodexExtractImageResults_PrefersCompletedOutputOverItems.
    #[test]
    fn prefers_the_completed_output_over_items() {
        let completed = json!({
            "type": "response.completed",
            "response": { "created_at": 333, "output": [image_gen_item("FROM_OUTPUT", "png")] },
        });
        let items = collected(&[(Some(0), image_gen_item("FROM_ITEMS", "png"))]);
        let extracted = extract(&completed, &items).unwrap();
        assert_eq!(results(&extracted), ["FROM_OUTPUT"]);
    }

    // Ported from upstream's TestCodexExtractImageResults_WrongEventType.
    #[test]
    fn refuses_other_event_types() {
        let event = json!({ "type": "response.in_progress" });
        assert!(extract(&event, &OutputItems::default()).is_err());
    }

    // Ported from upstream's TestCodexExtractImageResults_FallbackList.
    #[test]
    fn reads_items_without_an_index() {
        let completed = json!({
            "type": "response.completed",
            "response": { "created_at": 444 },
        });
        let items = collected(&[(None, image_gen_item("FB", "webp"))]);
        let extracted = extract(&completed, &items).unwrap();
        assert_eq!(results(&extracted), ["FB"]);
        assert_eq!(extracted.calls[0].output_format, "webp");
    }

    // Not upstream's: indexed items come before the others, results are
    // trimmed, items without one are skipped, a missing creation time is now,
    // and the answer takes its fields from the first image and its data
    // URLs' type from each image's format.
    #[test]
    fn builds_the_answer() {
        let mut first = image_gen_item(" one ", "webp");
        set(&mut first, "revised_prompt", Value::from("a fox"));
        set(&mut first, "size", Value::from("1024x1024"));
        let items = collected(&[
            (None, image_gen_item("three", "jpeg")),
            (Some(1), image_gen_item("  ", "png")),
            (Some(0), first),
            (Some(4), json!({ "type": "message", "result": "x" })),
        ]);
        let completed = json!({
            "type": "response.completed",
            "response": { "created_at": 0, "tool_usage": { "image_gen": { "images": 2 } } },
        });
        let extracted = extract(&completed, &items).unwrap();
        assert!(extracted.created > 0);
        assert_eq!(results(&extracted), ["one", "three"]);
        let mut out = build_response(&extracted, " URL ");
        set(&mut out, "created", Value::from(1));
        assert_eq!(
            out.to_string(),
            json!({
                "created": 1,
                "data": [
                    { "revised_prompt": "a fox", "url": "data:image/webp;base64,one" },
                    { "url": "data:image/jpeg;base64,three" },
                ],
                "output_format": "webp",
                "size": "1024x1024",
                "usage": { "images": 2 },
            })
            .to_string()
        );
        let out = build_response(&extracted, "b64_json");
        assert_eq!(get(&out, "data.1"), Some(&json!({ "b64_json": "three" })));
    }

    // Not upstream's: models are read without a suffix, a prefix or their
    // case, from the body first.
    #[test]
    fn direct_models() {
        let options = |path: &str| {
            let mut options = Options::new(Format::OPENAI_IMAGE);
            options.metadata.request_path = path.to_owned();
            options
        };
        let request = |model: &str, payload: &str| Request {
            model: model.to_owned(),
            payload: Bytes::from(payload.to_owned()),
        };
        assert_eq!(
            image_base_model("codex/GPT-Image-2.5-Flare(high)"),
            "gpt-image-2.5-flare"
        );
        assert_eq!(image_base_model("codex/"), "codex/");
        assert_eq!(
            direct(
                &request("alias", r#"{"model":"gpt-image-1.5"}"#),
                &options(EDITS)
            ),
            Some(("gpt-image-1.5".to_owned(), DIRECT_EDITS))
        );
        assert_eq!(
            direct(&request("gpt-image-2", "not json"), &options(GENERATIONS)),
            Some(("gpt-image-2".to_owned(), DIRECT_GENERATIONS))
        );
        assert_eq!(
            direct(&request("dall-e-3", "{}"), &options(GENERATIONS)),
            None
        );
        assert!(is_image_request(&options(" /v1/images/edits ")));
        assert!(!is_image_request(&options("/v1/images/variations")));
    }

    // Not upstream's: a form field sent more than once becomes an array, the
    // integer fields read as integers when they are, and the mask fields'
    // names become paths.
    #[test]
    fn form_values() {
        let mut out = json!({});
        set_form_values(&mut out, "n", &[Bytes::from_static(b" 2 ")]);
        set_form_values(&mut out, "partial_images", &[Bytes::from_static(b"x")]);
        set_form_values(&mut out, "mask[file_id]", &[Bytes::from_static(b"file-1")]);
        set_form_values(
            &mut out,
            "output_compression",
            &[Bytes::from_static(b"1"), Bytes::from_static(b"y")],
        );
        assert_eq!(
            out,
            json!({
                "n": 2,
                "partial_images": "x",
                "mask": { "file_id": "file-1" },
                "output_compression": [1, "y"],
            })
        );
    }

    // Not upstream's: the frames a stream gives.
    #[test]
    fn frames() {
        let partial = json!({
            "partial_image_b64": " AA== ",
            "partial_image_index": 1,
            "output_format": "jpeg",
        });
        assert_eq!(
            &partial_frame(&partial, "url", " image_edit ").unwrap()[..],
            b"event: image_edit.partial_image\ndata: {\"type\":\"image_edit.partial_image\",\"partial_image_index\":1,\"url\":\"data:image/jpeg;base64,AA==\"}\n\n"
        );
        assert_eq!(partial_frame(&json!({}), "url", "image_edit"), None);
        let call = image_call(&image_gen_item("BB==", "png")).unwrap();
        assert_eq!(
            &completed_frame(&call, Some(&json!({"images": 1})), "b64_json", "image_generation")[..],
            b"event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"usage\":{\"images\":1},\"b64_json\":\"BB==\"}\n\n"
        );
    }
}
