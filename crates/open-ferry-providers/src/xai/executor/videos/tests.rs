// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_test.go
// (TestXAIExecutorExecuteVideosCreate,
// TestXAIExecutorExecuteVideosPublishesFailureUsage,
// TestXAIExecutorExecuteVideosPublishesRequestBuildFailureUsage,
// TestXAIExecutorExecuteVideosRetrieve,
// TestXAIExecutorExecuteVideosUsesNativeEndpointFromRequestPath,
// TestXAIExecutorExecuteVideosOAuthBaseURLResolution) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Video calls and downloads against a mock xAI server on 127.0.0.1. The
//! mock records each request's method, target, headers and body. Upstream's
//! tests sign in with OAuth; these use a dummy API key, the only credential
//! served. Their usage records come from a usage queue of the test's own,
//! fed by the call's taps as a server handler's are.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::{Method as AxumMethod, Uri};
use bytes::Bytes;
use http::HeaderMap;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Options, Request};
use open_ferry_core::executor::ProviderExecutor;

use super::*;
use crate::xai::executor::tests::{assert_no_tokens, execute_observed, records, usage_queue};
use crate::xai::request::DEFAULT_BASE_URL;

/// The dummy API key the tests send.
const API_KEY: &str = "xai-test-key";

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    /// The request target as it came: a path, or a whole URL through a
    /// proxy.
    target: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

/// What the mock answers every request with.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: Bytes,
}

impl Reply {
    fn json(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "application/json")],
            body: Bytes::from(body.to_owned()),
        }
    }

    fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }
}

/// A mock server bound to an ephemeral port on 127.0.0.1.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(reply: Reply) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(
            move |method: AxumMethod, uri: Uri, headers: HeaderMap, body: Bytes| {
                let reply = reply.clone();
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Seen {
                            method: method.to_string(),
                            target: uri.to_string(),
                            headers,
                            body: body.to_vec(),
                        });
                    let mut response = axum::response::Response::builder().status(reply.status);
                    for (name, value) in reply.headers {
                        response = response.header(name, value);
                    }
                    response.body(axum::body::Body::from(reply.body)).unwrap()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn last(&self) -> Seen {
        self.requests().pop().expect("no request reached the mock")
    }
}

/// An executor that doesn't use the environment's proxy.
fn executor() -> XaiExecutor {
    XaiExecutor::new("direct")
}

/// An API key credential for `base_url`.
fn api_key_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        id: "xai-1".into(),
        provider: "xai".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), API_KEY.into());
    Arc::new(auth)
}

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

/// Options for a call from the video endpoints at `path`.
fn video_options(path: &str) -> Options {
    let mut options = Options::new(Format::OPENAI_VIDEO);
    options.metadata.request_path = path.into();
    options
}

async fn execute(
    executor: &XaiExecutor,
    auth: Arc<Auth>,
    request: Request,
    options: Options,
) -> Result<Response, ExecError> {
    ProviderExecutor::execute(executor, auth, request, options).await
}

async fn collect(body: open_ferry_core::exec::ChunkStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut body = body;
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

// Ported from TestXAIExecutorExecuteVideosCreate: a create posts the body
// as it came to /videos/generations, with the key and the idempotency key,
// xAI's answer comes back, and the one usage record names the model and no
// tokens.
#[tokio::test]
async fn video_create_posts_to_generations() {
    let mock = Mock::start(Reply::json(r#"{"request_id":"vid_123"}"#)).await;
    let queue = usage_queue();
    let payload = r#"{"model":"grok-imagine-video","prompt":"animate","duration":4}"#;
    let mut options = Options::new(Format::OPENAI_VIDEO);
    options.metadata.idempotency_key = Some("idem-123".into());

    let response = execute_observed(
        &executor(),
        &queue,
        api_key_auth(&mock.url),
        request("grok-imagine-video", payload),
        options,
    )
    .await
    .unwrap();

    let seen = mock.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.target, "/videos/generations");
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(seen.header("x-idempotency-key"), Some("idem-123"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_eq!(seen.body, payload.as_bytes());
    assert_eq!(&response.payload[..], br#"{"request_id":"vid_123"}"#);
    assert_eq!(
        response.headers.get("content-type").unwrap(),
        "application/json"
    );

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-video", "{record}");
    assert_eq!(record["provider"], "xai", "{record}");
    assert_eq!(record["executor_type"], "XAIExecutor", "{record}");
    assert_eq!(record["failed"], false, "{record}");
    assert_no_tokens(record);
    assert!(record["ttft_ms"].as_i64().unwrap() >= 0, "{record}");
}

// Ported from TestXAIExecutorExecuteVideosPublishesFailureUsage: xAI's
// failure is an error with its status, and the one record a failure with
// it, naming the body's model.
#[tokio::test]
async fn video_failure_keeps_xais_status() {
    let mock = Mock::start(Reply::status(429, r#"{"error":"rate limited"}"#)).await;
    let queue = usage_queue();
    let error = execute_observed(
        &executor(),
        &queue,
        api_key_auth(&mock.url),
        request(
            "video-model-alias",
            r#"{"model":"grok-imagine-video-failure","prompt":"animate"}"#,
        ),
        Options::new(Format::OPENAI_VIDEO),
    )
    .await
    .unwrap_err();
    assert_eq!(error.http_status(), 429, "{error:?}");
    assert_eq!(error.message, r#"{"error":"rate limited"}"#);

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-video-failure", "{record}");
    assert_eq!(record["failed"], true, "{record}");
    assert_eq!(record["fail"]["status_code"], 429, "{record}");
}

// Ported from TestXAIExecutorExecuteVideosPublishesRequestBuildFailureUsage:
// a base URL that isn't one fails the call before anything is sent, and the
// one record is a failure naming the request's model, the body having none.
#[tokio::test]
async fn video_request_with_a_broken_base_url_fails() {
    let mut auth = Auth {
        provider: "xai".into(),
        ..Auth::default()
    };
    auth.attributes
        .insert("base_url".into(), "://invalid".into());
    let queue = usage_queue();
    let error = execute_observed(
        &executor(),
        &queue,
        Arc::new(auth),
        request("grok-imagine-video-fallback", r#"{"prompt":"animate"}"#),
        Options::new(Format::OPENAI_VIDEO),
    )
    .await
    .unwrap_err();
    assert_eq!(error.http_status(), 0, "{error:?}");

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-video-fallback", "{record}");
    assert_eq!(record["failed"], true, "{record}");
}

// Ported from TestXAIExecutorExecuteVideosRetrieve: a body with a
// `request_id` gets the video's state, with no body.
#[tokio::test]
async fn video_retrieve_gets_the_video() {
    let answer = r#"{"status":"done","video":{"url":"https://vidgen.x.ai/video.mp4","duration":6},"model":"grok-imagine-video","progress":100}"#;
    let mock = Mock::start(Reply::json(answer)).await;
    let mut options = Options::new(Format::OPENAI_VIDEO);
    options.metadata.idempotency_key = Some("idem-123".into());
    let response = execute(
        &executor(),
        api_key_auth(&mock.url),
        request("grok-imagine-video", r#"{"request_id":"vid_123"}"#),
        options,
    )
    .await
    .unwrap();

    let seen = mock.last();
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.target, "/videos/vid_123");
    assert!(seen.body.is_empty());
    assert_eq!(seen.header("x-idempotency-key"), None);
    assert_eq!(&response.payload[..], answer.as_bytes());
}

// Not upstream's: a retrieve publishes a record as upstream's
// `executeVideos` does, naming the request's model, the body having none,
// and the model xAI's answer names, with no tokens even when the answer
// names some.
#[tokio::test]
async fn video_retrieves_publish_usage() {
    let answer = concat!(
        r#"{"status":"done","video":{"url":"https://vidgen.x.ai/video.mp4"},"#,
        r#""model":"grok-imagine-video-0801","usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}"#
    );
    let mock = Mock::start(Reply::json(answer)).await;
    let queue = usage_queue();
    execute_observed(
        &executor(),
        &queue,
        api_key_auth(&mock.url),
        request("grok-imagine-video", r#"{"request_id":"vid_123"}"#),
        video_options("/openai/v1/videos/vid_123"),
    )
    .await
    .unwrap();
    assert_eq!(mock.last().method, "GET");

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-imagine-video", "{record}");
    assert_eq!(
        record["response_model"], "grok-imagine-video-0801",
        "{record}"
    );
    assert_eq!(record["failed"], false, "{record}");
    assert_no_tokens(record);
}

// Ported from TestXAIExecutorExecuteVideosUsesNativeEndpointFromRequestPath:
// a native route posts to its own path, a request_id or not.
#[tokio::test]
async fn video_native_routes_post_to_their_path() {
    let mock = Mock::start(Reply::json(r#"{"request_id":"vid_123"}"#)).await;
    for (route, want) in [
        ("/v1/videos/generations", "/videos/generations"),
        ("/v1/videos/edits", "/videos/edits"),
        ("/v1/videos/extensions", "/videos/extensions"),
    ] {
        for payload in [
            r#"{"model":"grok-imagine-video","prompt":"animate"}"#,
            // Not upstream's: the path wins over a request_id.
            r#"{"model":"grok-imagine-video","request_id":"vid_123"}"#,
        ] {
            execute(
                &executor(),
                api_key_auth(&mock.url),
                request("grok-imagine-video", payload),
                video_options(route),
            )
            .await
            .unwrap();
            let seen = mock.last();
            assert_eq!(seen.method, "POST", "{route}");
            assert_eq!(seen.target, want, "{route}");
            assert_eq!(seen.body, payload.as_bytes(), "{route}");
        }
    }
}

// Ported from TestXAIExecutorExecuteVideosOAuthBaseURLResolution's API-key
// cases: an API key without a base URL goes to xAI's API, one with a base
// URL to it. The OAuth cases aren't ported (only API keys are served).
#[test]
fn video_calls_go_under_the_credentials_base_url() {
    let body = Bytes::from_static(br#"{"model":"grok-imagine-video","prompt":"a flying bird"}"#);
    let sent = serde_json::from_slice(&body).unwrap();
    let options = video_options("/v1/videos/generations");

    let mut auth = Auth::default();
    auth.attributes.insert("api_key".into(), API_KEY.into());
    let (method, url, _) = endpoint(base_url(&auth), &options, body.clone(), &sent);
    assert_eq!(method, Method::POST);
    assert_eq!(url, format!("{DEFAULT_BASE_URL}/videos/generations"));
    assert_eq!(url, "https://api.x.ai/v1/videos/generations");

    auth.attributes.insert(
        "base_url".into(),
        "https://custom-gateway.example.com/v1/".into(),
    );
    let (_, url, _) = endpoint(base_url(&auth), &options, body, &sent);
    assert_eq!(
        url,
        "https://custom-gateway.example.com/v1/videos/generations"
    );
}

// Not upstream's: a request ID is escaped as Go's url.PathEscape escapes a
// path segment.
#[tokio::test]
async fn video_request_ids_are_path_escaped() {
    assert_eq!(path_escape("vid_1-2.3~"), "vid_1-2.3~");
    assert_eq!(path_escape("a$&+:=@b"), "a$&+:=@b");
    assert_eq!(
        path_escape("a/b;c,d?e f#g%h"),
        "a%2Fb%3Bc%2Cd%3Fe%20f%23g%25h"
    );
    assert_eq!(path_escape("é\"<>"), "%C3%A9%22%3C%3E");

    let mock = Mock::start(Reply::json("{}")).await;
    execute(
        &executor(),
        api_key_auth(&mock.url),
        request("grok-imagine-video", r#"{"request_id":" ../a/b?c "}"#),
        Options::new(Format::OPENAI_VIDEO),
    )
    .await
    .unwrap();
    assert_eq!(mock.last().target, "/videos/..%2Fa%2Fb%3Fc");
}

// Not upstream's: without the client's Idempotency-Key, its own
// x-idempotency-key is sent on, trimmed; an empty one isn't.
#[tokio::test]
async fn video_posts_take_the_clients_idempotency_header() {
    let mock = Mock::start(Reply::json(r#"{"request_id":"vid_123"}"#)).await;
    for (metadata, header, want) in [
        (None, Some(" header-key "), Some("header-key")),
        (Some(" meta-key "), Some("header-key"), Some("meta-key")),
        (Some("  "), Some("header-key"), Some("header-key")),
        (None, Some("  "), None),
        (None, None, None),
    ] {
        let mut options = Options::new(Format::OPENAI_VIDEO);
        options.metadata.idempotency_key = metadata.map(str::to_owned);
        if let Some(header) = header {
            options
                .headers
                .insert("x-idempotency-key", header.parse().unwrap());
        }
        execute(
            &executor(),
            api_key_auth(&mock.url),
            request("grok-imagine-video", r#"{"prompt":"animate"}"#),
            options,
        )
        .await
        .unwrap();
        assert_eq!(
            mock.last().header("x-idempotency-key"),
            want,
            "{metadata:?} {header:?}"
        );
    }
}

// Not upstream's: image references are put in xAI's shape and the payload
// rules apply for the body's model; a body neither changes is sent byte for
// byte.
#[tokio::test]
async fn video_bodies_get_image_refs_and_payload_rules() {
    let mock = Mock::start(Reply::json(r#"{"request_id":"vid_123"}"#)).await;
    let config = Config::parse(
        "payload:\n  override:\n    - models:\n        - name: grok-imagine-video\n          protocol: openai\n      params:\n        resolution: 480p\n",
    )
    .unwrap();
    let executor = executor().with_config(Arc::new(config));

    let payload = r#"{"model":"grok-imagine-video","prompt":"animate","image":{"image_url":{"url":"https://example.com/a.png"}},"duration":4.0}"#;
    execute(
        &executor,
        api_key_auth(&mock.url),
        request("alias", payload),
        Options::new(Format::OPENAI_VIDEO),
    )
    .await
    .unwrap();
    assert_eq!(
        String::from_utf8(mock.last().body).unwrap(),
        r#"{"model":"grok-imagine-video","prompt":"animate","image":{"url":"https://example.com/a.png"},"duration":4.0,"resolution":"480p"}"#
    );

    // Another model: no rule applies, and the body goes as it came.
    let payload = "{ \"model\" : \"grok-imagine-video-1.5\", \"prompt\":\"animate\" }";
    execute(
        &executor,
        api_key_auth(&mock.url),
        request("alias", payload),
        Options::new(Format::OPENAI_VIDEO),
    )
    .await
    .unwrap();
    assert_eq!(mock.last().body, payload.as_bytes());
}

// Not upstream's: the key the request sent is redacted from xAI's answer,
// a failure's or a success's.
#[tokio::test]
async fn video_answers_lose_the_requests_secrets() {
    let echo = format!(r#"{{"error":"bad key {API_KEY}"}}"#);
    for status in [401, 200] {
        let mock = Mock::start(Reply::status(status, &echo)).await;
        let result = execute(
            &executor(),
            api_key_auth(&mock.url),
            request("grok-imagine-video", r#"{"prompt":"animate"}"#),
            Options::new(Format::OPENAI_VIDEO),
        )
        .await;
        let text = match result {
            Ok(response) => String::from_utf8(response.payload.to_vec()).unwrap(),
            Err(error) => error.message,
        };
        assert!(!text.contains(API_KEY), "{status}: {text}");
        assert!(text.contains("bad key"), "{status}: {text}");
    }
}

// Not upstream's: a streaming video call, or one for compaction, is still
// refused before anything is sent.
#[tokio::test]
async fn video_streams_and_compactions_are_refused() {
    let mock = Mock::start(Reply::json("{}")).await;
    let mut options = Options::new(Format::OPENAI_VIDEO);
    options.stream = true;
    let result = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("grok-imagine-video", r#"{"prompt":"animate"}"#),
            options,
        )
        .await;
    let Err(error) = result else {
        panic!("a video stream started");
    };
    assert_eq!(error.http_status(), 400);

    let mut options = Options::new(Format::OPENAI_VIDEO);
    options.alt = "responses/compact".into();
    let error = execute(
        &executor(),
        api_key_auth(&mock.url),
        request("grok-imagine-video", r#"{"prompt":"animate"}"#),
        options,
    )
    .await
    .unwrap_err();
    assert_eq!(error.http_status(), 400);
    assert!(mock.requests().is_empty());
}

/// A credential that names `proxy_url`, with a custom header.
fn proxied_auth(proxy_url: &str) -> Arc<Auth> {
    let mut auth = (*api_key_auth("http://127.0.0.1:9")).clone();
    auth.proxy_url = proxy_url.into();
    auth.attributes
        .insert("header:X-Custom".into(), "custom-value".into());
    Arc::new(auth)
}

// Not upstream's (upstream's handler test fakes the transport): a download
// streams the file with its headers, and sends neither the key nor a
// custom header.
#[tokio::test]
async fn download_streams_the_file_without_credentials() {
    let mock = Mock::start(Reply {
        status: 200,
        headers: vec![
            ("content-type", "video/mp4"),
            ("content-disposition", "attachment; filename=\"v.mp4\""),
            ("etag", "\"abc\""),
        ],
        body: Bytes::from_static(b"\x00\x00\x00\x18ftypmp42video"),
    })
    .await;
    let url = format!("{}/files/v.mp4?token=DOWNLOADSECRET123", mock.url);
    let downloaded = executor()
        .download(Some(proxied_auth("direct")), url)
        .await
        .unwrap();
    assert_eq!(downloaded.status, 200);
    assert_eq!(downloaded.status_text, "200 OK");
    assert_eq!(downloaded.headers.get("content-type").unwrap(), "video/mp4");
    assert_eq!(downloaded.headers.get("etag").unwrap(), "\"abc\"");
    assert_eq!(
        collect(downloaded.body).await,
        b"\x00\x00\x00\x18ftypmp42video"
    );

    let seen = mock.last();
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.target, "/files/v.mp4?token=DOWNLOADSECRET123");
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(seen.header("x-custom"), None);
    assert!(seen.body.is_empty());
}

// Not upstream's: a failed download comes back with its status and its
// body, redacted of the URL's secrets; an empty body as no chunk.
#[tokio::test]
async fn download_failures_come_back_redacted() {
    let mock = Mock::start(Reply::status(403, "denied DOWNLOADSECRET123")).await;
    let url = format!("{}/v.mp4?token=DOWNLOADSECRET123", mock.url);
    let downloaded = executor().download(None, url).await.unwrap();
    assert_eq!(downloaded.status, 403);
    assert_eq!(downloaded.status_text, "403 Forbidden");
    let body = String::from_utf8(collect(downloaded.body).await).unwrap();
    assert!(body.starts_with("denied "), "{body}");
    assert!(!body.contains("DOWNLOADSECRET123"), "{body}");

    let mock = Mock::start(Reply::status(404, "")).await;
    let downloaded = executor()
        .download(None, format!("{}/v.mp4", mock.url))
        .await
        .unwrap();
    assert_eq!(downloaded.status, 404);
    assert_eq!(downloaded.status_text, "404 Not Found");
    let chunks: Vec<_> = downloaded.body.collect().await;
    assert!(chunks.is_empty());
}

// Not upstream's: a download goes through the credential's proxy, or the
// global proxy without a credential.
#[tokio::test]
async fn download_goes_through_the_credentials_proxy() {
    let proxy = Mock::start(Reply::json("proxied")).await;
    let target = "http://127.0.0.1:9/v.mp4";

    let downloaded = executor()
        .download(Some(proxied_auth(&proxy.url)), target.into())
        .await
        .unwrap();
    assert_eq!(collect(downloaded.body).await, b"proxied");
    assert_eq!(proxy.last().target, target);

    let global = XaiExecutor::new(proxy.url.clone());
    let downloaded = global.download(None, target.into()).await.unwrap();
    assert_eq!(collect(downloaded.body).await, b"proxied");
    assert_eq!(proxy.requests().len(), 2);
    assert_eq!(proxy.last().target, target);
}

// Not upstream's: no answer is a 502, and a URL with a control character
// isn't fetched.
#[tokio::test]
async fn download_without_an_answer_is_a_bad_gateway() {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let closed = socket.local_addr().unwrap();
    let error = executor()
        .download(
            None,
            format!("http://{closed}/v.mp4?token=DOWNLOADSECRET123"),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.http_status(), 502);
    assert!(!error.message.contains("DOWNLOADSECRET123"), "{error:?}");

    let mock = Mock::start(Reply::json("{}")).await;
    let error = executor()
        .download(None, format!("{}/v\n.mp4", mock.url))
        .await
        .err()
        .unwrap();
    assert_eq!(error.http_status(), 502);
    assert!(mock.requests().is_empty());
}
