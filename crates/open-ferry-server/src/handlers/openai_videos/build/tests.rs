//! Ports the model, request and answer tests of CLIProxyAPI
//! sdk/api/handlers/openai/openai_videos_handlers_test.go (v8.0.15, MIT)
//! that don't need a server, with tests of our own for what they don't
//! cover. The tests that do are in the handler's own tests.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, header};
use open_ferry_core::multipart::Writer;
use serde_json::{Value, json};

use super::*;

fn parse(raw: &[u8]) -> Value {
    serde_json::from_slice(raw).expect("JSON")
}

fn build(raw: &str, model: &str) -> (Value, CreateMeta) {
    let (request, meta) = create_request(raw.as_bytes(), model).expect("built");
    (parse(&request), meta)
}

fn build_err(raw: &str) -> String {
    create_request(raw.as_bytes(), DEFAULT_MODEL).expect_err("refused")
}

fn headers(content_type: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("header"),
    );
    headers
}

fn form(content_type: &str, body: &[u8]) -> Value {
    parse(&form_request(
        &headers(content_type),
        &Bytes::copy_from_slice(body),
    ))
}

fn urlencoded_form(body: &str) -> Value {
    form("application/x-www-form-urlencoded", body.as_bytes())
}

fn multipart_form(fields: &[(&str, &str)]) -> Value {
    let mut writer = Writer::with_boundary("video-boundary").expect("boundary");
    for (name, value) in fields {
        writer.write_field(name, value.as_bytes());
    }
    let content_type = writer.form_data_content_type();
    form(&content_type, &writer.finish())
}

#[test]
fn videos_model_validation_allows_xai_video_model() {
    // TestVideosModelValidationAllowsXAIVideoModel.
    for model in [
        "grok-imagine-video",
        "xai/grok-imagine-video",
        "x-ai/grok-imagine-video",
        "grok/grok-imagine-video",
        "grok-imagine-video-1.5",
        "xai/grok-imagine-video-1.5",
        "x-ai/grok-imagine-video-1.5",
        "grok/grok-imagine-video-1.5",
        "grok-imagine-video-1.5-preview",
        "xai/grok-imagine-video-1.5-preview",
        "x-ai/grok-imagine-video-1.5-preview",
        "grok/grok-imagine-video-1.5-preview",
    ] {
        assert!(is_supported_model(model), "{model}");
    }
    assert!(is_supported_model("sora-2"));
    assert!(!is_xai_model("sora-2"));
    for model in [
        "codex/grok-imagine-video",
        "codex/grok-imagine-video-1.5",
        "codex/grok-imagine-video-1.5-preview",
    ] {
        assert!(!is_supported_model(model), "{model}");
    }
}

// Not upstream's: models are read trimmed and in any case, the base after
// the last `/`; Sora's variants count but not models that merely start
// with its name.
#[test]
fn models_are_read_trimmed_in_any_case() {
    for model in [
        " XAI/Grok-Imagine-Video ",
        "Grok / grok-imagine-video-1.5",
        "sora-2-pro",
        "openai/SORA-2",
        "a/b/sora-2",
    ] {
        assert!(is_supported_model(model), "{model}");
    }
    for model in [
        "",
        "sora-20",
        "sora",
        "grok-imagine-video/",
        "grok-imagine-image",
    ] {
        assert!(!is_supported_model(model), "{model}");
    }
    assert!(!is_xai_model("a/xai/grok-imagine-video"));
    assert_eq!(model_parts(" xai / grok "), ("xai", "grok"));
    assert_eq!(model_parts("grok/"), ("", "grok/"));
}

// Not upstream's: what each model is sent as and routed by.
#[test]
fn models_map_to_xais() {
    for (model, canonical, routing) in [
        ("sora-2", DEFAULT_MODEL, DEFAULT_MODEL),
        ("sora-2-pro", DEFAULT_MODEL, DEFAULT_MODEL),
        ("xai/GROK-IMAGINE-VIDEO", DEFAULT_MODEL, DEFAULT_MODEL),
        ("grok-imagine-video-1.5", MODEL_15, MODEL_15),
        (
            "grok/grok-imagine-video-1.5-preview",
            MODEL_15,
            MODEL_15_PREVIEW,
        ),
        ("unknown", DEFAULT_MODEL, DEFAULT_MODEL),
    ] {
        assert_eq!(canonical_model(model), canonical, "{model}");
        assert_eq!(routing_model(model), routing, "{model}");
    }
}

#[test]
fn build_xai_videos_create_request_maps_sora_model_to_xai_backend() {
    // TestBuildXAIVideosCreateRequestMapsSoraModelToXAIBackend.
    let (request, meta) = build(
        r#"{"model":"sora-2","prompt":"a cat playing piano","seconds":"8"}"#,
        "sora-2",
    );
    assert_eq!(request["model"], DEFAULT_MODEL);
    assert_eq!(meta.model, DEFAULT_MODEL);
}

#[test]
fn build_xai_videos_create_request() {
    // TestBuildXAIVideosCreateRequest.
    let (request, meta) = build(
        r#"{"model":"xai/grok-imagine-video","prompt":"a cat playing piano","seconds":"8","size":"1280x720","input_reference":{"image_url":"https://example.com/cat.png"}}"#,
        "xai/grok-imagine-video",
    );
    assert_eq!(
        request,
        json!({
            "model": DEFAULT_MODEL,
            "prompt": "a cat playing piano",
            "duration": 8,
            "aspect_ratio": "16:9",
            "resolution": "720p",
            "image": {"url": "https://example.com/cat.png"},
        })
    );
    assert_eq!(
        (
            meta.seconds.as_str(),
            meta.size.as_str(),
            meta.prompt.as_str()
        ),
        ("8", "1280x720", "a cat playing piano")
    );
}

#[test]
fn build_xai_videos_create_request_allows_video_15_model() {
    // TestBuildXAIVideosCreateRequestAllowsVideo15Model.
    let (request, meta) = build(
        r#"{"model":"xai/grok-imagine-video-1.5","prompt":"a cat playing piano","seconds":"8"}"#,
        "xai/grok-imagine-video-1.5",
    );
    assert_eq!(request["model"], MODEL_15);
    assert_eq!(meta.model, MODEL_15);
    assert_eq!(meta.routing_model, MODEL_15);
}

#[test]
fn build_xai_videos_create_request_normalizes_video_15_preview_alias() {
    // TestBuildXAIVideosCreateRequestNormalizesVideo15PreviewAlias.
    let (request, meta) = build(
        r#"{"model":"xai/grok-imagine-video-1.5-preview","prompt":"a cat playing piano","seconds":"8"}"#,
        "xai/grok-imagine-video-1.5-preview",
    );
    assert_eq!(request["model"], MODEL_15);
    assert_eq!(meta.model, MODEL_15);
    assert_eq!(meta.routing_model, MODEL_15_PREVIEW);
}

#[test]
fn build_xai_videos_create_request_allows_custom_seconds() {
    // TestBuildXAIVideosCreateRequestAllowsCustomSeconds.
    let (request, meta) = build(
        r#"{"model":"grok-imagine-video","prompt":"a cat playing piano","seconds":"6"}"#,
        "grok-imagine-video",
    );
    assert_eq!(request["duration"], 6);
    assert_eq!(meta.seconds, "6");
}

#[test]
fn build_xai_videos_create_request_preserves_multi_reference_seconds() {
    // TestBuildXAIVideosCreateRequestPreservesMultiReferenceSeconds.
    for (seconds, duration) in [("1", 1), ("6", 6), ("10", 10), ("11", 11), ("15", 15)] {
        let raw = format!(
            r#"{{"prompt":"animate","seconds":"{seconds}","reference_images":["https://example.com/first.png","https://example.com/second.png"]}}"#
        );
        let (request, meta) = build(&raw, DEFAULT_MODEL);
        assert_eq!(request["duration"], duration, "{seconds}");
        assert_eq!(meta.seconds, seconds);
        assert_eq!(
            request["reference_images"],
            json!([
                {"url": "https://example.com/first.png"},
                {"url": "https://example.com/second.png"},
            ])
        );
    }
}

#[test]
fn build_xai_videos_create_request_rejects_file_id_reference() {
    // TestBuildXAIVideosCreateRequestRejectsFileIDReference.
    let err = build_err(r#"{"prompt":"animate","input_reference":{"file_id":"file_123"}}"#);
    assert!(
        err.contains("input_reference.file_id is not supported"),
        "{err}"
    );
}

// Not upstream's: the defaults, and seconds held to 1 through 15 and
// written as read.
#[test]
fn seconds_and_size_have_defaults_and_bounds() {
    let (request, meta) = build(r#"{"prompt":" go "}"#, DEFAULT_MODEL);
    assert_eq!(
        request,
        json!({
            "model": DEFAULT_MODEL,
            "prompt": "go",
            "duration": 4,
            "aspect_ratio": "9:16",
            "resolution": "720p",
        })
    );
    assert_eq!(
        (
            meta.seconds.as_str(),
            meta.size.as_str(),
            meta.prompt.as_str()
        ),
        ("4", "720x1280", "go")
    );
    for (seconds, duration) in [
        (r#""0""#, 1),
        (r#""-3""#, 1),
        (r#""+16""#, 15),
        (r#"" 9 ""#, 9),
        ("12", 12),
        ("null", 4),
    ] {
        let (request, meta) = build(
            &format!(r#"{{"prompt":"p","seconds":{seconds}}}"#),
            DEFAULT_MODEL,
        );
        assert_eq!(request["duration"], duration, "{seconds}");
        assert_eq!(meta.seconds, duration.to_string());
    }
    for (size, aspect_ratio) in [
        ("1024x1792", "9:16"),
        ("1792x1024", "16:9"),
        (" 720x1280 ", "9:16"),
    ] {
        let (request, meta) = build(
            &format!(r#"{{"prompt":"p","size":"{size}"}}"#),
            DEFAULT_MODEL,
        );
        assert_eq!(request["aspect_ratio"], aspect_ratio);
        assert_eq!(meta.size, size.trim());
    }
}

// Not upstream's: why a create is refused.
#[test]
fn creates_are_refused_with_reasons() {
    for (raw, reason) in [
        (r#"{"prompt":"  "}"#, "prompt is required"),
        (r#"{}"#, "prompt is required"),
        (
            r#"{"prompt":"p","seconds":"4.5"}"#,
            "seconds must be an integer",
        ),
        (
            r#"{"prompt":"p","seconds":"99999999999999999999"}"#,
            "seconds must be an integer",
        ),
        (
            r#"{"prompt":"p","size":"1080x1920"}"#,
            "size must be one of 720x1280, 1280x720, 1024x1792, or 1792x1024",
        ),
        (
            r#"{"prompt":"p","input_reference":{"image_url":"u","file_id":"f"}}"#,
            "input_reference must provide exactly one of image_url or file_id",
        ),
        (
            r#"{"prompt":"p","reference_images":["1","2","3","4","5","6","7","8"]}"#,
            "reference_images supports at most 7 images on xAI",
        ),
        (
            r#"{"prompt":"p","image_url":"u","reference_images":["1"]}"#,
            "image and reference_images cannot be combined on xAI",
        ),
    ] {
        assert_eq!(build_err(raw), reason, "{raw}");
    }
}

// Not upstream's: an aspect ratio or resolution that's named overrides
// what the size gives; one that isn't is ignored.
#[test]
fn aspect_ratio_and_resolution_override_the_size() {
    for (aspect_ratio, want) in [
        ("1:1", "1:1"),
        ("Square", "1:1"),
        ("landscape", "16:9"),
        (" PORTRAIT ", "9:16"),
        ("4:3", "4:3"),
        ("3:4", "3:4"),
        ("3:2", "3:2"),
        ("2:3", "2:3"),
        ("21:9", "9:16"),
    ] {
        let (request, _) = build(
            &format!(r#"{{"prompt":"p","aspect_ratio":"{aspect_ratio}"}}"#),
            DEFAULT_MODEL,
        );
        assert_eq!(request["aspect_ratio"], want, "{aspect_ratio}");
    }
    for (resolution, want) in [("480P", "480p"), ("720p", "720p"), ("1080p", "720p")] {
        let (request, _) = build(
            &format!(r#"{{"prompt":"p","resolution":"{resolution}"}}"#),
            DEFAULT_MODEL,
        );
        assert_eq!(request["resolution"], want, "{resolution}");
    }
}

// Not upstream's: where the input image's URL is found, in order.
#[test]
fn the_input_image_is_found_in_order() {
    for (fields, want) in [
        (
            r#""input_reference":{"image_url":" a "},"image":"b""#,
            Some("a"),
        ),
        (r#""input_reference":{},"image":"b""#, Some("b")),
        (r#""image":" b ","image_url":"c""#, Some("b")),
        (r#""image":{"url":" d "}"#, Some("d")),
        (r#""image":{"image_url":{"url":"e"}}"#, Some("e")),
        (r#""image":{"url":" "},"image_url":"f""#, Some("f")),
        (r#""image_url":" f ""#, Some("f")),
        (r#""image":"  ","image_url":"f""#, None),
        (r#""input_reference":"g""#, None),
    ] {
        let (request, _) = build(&format!(r#"{{"prompt":"p",{fields}}}"#), DEFAULT_MODEL);
        assert_eq!(request["image"]["url"].as_str(), want, "{fields}");
    }
}

// Not upstream's: reference images, as strings or objects, from both
// lists; an object's `url` that is only space hides its `image_url.url`.
#[test]
fn reference_images_come_from_both_lists() {
    let (request, _) = build(
        r#"{"prompt":"p","reference_images":[" a ",{"url":"b"},{"image_url":{"url":"c"}},{"url":" ","image_url":{"url":"x"}},5,""],"reference_image_urls":["d"],"reference_images_extra":["y"]}"#,
        DEFAULT_MODEL,
    );
    assert_eq!(
        request["reference_images"],
        json!([{"url": "a"}, {"url": "b"}, {"url": "c"}, {"url": "d"}])
    );
    let (request, _) = build(
        r#"{"prompt":"p","reference_images":"a","reference_image_urls":{"url":"b"}}"#,
        DEFAULT_MODEL,
    );
    assert!(request.get("reference_images").is_none(), "{request}");
}

#[test]
fn build_videos_create_api_response_from_xai() {
    // TestBuildVideosCreateAPIResponseFromXAI.
    let meta = CreateMeta {
        model: DEFAULT_MODEL,
        routing_model: DEFAULT_MODEL,
        prompt: "animate".to_owned(),
        seconds: "4".to_owned(),
        size: "720x1280".to_owned(),
        created_at: 123,
    };
    let out = create_response(br#"{"request_id":"vid_123"}"#, &meta).expect("built");
    assert_eq!(
        String::from_utf8_lossy(&out),
        concat!(
            r#"{"object":"video","progress":0,"status":"queued","id":"vid_123","#,
            r#""model":"grok-imagine-video","prompt":"animate","seconds":"4","#,
            r#""size":"720x1280","created_at":123}"#
        )
    );
}

// Not upstream's: a create answer's ID, status and progress.
#[test]
fn create_answers_take_xais_id_status_and_progress() {
    let meta = CreateMeta {
        model: MODEL_15,
        routing_model: MODEL_15_PREVIEW,
        prompt: "p".to_owned(),
        seconds: "6".to_owned(),
        size: "1280x720".to_owned(),
        created_at: 7,
    };
    let out = create_response(
        br#"{"request_id":" ","id":" v ","status":"Processing","progress":12.5}"#,
        &meta,
    )
    .expect("built");
    let out = parse(&out);
    assert_eq!(out["id"], "v");
    assert_eq!(out["model"], MODEL_15);
    assert_eq!(out["status"], "in_progress");
    assert_eq!(out["progress"], 12.5);
    let out = create_response(br#"{"request_id":"v","status":"odd"}"#, &meta).expect("built");
    assert_eq!(parse(&out)["status"], "queued");
    assert_eq!(
        create_response(br#"{"status":"queued"}"#, &meta),
        Err("xAI video response did not include request_id".to_owned())
    );
}

// Not upstream's: a failed video's defaults.
#[test]
fn failed_videos_have_defaults() {
    let out = parse(&failed_response(" ", "", " "));
    let id = out["id"].as_str().expect("id");
    let hex = id.strip_prefix("video_").expect("prefix");
    assert_eq!(hex.len(), 32);
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    assert_eq!(
        out,
        json!({
            "object": "video",
            "status": "failed",
            "progress": 0,
            "id": id,
            "model": DEFAULT_MODEL,
            "error": {"code": "invalid_request_error", "message": "Video generation failed"},
        })
    );
    let out = parse(&failed_response(" m ", " c ", " why "));
    assert_eq!(
        (&out["model"], &out["error"]),
        (&json!("m"), &json!({"code": "c", "message": "why"}))
    );
}

#[test]
fn build_videos_retrieve_api_response_from_xai() {
    // TestBuildVideosRetrieveAPIResponseFromXAI.
    let payload = br#"{"object":"video","id":"91989464-273f-95df-8197-703b4fefd40e","model":"grok-imagine-video","status":"completed","progress":100,"seconds":"4","video":{"url":"https://vidgen.x.ai/xai-vidgen-bucket/xai-video-08609066-e7e9-43ba-bd8d-bd29cb6221d9.mp4","duration":4,"respect_moderation":true},"usage":{"cost_in_usd_ticks":2800000000}}"#;
    let out = parse(&retrieve_response(
        "91989464-273f-95df-8197-703b4fefd40e",
        payload,
        SORA_MODEL,
    ));
    assert_eq!(
        out,
        json!({
            "object": "video",
            "id": "91989464-273f-95df-8197-703b4fefd40e",
            "model": DEFAULT_MODEL,
            "status": "completed",
            "progress": 100,
            "seconds": "4",
            "video_url": "https://vidgen.x.ai/xai-vidgen-bucket/xai-video-08609066-e7e9-43ba-bd8d-bd29cb6221d9.mp4",
        })
    );
}

#[test]
fn build_videos_retrieve_api_response_from_xai_normalizes_top_level_error() {
    // TestBuildVideosRetrieveAPIResponseFromXAINormalizesTopLevelError.
    let payload = br#"{"code":"invalid-argument","error":"1080p video resolution is not available for your team."}"#;
    let out = parse(&retrieve_response("video_123", payload, SORA_MODEL));
    assert_eq!(out["status"], "failed");
    assert_eq!(out["progress"], 0);
    assert_eq!(
        out["error"],
        json!({
            "code": "invalid-argument",
            "message": "1080p video resolution is not available for your team.",
        })
    );
}

#[test]
fn build_videos_retrieve_api_response_from_xai_normalizes_nested_error() {
    // TestBuildVideosRetrieveAPIResponseFromXAINormalizesNestedError.
    let payload = br#"{"status":"failed","error":{"message":"The request was rejected by the safety system.","type":"invalid_request_error","code":"content_policy_violation"}}"#;
    let out = parse(&retrieve_response("video_123", payload, SORA_MODEL));
    assert_eq!(
        out["error"],
        json!({
            "code": "content_policy_violation",
            "message": "The request was rejected by the safety system.",
        })
    );
}

// Not upstream's: the rest of a retrieve answer, and the errors upstream's
// tests don't cover.
#[test]
fn retrieve_answers_keep_xais_fields_and_errors() {
    let out = parse(&retrieve_response(
        "v",
        br#"{"model":" ","created_at":1,"completed_at":2,"expires_at":3,"prompt":"p","remixed_from_video_id":"r","size":"720x1280","status":"pending","progress":0,"video":{"duration":6.0,"url":" "}}"#,
        "grok-imagine-video-1.5-preview",
    ));
    assert_eq!(
        out,
        json!({
            "object": "video",
            "id": "v",
            "model": MODEL_15,
            "created_at": 1,
            "completed_at": 2,
            "expires_at": 3,
            "prompt": "p",
            "remixed_from_video_id": "r",
            "size": "720x1280",
            "status": "queued",
            "progress": 0,
            "seconds": "6",
        })
    );
    for (payload, error) in [
        (
            r#"{"code":" quota "}"#,
            json!({"code": "quota", "message": "quota"}),
        ),
        (
            r#"{"error":{"message":"m"}}"#,
            json!({"code": "video_generation_failed", "message": "m"}),
        ),
        (
            r#"{"code":"top","error":{"message":"m","code":"inner"}}"#,
            json!({"code": "top", "message": "m"}),
        ),
        (
            r#"{"error":" e "}"#,
            json!({"code": "video_generation_failed", "message": "e"}),
        ),
        (r#"{"error":{"code":"no-message"}}"#, Value::Null),
        (r#"{"error":["m"]}"#, Value::Null),
        (r#"{"error":null}"#, Value::Null),
    ] {
        let out = parse(&retrieve_response("v", payload.as_bytes(), SORA_MODEL));
        assert_eq!(out["status"], "failed", "{payload}");
        assert_eq!(out["progress"], 0, "{payload}");
        assert_eq!(out["error"], error, "{payload}");
    }
    let out = parse(&retrieve_response(
        "v",
        br#"{"status":"running","progress":40,"error":"e"}"#,
        SORA_MODEL,
    ));
    assert_eq!(
        (&out["status"], &out["progress"]),
        (&json!("in_progress"), &json!(40))
    );
    let out = parse(&retrieve_response("v", br#"{"status":"done"}"#, SORA_MODEL));
    assert!(out.get("error").is_none(), "{out}");
}

#[test]
fn xai_video_content_url_from_payload() {
    // TestXAIVideoContentURLFromPayload.
    let payload =
        br#"{"status":"done","video":{"url":"https://vidgen.x.ai/video.mp4","duration":6}}"#;
    assert_eq!(
        content_url(payload).as_deref(),
        Ok("https://vidgen.x.ai/video.mp4")
    );
}

// Not upstream's: the URLs a finished video isn't fetched from.
#[test]
fn content_urls_must_be_http_with_a_host() {
    assert_eq!(
        content_url(br#"{"video":{"url":" http://h/v "}}"#).as_deref(),
        Ok("http://h/v")
    );
    assert_eq!(
        content_url(br#"{"video":{"url":" "}}"#),
        Err("xAI video response did not include video.url".to_owned())
    );
    for url in [
        "ftp://h/v",
        "file:///etc/passwd",
        "https:///v",
        "/v",
        "https://h/%zz",
        "HTTPS",
    ] {
        assert_eq!(
            content_url(format!(r#"{{"video":{{"url":"{url}"}}}}"#).as_bytes()),
            Err("xAI video response included invalid video.url".to_owned()),
            "{url}"
        );
    }
    assert_eq!(
        content_url(br#"{"video":{"url":"HTTPS://H/v"}}"#).as_deref(),
        Ok("HTTPS://H/v")
    );
}

// Not upstream's: xAI's statuses as OpenAI's.
#[test]
fn statuses_map_to_openais() {
    for (status, want) in [
        ("queued", "queued"),
        ("Pending", "queued"),
        ("in_progress", "in_progress"),
        ("processing", "in_progress"),
        (" RUNNING ", "in_progress"),
        ("completed", "completed"),
        ("done", "completed"),
        ("succeeded", "completed"),
        ("success", "completed"),
        ("failed", "failed"),
        ("error", "failed"),
        ("expired", "failed"),
        ("cancelled", "failed"),
        ("canceled", "failed"),
        ("", ""),
        ("other", ""),
    ] {
        assert_eq!(video_status(status), want, "{status}");
    }
}

// Not upstream's: a payload's video ID.
#[test]
fn video_ids_come_from_the_request_id_then_the_id() {
    assert_eq!(video_id(br#"{"request_id":" a ","id":"b"}"#), "a");
    assert_eq!(video_id(br#"{"request_id":"","id":" b "}"#), "b");
    assert_eq!(video_id(br#"{"id":7}"#), "7");
    assert_eq!(video_id(b"{}"), "");
}

#[test]
fn videos_create_form_request() {
    // TestVideosCreateFormRequest.
    let raw = urlencoded_form(
        "model=grok-imagine-video&prompt=make+a+video&seconds=4&size=720x1280&input_reference%5Bimage_url%5D=https%3A%2F%2Fexample.com%2Fa.png",
    );
    assert_eq!(
        raw["input_reference"]["image_url"],
        "https://example.com/a.png"
    );
}

// Not upstream's: every field a form is read for, trimmed, with the input
// image's names tried in order and the reference images split at commas.
#[test]
fn form_fields_are_read_trimmed() {
    let raw = urlencoded_form(
        "model=+m+&prompt=p&seconds=6&size=1280x720&aspect_ratio=16%3A9&resolution=480p&other=x&input_reference.image_url=+&image_url=+i+&file_id=f&reference_image_urls=+a+,,b+,+&prompt=second",
    );
    assert_eq!(
        raw,
        json!({
            "model": "m",
            "prompt": "p",
            "seconds": "6",
            "size": "1280x720",
            "aspect_ratio": "16:9",
            "resolution": "480p",
            "input_reference": {"image_url": "i", "file_id": "f"},
            "reference_image_urls": ["a", "b"],
        })
    );
    assert_eq!(
        urlencoded_form("prompt=+&input_reference%5Bfile_id%5D=f1&input_reference.file_id=f2"),
        json!({"input_reference": {"file_id": "f1"}})
    );
    assert_eq!(urlencoded_form("reference_image_urls=+,+"), json!({}));
}

// Not upstream's: a multipart form is read as Go's `ParseMultipartForm`
// reads it.
#[test]
fn multipart_forms_are_read() {
    let raw = multipart_form(&[
        ("model", "grok-imagine-video-1.5"),
        ("prompt", " a \"quoted\" prompt "),
        ("input_reference[image_url]", "https://example.com/a.png"),
        ("reference_image_urls", "x,y"),
    ]);
    assert_eq!(
        raw,
        json!({
            "model": "grok-imagine-video-1.5",
            "prompt": "a \"quoted\" prompt",
            "input_reference": {"image_url": "https://example.com/a.png"},
            "reference_image_urls": ["x", "y"],
        })
    );
}

// Not upstream's: forms Go reads no values from.
#[test]
fn forms_without_values_read_as_empty() {
    // A multipart form without a boundary, or that doesn't parse.
    assert_eq!(form("multipart/form-data", b"prompt=p"), json!({}));
    assert_eq!(
        form("multipart/form-data; boundary=b", b"prompt=p"),
        json!({})
    );
    // A URL-encoded form whose parameters don't parse is still read.
    assert_eq!(
        form(
            "application/x-www-form-urlencoded; charset=\"utf-8",
            b"prompt=p"
        ),
        json!({"prompt": "p"})
    );
    // Over 10 MiB.
    let mut body = b"prompt=p&pad=".to_vec();
    body.resize(MAX_URLENCODED_FORM + 1, b'a');
    assert_eq!(form("application/x-www-form-urlencoded", &body), json!({}));
    body.truncate(MAX_URLENCODED_FORM);
    assert_eq!(
        form("application/x-www-form-urlencoded", &body),
        json!({"prompt": "p"})
    );
}

// Not upstream's: the media type gin reads.
#[test]
fn content_types_are_read_as_gin_reads_them() {
    for (value, want) in [
        ("Multipart/Form-Data; boundary=x", "multipart/form-data"),
        (" application/x-www-form-urlencoded", ""),
        ("application/json charset=utf-8", "application/json"),
        ("text/plain;x", "text/plain"),
    ] {
        assert_eq!(content_type(&headers(value)), want, "{value}");
    }
    assert_eq!(content_type(&HeaderMap::new()), "");
    assert!(is_form("multipart/form-data"));
    assert!(is_form("application/x-www-form-urlencoded"));
    assert!(!is_form("application/json"));
}
