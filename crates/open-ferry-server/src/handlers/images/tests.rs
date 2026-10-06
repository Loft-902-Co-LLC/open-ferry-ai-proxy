// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_images_handlers_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The images endpoints, their request builders and their answers.
//!
//! Changed from upstream:
//! - The handler tests go through the router, with the test key; upstream
//!   calls the handlers alone.
//! - `TestImagesModelValidationAllowsOpenAICompatImageModels` registers its
//!   models in a fake catalog rather than the global registry.
//! - `TestForwardRawImageStreamPrefersPendingErrorOnClose` sends a stream
//!   whose error follows a payload once; upstream races its channels 100
//!   times.
//! - The tests of the Responses-tool fallback (`TestSSEFrameAccumulator*`,
//!   `TestCollectImages*` and `TestForwardImagesStreamCancelsWithPayloadError`)
//!   aren't ported, since the fallback isn't.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::config::DisableImageGeneration;
use open_ferry_core::exec::{ExecError, Format, Response as ExecResponse};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::multipart::{
    Form, Header, MAX_FORM_MEMORY, Reader, Writer, file_content_disposition, parse_media_type,
};
use open_ferry_core::registry::registration::OPENAI_IMAGE_MODEL_TYPE;
use tower::ServiceExt;

use super::xai::{self, Options};
use super::{
    compat_form_request, compat_json_request, error_event, model_parts, parse_bool, parse_int,
    reject,
};
use crate::config::{ServerConfig, StreamingConfig};
use crate::errors::ErrorMessage;
use crate::json;
use crate::router;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

const GENERATIONS: &str = "/v1/images/generations";
const EDITS: &str = "/v1/images/edits";

/// The eight bytes every PNG starts with.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// [`PNG`] in base64.
const PNG_BASE64: &str = "iVBORw0KGgo=";

/// An `openai-compatibility` image model named `id`.
fn image_model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        object: "model".into(),
        owned_by: "compat".into(),
        model_type: OPENAI_IMAGE_MODEL_TYPE.into(),
        ..ModelInfo::default()
    }
}

/// An `openai-compatibility` chat model named `id`.
fn chat_model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        object: "model".into(),
        owned_by: "compat".into(),
        model_type: "openai-compatibility".into(),
        ..ModelInfo::default()
    }
}

/// A server with `outcomes` and the key `sk-test`, serving `gpt-image-2`
/// through `codex`, `grok-imagine-image` through `xai` and the
/// `openai-compatibility` image model `compat-image` through `compat`.
fn server(config: ServerConfig, outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    let catalog = FakeCatalog::new()
        .serve("gpt-image-2", &["codex"])
        .serve("grok-imagine-image", &["xai"])
        .serve("compat-image", &["compat"])
        .info(image_model("compat-image"));
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..config
    };
    (router(state(config, catalog, &dispatcher)), dispatcher)
}

fn app(outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    server(ServerConfig::default(), outcomes)
}

/// The settings with `passthrough-headers` on.
fn passthrough() -> ServerConfig {
    ServerConfig {
        passthrough_headers: true,
        ..ServerConfig::default()
    }
}

/// The settings with a keep-alive every `seconds` in a stream, and
/// `passthrough-headers` on.
fn keepalive(seconds: u64) -> ServerConfig {
    ServerConfig {
        passthrough_headers: true,
        streaming: StreamingConfig {
            keepalive: Some(Duration::from_secs(seconds)),
            ..StreamingConfig::default()
        },
        ..ServerConfig::default()
    }
}

/// A POST with the test key, and `content_type` if any.
fn post(uri: &str, content_type: Option<&str>, body: impl Into<Body>) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer sk-test");
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    request.body(body.into()).unwrap()
}

/// A JSON POST with the test key.
fn post_json(uri: &str, body: &str) -> Request<Body> {
    post(uri, Some("application/json"), body.to_owned())
}

/// A form of `fields`, then of `files` as their field names, file names,
/// content types if any and data; and its content type.
fn form(fields: &[(&str, &str)], files: &[(&str, &str, Option<&str>, &[u8])]) -> (Vec<u8>, String) {
    let mut writer = Writer::new();
    for (name, value) in fields {
        writer.write_field(name, value.as_bytes());
    }
    for (name, filename, content_type, data) in files {
        let mut header = Header::new();
        header.set(
            "Content-Disposition",
            file_content_disposition(name, filename),
        );
        if let Some(content_type) = content_type {
            header.set("Content-Type", *content_type);
        }
        writer.write_part(&header, data);
    }
    let content_type = writer.form_data_content_type();
    (writer.finish(), content_type)
}

/// A form POST of `fields` and `files` with the test key.
fn post_form(
    fields: &[(&str, &str)],
    files: &[(&str, &str, Option<&str>, &[u8])],
) -> Request<Body> {
    let (body, content_type) = form(fields, files);
    post(EDITS, Some(&content_type), body)
}

/// `body` read as a form whose content type is `content_type`.
fn read_form(body: &[u8], content_type: &[u8]) -> Form {
    let (kind, params) = parse_media_type(content_type).unwrap();
    assert_eq!(kind, "multipart/form-data");
    Reader::new(Bytes::copy_from_slice(body), &params["boundary"])
        .read_form(MAX_FORM_MEMORY)
        .unwrap()
}

/// Every value of field `name` in `form`.
fn values<'a>(form: &'a Form, name: &str) -> Vec<&'a [u8]> {
    form.values()
        .filter(|(field, _)| *field == name)
        .flat_map(|(_, values)| values.iter().map(|value| &value[..]))
        .collect()
}

/// The status, headers and body of `request`'s response.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .map_or("", |v| v.to_str().unwrap())
}

/// The 400 `invalid_request_error` with `message`.
fn invalid(message: &str) -> String {
    format!(r#"{{"error":{{"message":"{message}","type":"invalid_request_error"}}}}"#)
}

/// The 400 for an unsupported `model` (upstream's
/// `assertUnsupportedImagesModelResponse`).
fn unsupported(model: &str) -> String {
    invalid(&format!(
        "Model {model} is not supported on /v1/images/generations or /v1/images/edits. Use \
         gpt-image-1.5, gpt-image-2, gpt-image-2.5-flare, gpt-image-2.5-sunburst, gpt-image-2.5, \
         grok-imagine-image, grok-imagine-image-quality, grok-imagine-image-2.0, or a configured \
         openai-compatibility image model."
    ))
}

/// An answer of `body` with `headers`.
fn reply_with(body: &str, headers: &[(&'static str, &'static str)]) -> Outcome {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(*name, HeaderValue::from_static(value));
    }
    Outcome::Reply(ExecResponse {
        payload: Bytes::copy_from_slice(body.as_bytes()),
        headers: map,
    })
}

// Ports TestImagesModelValidationAllowsGPTImageAndXAIModels.
#[test]
fn images_model_validation_allows_gpt_image_and_xai_models() {
    let state = state(
        ServerConfig::default(),
        FakeCatalog::new(),
        &FakeDispatcher::new([]),
    );
    for model in [
        "gpt-image-1.5",
        "codex/gpt-image-1.5",
        "gpt-image-2",
        "codex/gpt-image-2",
        "gpt-image-2.5-flare",
        "codex/gpt-image-2.5-flare",
        "gpt-image-2.5-sunburst",
        "codex/gpt-image-2.5-sunburst",
        "gpt-image-2.5",
        "codex/gpt-image-2.5",
        "grok-imagine-image",
        "xai/grok-imagine-image",
        "grok-imagine-image-quality",
        "xai/grok-imagine-image-quality",
        "grok-imagine-image-2.0",
        "xai/grok-imagine-image-2.0",
    ] {
        assert!(reject(&state, model).is_none(), "{model}");
    }
    assert!(reject(&state, "gpt-5.4-mini").is_some());
    assert!(reject(&state, "codex/grok-imagine-image").is_some());
}

// Ports TestImagesModelValidationAllowsOpenAICompatImageModels.
#[test]
fn images_model_validation_allows_openai_compat_image_models() {
    let catalog = FakeCatalog::new()
        .info(image_model("compat-image-model"))
        .info(chat_model("compat-chat-model"));
    let state = state(ServerConfig::default(), catalog, &FakeDispatcher::new([]));
    assert!(reject(&state, "compat-image-model").is_none());
    assert!(reject(&state, "compat-chat-model").is_some());
}

// Ports TestCanonicalXAIImagesModelPreservesImage20.
#[test]
fn canonical_xai_images_model_preserves_image_2_0() {
    for model in [
        "grok-imagine-image-2.0",
        "xai/grok-imagine-image-2.0",
        "XAI/Grok-Imagine-Image-2.0",
    ] {
        assert_eq!(
            xai::canonical_model(model),
            "grok-imagine-image-2.0",
            "{model}"
        );
    }
}

// Ports TestBuildXAIImagesGenerationsRequest.
#[test]
fn build_xai_images_generations_request() {
    let raw = br#"{"model":"xai/grok-imagine-image-quality","prompt":"abstract art","aspect_ratio":"landscape","resolution":"2k","n":2,"response_format":"url"}"#;
    let req = xai::generations_request(raw, "xai/grok-imagine-image-quality", "url");
    assert_eq!(json::str_at(&req, "model"), "grok-imagine-image-quality");
    assert_eq!(json::str_at(&req, "prompt"), "abstract art");
    assert_eq!(json::str_at(&req, "aspect_ratio"), "16:9");
    assert_eq!(json::str_at(&req, "resolution"), "2k");
    assert_eq!(json::str_at(&req, "response_format"), "url");
    assert_eq!(json::get(&req, "n").unwrap().int(), 2);
}

// Ports TestBuildXAIImagesGenerationsRequestPreservesQuality.
#[test]
fn build_xai_images_generations_request_preserves_quality() {
    for (name, raw, want) in [
        (
            "explicit medium",
            r#"{"model":"grok-imagine-image-2.0","prompt":"circle","quality":"medium"}"#,
            Some("medium"),
        ),
        (
            "explicit low",
            r#"{"model":"grok-imagine-image-2.0","prompt":"circle","quality":"low"}"#,
            Some("low"),
        ),
        (
            "trimmed whitespace",
            r#"{"model":"grok-imagine-image-2.0","prompt":"circle","quality":"  high  "}"#,
            Some("high"),
        ),
        (
            "empty quality omitted",
            r#"{"model":"grok-imagine-image-2.0","prompt":"circle","quality":""}"#,
            None,
        ),
        (
            "blank quality omitted",
            r#"{"model":"grok-imagine-image-2.0","prompt":"circle","quality":"   "}"#,
            None,
        ),
        (
            "omitted quality omitted",
            r#"{"model":"grok-imagine-image-2.0","prompt":"circle"}"#,
            None,
        ),
    ] {
        let req = xai::generations_request(raw.as_bytes(), "grok-imagine-image-2.0", "b64_json");
        let quality = json::get(&req, "quality").map(|q| q.str());
        assert_eq!(quality.as_deref(), want, "{name}");
    }
}

// Ports TestBuildXAIImagesGenerationsRequestNineByTwenty,
// TestBuildXAIImagesGenerationsRequestNineByTwentyFromSize,
// TestBuildXAIImagesGenerationsRequestTwentyByNine and
// TestBuildXAIImagesGenerationsRequestTwentyByNineFromSize.
#[test]
fn build_xai_images_generations_request_nine_by_twenty_and_twenty_by_nine() {
    for (raw, want) in [
        (
            r#"{"model":"grok-imagine-image-quality","prompt":"kitten","aspect_ratio":"9:20","resolution":"2k","n":1,"response_format":"b64_json"}"#,
            "9:20",
        ),
        (
            r#"{"model":"grok-imagine-image-quality","prompt":"kitten","size":"9:20","resolution":"2k"}"#,
            "9:20",
        ),
        (
            r#"{"model":"grok-imagine-image-quality","prompt":"kitten","aspect_ratio":"20:9","resolution":"2k","n":1,"response_format":"b64_json"}"#,
            "20:9",
        ),
        (
            r#"{"model":"grok-imagine-image-quality","prompt":"kitten","size":"20:9","resolution":"2k"}"#,
            "20:9",
        ),
    ] {
        let req =
            xai::generations_request(raw.as_bytes(), "grok-imagine-image-quality", "b64_json");
        assert_eq!(json::str_at(&req, "aspect_ratio"), want, "{raw}");
        assert_eq!(json::str_at(&req, "resolution"), "2k", "{raw}");
    }
}

// Ports TestXAIImagesAspectRatioNineByTwenty.
#[test]
fn xai_images_aspect_ratio_nine_by_twenty() {
    assert_eq!(xai::aspect_ratio("9:20", "1:1"), "9:20");
    assert_eq!(xai::aspect_ratio("20:9", "1:1"), "20:9");
    assert_eq!(xai::aspect_ratio("9:21", "1:1"), "1:1");
    assert_eq!(xai::aspect_ratio_from_size("9:20", ""), "9:20");
    assert_eq!(xai::aspect_ratio_from_size("20:9", ""), "20:9");
}

// Ports TestBuildXAIImagesEditRequest.
#[test]
fn build_xai_images_edit_request() {
    let images = [
        "data:image/png;base64,AA==".to_owned(),
        "https://example.com/image.png".to_owned(),
    ];
    let options = Options {
        aspect_ratio: "3:2".into(),
        resolution: "1k".into(),
        quality: "medium".into(),
        n: 0,
    };
    let req = xai::edit_request(
        "grok-imagine-image",
        "edit it",
        &images,
        "b64_json",
        &options,
    );
    assert_eq!(json::str_at(&req, "model"), "grok-imagine-image");
    assert_eq!(json::str_at(&req, "quality"), "medium");
    let refs: Vec<_> = json::get(&req, "images")
        .unwrap()
        .array()
        .into_iter()
        .collect();
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].get("type").unwrap().str(), "image_url");
    assert_eq!(
        refs[0].get("url").unwrap().str(),
        "data:image/png;base64,AA=="
    );
    assert_eq!(
        refs[1].get("url").unwrap().str(),
        "https://example.com/image.png"
    );
    assert!(
        json::get(&req, "image").is_none(),
        "{}",
        String::from_utf8_lossy(&req)
    );
}

// Ports TestBuildXAIImagesEditRequestSingleImage.
#[test]
fn build_xai_images_edit_request_single_image() {
    let images = ["https://example.com/image.png".to_owned()];
    let req = xai::edit_request(
        "grok-imagine-image",
        "edit it",
        &images,
        "url",
        &Options::default(),
    );
    assert_eq!(json::str_at(&req, "image.type"), "image_url");
    assert_eq!(
        json::str_at(&req, "image.url"),
        "https://example.com/image.png"
    );
    assert!(json::get(&req, "quality").is_none());
    assert!(json::get(&req, "images").is_none());
}

// Ports TestXAIImagesEditOptionsFromJSONPreservesQuality.
#[test]
fn xai_images_edit_options_from_json_preserves_quality() {
    let options = xai::edit_options_from_json(
        br#"{"quality":"low","size":"1024x1024","resolution":"1k","n":1}"#,
    );
    assert_eq!(
        options,
        Options {
            aspect_ratio: "1:1".into(),
            resolution: "1k".into(),
            quality: "low".into(),
            n: 1,
        }
    );
    assert_eq!(xai::edit_options_from_json(br#"{"n":1}"#).quality, "");
}

// Ports TestBuildOpenAICompatImagesJSONRequestPreservesStreamForStreaming.
#[test]
fn build_openai_compat_images_json_request_preserves_stream_for_streaming() {
    let req = compat_json_request(
        br#"{"model":"compat-image","prompt":"draw","stream":false}"#,
        "upstream-image",
        true,
    );
    assert_eq!(
        &req[..],
        br#"{"model":"upstream-image","prompt":"draw","stream":true}"#
    );
}

// Ports TestBuildOpenAICompatImagesJSONRequestDropsStreamForNonStreaming.
#[test]
fn build_openai_compat_images_json_request_drops_stream_for_non_streaming() {
    let req = compat_json_request(
        br#"{"model":"compat-image","prompt":"draw","stream":true}"#,
        "upstream-image",
        false,
    );
    assert_eq!(&req[..], br#"{"model":"upstream-image","prompt":"draw"}"#);
}

// Ports TestBuildOpenAICompatImagesMultipartRequestPreservesStreamAndFileContentType.
#[test]
fn build_openai_compat_images_multipart_request_preserves_stream_and_file_content_type() {
    let (body, content_type) = form(
        &[
            ("model", "compat-image"),
            ("stream", "false"),
            ("prompt", "edit"),
        ],
        &[("image", "image.png", Some("image/png"), b"png-data")],
    );
    let source = read_form(&body, content_type.as_bytes());

    let (out, content_type) = compat_form_request(&source, "upstream-image", true);
    let rewritten = read_form(&out, content_type.as_bytes());
    assert_eq!(values(&rewritten, "model"), [b"upstream-image"]);
    assert_eq!(values(&rewritten, "stream"), [b"true"]);
    assert_eq!(values(&rewritten, "prompt"), [b"edit"]);
    let files = rewritten.files_of("image");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].header.get_str("Content-Type"), "image/png");
    assert_eq!(files[0].filename, "image.png");
    assert_eq!(&files[0].data[..], b"png-data");
}

// Not upstream's: a file with no content type is sent as
// `application/octet-stream`, and `stream` is left out of a form that
// doesn't stream.
#[test]
fn compat_multipart_request_names_a_content_type_for_every_file() {
    let (body, content_type) = form(
        &[("prompt", "edit"), ("stream", "true")],
        &[("image[]", "a.png", None, PNG)],
    );
    let source = read_form(&body, content_type.as_bytes());

    let (out, content_type) = compat_form_request(&source, "compat-image", false);
    let rewritten = read_form(&out, content_type.as_bytes());
    assert_eq!(values(&rewritten, "model"), [b"compat-image"]);
    assert!(values(&rewritten, "stream").is_empty());
    let files = rewritten.files_of("image[]");
    assert_eq!(
        files[0].header.get_str("Content-Type"),
        "application/octet-stream"
    );
}

// Ports TestBuildImagesAPIResponseFromXAI.
#[test]
fn build_images_api_response_from_xai() {
    let payload = br#"{"created":123,"data":[{"b64_json":"AA==","revised_prompt":"refined","mime_type":"image/png"}],"usage":{"total_tokens":0}}"#;
    let out = xai::images_api_response(payload, "b64_json").unwrap();
    assert_eq!(
        &out[..],
        br#"{"created":123,"data":[{"b64_json":"AA==","revised_prompt":"refined"}],"usage":{"total_tokens":0}}"#
    );
}

// Not upstream's: an answer with no images, or that isn't JSON, is turned
// down.
#[test]
fn images_api_response_needs_images() {
    assert_eq!(
        xai::images_api_response(br#"{"data":[{"url":" "}]}"#, "url"),
        Err("upstream did not return image output")
    );
    assert_eq!(
        xai::images_api_response(b"{", "url"),
        Err("upstream returned invalid image response JSON")
    );
}

// Not upstream's: the field parsers and model names.
#[test]
fn parses_fields_and_model_names() {
    for raw in ["1", " TRUE ", "yes", "On"] {
        assert!(parse_bool(raw, false), "{raw}");
    }
    for raw in ["0", "false", "No", " off"] {
        assert!(!parse_bool(raw, true), "{raw}");
    }
    assert!(parse_bool("maybe", true));
    assert!(!parse_bool("", false));
    assert_eq!(parse_int(" 3 ", 0), 3);
    assert_eq!(parse_int("-2", 0), -2);
    assert_eq!(parse_int("2.5", 7), 7);
    assert_eq!(parse_int("", 7), 7);
    assert_eq!(model_parts(" codex/gpt-image-2 "), ("codex", "gpt-image-2"));
    assert_eq!(model_parts("a/b/c"), ("a/b", "c"));
    assert_eq!(model_parts("gpt-image-2/"), ("", "gpt-image-2/"));
    assert_eq!(model_parts("gpt-image-2"), ("", "gpt-image-2"));
}

// Ports TestImagesGenerationsRejectsUnsupportedModel.
#[tokio::test]
async fn images_generations_rejects_unsupported_model() {
    let (app, dispatcher) = app(vec![]);
    let request = post_json(
        GENERATIONS,
        r#"{"model":"gpt-5.4-mini","prompt":"draw a square"}"#,
    );
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, unsupported("gpt-5.4-mini"));
    assert!(dispatcher.calls().is_empty());
}

// Ports TestImagesEditsJSONRejectsUnsupportedModel.
#[tokio::test]
async fn images_edits_json_rejects_unsupported_model() {
    let (app, _) = app(vec![]);
    let request = post_json(
        EDITS,
        r#"{"model":"gpt-5.4-mini","prompt":"edit this","images":[{"image_url":"data:image/png;base64,AA=="}]}"#,
    );
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, unsupported("gpt-5.4-mini"));
}

// Ports TestImagesEditsMultipartRejectsUnsupportedModel.
#[tokio::test]
async fn images_edits_multipart_rejects_unsupported_model() {
    let (app, _) = app(vec![]);
    let request = post_form(&[("model", "gpt-5.4-mini"), ("prompt", "edit this")], &[]);
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, unsupported("gpt-5.4-mini"));
}

// Ports TestImagesGenerations_DisableImageGeneration_Returns404 and
// TestImagesEdits_DisableImageGeneration_Returns404.
#[tokio::test]
async fn disable_image_generation_returns_404() {
    let config = ServerConfig {
        disable_image_generation: DisableImageGeneration::All,
        ..ServerConfig::default()
    };
    let (app, _) = server(config, vec![]);
    for (uri, body) in [
        (GENERATIONS, r#"{"prompt":"draw a square"}"#),
        (
            EDITS,
            r#"{"prompt":"edit this","images":[{"image_url":"data:image/png;base64,AA=="}]}"#,
        ),
    ] {
        let (status, headers, body) = send(&app, post_json(uri, body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body, "", "{uri}");
        assert_eq!(content_type(&headers), "", "{uri}");
    }
}

// Ports TestImagesGenerations_DisableImageGenerationChat_DoesNotReturn404 and
// TestImagesEdits_DisableImageGenerationChat_DoesNotReturn404.
#[tokio::test]
async fn disable_image_generation_chat_does_not_return_404() {
    let config = ServerConfig {
        disable_image_generation: DisableImageGeneration::Chat,
        ..ServerConfig::default()
    };
    let (app, _) = server(config, vec![]);
    for (uri, body) in [
        (
            GENERATIONS,
            r#"{"model":"gpt-5.4-mini","prompt":"draw a square"}"#,
        ),
        (
            EDITS,
            r#"{"model":"gpt-5.4-mini","prompt":"edit this","images":[{"image_url":"data:image/png;base64,AA=="}]}"#,
        ),
    ] {
        let (status, _, _) = send(&app, post_json(uri, body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

// Not upstream's: the endpoints need a key even when they are taken away.
#[tokio::test]
async fn disabled_endpoints_still_need_a_key() {
    let config = ServerConfig {
        disable_image_generation: DisableImageGeneration::All,
        ..ServerConfig::default()
    };
    let (app, _) = server(config, vec![]);
    let request = Request::builder()
        .method(Method::POST)
        .uri(GENERATIONS)
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// Ports TestWriteImagesStreamErrorEventSanitizesPayload.
#[test]
fn write_images_stream_error_event_sanitizes_payload() {
    let raw = format!(
        r#"{{"error":{{"code":"upstream_failed","message":"token=image-secret"}},"debug":"{}"}}"#,
        "x".repeat(8192)
    );
    let event = error_event(ErrorMessage::new(502, raw));
    let body = String::from_utf8(event.to_vec()).unwrap();
    assert!(!body.contains("image-secret"), "{body}");
    assert!(body.len() <= 4096, "{}", body.len());
    assert!(body.contains("[REDACTED]"), "{body}");
    assert!(body.starts_with("event: error\ndata: {"), "{body}");
    assert!(body.ends_with("}\n\n"), "{body}");
}

// Ports TestForwardRawImageStreamPrefersPendingErrorOnClose: an error after
// a payload is written as an event.
#[tokio::test]
async fn forward_raw_image_stream_writes_an_error_after_a_payload() {
    let chunks = vec![
        Ok(Bytes::from_static(b"data: {\"partial\":1}\n\n")),
        Err(ExecError::upstream(429, "image upstream busy")),
    ];
    let (app, _) = app(vec![Outcome::Stream(HeaderMap::new(), chunks)]);
    let request = post_json(
        GENERATIONS,
        r#"{"model":"gpt-image-2","prompt":"draw","stream":true}"#,
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert!(
        body.starts_with("data: {\"partial\":1}\n\nevent: error\ndata: {"),
        "{body}"
    );
    assert!(body.contains("image upstream busy"), "{body}");
}

// Not upstream's: a Codex generation goes to Codex as the client sent it,
// with `stream` taken out, never on a free credential, and its answer comes
// back as it came.
#[tokio::test]
async fn codex_generation_is_sent_on() {
    let (app, dispatcher) = server(
        passthrough(),
        vec![reply_with(
            r#"{"created":1,"data":[{"b64_json":"AA=="}]}"#,
            &[("x-upstream", "1")],
        )],
    );
    let request = post_json(
        GENERATIONS,
        r#"{"model":"gpt-image-2","prompt":"draw","stream":false,"size":"1024x1024"}"#,
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"created":1,"data":[{"b64_json":"AA=="}]}"#);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(headers["x-upstream"], "1");

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["codex"]);
    assert_eq!(call.request.model, "gpt-image-2");
    assert_eq!(
        &call.request.payload[..],
        br#"{"model":"gpt-image-2","prompt":"draw","size":"1024x1024"}"#
    );
    assert_eq!(call.options.source_format, Format::OPENAI_IMAGE);
    assert!(call.options.metadata.disallow_free_auth);
    assert!(!call.options.stream);
    assert_eq!(call.options.metadata.request_path, GENERATIONS);
}

// Not upstream's: a request that names no model asks for `gpt-image-2`.
#[tokio::test]
async fn codex_is_the_default() {
    let (app, dispatcher) = app(vec![Outcome::reply("{}")]);
    let (status, _, _) = send(&app, post_json(GENERATIONS, r#"{"prompt":"draw"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    let calls = dispatcher.calls();
    assert_eq!(
        &calls[0].request.payload[..],
        br#"{"prompt":"draw","model":"gpt-image-2"}"#
    );
}

// Not upstream's: a Codex stream comes back as it came, and one that ends
// with nothing writes a newline.
#[tokio::test]
async fn codex_streams_are_sent_back_as_they_came() {
    let (app, dispatcher) = app(vec![
        Outcome::chunks(&[
            "event: image_generation.partial_image\ndata: {\"a\":1}\n\n",
            "event: image_generation.completed\ndata: {\"b\":2}\n\n",
        ]),
        Outcome::chunks(&[]),
    ]);
    let body = r#"{"model":"gpt-image-2","prompt":"draw","stream":true}"#;
    let (status, headers, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(
        text,
        "event: image_generation.partial_image\ndata: {\"a\":1}\n\n\
         event: image_generation.completed\ndata: {\"b\":2}\n\n"
    );
    let (status, _, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, "\n");

    let calls = dispatcher.calls();
    assert_eq!(calls[0].method, "execute_stream");
    assert!(calls[0].options.stream);
    assert!(calls[0].options.metadata.disallow_free_auth);
    assert_eq!(&calls[0].request.payload[..], body.as_bytes());
}

// Not upstream's: a stream that fails before its first payload answers
// with the error.
#[tokio::test]
async fn a_stream_that_fails_first_answers_with_the_error() {
    let chunks = vec![Err(ExecError::upstream(429, "image upstream busy"))];
    let (app, _) = app(vec![Outcome::Stream(HeaderMap::new(), chunks)]);
    let request = post_json(
        GENERATIONS,
        r#"{"model":"gpt-image-2","prompt":"draw","stream":true}"#,
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(content_type(&headers), "application/json");
    assert!(body.contains("image upstream busy"), "{body}");
}

// Not upstream's: while a stream waits for its first payload it writes
// keep-alives, without the provider's headers (see the module docs), and
// dropping the response cancels the call.
#[tokio::test(start_paused = true)]
async fn a_waiting_stream_writes_keep_alives() {
    let mut upstream = HeaderMap::new();
    upstream.insert("x-upstream", HeaderValue::from_static("1"));
    let (app, dispatcher) = server(keepalive(10), vec![Outcome::Hang(upstream, vec![])]);
    let request = post_json(
        GENERATIONS,
        r#"{"model":"gpt-image-2","prompt":"draw","stream":true}"#,
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(response.headers()), "text/event-stream");
    assert!(response.headers().get("x-upstream").is_none());
    let mut body = response.into_body();
    for _ in 0..2 {
        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), ": keep-alive\n\n");
    }
    assert_eq!(dispatcher.live_streams(), 1);
    drop(body);
    assert_eq!(dispatcher.live_streams(), 0);
}

// Not upstream's: a Codex form edit goes on as a form written again, with
// its own content type.
#[tokio::test]
async fn codex_form_edits_are_written_again() {
    let (app, dispatcher) = app(vec![Outcome::reply(r#"{"data":[]}"#)]);
    let request = post_form(
        &[
            ("prompt", "edit"),
            ("model", "gpt-image-2"),
            ("size", "1024x1024"),
        ],
        &[("image", "cat.png", Some("image/png"), PNG)],
    );
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"data":[]}"#);

    let calls = dispatcher.calls();
    let call = &calls[0];
    assert_eq!(call.method, "execute");
    assert!(call.options.metadata.disallow_free_auth);
    assert_eq!(call.options.metadata.request_path, EDITS);
    let content_type = call.options.headers[header::CONTENT_TYPE].as_bytes();
    let sent = read_form(&call.request.payload, content_type);
    assert_eq!(values(&sent, "model"), [b"gpt-image-2"]);
    assert_eq!(values(&sent, "prompt"), [b"edit"]);
    assert_eq!(values(&sent, "size"), [b"1024x1024"]);
    assert!(values(&sent, "stream").is_empty());
    let files = sent.files_of("image");
    assert_eq!(files[0].header.get_str("Content-Type"), "image/png");
    assert_eq!(&files[0].data[..], PNG);
    let text = String::from_utf8_lossy(&call.request.payload);
    let model = text.find("name=\"model\"").unwrap();
    assert!(model < text.find("name=\"prompt\"").unwrap(), "{text}");
}

// Not upstream's: an xAI generation becomes an xAI request, and xAI's
// answer comes back in the OpenAI images API's shape.
#[tokio::test]
async fn xai_generations_are_answered_in_openai_shape() {
    let (app, dispatcher) = server(
        passthrough(),
        vec![reply_with(
            r#"{"created":5,"data":[{"url":"https://img.test/1.png","revised_prompt":"a cat"}],"usage":{"total_tokens":1}}"#,
            &[("x-upstream", "1")],
        )],
    );
    let request = post_json(
        GENERATIONS,
        r#"{"model":"grok-imagine-image","prompt":" cat ","response_format":"url"}"#,
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"created":5,"data":[{"url":"https://img.test/1.png","revised_prompt":"a cat"}],"usage":{"total_tokens":1}}"#
    );
    assert_eq!(headers["x-upstream"], "1");

    let calls = dispatcher.calls();
    let call = &calls[0];
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["xai"]);
    assert!(!call.options.metadata.disallow_free_auth);
    assert_eq!(
        &call.request.payload[..],
        br#"{"model":"grok-imagine-image","prompt":"cat","response_format":"url","aspect_ratio":"1:1","resolution":"1k"}"#
    );
}

// Not upstream's: an xAI stream is made without one, and written as
// `completed` events.
#[tokio::test]
async fn xai_streams_are_written_as_completed_events() {
    let (app, dispatcher) = app(vec![Outcome::reply(
        r#"{"data":[{"b64_json":"AA=="},{"url":"https://img.test/2.png"}],"usage":{"total_tokens":2}}"#,
    )]);
    let request = post_json(
        GENERATIONS,
        r#"{"model":"grok-imagine-image","prompt":"cat","stream":true}"#,
    );
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(
        body,
        "event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\
         \"b64_json\":\"AA==\",\"usage\":{\"total_tokens\":2}}\n\n\
         event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\
         \"url\":\"https://img.test/2.png\",\"usage\":{\"total_tokens\":2}}\n\n"
    );
    let calls = dispatcher.calls();
    assert_eq!(calls[0].method, "execute");
    assert!(!calls[0].options.stream);
}

// Not upstream's: an xAI stream that waits writes keep-alives, then its
// events; one with no images answers 502.
#[tokio::test(start_paused = true)]
async fn xai_streams_keep_alive_while_they_wait() {
    let (app, _) = server(
        keepalive(10),
        vec![
            Outcome::Slow(
                Duration::from_secs(25),
                ExecResponse {
                    payload: Bytes::from_static(br#"{"data":[{"b64_json":"AA=="}]}"#),
                    headers: HeaderMap::new(),
                },
            ),
            Outcome::reply(r#"{"data":[]}"#),
        ],
    );
    let body = r#"{"model":"grok-imagine-image","prompt":"cat","stream":true}"#;
    let (status, headers, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(
        text,
        ": keep-alive\n\n: keep-alive\n\nevent: image_generation.completed\ndata: \
         {\"type\":\"image_generation.completed\",\"b64_json\":\"AA==\"}\n\n"
    );

    let (status, _, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        text.contains("upstream did not return image output"),
        "{text}"
    );
}

// Not upstream's: an xAI form edit sends its images as data URLs, typed by
// their content type or by their data.
#[tokio::test]
async fn xai_form_edits_send_data_urls() {
    let (app, dispatcher) = app(vec![Outcome::reply(
        r#"{"created":9,"data":[{"b64_json":"AA=="}]}"#,
    )]);
    let request = post_form(
        &[
            ("model", "grok-imagine-image"),
            ("prompt", "edit"),
            ("size", "1024x1024"),
            ("quality", " high "),
            ("n", "2"),
        ],
        &[
            ("image[]", "a.jpg", Some("image/jpeg"), b"jpeg"),
            ("image[]", "b.png", None, PNG),
            ("image", "c.png", None, PNG),
        ],
    );
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"created":9,"data":[{"b64_json":"AA=="}]}"#);

    let calls = dispatcher.calls();
    let want = format!(
        r#"{{"model":"grok-imagine-image","prompt":"edit","response_format":"b64_json","aspect_ratio":"1:1","quality":"high","n":2,"images":[{{"type":"image_url","url":"data:image/jpeg;base64,anBlZw=="}},{{"type":"image_url","url":"data:image/png;base64,{PNG_BASE64}"}}]}}"#
    );
    assert_eq!(String::from_utf8_lossy(&calls[0].request.payload), want);
}

// Not upstream's: an `openai-compatibility` model's request goes on as the
// client sent it, and its answer is read as xAI's.
#[tokio::test]
async fn compat_generations_are_read_as_xai_answers() {
    let (app, dispatcher) = app(vec![
        Outcome::reply(r#"{"created":7,"data":[{"b64_json":"AA==","extra":1}]}"#),
        Outcome::reply(r#"{"data":[]}"#),
    ]);
    let body = r#"{"model":"compat-image","prompt":"draw","stream":false}"#;
    let (status, _, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"created":7,"data":[{"b64_json":"AA=="}]}"#);
    let (status, _, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        text.contains("upstream did not return image output"),
        "{text}"
    );

    let calls = dispatcher.calls();
    assert_eq!(calls[0].providers, ["compat"]);
    assert!(!calls[0].options.metadata.disallow_free_auth);
    assert_eq!(
        &calls[0].request.payload[..],
        br#"{"model":"compat-image","prompt":"draw"}"#
    );
}

// Not upstream's: an `openai-compatibility` stream comes back as it came,
// one that ends with nothing writes nothing, and an error after a payload
// is sanitized.
#[tokio::test]
async fn compat_streams_are_sent_back_as_they_came() {
    let (app, _) = app(vec![
        Outcome::chunks(&["data: {\"a\":1}\n\n"]),
        Outcome::chunks(&[]),
        Outcome::Stream(
            HeaderMap::new(),
            vec![
                Ok(Bytes::from_static(b"data: {}\n\n")),
                Err(ExecError::upstream(500, "token=compat-secret")),
            ],
        ),
    ]);
    let body = r#"{"model":"compat-image","prompt":"draw","stream":true}"#;
    let (status, _, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, "data: {\"a\":1}\n\n");

    let (status, headers, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(text, "");

    let (status, _, text) = send(&app, post_json(GENERATIONS, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.starts_with("data: {}\n\nevent: error\ndata: "),
        "{text}"
    );
    assert!(text.contains("[REDACTED]"), "{text}");
    assert!(!text.contains("compat-secret"), "{text}");
}

// Not upstream's: an `openai-compatibility` form edit is written again.
#[tokio::test]
async fn compat_form_edits_are_written_again() {
    let (app, dispatcher) = app(vec![Outcome::chunks(&["data: {}\n\n"])]);
    let request = post_form(
        &[
            ("model", "compat-image"),
            ("prompt", "edit"),
            ("stream", "yes"),
        ],
        &[("image", "a.png", None, PNG)],
    );
    let (status, _, text) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, "data: {}\n\n");

    let calls = dispatcher.calls();
    let call = &calls[0];
    assert_eq!(call.method, "execute_stream");
    assert!(!call.options.metadata.disallow_free_auth);
    let content_type = call.options.headers[header::CONTENT_TYPE].as_bytes();
    let sent = read_form(&call.request.payload, content_type);
    assert_eq!(values(&sent, "model"), [b"compat-image"]);
    assert_eq!(values(&sent, "stream"), [b"true"]);
    assert_eq!(
        sent.files_of("image")[0].header.get_str("Content-Type"),
        "application/octet-stream"
    );
}

// Not upstream's: how requests that can't be served are answered.
#[tokio::test]
async fn bad_requests_are_turned_away() {
    let config = ServerConfig {
        body_limit: 4096,
        ..ServerConfig::default()
    };
    let (app, dispatcher) = server(config, vec![]);
    let cases = [
        (
            post(EDITS, Some("text/plain"), "x"),
            StatusCode::BAD_REQUEST,
            invalid(r#"Invalid request: unsupported Content-Type \"text/plain\""#),
        ),
        (
            post(EDITS, Some("Multipart/Form-Data"), "x"),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: no multipart boundary param in Content-Type"),
        ),
        (
            post(EDITS, None, "x"),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: request Content-Type isn't multipart/form-data"),
        ),
        (
            post_json(GENERATIONS, "{"),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: body must be valid JSON"),
        ),
        (
            post_json(GENERATIONS, r#"{"prompt":" "}"#),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: prompt is required"),
        ),
        (
            post_form(&[("model", "gpt-image-2")], &[]),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: prompt is required"),
        ),
        (
            post_form(&[("prompt", "edit")], &[("mask", "m.png", None, PNG)]),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: image is required"),
        ),
        (
            post_json(EDITS, r#"{"model":"grok-imagine-image","prompt":"cat"}"#),
            StatusCode::BAD_REQUEST,
            invalid("Invalid request: image is required"),
        ),
    ];
    for (request, want_status, want_body) in cases {
        let (status, _, body) = send(&app, request).await;
        assert_eq!((status, body), (want_status, want_body));
    }

    let big = "x".repeat(8192);
    let (status, _, _) = send(&app, post_form(&[("prompt", &big)], &[])).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(dispatcher.calls().is_empty());
}
