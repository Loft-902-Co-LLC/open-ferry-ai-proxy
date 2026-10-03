//! The router, end to end, against a fake dispatcher.

use std::io::Write;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::exec::{ExecError, Format};
use open_ferry_core::models::ModelInfo;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::config::{ServerConfig, StreamingConfig};
use crate::router;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

/// A server with `outcomes`, serving `gpt-5` through `codex` and
/// `claude-sonnet` through `claude`, with the key `sk-test`.
fn app(config: ServerConfig, outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    let catalog = FakeCatalog::new()
        .serve("gpt-5", &["codex"])
        .serve("claude-sonnet", &["claude"])
        .first("gpt-5")
        .models(vec![
            ModelInfo {
                id: "gpt-5".into(),
                owned_by: "openai".into(),
                created: 1_754_524_800,
                display_name: "GPT 5".into(),
                ..ModelInfo::default()
            },
            ModelInfo {
                id: "claude-sonnet".into(),
                owned_by: "anthropic".into(),
                created: 1_747_872_000,
                display_name: "Claude Sonnet".into(),
                context_length: 1_000_000,
                ..ModelInfo::default()
            },
        ]);
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..config
    };
    (router(state(config, catalog, &dispatcher)), dispatcher)
}

/// A request with the test key.
fn authed(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer sk-test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
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

#[tokio::test]
async fn serves_health_and_turns_away_unknown_routes() {
    let (app, _) = app(ServerConfig::default(), vec![]);
    let get = |uri: &str| Request::get(uri).body(Body::empty()).unwrap();

    let (status, _, body) = send(&app, get("/healthz")).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"status":"ok"}"#)
    );

    let (status, _, body) = send(&app, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("open-ferry-ai-proxy"), "{body}");

    for request in [
        get("/nope"),
        get("/v1/models/"),
        Request::delete("/v1/models").body(Body::empty()).unwrap(),
    ] {
        let (status, headers, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "404 page not found");
        assert_eq!(content_type(&headers), "text/plain");
        assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    }

    let options = Request::builder()
        .method(Method::OPTIONS)
        .uri("/v1/chat/completions")
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = send(&app, options).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_HEADERS], "*");
}

#[tokio::test]
async fn requires_a_key() {
    let (app, dispatcher) = app(ServerConfig::default(), vec![]);
    let request = |key: Option<&str>| {
        let mut builder = Request::post("/v1/chat/completions");
        if let Some(key) = key {
            builder = builder.header("x-api-key", key);
        }
        builder.body(Body::from("{}")).unwrap()
    };
    let (status, _, body) = send(&app, request(None)).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNAUTHORIZED, r#"{"error":"Missing API key"}"#)
    );
    let (status, _, body) = send(&app, request(Some("sk-wrong"))).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNAUTHORIZED, r#"{"error":"Invalid API key"}"#)
    );
    assert!(dispatcher.calls().is_empty());

    let (status, _, _) = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn safe_mode_shuts_the_proxy_routes() {
    let config = ServerConfig {
        api_keys: vec!["sk-test".into(), "your-api-key-1".into()],
        ..ServerConfig::default()
    };
    let dispatcher = FakeDispatcher::new([]);
    let app = router(state(config, FakeCatalog::new(), &dispatcher));
    let (status, headers, body) = send(&app, authed(Method::GET, "/v1/models", "")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(headers["x-cpa-safe-mode"], "example-api-key");
    assert!(body.contains("unsafe_example_api_key"), "{body}");
    let (status, _, _) = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn lists_models_in_either_format() {
    let (app, _) = app(ServerConfig::default(), vec![]);
    let (status, headers, body) = send(&app, authed(Method::GET, "/v1/models", "")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json; charset=utf-8");
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list["object"], "list");
    assert_eq!(list["data"][0]["id"], "claude-sonnet");
    assert_eq!(list["data"][1]["id"], "gpt-5");

    let mut request = authed(Method::GET, "/v1/models", "");
    request
        .headers_mut()
        .insert("anthropic-version", "2023-06-01".parse().unwrap());
    let (_, _, body) = send(&app, request).await;
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list["data"][0]["id"], "claude-sonnet");
    assert_eq!(list["data"][0]["max_input_tokens"], 1_000_000);
    assert_eq!(list["first_id"], "claude-sonnet");

    let (_, _, body) = send(&app, authed(Method::GET, "/v1/models?client_version=1", "")).await;
    assert_eq!(body, r#"{"models":[]}"#);
}

#[tokio::test]
async fn chat_completions_call_the_dispatcher() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::reply(r#"{"id":"c1"}"#)],
    );
    let mut request = authed(
        Method::POST,
        "/v1/chat/completions?key=sk-test&x=1",
        r#"{"model":"gpt-5(high)","messages":[]}"#,
    );
    request
        .headers_mut()
        .insert("idempotency-key", " k1 ".parse().unwrap());
    let (status, headers, body) = send(&app, request).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, r#"{"id":"c1"}"#));
    assert_eq!(content_type(&headers), "application/json");

    let calls = dispatcher.calls();
    let call = &calls[0];
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["codex"]);
    assert_eq!(call.request.model, "gpt-5(high)");
    assert_eq!(call.options.source_format, Format::OPENAI);
    assert!(!call.options.stream);
    assert!(call.options.headers.get(header::AUTHORIZATION).is_none());
    assert_eq!(call.options.query, [("x".to_owned(), "1".to_owned())]);
    assert_eq!(call.options.metadata.request_path, "/v1/chat/completions");
    assert_eq!(call.options.metadata.requested_model, "gpt-5(high)");
    assert_eq!(call.options.metadata.idempotency_key.as_deref(), Some("k1"));
}

#[tokio::test]
async fn unknown_models_and_failed_calls_answer_with_errors() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::Fail(ExecError::upstream(429, "slow down")),
            Outcome::Fail(ExecError::auth_not_found()),
        ],
    );
    let (status, _, body) = send(
        &app,
        authed(Method::POST, "/v1/chat/completions", r#"{"model":"nope"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("unknown provider for model nope"), "{body}");
    assert!(dispatcher.calls().is_empty());

    let chat = r#"{"model":"gpt-5","messages":[]}"#;
    let (status, _, body) = send(&app, authed(Method::POST, "/v1/chat/completions", chat)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body,
        r#"{"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
    );

    let (status, _, body) = send(&app, authed(Method::POST, "/v1/chat/completions", chat)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("providers=codex, model=gpt-5"), "{body}");
}

#[tokio::test]
async fn chat_completions_stream() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::chunks(&[r#"{"n":1}"#, "", r#"{"n":2}"#]),
            Outcome::Stream(
                HeaderMap::new(),
                vec![
                    Ok(Bytes::from_static(br#"{"n":1}"#)),
                    Err(ExecError::upstream(500, "boom")),
                ],
            ),
            Outcome::Fail(ExecError::upstream(400, r#"{"error":{"message":"bad"}}"#)),
            Outcome::chunks(&[]),
        ],
    );
    let chat = r#"{"model":"gpt-5","messages":[],"stream":true}"#;
    let post = || authed(Method::POST, "/v1/chat/completions", chat);

    let (status, headers, body) = send(&app, post()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(
        body,
        "data: {\"n\":1}\n\ndata: {\"n\":2}\n\ndata: [DONE]\n\n"
    );
    assert!(dispatcher.calls()[0].options.stream);

    let (_, _, body) = send(&app, post()).await;
    assert_eq!(
        body,
        "data: {\"n\":1}\n\ndata: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\",\"code\":\"internal_server_error\"}}\n\n"
    );

    let (status, _, body) = send(&app, post()).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, r#"{"error":{"message":"bad"}}"#)
    );

    let (status, _, body) = send(&app, post()).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "data: [DONE]\n\n")
    );
}

#[tokio::test]
async fn streams_restart_when_they_fail_before_their_first_payload() {
    let config = ServerConfig {
        streaming: StreamingConfig {
            bootstrap_retries: 1,
            ..StreamingConfig::default()
        },
        ..ServerConfig::default()
    };
    let (app, dispatcher) = app(
        config,
        vec![
            Outcome::Stream(
                HeaderMap::new(),
                vec![Err(ExecError::upstream(503, "busy"))],
            ),
            Outcome::chunks(&[r#"{"n":1}"#]),
            Outcome::Stream(HeaderMap::new(), vec![Err(ExecError::upstream(400, "no"))]),
        ],
    );
    let chat = r#"{"model":"gpt-5","messages":[],"stream":true}"#;
    let (status, _, body) = send(&app, authed(Method::POST, "/v1/chat/completions", chat)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "data: {\"n\":1}\n\ndata: [DONE]\n\n");
    assert_eq!(dispatcher.calls().len(), 2);

    // A 400 isn't worth a restart.
    let (status, _, _) = send(&app, authed(Method::POST, "/v1/chat/completions", chat)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(dispatcher.calls().len(), 3);
}

#[tokio::test]
async fn completions_convert_to_and_from_chat() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::reply(
                r#"{"id":"c1","object":"chat.completion","created":1,"model":"gpt-5","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#,
            ),
            Outcome::chunks(&[
                r#"{"id":"c2","object":"chat.completion.chunk","created":1,"model":"gpt-5","choices":[{"index":0,"delta":{"content":"yo"}}]}"#,
            ]),
        ],
    );
    let (status, _, body) = send(
        &app,
        authed(
            Method::POST,
            "/v1/completions",
            r#"{"model":"gpt-5","prompt":"say hi"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["object"], "text_completion");
    assert_eq!(reply["choices"][0]["text"], "hi");
    let sent: Value = serde_json::from_slice(&dispatcher.calls()[0].request.payload).unwrap();
    assert_eq!(
        sent["messages"],
        json!([{"role": "user", "content": "say hi"}])
    );

    let (_, _, body) = send(
        &app,
        authed(
            Method::POST,
            "/v1/completions",
            r#"{"model":"gpt-5","prompt":"say yo","stream":true}"#,
        ),
    )
    .await;
    let first = body
        .strip_prefix("data: ")
        .unwrap()
        .split("\n\n")
        .next()
        .unwrap();
    let chunk: Value = serde_json::from_str(first).unwrap();
    assert_eq!(chunk["choices"][0]["text"], "yo");
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    assert_eq!(dispatcher.calls()[1].options.alt, "");
}

#[tokio::test]
async fn claude_messages() {
    let mut zipped = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    zipped.write_all(br#"{"type":"message"}"#).unwrap();
    let zipped = zipped.finish().unwrap();
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![
            Outcome::Reply(open_ferry_core::exec::Response {
                payload: Bytes::from(zipped),
                headers: HeaderMap::new(),
            }),
            Outcome::chunks(&["event: ping\ndata: {}\n\n"]),
            Outcome::Stream(
                HeaderMap::new(),
                vec![
                    Ok(Bytes::from_static(b"event: ping\ndata: {}\n\n")),
                    Err(ExecError::upstream(529, "overloaded")),
                ],
            ),
            Outcome::reply(r#"{"input_tokens":3}"#),
        ],
    );
    let (status, _, body) = send(
        &app,
        authed(Method::POST, "/v1/messages", r#"{"model":"claude-sonnet"}"#),
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"type":"message"}"#)
    );

    // A `null` stream streams.
    let stream = r#"{"model":"claude-sonnet","stream":null}"#;
    let (_, headers, body) = send(&app, authed(Method::POST, "/v1/messages", stream)).await;
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(body, "event: ping\ndata: {}\n\n");

    let (_, _, body) = send(&app, authed(Method::POST, "/v1/messages", stream)).await;
    assert_eq!(
        body,
        "event: ping\ndata: {}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n"
    );

    let (status, _, body) = send(
        &app,
        authed(
            Method::POST,
            "/v1/messages/count_tokens",
            r#"{"model":"claude-sonnet"}"#,
        ),
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"input_tokens":3}"#)
    );

    let calls = dispatcher.calls();
    assert_eq!(calls[0].options.source_format, Format::CLAUDE);
    assert!(calls[1].options.stream);
    assert_eq!(calls[3].method, "count_tokens");
    assert_eq!(calls[3].options.response_format, Format::from_static(""));
}

#[tokio::test]
async fn bodies_are_limited_and_decoded() {
    let config = ServerConfig {
        body_limit: 64,
        ..ServerConfig::default()
    };
    let (app, dispatcher) = app(config, vec![Outcome::reply("{}")]);
    let big = format!(
        r#"{{"model":"gpt-5","messages":[],"pad":"{}"}}"#,
        "x".repeat(64)
    );
    let (status, _, _) = send(&app, authed(Method::POST, "/v1/chat/completions", &big)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let mut zipped = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    zipped
        .write_all(br#"{"model":"gpt-5","messages":[]}"#)
        .unwrap();
    let mut request = Request::post("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer sk-test")
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from(zipped.finish().unwrap()))
        .unwrap();
    request.headers_mut().remove(header::CONTENT_LENGTH);
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        &dispatcher.calls()[0].request.payload[..],
        br#"{"model":"gpt-5","messages":[]}"#
    );
}

#[tokio::test(start_paused = true)]
async fn streams_keep_alive_while_the_provider_is_quiet() {
    let config = ServerConfig {
        streaming: StreamingConfig {
            keepalive: Some(std::time::Duration::from_secs(5)),
            ..StreamingConfig::default()
        },
        ..ServerConfig::default()
    };
    let (app, _) = app(
        config,
        vec![Outcome::Hang(
            HeaderMap::new(),
            vec![Ok(Bytes::from_static(br#"{"n":1}"#))],
        )],
    );
    let chat = r#"{"model":"gpt-5","messages":[],"stream":true}"#;
    let response = app
        .oneshot(authed(Method::POST, "/v1/chat/completions", chat))
        .await
        .unwrap();
    let mut body = response.into_body();
    let mut next = async || {
        let frame = body.frame().await.unwrap().unwrap();
        String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
    };
    assert_eq!(next().await, "data: {\"n\":1}\n\n");
    assert_eq!(next().await, ": keep-alive\n\n");
    assert_eq!(next().await, ": keep-alive\n\n");
}

#[tokio::test]
async fn chat_completions_take_responses_bodies() {
    let (app, dispatcher) = app(
        ServerConfig::default(),
        vec![Outcome::chunks(&[r#"{"n":1}"#])],
    );
    let body = r#"{"model":"gpt-5","instructions":"be brief","input":"hi","stream":true}"#;
    let (status, _, body) = send(&app, authed(Method::POST, "/v1/chat/completions", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "data: {\"n\":1}\n\ndata: [DONE]\n\n");

    let call = &dispatcher.calls()[0];
    assert!(call.options.stream);
    let sent: Value = serde_json::from_slice(&call.request.payload).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(
        sent["messages"],
        json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hi"},
        ])
    );
}
