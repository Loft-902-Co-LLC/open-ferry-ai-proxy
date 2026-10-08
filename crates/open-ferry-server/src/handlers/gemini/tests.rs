// Ported from CLIProxyAPI sdk/api/handlers/gemini/gemini_handlers_stream_error_test.go
// and gemini_models_display_name_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini routes end to end against a fake dispatcher. The expected
//! bytes are what upstream's handlers, built with Go 1.26.4, write for the
//! same requests and results. In the expected bodies, `~` stands for a
//! backslash.
//!
//! Changed from upstream:
//! - `TestGeminiStreamGenerateContentDoesNotLoseErrorBeforeFirstPayload`
//!   sends its 100 requests at once through one router and fake dispatcher,
//!   where upstream builds a credential manager for each. It checks the same
//!   status and body.
//! - `TestGeminiModelsResponseUsesConfiguredDisplayName` reads the model
//!   from a fake catalog rather than the global registry.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use futures_util::future::join_all;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Response as ExecResponse};
use open_ferry_core::models::ModelInfo;
use tower::ServiceExt;

use super::{ACTION_ROUTE, decode_path, go_string, marshal};
use crate::config::{ServerConfig, StreamingConfig};
use crate::router;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

const MODEL: &str = "gemini-2.5-pro";
const BODY: &str = r#"{"contents":[{"parts":[{"text":"hi"}]}]}"#;
const UNKNOWN: &str = r#"{"error":{"message":"unknown provider for model nope","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#;
const BOOM: &str =
    r#"{"error":{"message":"boom","type":"server_error","code":"internal_server_error"}}"#;
const SLOW_DOWN: &str =
    r#"{"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#;
const NOT_FOUND: &str = r#"{"error":{"message":"Not Found","type":"not_found"}}"#;
const JSON_UTF8: &str = "application/json; charset=utf-8";

/// `s` with each `~` made a backslash.
fn go(s: &str) -> String {
    s.replace('~', "\x5c")
}

/// The models the list tests use: one with every Gemini field and a
/// prefixed name, one with a bare name, and one with only an ID.
fn listed_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "plain".into(),
            ..ModelInfo::default()
        },
        ModelInfo {
            id: "full".into(),
            name: "models/full-name".into(),
            version: "001".into(),
            display_name: "Full <Model>".into(),
            description: "A & B".into(),
            input_token_limit: 1_048_576,
            output_token_limit: 65_536,
            supported_generation_methods: vec!["generateContent".into(), "countTokens".into()],
            supported_input_modalities: vec!["TEXT".into(), "IMAGE".into()],
            supported_output_modalities: vec!["TEXT".into()],
            ..ModelInfo::default()
        },
        ModelInfo {
            id: "named".into(),
            name: "bare-name".into(),
            display_name: "Shown".into(),
            ..ModelInfo::default()
        },
    ]
}

/// A server with `outcomes` and `models`, serving `gemini-2.5-pro`, `a/b`
/// and `m` through `gemini`, with the key `sk-test`.
fn server(
    config: ServerConfig,
    models: Vec<ModelInfo>,
    outcomes: Vec<Outcome>,
) -> (Router, Arc<FakeDispatcher>) {
    let catalog = FakeCatalog::new()
        .serve(MODEL, &["gemini"])
        .serve("a/b", &["gemini"])
        .serve("m", &["gemini"])
        .models(models);
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..config
    };
    (router(state(config, catalog, &dispatcher)), dispatcher)
}

fn app(outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    server(ServerConfig::default(), listed_models(), outcomes)
}

/// A request with the test key in `X-Goog-Api-Key`.
fn authed(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-goog-api-key", "sk-test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn post(uri: &str) -> Request<Body> {
    authed(Method::POST, uri, BODY)
}

fn get(uri: &str) -> Request<Body> {
    authed(Method::GET, uri, "")
}

/// The status, headers and body of `request`'s response.
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

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .map_or("", |v| v.to_str().unwrap())
}

fn chunk(s: &str) -> Result<Bytes, ExecError> {
    Ok(Bytes::copy_from_slice(s.as_bytes()))
}

fn upstream_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/x-up"));
    headers.insert("x-up", HeaderValue::from_static("1"));
    headers
}

fn passthrough() -> ServerConfig {
    ServerConfig {
        passthrough_headers: true,
        ..ServerConfig::default()
    }
}

fn action(method: &str) -> String {
    format!("/v1beta/models/{MODEL}:{method}")
}

#[tokio::test]
async fn lists_models_in_gemini_format() {
    let (app, _) = app(vec![]);
    let (status, headers, body) = send(&app, get("/v1beta/models")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), JSON_UTF8);
    let want = go(concat!(
        r#"{"models":["#,
        r#"{"description":"A ~u0026 B","displayName":"Full ~u003cModel~u003e","inputTokenLimit":1048576,"name":"models/full-name","outputTokenLimit":65536,"supportedGenerationMethods":["generateContent","countTokens"],"supportedInputModalities":["TEXT","IMAGE"],"supportedOutputModalities":["TEXT"],"version":"001"},"#,
        r#"{"description":"bare-name","displayName":"Shown","name":"models/bare-name","supportedGenerationMethods":["generateContent"]},"#,
        r#"{"description":"plain","displayName":"plain","name":"models/plain","supportedGenerationMethods":["generateContent"]}"#,
        "]}"
    ));
    assert_eq!(body, want);

    let (app, _) = server(ServerConfig::default(), vec![], vec![]);
    let (status, _, body) = send(&app, get("/v1beta/models")).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"models":[]}"#)
    );
}

// TestGeminiModelsResponseUsesConfiguredDisplayName
#[tokio::test]
async fn model_list_uses_the_configured_display_name() {
    const ID: &str = "gemini-display-name-catalog-test";
    let models = vec![ModelInfo {
        id: ID.into(),
        name: ID.into(),
        display_name: "Configured Gemini Name".into(),
        ..ModelInfo::default()
    }];
    let (app, _) = server(ServerConfig::default(), models, vec![]);
    let (_, _, body) = send(&app, get("/v1beta/models")).await;
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let found = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["name"] == format!("models/{ID}"))
        .expect("model not found in response");
    assert_eq!(found["displayName"], "Configured Gemini Name");
}

#[tokio::test]
async fn gets_one_model_by_name() {
    let (app, _) = app(vec![]);
    let full = go(concat!(
        r#"{"description":"A ~u0026 B","displayName":"Full ~u003cModel~u003e","inputTokenLimit":1048576,"name":"models/full-name","outputTokenLimit":65536,"#,
        r#""supportedGenerationMethods":["generateContent","countTokens"],"supportedInputModalities":["TEXT","IMAGE"],"supportedOutputModalities":["TEXT"],"version":"001"}"#
    ));
    for (uri, want) in [
        ("/v1beta/models/plain", r#"{"name":"models/plain"}"#),
        ("/v1beta/models/full-name", full.as_str()),
        ("/v1beta/models/models/full-name", full.as_str()),
        (
            "/v1beta/models/bare-name",
            r#"{"displayName":"Shown","name":"models/bare-name"}"#,
        ),
    ] {
        let (status, headers, body) = send(&app, get(uri)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, want), "{uri}");
        assert_eq!(content_type(&headers), JSON_UTF8);
    }
    for uri in [
        // An ID finds nothing when the model has a name.
        "/v1beta/models/full",
        // Only one `models/` comes off.
        "/v1beta/models/models/models/full-name",
        "/v1beta/models/models/bare-name",
        "/v1beta/models//plain",
        "/v1beta/models/plain/",
        "/v1beta/models/",
        "/v1beta/models/plain:generateContent",
    ] {
        let (status, headers, body) = send(&app, get(uri)).await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::NOT_FOUND, NOT_FOUND),
            "{uri}"
        );
        assert_eq!(content_type(&headers), JSON_UTF8);
    }
}

#[tokio::test]
async fn head_and_unrouted_methods_get_gins_404() {
    let (app, dispatcher) = app(vec![]);
    for request in [
        authed(Method::HEAD, "/v1beta/models", ""),
        authed(Method::HEAD, "/v1beta/models/plain", ""),
        authed(Method::PUT, "/v1beta/models/plain", ""),
        // Gin redirects this one to `/v1beta/models/`.
        authed(Method::POST, "/v1beta/models", BODY),
    ] {
        let (status, headers, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(content_type(&headers), "text/plain");
    }
    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn takes_the_key_from_a_header_or_the_query() {
    let (app, dispatcher) = app(vec![
        Outcome::reply("{}"),
        Outcome::reply("{}"),
        Outcome::reply("{}"),
    ]);
    let uri = action("generateContent");
    let bare = |uri: &str| Request::post(uri).body(Body::from(BODY)).unwrap();
    let bare_get = |uri: &str| Request::get(uri).body(Body::empty()).unwrap();

    let (status, _, body) = send(&app, bare(&uri)).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNAUTHORIZED, r#"{"error":"Missing API key"}"#)
    );
    let (status, _, _) = send(&app, bare(&format!("{uri}?key=sk-wrong"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&app, bare_get("/v1beta/models")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(dispatcher.calls().is_empty());

    let (status, _, _) = send(&app, post(&uri)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, bare(&format!("{uri}?key=sk-test&x=1"))).await;
    assert_eq!(status, StatusCode::OK);
    let bearer = Request::post(&uri)
        .header(header::AUTHORIZATION, "Bearer sk-test")
        .body(Body::from(BODY))
        .unwrap();
    let (status, _, _) = send(&app, bearer).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, bare_get("/v1beta/models?key=sk-test")).await;
    assert_eq!(status, StatusCode::OK);

    // The client's key goes no further.
    let calls = dispatcher.calls();
    assert!(calls[0].options.headers.get("x-goog-api-key").is_none());
    assert_eq!(calls[1].options.query, [("x".to_owned(), "1".to_owned())]);
    assert!(
        calls[2]
            .options
            .headers
            .get(header::AUTHORIZATION)
            .is_none()
    );
}

#[tokio::test]
async fn generate_content() {
    let (app, dispatcher) = app(vec![Outcome::reply(r#"{"candidates":[]}"#)]);
    let uri = format!("{}?x=1", action("generateContent"));
    let (status, headers, body) = send(&app, post(&uri)).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"candidates":[]}"#)
    );
    assert_eq!(content_type(&headers), "application/json");

    let call = &dispatcher.calls()[0];
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["gemini"]);
    assert_eq!(call.request.model, MODEL);
    assert_eq!(&call.request.payload[..], BODY.as_bytes());
    assert_eq!(call.options.source_format, Format::GEMINI);
    assert_eq!(call.options.response_format, Format::GEMINI);
    assert!(!call.options.stream);
    assert_eq!(call.options.alt, "");
    assert_eq!(call.options.query, [("x".to_owned(), "1".to_owned())]);
    assert_eq!(call.options.metadata.request_path, ACTION_ROUTE);
    assert_eq!(call.options.metadata.requested_model, MODEL);
}

#[tokio::test]
async fn generate_content_keeps_its_content_type_over_the_providers() {
    let reply = ExecResponse {
        payload: Bytes::from_static(b"{}"),
        headers: upstream_headers(),
    };
    let (app, _) = server(passthrough(), vec![], vec![Outcome::Reply(reply)]);
    let (status, headers, _) = send(&app, post(&action("generateContent"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json");
    assert_eq!(headers["x-up"], "1");
}

#[tokio::test(start_paused = true)]
async fn only_generate_content_keeps_a_slow_call_alive() {
    let config = ServerConfig {
        nonstream_keepalive: Some(Duration::from_secs(1)),
        ..ServerConfig::default()
    };
    let slow = |body: &'static str| {
        Outcome::Slow(
            Duration::from_millis(2500),
            ExecResponse {
                payload: Bytes::from_static(body.as_bytes()),
                headers: HeaderMap::new(),
            },
        )
    };
    let (app, _) = server(
        config,
        vec![],
        vec![slow(r#"{"n":1}"#), slow(r#"{"totalTokens":3}"#)],
    );
    let (status, headers, body) = send(&app, post(&action("generateContent"))).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "\n\n{\"n\":1}"));
    assert_eq!(content_type(&headers), "application/json");

    let (status, _, body) = send(&app, post(&action("countTokens"))).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"totalTokens":3}"#)
    );
}

#[tokio::test]
async fn count_tokens() {
    let (app, dispatcher) = app(vec![Outcome::reply(r#"{"totalTokens":3}"#)]);
    let uri = format!("{}?alt=json", action("countTokens"));
    let (status, headers, body) = send(&app, post(&uri)).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"totalTokens":3}"#)
    );
    assert_eq!(content_type(&headers), "application/json");

    let call = &dispatcher.calls()[0];
    assert_eq!(call.method, "count_tokens");
    assert_eq!(call.request.model, MODEL);
    assert_eq!(call.options.source_format, Format::GEMINI);
    assert_eq!(call.options.response_format, Format::from_static(""));
    assert_eq!(call.options.alt, "json");
    assert!(!call.options.stream);
    assert_eq!(call.options.metadata.request_path, ACTION_ROUTE);
}

#[tokio::test]
async fn failures_before_a_response_are_openai_errors() {
    let refused = || Outcome::Fail(ExecError::upstream(429, "slow down"));
    let refused_stream = || {
        Outcome::Stream(
            HeaderMap::new(),
            vec![Err(ExecError::upstream(429, "slow down"))],
        )
    };
    let bad = r#"{"error":{"message":"bad"}}"#;
    let (app, _) = app(vec![
        refused(),
        Outcome::Fail(ExecError::upstream(400, bad)),
        refused(),
        refused_stream(),
        refused_stream(),
        Outcome::Stream(HeaderMap::new(), vec![Err(ExecError::upstream(503, ""))]),
    ]);
    let unavailable = r#"{"error":{"message":"Service Unavailable","type":"server_error","code":"internal_server_error"}}"#;
    for (method, status, want) in [
        ("generateContent", StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN),
        ("generateContent", StatusCode::BAD_REQUEST, bad),
        ("countTokens", StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN),
        (
            "streamGenerateContent",
            StatusCode::TOO_MANY_REQUESTS,
            SLOW_DOWN,
        ),
        (
            "streamGenerateContent?alt=json",
            StatusCode::TOO_MANY_REQUESTS,
            SLOW_DOWN,
        ),
        (
            "streamGenerateContent",
            StatusCode::SERVICE_UNAVAILABLE,
            unavailable,
        ),
    ] {
        let (got, headers, body) = send(&app, post(&action(method))).await;
        assert_eq!((got, body.as_str()), (status, want), "{method}");
        assert_eq!(content_type(&headers), "application/json");
    }

    // An unknown model never reaches the dispatcher.
    for method in ["generateContent", "streamGenerateContent", "countTokens"] {
        let uri = format!("/v1beta/models/nope:{method}");
        let (status, headers, body) = send(&app, post(&uri)).await;
        assert_eq!((status, body.as_str()), (StatusCode::BAD_REQUEST, UNKNOWN));
        assert_eq!(content_type(&headers), "application/json");
    }
}

// TestGeminiStreamGenerateContentDoesNotLoseErrorBeforeFirstPayload
#[tokio::test]
async fn a_stream_does_not_lose_an_error_before_its_first_payload() {
    let failure = || {
        Outcome::Stream(
            HeaderMap::new(),
            vec![Err(ExecError::new(
                ErrorKind::Upstream,
                "upstream failed before first payload",
            ))],
        )
    };
    let (app, _) = app((0..100).map(|_| failure()).collect());
    let uri = action("streamGenerateContent");
    let responses = join_all((0..100).map(|_| send(&app, post(&uri)))).await;
    for (n, (status, _, body)) in responses.into_iter().enumerate() {
        assert_ne!(status, StatusCode::OK, "request {n}: {body}");
        assert!(
            body.contains("upstream failed before first payload"),
            "request {n}: {status} {body}"
        );
    }
}

#[tokio::test]
async fn streams_sse_unless_another_alt_is_asked_for() {
    let two = || Outcome::chunks(&[r#"{"n":1}"#, r#"{"n":2}"#]);
    let (app, dispatcher) = app(vec![two(), two(), two()]);
    for query in ["", "?alt=sse", "?alt=&$alt=json"] {
        let uri = format!("{}{query}", action("streamGenerateContent"));
        let (status, headers, body) = send(&app, post(&uri)).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(body, "data: {\"n\":1}\n\ndata: {\"n\":2}\n\n", "{query}");
        assert_eq!(content_type(&headers), "text/event-stream");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(headers[header::CONNECTION], "keep-alive");
        assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    }
    let call = &dispatcher.calls()[0];
    assert_eq!(call.method, "execute_stream");
    assert!(call.options.stream);
    assert_eq!(call.options.alt, "");
    assert_eq!(call.options.source_format, Format::GEMINI);
    assert_eq!(call.options.response_format, Format::GEMINI);
    assert_eq!(call.options.metadata.request_path, ACTION_ROUTE);
}

#[tokio::test]
async fn streams_raw_for_another_alt() {
    let (app, dispatcher) = app(vec![
        Outcome::chunks(&[r#"[{"n":1}"#, r#",{"n":2}]"#]),
        Outcome::chunks(&[r#"[{"n":1}"#]),
        Outcome::chunks(&["\x00\x01\x02"]),
        Outcome::chunks(&["  <html><body>"]),
    ]);
    let uri = |query: &str| format!("{}?{query}", action("streamGenerateContent"));

    let (status, headers, body) = send(&app, post(&uri("alt=json"))).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"[{"n":1},{"n":2}]"#)
    );
    // Go's server sniffs the type from the first payload.
    assert_eq!(content_type(&headers), "text/plain; charset=utf-8");
    assert!(headers.get(header::CACHE_CONTROL).is_none());
    assert_eq!(dispatcher.calls()[0].options.alt, "json");

    let (_, headers, _) = send(&app, post(&uri("$alt=json"))).await;
    assert_eq!(content_type(&headers), "text/plain; charset=utf-8");
    assert_eq!(dispatcher.calls()[1].options.alt, "json");

    let (_, headers, _) = send(&app, post(&uri("alt=json"))).await;
    assert_eq!(content_type(&headers), "application/octet-stream");

    let (_, headers, _) = send(&app, post(&uri("alt=json"))).await;
    assert_eq!(content_type(&headers), "text/html; charset=utf-8");
}

#[tokio::test]
async fn streams_pass_the_providers_headers_on() {
    let stream = || Outcome::Stream(upstream_headers(), vec![chunk(r#"{"n":1}"#)]);
    let (app, _) = server(passthrough(), vec![], vec![stream(), stream()]);
    let uri = action("streamGenerateContent");
    let (_, headers, _) = send(&app, post(&uri)).await;
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(headers["x-up"], "1");

    let (_, headers, body) = send(&app, post(&format!("{uri}?alt=json"))).await;
    assert_eq!(body, r#"{"n":1}"#);
    assert_eq!(content_type(&headers), "text/x-up");
    assert_eq!(headers["x-up"], "1");
}

#[tokio::test]
async fn a_failure_partway_ends_the_stream_with_its_error() {
    let failing =
        |error: ExecError| Outcome::Stream(HeaderMap::new(), vec![chunk(r#"{"n":1}"#), Err(error)]);
    let json_error = r#"{"error":{"code":400,"message":"bad"}}"#;
    let (app, _) = app(vec![
        failing(ExecError::upstream(500, "boom")),
        failing(ExecError::upstream(500, "boom")),
        failing(ExecError::upstream(400, json_error)),
        failing(ExecError::upstream(400, json_error)),
    ]);
    let uri = action("streamGenerateContent");
    let raw = format!("{uri}?alt=json");

    let (status, _, body) = send(&app, post(&uri)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        format!("data: {{\"n\":1}}\n\nevent: error\ndata: {BOOM}\n\n")
    );

    let (status, _, body) = send(&app, post(&raw)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, format!("{{\"n\":1}}{BOOM}"));

    let (_, _, body) = send(&app, post(&uri)).await;
    assert_eq!(
        body,
        format!("data: {{\"n\":1}}\n\nevent: error\ndata: {json_error}\n\n")
    );

    let (_, _, body) = send(&app, post(&raw)).await;
    assert_eq!(body, format!("{{\"n\":1}}{json_error}"));
}

#[tokio::test]
async fn a_stream_that_closes_empty_writes_nothing() {
    let (app, _) = app(vec![Outcome::chunks(&[]), Outcome::chunks(&[])]);
    let uri = action("streamGenerateContent");
    let (status, headers, body) = send(&app, post(&uri)).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    assert_eq!(content_type(&headers), "text/event-stream");

    let (status, headers, body) = send(&app, post(&format!("{uri}?alt=json"))).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    assert!(headers.get(header::CONTENT_TYPE).is_none());
}

#[tokio::test(start_paused = true)]
async fn only_sse_streams_keep_alive() {
    let config = ServerConfig {
        streaming: StreamingConfig {
            keepalive: Some(Duration::from_secs(1)),
            ..StreamingConfig::default()
        },
        ..ServerConfig::default()
    };
    let hang = || Outcome::Hang(HeaderMap::new(), vec![chunk(r#"{"n":1}"#)]);
    let (app, _) = server(config, vec![], vec![hang(), hang()]);
    let uri = action("streamGenerateContent");

    let response = app.clone().oneshot(post(&uri)).await.unwrap();
    let mut body = response.into_body();
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&frame[..], b"data: {\"n\":1}\n\n");
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&frame[..], b": keep-alive\n\n");

    let response = app.oneshot(post(&format!("{uri}?alt=json"))).await.unwrap();
    let mut body = response.into_body();
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&frame[..], br#"{"n":1}"#);
    let next = tokio::time::timeout(Duration::from_secs(10), body.frame()).await;
    assert!(next.is_err(), "a raw stream wrote a keep-alive");
}

#[tokio::test]
async fn other_methods_get_an_empty_200() {
    let (app, dispatcher) = app(vec![]);
    for uri in [action("embedContent"), action("")] {
        let (status, headers, body) = send(&app, post(&uri)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, ""), "{uri}");
        assert!(headers.get(header::CONTENT_TYPE).is_none());
    }
    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn actions_that_are_not_model_and_method_get_404() {
    let (app, dispatcher) = app(vec![]);
    for (uri, message) in [
        ("/v1beta/models/ok", "/v1beta/models/ok"),
        ("/v1beta/models/ok:a:b", "/v1beta/models/ok:a:b"),
        ("/v1beta/models/", "/v1beta/models/"),
        ("/v1beta/models/a+b", "/v1beta/models/a+b"),
        // The path is decoded, then escaped as Go's encoder escapes it.
        ("/v1beta/models/a%3Cb%26c", "/v1beta/models/a~u003cb~u0026c"),
        (
            "/v1beta/models/a%E2%80%A8%22b",
            "/v1beta/models/a~u2028~\"b",
        ),
        (
            "/v1beta/models/a%FFb%E2%82c",
            "/v1beta/models/a~ufffdb~ufffd~ufffdc",
        ),
    ] {
        let (status, headers, body) = send(&app, post(uri)).await;
        let want = go(&format!(
            r#"{{"error":{{"message":"{message} not found.","type":"invalid_request_error"}}}}"#
        ));
        assert_eq!((status, body), (StatusCode::NOT_FOUND, want), "{uri}");
        assert_eq!(content_type(&headers), JSON_UTF8);
    }
    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn the_model_comes_from_the_path() {
    let (app, dispatcher) = app(vec![
        Outcome::reply("{}"),
        Outcome::reply("{}"),
        Outcome::reply("{}"),
        Outcome::reply("{}"),
    ]);
    for uri in [
        "/v1beta/models/a/b:generateContent",
        "/v1beta/models/m(high):generateContent",
        "/v1beta/models/gemini-2.5-pro%3AgenerateContent",
    ] {
        let (status, _, _) = send(&app, post(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    // An empty body goes on empty.
    let empty = authed(Method::POST, &action("generateContent"), "");
    let (status, _, _) = send(&app, empty).await;
    assert_eq!(status, StatusCode::OK);

    let calls = dispatcher.calls();
    assert_eq!(calls[0].request.model, "a/b");
    assert_eq!(calls[1].request.model, "m(high)");
    assert_eq!(calls[1].options.metadata.requested_model, "m(high)");
    assert_eq!(calls[2].request.model, MODEL);
    assert!(calls[3].request.payload.is_empty());

    for (uri, model) in [
        ("/v1beta/models/:generateContent", ""),
        (
            "/v1beta/models//gemini-2.5-pro:generateContent",
            "/gemini-2.5-pro",
        ),
    ] {
        let (status, _, body) = send(&app, post(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(
            body.contains(&format!("\"unknown provider for model {model}\"")),
            "{uri}: {body}"
        );
    }
    assert_eq!(dispatcher.calls().len(), 4);
}

#[test]
fn marshals_as_go_does() {
    let value = serde_json::json!({"b": [1, "x<y"], "a": {"d": true, "c": null}});
    assert_eq!(
        marshal(&value),
        go(r#"{"a":{"c":null,"d":true},"b":[1,"x~u003cy"]}"#)
    );
    assert_eq!(go_string(b"a\xffb"), go(r#""a~ufffdb""#));
    assert_eq!(go_string("\u{2029}&".as_bytes()), go(r#""~u2029~u0026""#));
    assert_eq!(decode_path("/a%3a%3Ab%zz%4"), b"/a::b%zz%4");
}
