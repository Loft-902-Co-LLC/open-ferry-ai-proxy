// Ported from CLIProxyAPI
// internal/runtime/executor/codex_openai_images_test.go
// (TestCodexExecutorDirectOpenAIImageGenerationUsesImagesEndpoint,
// TestCodexExecutorDirectOpenAIImageGenerationStreamsImagesEndpoint,
// TestCodexExecutorDirectOpenAIImageEditUsesImagesEditEndpointForJSON,
// TestCodexExecutorDirectOpenAIImageEditUsesImagesEditEndpointForMultipart
// and TestCodexExecutorDirectOpenAIImage25Models) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OpenAI Images endpoints served through Codex, end to end against a
//! mock Codex.

use open_ferry_core::config::Config;
use open_ferry_core::multipart::{Header, Writer};

use super::*;

const GENERATIONS: &str = "/v1/images/generations";
const EDITS: &str = "/v1/images/edits";

/// `codexOpenAIImageTestOptions`.
fn image_options(path: &str, stream: bool) -> Options {
    let mut options = options("openai-image");
    options.stream = stream;
    options.metadata.request_path = path.to_owned();
    options
}

/// A form edit for `model` and its `Content-Type`.
fn form(model: &str, build: impl FnOnce(&mut Writer)) -> (Request, String) {
    let mut writer = Writer::new();
    build(&mut writer);
    let content_type = writer.form_data_content_type();
    let request = Request {
        model: model.to_owned(),
        payload: Bytes::from(writer.finish()),
    };
    (request, content_type)
}

const DIRECT_ANSWER: &str = r#"{"created":1713833628,"data":[{"b64_json":"AA=="}],"usage":{"total_tokens":100,"input_tokens":50,"output_tokens":50}}"#;

// Ported from upstream's
// TestCodexExecutorDirectOpenAIImageGenerationUsesImagesEndpoint. The call
// sends open-ferry's User-Agent, and the client's Originator where upstream
// sends Codex's own.
#[tokio::test]
async fn direct_generation_uses_the_images_endpoint() {
    let mock = Mock::start(Reply::json(DIRECT_ANSWER)).await;
    let mut options = image_options(GENERATIONS, false);
    for (name, value) in [
        ("user-agent", "downstream-client/9.9"),
        ("version", "0.135.0"),
        ("x-codex-turn-metadata", r#"{"turn_id":"turn-1"}"#),
        ("x-client-request-id", "client-request-1"),
        ("originator", "Codex Desktop"),
    ] {
        options = with_header(options, name, value);
    }
    let payload = r#"{"model":"codex/gpt-image-1.5","prompt":"A cute baby sea otter","n":1,"size":"1024x1024","quality":"high","background":"opaque","output_format":"jpeg","output_compression":70,"moderation":"low","extra":{"preserve":true},"stream":false}"#;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("codex/gpt-image-1.5", payload),
            options,
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/images/generations");
    assert_eq!(seen.header("authorization"), Some("Bearer test"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert_eq!(seen.header("version"), Some("0.135.0"));
    assert_eq!(
        seen.header("x-codex-turn-metadata"),
        Some(r#"{"turn_id":"turn-1"}"#)
    );
    assert_eq!(seen.header("x-client-request-id"), Some("client-request-1"));
    assert_eq!(seen.header("originator"), Some("Codex Desktop"));
    let body = seen.json();
    assert_eq!(get(&body, "model"), Some(&json!("gpt-image-1.5")));
    assert_eq!(get(&body, "extra.preserve"), Some(&json!(true)));
    assert_eq!(get(&body, "output_compression"), Some(&json!(70)));
    assert!(!exists(&body, "stream"), "{body}");
    assert_eq!(&response.payload[..], DIRECT_ANSWER.as_bytes());
}

// Ported from upstream's
// TestCodexExecutorDirectOpenAIImageGenerationStreamsImagesEndpoint.
#[tokio::test]
async fn direct_generation_streams_the_images_endpoint() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: image_generation.partial_image\n",
        "data: {\"type\":\"image_generation.partial_image\",\"b64_json\":\"AA==\",\"partial_image_index\":0}\n\n",
        "event: image_generation.completed\n",
        "data: {\"type\":\"image_generation.completed\",\"b64_json\":\"BB==\",\"usage\":{\"total_tokens\":10,\"input_tokens\":4,\"output_tokens\":6}}\n\n",
    )))
    .await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "gpt-image-2",
                r#"{"model":"gpt-image-2","prompt":"A cute baby sea otter","partial_images":2}"#,
            ),
            image_options(GENERATIONS, true),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");

    let seen = mock.last();
    assert_eq!(seen.path, "/images/generations");
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    let body = seen.json();
    assert_eq!(get(&body, "stream"), Some(&json!(true)));
    assert_eq!(get(&body, "partial_images"), Some(&json!(2)));
    assert!(
        text.contains("event: image_generation.partial_image")
            && text.contains("event: image_generation.completed"),
        "{text}"
    );
}

// Ported from upstream's
// TestCodexExecutorDirectOpenAIImageEditUsesImagesEditEndpointForJSON.
#[tokio::test]
async fn direct_json_edit_uses_the_edits_endpoint() {
    let mock = Mock::start(Reply::json(
        r#"{"created":1713833628,"data":[{"b64_json":"AA=="}],"usage":{"total_tokens":10}}"#,
    ))
    .await;
    let payload = r#"{"model":"gpt-image-2","prompt":"Replace the background","images":[{"file_id":"file-abc123"}],"mask":{"file_id":"file-mask123"},"size":"1024x1024","quality":"high","output_format":"png","output_compression":100,"stream":false}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-image-2", payload),
            image_options(EDITS, false),
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/images/edits");
    let body = seen.json();
    assert_eq!(get(&body, "model"), Some(&json!("gpt-image-2")));
    assert_eq!(get(&body, "images.0.file_id"), Some(&json!("file-abc123")));
    assert_eq!(get(&body, "mask.file_id"), Some(&json!("file-mask123")));
    assert!(!exists(&body, "stream"), "{body}");
}

// Ported from upstream's
// TestCodexExecutorDirectOpenAIImageEditUsesImagesEditEndpointForMultipart.
#[tokio::test]
async fn direct_form_edit_goes_as_json() {
    let (request, content_type) = form("codex/gpt-image-1.5", |writer| {
        writer.write_field("model", b"codex/gpt-image-1.5");
        writer.write_field("prompt", b"Create a lovely gift basket");
        writer.write_field("output_format", b"webp");
        writer.write_field("n", b"2");
        writer.write_field("stream", b"false");
        writer.write_file("image[]", "source.png", b"png-data");
        writer.write_file("mask", "mask.png", b"mask-data");
    });
    let mock = Mock::start(Reply::json(
        r#"{"created":1713833628,"data":[{"b64_json":"AA=="}]}"#,
    ))
    .await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request,
            with_header(image_options(EDITS, false), "content-type", &content_type),
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/images/edits");
    assert!(
        seen.header("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "{:?}",
        seen.header("content-type")
    );
    let body = seen.json();
    assert_eq!(get(&body, "model"), Some(&json!("gpt-image-1.5")));
    assert_eq!(
        get(&body, "prompt"),
        Some(&json!("Create a lovely gift basket"))
    );
    assert_eq!(get(&body, "output_format"), Some(&json!("webp")));
    assert_eq!(get(&body, "n"), Some(&json!(2)));
    assert!(!exists(&body, "stream"), "{body}");
    let image = crate::json::str_at(&body, "images.0.image_url");
    assert!(image.contains(";base64,cG5nLWRhdGE="), "{body}");
    let mask = crate::json::str_at(&body, "mask.image_url");
    assert!(mask.contains(";base64,bWFzay1kYXRh"), "{body}");
}

// Ported from upstream's TestCodexExecutorDirectOpenAIImage25Models.
#[tokio::test]
async fn direct_2_5_models() {
    let json_mock = Mock::start(Reply::json(
        r#"{"created":1713833628,"data":[{"b64_json":"AA=="}],"usage":{"total_tokens":10}}"#,
    ))
    .await;
    let sse_mock = Mock::start(Reply::sse(
        "event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"BB==\"}\n\n",
    ))
    .await;
    for (model, base) in [
        ("gpt-image-2.5", "gpt-image-2.5"),
        ("gpt-image-2.5-flare", "gpt-image-2.5-flare"),
        ("gpt-image-2.5-sunburst", "gpt-image-2.5-sunburst"),
        ("codex/gpt-image-2.5", "gpt-image-2.5"),
        ("codex/gpt-image-2.5-flare", "gpt-image-2.5-flare"),
        ("codex/gpt-image-2.5-sunburst", "gpt-image-2.5-sunburst"),
        ("GPT-Image-2.5(medium)", "gpt-image-2.5"),
        ("codex/GPT-Image-2.5-Flare(high)", "gpt-image-2.5-flare"),
    ] {
        // generate/
        let payload = format!(r#"{{"model":"{model}","prompt":"draw something"}}"#);
        executor()
            .execute(
                api_key_auth(&json_mock.url),
                request(model, &payload),
                image_options(GENERATIONS, false),
            )
            .await
            .unwrap();
        let seen = json_mock.last();
        assert_eq!(seen.path, "/images/generations", "{model}");
        assert_eq!(get(&seen.json(), "model"), Some(&json!(base)), "{model}");

        // edit/
        let payload = format!(
            r#"{{"model":"{model}","prompt":"edit something","images":[{{"file_id":"f1"}}]}}"#
        );
        executor()
            .execute(
                api_key_auth(&json_mock.url),
                request(model, &payload),
                image_options(EDITS, false),
            )
            .await
            .unwrap();
        let seen = json_mock.last();
        assert_eq!(seen.path, "/images/edits", "{model}");
        assert_eq!(get(&seen.json(), "model"), Some(&json!(base)), "{model}");

        // stream/
        let payload = format!(r#"{{"model":"{model}","prompt":"stream something"}}"#);
        let response = executor()
            .execute_stream(
                api_key_auth(&sse_mock.url),
                request(model, &payload),
                image_options(GENERATIONS, true),
            )
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "{model}: {error:?}");
        let seen = sse_mock.last();
        assert_eq!(seen.path, "/images/generations", "{model}");
        assert_eq!(get(&seen.json(), "model"), Some(&json!(base)), "{model}");
    }
}

/// A tool call's stream: a partial image, the image as an output item, and
/// the completed event with no output of its own.
const TOOL_STREAM: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "data: {\"type\":\"response.image_generation_call.partial_image\",\"partial_image_b64\":\"UA==\",\"partial_image_index\":0,\"output_format\":\"jpeg\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"image_generation_call\",\"result\":\"QUJD\",\"revised_prompt\":\"a red fox\",\"output_format\":\"jpeg\",\"size\":\"1024x1024\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"created_at\":1700000000,\"output\":[],\"tool_usage\":{\"image_gen\":{\"images\":1}},\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
);

/// The Responses request a tool call sends for `tool` and `content`.
fn tool_request(tool: Value, content: Value) -> Value {
    json!({
        "instructions": "",
        "stream": true,
        "reasoning": { "effort": "medium", "summary": "auto" },
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "model": "gpt-5.4-mini",
        "store": false,
        "tool_choice": { "type": "image_generation" },
        "tools": [tool],
        "input": [{ "type": "message", "role": "user", "content": content }],
    })
}

// Not upstream's: a generation for a model the Image API doesn't serve goes
// through the image generation tool, and its image is answered as the
// Images API answers.
#[tokio::test]
async fn tool_generation() {
    let mock = Mock::start(Reply::sse(TOOL_STREAM)).await;
    let payload = r#"{"model":"gpt-image-1","prompt":" A red fox ","size":"1024x1024","quality":" ","output_compression":50,"partial_images":"2","response_format":"URL","n":1}"#;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-image-1", payload),
            with_header(image_options(GENERATIONS, false), "originator", "my-client"),
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/responses");
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    assert_eq!(seen.header("originator"), Some("my-client"));
    assert_eq!(
        seen.json(),
        tool_request(
            json!({
                "type": "image_generation",
                "action": "generate",
                "model": "gpt-image-1",
                "size": "1024x1024",
                "output_compression": 50,
            }),
            json!([{ "type": "input_text", "text": "A red fox" }]),
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&response.payload),
        r#"{"created":1700000000,"data":[{"revised_prompt":"a red fox","url":"data:image/jpeg;base64,QUJD"}],"output_format":"jpeg","size":"1024x1024","usage":{"images":1}}"#
    );
}

// Not upstream's: a JSON edit through the tool takes its images and mask
// as URLs, and the main model can come from the config.
#[tokio::test]
async fn tool_json_edit() {
    let completed = "data: {\"type\":\"response.completed\",\"response\":{\"created_at\":5,\"output\":[{\"type\":\"image_generation_call\",\"result\":\"RUZH\",\"output_format\":\"png\"}]}}\n\n";
    let mock = Mock::start(Reply::sse(completed)).await;
    let config = Config::parse("gpt-image-2-base-model: GPT-5.5\n").unwrap();
    let payload = r#"{"prompt":"Add a hat","images":[{"image_url":" https://example.com/a.png "},{"file_id":"file-1"},{"image_url":"data:image/png;base64,AA=="}],"mask":{"image_url":"https://example.com/m.png"},"input_fidelity":"high","partial_images":1.9}"#;
    let response = executor()
        .with_config(Arc::new(config))
        .execute(
            api_key_auth(&mock.url),
            request("my-image", payload),
            image_options(EDITS, false),
        )
        .await
        .unwrap();

    let mut expected = tool_request(
        json!({
            "type": "image_generation",
            "action": "edit",
            "model": "my-image",
            "input_fidelity": "high",
            "partial_images": 1,
            "input_image_mask": { "image_url": "https://example.com/m.png" },
        }),
        json!([
            { "type": "input_text", "text": "Add a hat" },
            { "type": "input_image", "image_url": "https://example.com/a.png" },
            { "type": "input_image", "image_url": "data:image/png;base64,AA==" },
        ]),
    );
    crate::json::set(&mut expected, "model", json!("GPT-5.5"));
    assert_eq!(mock.last().json(), expected);
    assert_eq!(
        String::from_utf8_lossy(&response.payload),
        r#"{"created":5,"data":[{"b64_json":"RUZH"}],"output_format":"png"}"#
    );
}

// Not upstream's: a form edit through the tool sends its images and mask
// as data URLs, a file's type sniffed when its part names none.
#[tokio::test]
async fn tool_form_edit() {
    let png = b"\x89PNG\r\n\x1a\nrest";
    let (request, content_type) = form("my-image", |writer| {
        writer.write_field("prompt", b" Add a hat ");
        writer.write_field("size", b"1024x1024");
        writer.write_field("output_compression", b" 80 ");
        writer.write_field("partial_images", b"two");
        writer.write_field("response_format", b"url");
        writer.write_file("image", "a.bin", b"png-data");
        let mut header = Header::new();
        header.set(
            "Content-Disposition",
            "form-data; name=\"image\"; filename=\"b.png\"",
        );
        writer.write_part(&header, png);
        writer.write_file("mask", "mask.png", b"mask-data");
    });
    let mock = Mock::start(Reply::sse(TOOL_STREAM)).await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request,
            with_header(image_options(EDITS, false), "content-type", &content_type),
        )
        .await
        .unwrap();

    assert_eq!(
        mock.last().json(),
        tool_request(
            json!({
                "type": "image_generation",
                "action": "edit",
                "model": "my-image",
                "size": "1024x1024",
                "output_compression": 80,
                "input_image_mask": {
                    "image_url": "data:application/octet-stream;base64,bWFzay1kYXRh",
                },
            }),
            json!([
                { "type": "input_text", "text": "Add a hat" },
                {
                    "type": "input_image",
                    "image_url": "data:application/octet-stream;base64,cG5nLWRhdGE=",
                },
                { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgpyZXN0" },
            ]),
        )
    );
    assert_eq!(
        get(&payload_json(&response), "data.0.url"),
        Some(&json!("data:image/jpeg;base64,QUJD"))
    );
}

// Not upstream's: a tool call's stream gives a partial image event, then an
// event for each image.
#[tokio::test]
async fn tool_stream() {
    let mock = Mock::start(Reply::sse(TOOL_STREAM)).await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-image-1", r#"{"prompt":"A red fox","images":[]}"#),
            image_options(EDITS, true),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        text,
        concat!(
            "event: image_edit.partial_image\n",
            "data: {\"type\":\"image_edit.partial_image\",\"partial_image_index\":0,\"b64_json\":\"UA==\"}\n\n\n",
            "event: image_edit.completed\n",
            "data: {\"type\":\"image_edit.completed\",\"usage\":{\"images\":1},\"b64_json\":\"QUJD\"}\n\n\n",
        )
    );
    assert_eq!(get(&mock.last().json(), "stream"), Some(&json!(true)));
}

// Not upstream's: a call that ends without an image is a bad gateway, and
// one that ends without completing is a timeout, or for a stream just ends.
#[tokio::test]
async fn tool_calls_without_images() {
    let empty = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\"}]}}\n\n";
    let mock = Mock::start(Reply::sse(empty)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-image-1", r#"{"prompt":"x"}"#),
            image_options(GENERATIONS, false),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 502, "{error:?}");
    assert_eq!(error.message, "upstream did not return image output");
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-image-1", r#"{"prompt":"x"}"#),
            image_options(GENERATIONS, true),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(text.is_empty(), "{text}");
    assert_eq!(error.map(|error| error.status), Some(502));

    let created = "data: {\"type\":\"response.created\",\"response\":{}}\n\n";
    let mock = Mock::start(Reply::sse(created)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-image-1", r#"{"prompt":"x"}"#),
            image_options(GENERATIONS, false),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 504, "{error:?}");
    assert_eq!(
        error.message,
        "stream error: stream disconnected before completion"
    );
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-image-1", r#"{"prompt":"x"}"#),
            image_options(GENERATIONS, true),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(text.is_empty() && error.is_none(), "{text} {error:?}");
}

// Not upstream's: Codex's error statuses come back, either way, with the
// credential's key redacted, as does an Image API answer.
#[tokio::test]
async fn error_statuses_and_answers_hide_the_key() {
    let key = "sk-codex-secret";
    let with_key = |url: &str| {
        let mut auth = (*api_key_auth(url)).clone();
        auth.attributes.insert("api_key".into(), key.into());
        Arc::new(auth)
    };
    let mock = Mock::start(Reply::error(
        429,
        r#"{"error":{"message":"slow down, sk-codex-secret"}}"#,
    ))
    .await;
    for model in ["gpt-image-2", "gpt-image-1"] {
        let error = executor()
            .execute(
                with_key(&mock.url),
                request(model, r#"{"prompt":"x"}"#),
                image_options(GENERATIONS, false),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 429, "{model}: {error:?}");
        assert!(
            error.message.contains("slow down, [redacted]"),
            "{model}: {error:?}"
        );
        let error = refused(
            executor()
                .execute_stream(
                    with_key(&mock.url),
                    request(model, r#"{"prompt":"x"}"#),
                    image_options(GENERATIONS, true),
                )
                .await,
        );
        assert_eq!(error.status, 429, "{model}: {error:?}");
        assert!(!error.message.contains(key), "{model}: {error:?}");
    }

    let mock = Mock::start(Reply::json(r#"{"echo":"sk-codex-secret"}"#)).await;
    let response = executor()
        .execute(
            with_key(&mock.url),
            request("gpt-image-2", r#"{"prompt":"x"}"#),
            image_options(GENERATIONS, false),
        )
        .await
        .unwrap();
    assert_eq!(&response.payload[..], br#"{"echo":"[redacted]"}"#);
    let response = executor()
        .execute_stream(
            with_key(&mock.url),
            request("gpt-image-2", r#"{"prompt":"x"}"#),
            image_options(GENERATIONS, true),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("[redacted]") && !text.contains(key), "{text}");
}

// Not upstream's: the main model of a tool call is the config's
// `gpt-image-2-base-model` when it starts with `gpt-`.
#[tokio::test]
async fn tool_main_model() {
    let mock = Mock::start(Reply::sse(TOOL_STREAM)).await;
    for (setting, model) in [
        ("debug: false\n", "gpt-5.4-mini"),
        ("gpt-image-2-base-model: ' gpt-5.5 '\n", "gpt-5.5"),
        ("gpt-image-2-base-model: o3\n", "gpt-5.4-mini"),
    ] {
        executor()
            .with_config(Arc::new(Config::parse(setting).unwrap()))
            .execute(
                api_key_auth(&mock.url),
                request("gpt-image-1", r#"{"prompt":"x"}"#),
                image_options(GENERATIONS, false),
            )
            .await
            .unwrap();
        assert_eq!(get(&mock.last().json(), "model"), Some(&json!(model)));
    }
}

// Not upstream's: requests the executor can't make are refused unsent.
#[tokio::test]
async fn refuses_requests_it_cannot_make() {
    let mock = Mock::start(Reply::sse(TOOL_STREAM)).await;
    let refuse = |model: &'static str, payload: &'static str, options: Options| {
        let url = mock.url.clone();
        async move {
            executor()
                .execute(api_key_auth(&url), request(model, payload), options)
                .await
                .unwrap_err()
                .message
        }
    };
    assert_eq!(
        refuse(
            "gpt-image-1",
            "{}",
            image_options("/v1/images/generations ", false)
        )
        .await,
        r#"unsupported OpenAI image endpoint path "/v1/images/generations ""#
    );
    assert_eq!(
        refuse("gpt-image-1", "not json", image_options(GENERATIONS, false)).await,
        "invalid OpenAI image generation request JSON"
    );
    assert_eq!(
        refuse("gpt-image-1", "not json", image_options(EDITS, false)).await,
        "invalid OpenAI image edit request JSON"
    );
    assert_eq!(
        refuse(
            "gpt-image-1",
            "x",
            with_header(
                image_options(EDITS, false),
                "content-type",
                "multipart/form-data"
            )
        )
        .await,
        "multipart boundary is required"
    );
    assert_eq!(
        refuse(
            "gpt-image-2",
            "not json",
            with_header(image_options(EDITS, false), "content-type", "text/plain")
        )
        .await,
        r#"unsupported OpenAI image edit Content-Type "text/plain""#
    );
    assert_eq!(
        refuse(
            "gpt-image-2",
            "not json",
            with_header(
                image_options(EDITS, false),
                "content-type",
                "multipart/form-data; boundary=\" \""
            )
        )
        .await,
        "multipart boundary is missing"
    );
    assert!(mock.requests().is_empty());
}
