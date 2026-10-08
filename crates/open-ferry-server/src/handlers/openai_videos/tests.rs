// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers_test.go
// (TestVideosCreateRejectsUnsupportedModel,
// TestVideosCreateInvalidSizeReturnsFailedVideoResource,
// TestXAIVideosNativeRejectsUnsupportedModel,
// TestXAIVideosNativeRejectsInvalidJSON,
// TestWriteVideoContentFromURLUsesPinnedAuthProxy,
// TestWriteVideoContentFromURLFallsBackToGlobalProxy) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The video routes end to end over a fake dispatcher, which records the
//! calls and the downloads the handlers ask for; [`native`] has them
//! through the credential manager and the real xAI executor.
//!
//! Changed from upstream:
//! - The requests go through the router, with a client key, where
//!   upstream's call a handler on a route of its own.
//! - `TestWriteVideoContentFromURLUsesPinnedAuthProxy` and
//!   `TestWriteVideoContentFromURLFallsBackToGlobalProxy` check the
//!   credential the download is asked for with, whose proxy it goes
//!   through, where upstream's check the proxy of the HTTP client the
//!   handler makes. The proxy a credential, or none, gives is checked by the
//!   manager's and the xAI executor's tests, and in [`native`].

mod native;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::exec::{Downloaded, ErrorKind, ExecError, Format};
use open_ferry_core::multipart::Writer;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::bindings::Binding;
use crate::config::ServerConfig;
use crate::errors::JSON_UTF8;
use crate::router;
use crate::state::AppState;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, Recorded, state};

const KEY: &str = "sk-test";
const OPENAI_PATH: &str = "/openai/v1/videos";
const DEFAULT_MODEL: &str = "grok-imagine-video";
const MODEL_15: &str = "grok-imagine-video-1.5";
const PREVIEW: &str = "grok-imagine-video-1.5-preview";
const VIDEO_URL: &str = "https://vidgen.x.ai/video.mp4";

/// A router over a fake dispatcher, and the state it serves.
struct Server {
    app: Router,
    dispatcher: Arc<FakeDispatcher>,
    state: AppState,
}

impl Server {
    /// The only call the dispatcher was given.
    fn only_call(&self) -> Recorded {
        let calls = self.dispatcher.calls();
        let [call] = calls.as_slice() else {
            panic!("calls: {calls:?}");
        };
        call.clone()
    }

    /// The credential and model held for `video_id`.
    fn bound(&self, video_id: &str) -> Option<Binding> {
        self.state.video_bindings().get(video_id)
    }

    /// Holds `video_id` as made by `auth_id` with `model`, for a minute.
    fn hold(&self, video_id: &str, auth_id: &str, model: &str) {
        self.state
            .video_bindings()
            .set(video_id, auth_id, model, Duration::from_secs(60));
    }
}

/// A server with `config` and the client key `sk-test`, serving the xAI
/// video models through `xai`, its dispatcher giving `outcomes`.
fn server_with(config: ServerConfig, outcomes: Vec<Outcome>) -> Server {
    let catalog = FakeCatalog::new()
        .serve(DEFAULT_MODEL, &["xai"])
        .serve(MODEL_15, &["xai"])
        .serve(PREVIEW, &["xai"]);
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: vec![KEY.into()],
        ..config
    };
    let state = state(config, catalog, &dispatcher);
    Server {
        app: router(state.clone()),
        dispatcher,
        state,
    }
}

fn server(outcomes: Vec<Outcome>) -> Server {
    server_with(ServerConfig::default(), outcomes)
}

/// A request with the client key, and `content_type` if there is one.
fn request(
    method: Method,
    uri: &str,
    content_type: Option<&str>,
    body: impl Into<Body>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"));
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder.body(body.into()).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    request(Method::GET, uri, None, Body::empty())
}

fn post(uri: &str, body: &str) -> Request<Body> {
    request(Method::POST, uri, Some("application/json"), body.to_owned())
}

/// The status, headers and body of the response to `request`.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|err| panic!("{err}: {}", String::from_utf8_lossy(body)))
}

fn header_of<'h>(headers: &'h HeaderMap, name: &str) -> &'h str {
    headers
        .get(name)
        .map_or("", |value| value.to_str().unwrap())
}

fn content_type(headers: &HeaderMap) -> &str {
    header_of(headers, "content-type")
}

fn binding(auth_id: &str, model: &str) -> Option<Binding> {
    Some(Binding {
        auth_id: auth_id.to_owned(),
        model: model.to_owned(),
    })
}

/// A download's answer: `status`, `headers` and `chunks`.
fn downloaded(
    status: u16,
    status_text: &str,
    headers: &[(&str, &str)],
    chunks: &[&str],
) -> Downloaded {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    let chunks: Vec<Result<Bytes, ExecError>> = chunks
        .iter()
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk.as_bytes())))
        .collect();
    Downloaded {
        status,
        status_text: status_text.to_owned(),
        headers: map,
        body: stream::iter(chunks).boxed(),
    }
}

/// xAI's answer about the finished video `id`, its file at `url`.
fn finished(id: &str, url: &str) -> Outcome {
    let body = json!({
        "request_id": id,
        "status": "done",
        "progress": 100,
        "video": {"url": url, "duration": 4},
    });
    Outcome::reply(&body.to_string())
}

/// The OpenAI error message in `body`.
fn error_message(body: &str) -> String {
    parse(body.as_bytes())["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("no error message: {body}"))
        .to_owned()
}

/// An error a handler answers itself, as gin's `c.JSON` writes it.
fn local_error(message: &str) -> Value {
    json!({"error": {"message": message, "type": "invalid_request_error"}})
}

#[tokio::test]
async fn videos_create_rejects_unsupported_model() {
    // TestVideosCreateRejectsUnsupportedModel.
    let server = server(vec![]);
    let (status, headers, body) = send(
        &server.app,
        post(
            OPENAI_PATH,
            r#"{"model":"not-a-video-model","prompt":"make a video"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(content_type(&headers), "application/json");
    let answer = parse(body.as_bytes());
    assert_eq!(answer["object"], "video");
    assert_eq!(answer["model"], "not-a-video-model");
    assert_eq!(answer["status"], "failed");
    assert_eq!(answer["progress"], 0);
    assert_eq!(answer["error"]["code"], "invalid_request_error");
    assert_eq!(
        answer["error"]["message"],
        "Model not-a-video-model is not supported on /openai/v1/videos. Use sora-2."
    );
    assert!(answer["error"].get("type").is_none(), "{body}");
    let id = answer["id"].as_str().unwrap();
    assert!(id.starts_with("video_"), "{body}");
    assert!(server.dispatcher.calls().is_empty());
}

#[tokio::test]
async fn videos_create_invalid_size_returns_failed_video_resource() {
    // TestVideosCreateInvalidSizeReturnsFailedVideoResource.
    let server = server(vec![]);
    let (status, _, body) = send(
        &server.app,
        post(
            OPENAI_PATH,
            r#"{"model":"sora-2","prompt":"make a video","size":"1080x1920"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let answer = parse(body.as_bytes());
    assert_eq!(answer["object"], "video");
    assert_eq!(answer["model"], DEFAULT_MODEL);
    assert_eq!(answer["status"], "failed");
    assert_eq!(answer["progress"], 0);
    assert_eq!(answer["error"]["code"], "invalid_request_error");
    assert_eq!(
        answer["error"]["message"],
        "Invalid request: size must be one of 720x1280, 1280x720, 1024x1792, or 1792x1024"
    );
    assert!(answer["error"].get("type").is_none(), "{body}");
    assert!(server.dispatcher.calls().is_empty());
}

#[tokio::test]
async fn xai_videos_native_rejects_unsupported_model() {
    // TestXAIVideosNativeRejectsUnsupportedModel.
    let server = server(vec![]);
    let (status, headers, body) = send(
        &server.app,
        post(
            "/v1/videos/generations",
            r#"{"model":"sora-2","prompt":"make a video"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(content_type(&headers), JSON_UTF8);
    assert_eq!(
        parse(body.as_bytes()),
        local_error(
            "Model sora-2 is not supported on /v1/videos/generations, /v1/videos/edits, \
             or /v1/videos/extensions. Use grok-imagine-video."
        )
    );
    assert!(server.dispatcher.calls().is_empty());
}

#[tokio::test]
async fn xai_videos_native_rejects_invalid_json() {
    // TestXAIVideosNativeRejectsInvalidJSON.
    let server = server(vec![]);
    let (status, _, body) = send(&server.app, post("/v1/videos/edits", r#"{"model":"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        parse(body.as_bytes()),
        local_error("Invalid request: body must be valid JSON")
    );
    assert!(server.dispatcher.calls().is_empty());
}

#[tokio::test]
async fn write_video_content_from_url_uses_pinned_auth_proxy() {
    // TestWriteVideoContentFromURLUsesPinnedAuthProxy: the call and the
    // download go with the credential held for the video.
    let server = server(vec![finished("video_123", VIDEO_URL)]);
    server.hold("video_123", "video-content-auth", "");
    server.dispatcher.download_with(Ok(downloaded(
        200,
        "200 OK",
        &[("content-type", "video/mp4")],
        &["video-bytes"],
    )));
    let (status, _, body) = send(&server.app, get("/openai/v1/videos/video_123/content")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "video-bytes");

    let call = server.only_call();
    assert_eq!(
        call.options.metadata.pinned_auth_id.as_deref(),
        Some("video-content-auth")
    );
    let downloads = server.dispatcher.downloads();
    let [download] = downloads.as_slice() else {
        panic!("downloads: {downloads:?}");
    };
    assert_eq!(download.provider, "xai");
    assert_eq!(download.auth_id, "video-content-auth");
    assert_eq!(download.url, VIDEO_URL);
}

#[tokio::test]
async fn write_video_content_from_url_falls_back_to_global_proxy() {
    // TestWriteVideoContentFromURLFallsBackToGlobalProxy: with no
    // credential held for the video, the download names none, so it goes
    // through the global proxy.
    let server = server(vec![finished("video_456", VIDEO_URL)]);
    server
        .dispatcher
        .download_with(Ok(downloaded(200, "200 OK", &[], &["video-bytes"])));
    let (status, _, body) = send(&server.app, get("/openai/v1/videos/video_456/content")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(server.only_call().options.metadata.pinned_auth_id, None);
    let downloads = server.dispatcher.downloads();
    let [download] = downloads.as_slice() else {
        panic!("downloads: {downloads:?}");
    };
    assert_eq!(download.auth_id, "");
    assert_eq!(server.bound("video_456"), None);
}

// Not upstream's: an OpenAI create goes to xAI as xAI's request for the
// model it maps to, is answered with an OpenAI video object, and has its
// video held with the credential that made it.
#[tokio::test]
async fn creates_go_to_xai_and_answer_with_a_video_object() {
    let server = server(vec![Outcome::via(
        &["xai-a"],
        Outcome::reply(r#"{"request_id":"vid-1","status":"pending"}"#),
    )]);
    let (status, headers, body) = send(
        &server.app,
        post(
            OPENAI_PATH,
            r#"{"model":"sora-2","prompt":"make a video","seconds":"8","size":"1280x720"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type(&headers), "application/json");
    let mut answer = parse(body.as_bytes());
    let created_at = answer
        .as_object_mut()
        .unwrap()
        .remove("created_at")
        .unwrap();
    assert!(created_at.as_i64().unwrap() > 0, "{body}");
    assert_eq!(
        answer,
        json!({
            "id": "vid-1",
            "object": "video",
            "model": DEFAULT_MODEL,
            "status": "queued",
            "progress": 0,
            "prompt": "make a video",
            "seconds": "8",
            "size": "1280x720",
        })
    );

    let call = server.only_call();
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["xai"]);
    assert_eq!(call.request.model, DEFAULT_MODEL);
    assert_eq!(
        parse(&call.request.payload),
        json!({
            "model": DEFAULT_MODEL,
            "prompt": "make a video",
            "duration": 8,
            "aspect_ratio": "16:9",
            "resolution": "720p",
        })
    );
    assert_eq!(call.options.source_format, Format::OPENAI_VIDEO);
    assert!(!call.options.stream);
    assert_eq!(call.options.metadata.request_path, OPENAI_PATH);
    assert_eq!(call.options.metadata.pinned_auth_id, None);
    assert_eq!(server.bound("vid-1"), binding("xai-a", DEFAULT_MODEL));
}

// Not upstream's: a create sent as a form, of either kind, is read from its
// fields.
#[tokio::test]
async fn form_creates_are_read() {
    let server = server(vec![
        Outcome::reply(r#"{"request_id":"vid-1"}"#),
        Outcome::reply(r#"{"request_id":"vid-2"}"#),
    ]);
    let urlencoded = "model=sora-2&prompt=a+dog&reference_image_urls=\
        https%3A%2F%2Fexample.com%2Fa.png%2C+https%3A%2F%2Fexample.com%2Fb.png";
    let (status, _, body) = send(
        &server.app,
        request(
            Method::POST,
            OPENAI_PATH,
            Some("application/x-www-form-urlencoded"),
            urlencoded,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut writer = Writer::with_boundary("video-boundary").unwrap();
    for (name, value) in [
        ("model", PREVIEW),
        ("prompt", " a cat "),
        ("seconds", "6"),
        ("size", "720x1280"),
        ("resolution", "480p"),
        ("image_url", "https://example.com/c.png"),
    ] {
        writer.write_field(name, value.as_bytes());
    }
    let form_type = writer.form_data_content_type();
    let (status, _, body) = send(
        &server.app,
        request(Method::POST, OPENAI_PATH, Some(&form_type), writer.finish()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(parse(body.as_bytes())["model"], MODEL_15);

    let calls = server.dispatcher.calls();
    let [dog, cat] = calls.as_slice() else {
        panic!("calls: {calls:?}");
    };
    assert_eq!(dog.request.model, DEFAULT_MODEL);
    assert_eq!(
        parse(&dog.request.payload),
        json!({
            "model": DEFAULT_MODEL,
            "prompt": "a dog",
            "duration": 4,
            "aspect_ratio": "9:16",
            "resolution": "720p",
            "reference_images": [
                {"url": "https://example.com/a.png"},
                {"url": "https://example.com/b.png"},
            ],
        })
    );
    assert_eq!(cat.request.model, PREVIEW);
    assert_eq!(
        parse(&cat.request.payload),
        json!({
            "model": MODEL_15,
            "prompt": "a cat",
            "duration": 6,
            "aspect_ratio": "9:16",
            "resolution": "480p",
            "image": {"url": "https://example.com/c.png"},
        })
    );
}

// Not upstream's: a refused OpenAI create says why in a failed video
// object, with the model it asked for, mapped where it is xAI's.
#[tokio::test]
async fn refused_creates_say_why() {
    let server = server(vec![]);
    for (media, sent, model, message) in [
        (
            "application/json",
            r#"{"model":"#,
            DEFAULT_MODEL,
            "Invalid request: body must be valid JSON",
        ),
        (
            "application/json",
            r#"{"model":"grok-imagine-video-1.5-preview"}"#,
            MODEL_15,
            "Invalid request: prompt is required",
        ),
        (
            "application/x-www-form-urlencoded",
            "model=dall-e-3&prompt=p",
            "dall-e-3",
            "Model dall-e-3 is not supported on /openai/v1/videos. Use sora-2.",
        ),
    ] {
        let (status, headers, body) = send(
            &server.app,
            request(Method::POST, OPENAI_PATH, Some(media), sent),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{sent}: {body}");
        assert_eq!(content_type(&headers), "application/json");
        let answer = parse(body.as_bytes());
        assert_eq!(answer["status"], "failed", "{sent}");
        assert_eq!(answer["model"], model, "{sent}");
        assert_eq!(answer["error"]["message"], message, "{sent}");
    }
    assert!(server.dispatcher.calls().is_empty());
}

// Not upstream's: a body over the limit gets 413 on each route that takes
// one.
#[tokio::test]
async fn bodies_over_the_limit_get_413() {
    let config = ServerConfig {
        body_limit: 16,
        ..ServerConfig::default()
    };
    let server = server_with(config, vec![]);
    let json = r#"{"model":"sora-2","prompt":"make a video"}"#;
    for sent in [
        post(OPENAI_PATH, json),
        request(
            Method::POST,
            OPENAI_PATH,
            Some("application/x-www-form-urlencoded"),
            "model=sora-2&prompt=make+a+video",
        ),
        post("/v1/videos/generations", json),
    ] {
        let uri = sent.uri().to_string();
        let (status, _, body) = send(&server.app, sent).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{uri}: {body}");
    }
    assert!(server.dispatcher.calls().is_empty());
}

// Not upstream's: a native create goes to xAI with the model it maps to in
// its body, picks its credential by the model it routes by, and is answered
// with xAI's own body; its video is held with that model.
#[tokio::test]
async fn native_creates_send_xais_answer_back() {
    let answer = r#"{"request_id":"vid-2"}"#;
    let server = server(vec![Outcome::via(&["xai-b"], Outcome::reply(answer))]);
    let (status, headers, body) = send(
        &server.app,
        post(
            "/v1/videos/generations",
            r#"{"model":"xai/grok-imagine-video-1.5-preview","prompt":"p","duration":6}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(body, answer);

    let call = server.only_call();
    assert_eq!(call.request.model, PREVIEW);
    assert_eq!(
        parse(&call.request.payload),
        json!({"model": MODEL_15, "prompt": "p", "duration": 6})
    );
    assert_eq!(call.options.source_format, Format::OPENAI_VIDEO);
    assert_eq!(call.options.metadata.request_path, "/v1/videos/generations");
    assert_eq!(server.bound("vid-2"), binding("xai-b", PREVIEW));
}

// Not upstream's: each native create route reaches the handler, which
// tells xAI's executor the route; a body without a model gets the default.
#[tokio::test]
async fn every_native_create_route_is_served() {
    let routes = [
        "/v1/videos",
        "/v1/videos/generations",
        "/v1/videos/edits",
        "/v1/videos/extensions",
    ];
    let server = server(
        routes
            .iter()
            .map(|_| Outcome::reply(r#"{"request_id":"v"}"#))
            .collect(),
    );
    for route in routes {
        let (status, _, body) = send(&server.app, post(route, r#"{"prompt":"p"}"#)).await;
        assert_eq!(status, StatusCode::OK, "{route}: {body}");
    }
    let calls = server.dispatcher.calls();
    assert_eq!(calls.len(), routes.len());
    for (call, route) in calls.iter().zip(routes) {
        assert_eq!(call.options.metadata.request_path, route);
        assert_eq!(call.request.model, DEFAULT_MODEL);
        assert_eq!(
            parse(&call.request.payload),
            json!({"prompt": "p", "model": DEFAULT_MODEL})
        );
    }
}

// Not upstream's: a native retrieve asks after the video with the
// credential and model it was made with, is answered with xAI's body, and
// holds the video again with the credential the call went with.
#[tokio::test]
async fn native_retrieves_are_pinned_to_the_videos_credential() {
    let answer = r#"{"request_id":"vid-1","status":"done"}"#;
    let server = server(vec![Outcome::via(&["xai-c"], Outcome::reply(answer))]);
    server.hold("vid-1", "xai-a", MODEL_15);
    let (status, headers, body) = send(&server.app, get("/v1/videos/vid-1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(body, answer);

    let call = server.only_call();
    assert_eq!(call.request.model, MODEL_15);
    assert_eq!(parse(&call.request.payload), json!({"request_id": "vid-1"}));
    assert_eq!(
        call.options.metadata.pinned_auth_id.as_deref(),
        Some("xai-a")
    );
    assert_eq!(
        call.options.metadata.request_path,
        "/v1/videos/{request_id}"
    );
    assert_eq!(server.bound("vid-1"), binding("xai-c", MODEL_15));
}

// Not upstream's: gin matches a `GET` of a native create route as the
// retrieve of a video by that ID, so the call is made as from the
// retrieve's route.
#[tokio::test]
async fn a_get_of_a_create_route_retrieves_that_id() {
    let ids = ["generations", "edits", "extensions"];
    let server = server(
        ids.iter()
            .map(|id| Outcome::reply(&format!(r#"{{"request_id":"{id}"}}"#)))
            .collect(),
    );
    for id in ids {
        let (status, _, body) = send(&server.app, get(&format!("/v1/videos/{id}"))).await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
    }
    let calls = server.dispatcher.calls();
    assert_eq!(calls.len(), ids.len());
    for (call, id) in calls.iter().zip(ids) {
        assert_eq!(parse(&call.request.payload), json!({"request_id": id}));
        assert_eq!(call.request.model, DEFAULT_MODEL);
        assert_eq!(
            call.options.metadata.request_path,
            "/v1/videos/{request_id}"
        );
    }
}

// Not upstream's: an OpenAI retrieve is pinned as a native one is, and
// answered with an OpenAI video object whose model is xAI's, else the
// default.
#[tokio::test]
async fn retrieves_answer_with_a_video_object() {
    let answer = json!({
        "request_id": "vid-1",
        "status": "done",
        "progress": 100,
        "video": {"url": VIDEO_URL, "duration": 6},
    });
    let server = server(vec![Outcome::via(
        &["xai-a"],
        Outcome::reply(&answer.to_string()),
    )]);
    server.hold("vid-1", "xai-a", PREVIEW);
    let (status, headers, body) = send(&server.app, get("/openai/v1/videos/vid-1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(
        parse(body.as_bytes()),
        json!({
            "id": "vid-1",
            "object": "video",
            "model": DEFAULT_MODEL,
            "status": "completed",
            "progress": 100,
            "seconds": "6",
            "video_url": VIDEO_URL,
        })
    );

    let call = server.only_call();
    assert_eq!(call.request.model, PREVIEW);
    assert_eq!(parse(&call.request.payload), json!({"request_id": "vid-1"}));
    assert_eq!(
        call.options.metadata.pinned_auth_id.as_deref(),
        Some("xai-a")
    );
    assert_eq!(
        call.options.metadata.request_path,
        "/openai/v1/videos/{video_id}"
    );
    assert_eq!(server.bound("vid-1"), binding("xai-a", PREVIEW));
}

// Not upstream's: an ID that is empty once trimmed, or isn't UTF-8 once
// decoded, is refused before any call.
#[tokio::test]
async fn missing_ids_are_refused() {
    let server = server(vec![]);
    for (uri, name) in [
        ("/openai/v1/videos/%20", "video_id"),
        ("/openai/v1/videos/%FF", "video_id"),
        ("/openai/v1/videos/%20/content", "video_id"),
        (
            "/openai/v1/videos/%20/content?variant=thumbnail",
            "video_id",
        ),
        ("/v1/videos/%20", "request_id"),
        ("/v1/videos/%FF", "request_id"),
    ] {
        let (status, headers, body) = send(&server.app, get(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(content_type(&headers), JSON_UTF8);
        assert_eq!(
            parse(body.as_bytes()),
            local_error(&format!("Invalid request: {name} is required")),
            "{uri}"
        );
    }
    assert!(server.dispatcher.calls().is_empty());
}

// Not upstream's: the finished video is fetched from the URL xAI gives,
// with the credential the call went with, and sent with its status and the
// headers upstream copies, and no others.
#[tokio::test]
async fn content_sends_the_video_with_its_headers() {
    let server = server(vec![Outcome::via(&["xai-a"], finished("vid-1", VIDEO_URL))]);
    let copied = [
        ("content-type", "video/mp4"),
        ("content-length", "11"),
        ("content-disposition", r#"attachment; filename="video.mp4""#),
        ("cache-control", "private, max-age=60"),
        ("etag", r#""abc""#),
        ("last-modified", "Mon, 05 Oct 2026 00:00:00 GMT"),
    ];
    let mut all = copied.to_vec();
    all.extend([("set-cookie", "session=secret"), ("x-upstream", "1")]);
    server
        .dispatcher
        .download_with(Ok(downloaded(200, "200 OK", &all, &["video-", "bytes"])));
    let (status, headers, body) = send(&server.app, get("/openai/v1/videos/vid-1/content")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "video-bytes");
    for (name, value) in copied {
        assert_eq!(header_of(&headers, name), value, "{name}");
    }
    assert!(!headers.contains_key("set-cookie"), "{headers:?}");
    assert!(!headers.contains_key("x-upstream"), "{headers:?}");

    let call = server.only_call();
    assert_eq!(call.request.model, DEFAULT_MODEL);
    assert_eq!(parse(&call.request.payload), json!({"request_id": "vid-1"}));
    assert_eq!(
        call.options.metadata.request_path,
        "/openai/v1/videos/{video_id}/content"
    );
    let downloads = server.dispatcher.downloads();
    let [download] = downloads.as_slice() else {
        panic!("downloads: {downloads:?}");
    };
    assert_eq!(download.provider, "xai");
    assert_eq!(download.auth_id, "xai-a");
    assert_eq!(download.url, VIDEO_URL);
    assert_eq!(server.bound("vid-1"), binding("xai-a", DEFAULT_MODEL));
}

// Not upstream's: a video without a type of its own is sent as bytes, with
// whatever success status the download had.
#[tokio::test]
async fn content_without_a_type_is_octet_stream() {
    let server = server(vec![finished("vid-1", VIDEO_URL)]);
    server.dispatcher.download_with(Ok(downloaded(
        203,
        "203 Non-Authoritative Information",
        &[("content-type", "")],
        &["v"],
    )));
    let (status, headers, body) = send(&server.app, get("/openai/v1/videos/vid-1/content")).await;
    assert_eq!(status, StatusCode::NON_AUTHORITATIVE_INFORMATION, "{body}");
    assert_eq!(content_type(&headers), "application/octet-stream");
    assert_eq!(body, "v");
}

// Not upstream's: the video is sent as it comes, not once it has all come.
#[tokio::test]
async fn a_video_is_streamed_as_it_comes() {
    let server = server(vec![finished("vid-1", VIDEO_URL)]);
    let first = stream::iter([Ok::<_, ExecError>(Bytes::from_static(b"first"))]);
    server.dispatcher.download_with(Ok(Downloaded {
        status: 200,
        status_text: "200 OK".into(),
        headers: HeaderMap::new(),
        body: first.chain(stream::pending()).boxed(),
    }));
    let response = server
        .app
        .clone()
        .oneshot(get("/openai/v1/videos/vid-1/content"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("the first chunk came before the video ended")
        .unwrap()
        .unwrap();
    assert_eq!(frame.into_data().unwrap(), "first");
}

// Not upstream's: only the video itself can be fetched, the variant read
// trimmed.
#[tokio::test]
async fn content_variants_other_than_video_are_refused() {
    let server = server(vec![
        finished("vid-1", VIDEO_URL),
        finished("vid-1", VIDEO_URL),
    ]);
    for (query, shown) in [("thumbnail", "thumbnail"), ("+spritesheet+", "spritesheet")] {
        let uri = format!("/openai/v1/videos/vid-1/content?variant={query}");
        let (status, headers, body) = send(&server.app, get(&uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(content_type(&headers), JSON_UTF8);
        assert_eq!(
            parse(body.as_bytes()),
            local_error(&format!(
                "Invalid request: variant \"{shown}\" is not available for xAI video downloads"
            ))
        );
    }
    assert!(server.dispatcher.calls().is_empty());

    for query in ["+video+", ""] {
        server
            .dispatcher
            .download_with(Ok(downloaded(200, "200 OK", &[], &["v"])));
        let uri = format!("/openai/v1/videos/vid-1/content?variant={query}");
        let (status, _, body) = send(&server.app, get(&uri)).await;
        assert_eq!(status, StatusCode::OK, "{query}: {body}");
    }
}

// Not upstream's: a download that fails is an OpenAI error with its status
// and its body, trimmed, else its status line; xAI's answer without a
// usable URL is a 502, with nothing fetched.
#[tokio::test]
async fn failed_downloads_are_errors() {
    let results = [
        (
            Ok(downloaded(404, "404 Not Found", &[], &["  gone \n"])),
            404,
            "video content download failed: gone",
        ),
        (
            Ok(downloaded(403, "403 Forbidden", &[], &[" "])),
            403,
            "video content download failed: 403 Forbidden",
        ),
        (
            Err(ExecError::new(ErrorKind::Upstream, "connection refused").with_status(502)),
            502,
            "connection refused",
        ),
    ];
    let server = server(
        results
            .iter()
            .map(|_| finished("vid-1", VIDEO_URL))
            .chain([
                Outcome::reply(r#"{"request_id":"vid-1","status":"pending"}"#),
                finished("vid-1", "ftp://vidgen.x.ai/video.mp4"),
            ])
            .collect(),
    );
    for (result, want, message) in results {
        server.dispatcher.download_with(result);
        let (status, headers, body) =
            send(&server.app, get("/openai/v1/videos/vid-1/content")).await;
        assert_eq!(status.as_u16(), want, "{body}");
        assert_eq!(content_type(&headers), "application/json");
        assert_eq!(error_message(&body), message);
    }
    for message in [
        "xAI video response did not include video.url",
        "xAI video response included invalid video.url",
    ] {
        let (status, _, body) = send(&server.app, get("/openai/v1/videos/vid-1/content")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(error_message(&body), message);
    }
    assert_eq!(server.dispatcher.downloads().len(), 3);
}

// Not upstream's: a failed download's body is read up to the body limit
// when that is under 4 MiB.
#[tokio::test]
async fn failed_download_bodies_are_read_up_to_the_limit() {
    let config = ServerConfig {
        body_limit: 8,
        ..ServerConfig::default()
    };
    let server = server_with(config, vec![finished("vid-1", VIDEO_URL)]);
    server.dispatcher.download_with(Ok(downloaded(
        500,
        "500 Internal Server Error",
        &[],
        &["0123", "456789abc", "def"],
    )));
    let (status, _, body) = send(&server.app, get("/openai/v1/videos/vid-1/content")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(
        error_message(&body),
        "video content download failed: 01234567"
    );
}

// Not upstream's: a call that fails is answered with xAI's status as an
// OpenAI error, on every route, and holds nothing.
#[tokio::test]
async fn failed_calls_are_errors() {
    let requests = [
        post(OPENAI_PATH, r#"{"prompt":"p"}"#),
        post("/v1/videos", r#"{"prompt":"p"}"#),
        get("/v1/videos/vid-1"),
        get("/openai/v1/videos/vid-1"),
        get("/openai/v1/videos/vid-1/content"),
    ];
    let server = server(
        requests
            .iter()
            .map(|_| {
                let error = ExecError::new(ErrorKind::Upstream, "slow down").with_status(429);
                Outcome::via(&["xai-a"], Outcome::Fail(error))
            })
            .collect(),
    );
    for sent in requests {
        let uri = sent.uri().to_string();
        let (status, headers, body) = send(&server.app, sent).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{uri}: {body}");
        assert_eq!(content_type(&headers), "application/json");
        assert_eq!(error_message(&body), "slow down", "{uri}");
    }
    assert!(server.dispatcher.downloads().is_empty());
    assert_eq!(server.bound("vid-1"), None);
}

// Not upstream's: gin answers a method a path has no route for, and a
// `HEAD`, with its 404.
#[tokio::test]
async fn other_methods_get_404() {
    let server = server(vec![]);
    for (method, uri) in [
        (Method::HEAD, "/v1/videos"),
        (Method::HEAD, "/v1/videos/generations"),
        (Method::HEAD, "/v1/videos/edits"),
        (Method::HEAD, "/v1/videos/extensions"),
        (Method::HEAD, "/v1/videos/vid-1"),
        (Method::HEAD, "/openai/v1/videos"),
        (Method::HEAD, "/openai/v1/videos/vid-1"),
        (Method::HEAD, "/openai/v1/videos/vid-1/content"),
        (Method::GET, "/v1/videos"),
        (Method::GET, "/v1/videos/"),
        (Method::GET, "/openai/v1/videos"),
        (Method::POST, "/v1/videos/vid-1"),
        (Method::POST, "/openai/v1/videos/vid-1"),
        (Method::POST, "/openai/v1/videos/vid-1/content"),
        (Method::DELETE, "/openai/v1/videos/vid-1"),
    ] {
        let (status, _, body) = send(
            &server.app,
            request(method.clone(), uri, None, Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        if method != Method::HEAD {
            assert_eq!(body, "404 page not found", "{method} {uri}");
        }
    }
    assert!(server.dispatcher.calls().is_empty());
}

// Not upstream's: every route needs a client key.
#[tokio::test]
async fn the_routes_need_a_client_key() {
    let server = server(vec![]);
    for (method, uri) in [
        (Method::POST, "/v1/videos"),
        (Method::POST, "/v1/videos/generations"),
        (Method::POST, "/v1/videos/edits"),
        (Method::POST, "/v1/videos/extensions"),
        (Method::GET, "/v1/videos/generations"),
        (Method::GET, "/v1/videos/vid-1"),
        (Method::POST, "/openai/v1/videos"),
        (Method::GET, "/openai/v1/videos/vid-1"),
        (Method::GET, "/openai/v1/videos/vid-1/content"),
    ] {
        let unkeyed = Request::builder()
            .method(method.clone())
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"prompt":"p"}"#))
            .unwrap();
        let (status, _, body) = send(&server.app, unkeyed).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}: {body}");
    }
    assert!(server.dispatcher.calls().is_empty());
}
