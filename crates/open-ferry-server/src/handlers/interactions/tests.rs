// Ported from CLIProxyAPI sdk/api/handlers/gemini/interactions_handlers_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1beta/interactions` end to end against a fake dispatcher, which
//! records the calls the handler makes, and, in `native`, through the
//! credential manager and the real `gemini-interactions` executor.
//!
//! Changed from upstream:
//! - The `TestInteractionsRejects*` tests go through the router, and check
//!   the whole body where upstream checks a part.
//! - `TestBuildInteractionsExecutionRequestUsesAgentAuthSelectionModel`
//!   checks the call the dispatcher gets, since the forced provider and the
//!   selection model are the call's metadata here.
//! - `TestInteractionsAntigravityModelUsesTranslatorBridge` isn't ported:
//!   the Antigravity provider isn't.

mod native;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::exec::{ExecError, Format};
use tower::ServiceExt;

use super::{InteractionsWriter, Target, parse_target, prepare_target};
use crate::config::{ServerConfig, StreamingConfig};
use crate::errors::{ErrorMessage, JSON_UTF8};
use crate::router;
use crate::stream::{StreamWriter, forward};
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

const PATH: &str = "/v1beta/interactions";
const MODEL: &str = "gemini-3.5-flash";
const AGENT: &str = "agents/test-agent";
const BOOM: &str =
    r#"{"error":{"message":"boom","type":"server_error","code":"internal_server_error"}}"#;
const SLOW_DOWN: &str =
    r#"{"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#;

/// A server with `outcomes`, serving `gemini-3.5-flash` through `gemini`
/// then `gemini-interactions`, with the key `sk-test`.
fn server(config: ServerConfig, outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    let catalog = FakeCatalog::new().serve(MODEL, &["gemini", "gemini-interactions"]);
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

/// A request to the route with `body` and the test key in
/// `X-Goog-Api-Key`.
fn post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("x-goog-api-key", "sk-test")
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

/// The 400 the handler answers a body it can't take with.
fn rejected(message: &str) -> String {
    format!(r#"{{"error":{{"message":"{message}","type":"invalid_request_error"}}}}"#)
}

/// What `writer` writes for `chunks` and then `error`, if there is one.
async fn written(chunks: &[&str], error: Option<ErrorMessage>) -> String {
    let items: Vec<Result<Bytes, ErrorMessage>> = chunks
        .iter()
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk.as_bytes())))
        .chain(error.map(Err))
        .collect();
    let body = forward(stream::iter(items).boxed(), InteractionsWriter, None);
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

// TestParseInteractionsRequestTarget
#[test]
fn parses_request_targets() {
    let target = |model: &str, agent: &str, stream: bool| Target {
        model: model.into(),
        agent: agent.into(),
        stream,
    };
    for (name, body, want) in [
        (
            "model",
            r#"{"model":"gemini-3.5-flash","input":"hi"}"#,
            Ok(target(MODEL, "", false)),
        ),
        (
            "model resource name",
            r#"{"model":"models/gemini-3.5-flash","input":"hi"}"#,
            Ok(target("models/gemini-3.5-flash", "", false)),
        ),
        (
            "agent",
            r#"{"agent":"agents/test-agent","input":"hi"}"#,
            Ok(target("", AGENT, false)),
        ),
        (
            "missing",
            r#"{"input":"hi"}"#,
            Err("request requires exactly one of model or agent"),
        ),
        (
            "both",
            r#"{"model":"gemini-3.5-flash","agent":"agents/test-agent","input":"hi"}"#,
            Err("request requires exactly one of model or agent"),
        ),
        (
            "stream string",
            r#"{"model":"gemini-3.5-flash","stream":"true","input":"hi"}"#,
            Err("stream must be a boolean"),
        ),
        (
            "stream true",
            r#"{"model":"gemini-3.5-flash","stream":true,"input":"hi"}"#,
            Ok(target(MODEL, "", true)),
        ),
    ] {
        assert_eq!(parse_target(body.as_bytes()), want, "{name}");
    }
}

// Not upstream's: the edges of parseInteractionsRequestTarget.
#[test]
fn parse_trims_and_checks_as_gjson_does() {
    let parsed = |body: &str| parse_target(body.as_bytes());
    assert_eq!(parsed("{"), Err("invalid JSON body"));
    assert_eq!(parsed(""), Err("invalid JSON body"));
    // Spaces alone name nothing, and a value that isn't a string is read
    // as gjson's String reads it.
    assert_eq!(
        parsed(r#"{"model":"  ","agent":" a "}"#).unwrap().agent,
        "a"
    );
    assert_eq!(parsed(r#"{"model":12}"#).unwrap().model, "12");
    assert_eq!(
        parsed(r#"{"model":null}"#),
        Err("request requires exactly one of model or agent")
    );
    assert_eq!(
        parsed(r#"["model"]"#),
        Err("request requires exactly one of model or agent")
    );
    // The first `model` counts, as gjson's does.
    assert_eq!(parsed(r#"{"model":"a","model":"b"}"#).unwrap().model, "a");
    for (stream, want) in [
        ("false", Ok(false)),
        ("true", Ok(true)),
        ("null", Err("stream must be a boolean")),
        ("1", Err("stream must be a boolean")),
        (r#""false""#, Err("stream must be a boolean")),
    ] {
        let body = format!(r#"{{"model":"m","stream":{stream}}}"#);
        assert_eq!(parsed(&body).map(|t| t.stream), want, "{stream}");
    }
}

// TestPrepareInteractionsExecutionTargetNormalizesModelResourceName
#[test]
fn prepare_makes_a_model_resource_name_bare() {
    let raw = r#"{"model":"models/gemini-3.5-flash","input":"hi"}"#;
    let target = parse_target(raw.as_bytes()).unwrap();
    let (model, body) = prepare_target(Bytes::from_static(raw.as_bytes()), &target);
    assert_eq!(model, MODEL);
    assert_eq!(&body[..], br#"{"model":"gemini-3.5-flash","input":"hi"}"#);
}

// TestPrepareInteractionsExecutionTargetPreservesBareModel
#[test]
fn prepare_keeps_a_bare_model() {
    let raw = r#"{"model":"gemini-3.5-flash","input":"hi"}"#;
    let target = parse_target(raw.as_bytes()).unwrap();
    let (model, body) = prepare_target(Bytes::from_static(raw.as_bytes()), &target);
    assert_eq!(model, MODEL);
    assert_eq!(&body[..], raw.as_bytes());
}

// Not upstream's: the edges of prepareInteractionsExecutionTarget and
// normalizeGeminiModelResourceName.
#[test]
fn prepare_edges() {
    let prepared = |raw: &'static str| {
        let target = parse_target(raw.as_bytes()).unwrap();
        let (model, body) = prepare_target(Bytes::from_static(raw.as_bytes()), &target);
        (model, String::from_utf8(body.to_vec()).unwrap())
    };
    // A bare `models/` is a name; a trimmed model goes on, the body as it
    // came.
    assert_eq!(
        prepared(r#"{"model":"models/"}"#),
        ("models/".into(), r#"{"model":"models/"}"#.into())
    );
    assert_eq!(
        prepared(r#"{"model":" m "}"#),
        ("m".into(), r#"{"model":" m "}"#.into())
    );
    assert_eq!(
        prepared(r#"{"model":" models/models/m "}"#),
        ("models/m".into(), r#"{"model":"models/m"}"#.into())
    );
    // An agent's body goes as it came.
    assert_eq!(
        prepared(r#"{"agent":" models/x "}"#),
        ("models/x".into(), r#"{"agent":" models/x "}"#.into())
    );
}

// TestInteractionsRejectsInvalidJSON, TestInteractionsRejectsMissingModelAndAgent,
// TestInteractionsRejectsBothModelAndAgent and TestInteractionsRejectsNonBooleanStream
#[tokio::test]
async fn rejects_bodies_it_cannot_take() {
    let (app, dispatcher) = app(vec![]);
    for (body, message) in [
        ("{", "invalid JSON body"),
        (
            r#"{"input":"hi"}"#,
            "request requires exactly one of model or agent",
        ),
        (
            r#"{"model":"gemini-3.5-flash","agent":"agents/test-agent","input":"hi"}"#,
            "request requires exactly one of model or agent",
        ),
        (
            r#"{"model":"gemini-3.5-flash","stream":"true","input":"hi"}"#,
            "stream must be a boolean",
        ),
    ] {
        let (status, headers, got) = send(&app, post(PATH, body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(got, rejected(message), "{body}");
        assert_eq!(content_type(&headers), JSON_UTF8);
    }
    assert!(dispatcher.calls().is_empty());
}

// Not upstream's: the route takes the key as the other routes do, and only
// POST.
#[tokio::test]
async fn needs_the_key_and_a_post() {
    let (app, _) = app(vec![]);
    let request = Request::builder()
        .method(Method::POST)
        .uri(PATH)
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let request = Request::builder()
        .method(Method::GET)
        .uri(PATH)
        .header("x-goog-api-key", "sk-test")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::NOT_FOUND, "404 page not found")
    );
}

// TestBuildInteractionsExecutionRequestUsesAgentAuthSelectionModel.
#[tokio::test]
async fn an_agent_is_forced_to_gemini_interactions() {
    let reply = r#"{"id":"interaction_1","object":"interaction","status":"completed"}"#;
    let (app, dispatcher) = app(vec![Outcome::reply(reply)]);
    let body = r#"{"agent":" agents/test-agent ","input":"hi"}"#;
    let (status, headers, got) = send(&app, post(PATH, body)).await;
    assert_eq!((status, got.as_str()), (StatusCode::OK, reply));
    assert_eq!(content_type(&headers), "application/json");

    let calls = dispatcher.calls();
    let [call] = calls.as_slice() else {
        panic!("calls = {calls:?}");
    };
    assert_eq!(call.method, "execute");
    assert_eq!(call.providers, ["gemini-interactions"]);
    assert_eq!(call.request.model, AGENT);
    assert_eq!(&call.request.payload[..], body.as_bytes());
    let options = &call.options;
    assert_eq!(options.source_format, Format::INTERACTIONS);
    assert_eq!(options.response_format, Format::INTERACTIONS);
    assert!(!options.stream);
    assert_eq!(
        options.metadata.forced_provider.as_deref(),
        Some("gemini-interactions")
    );
    assert_eq!(
        options.metadata.auth_selection_model.as_deref(),
        Some("gemini-2.5-flash")
    );
    assert_eq!(options.metadata.requested_model, AGENT);
    assert_eq!(options.metadata.request_path, PATH);
}

// Not upstream's: an agent named as an image-only model is turned away as
// upstream's validateImageOnlyModel does, before any call.
#[tokio::test]
async fn an_image_only_agent_is_turned_away() {
    let (app, dispatcher) = app(vec![]);
    let (status, _, body) = send(&app, post(PATH, r#"{"agent":"x/gpt-image-2"}"#)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body.contains("only supported on /v1/images/generations"),
        "{body}"
    );
    assert!(dispatcher.calls().is_empty());
}

// Not upstream's: a model is routed as on the other routes, with
// `gemini-interactions` first, and a resource name made bare.
#[tokio::test]
async fn a_model_is_routed_with_gemini_interactions_first() {
    let (app, dispatcher) = app(vec![Outcome::reply("{}"), Outcome::reply("{}")]);
    let body = r#"{"model":"models/gemini-3.5-flash","input":"hi"}"#;
    let (status, _, _) = send(&app, post(&format!("{PATH}?alt=json"), body)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, post(PATH, r#"{"model":" gemini-3.5-flash "}"#)).await;
    assert_eq!(status, StatusCode::OK);

    let calls = dispatcher.calls();
    let [resource, bare] = calls.as_slice() else {
        panic!("calls = {calls:?}");
    };
    assert_eq!(resource.providers, ["gemini-interactions", "gemini"]);
    assert_eq!(resource.request.model, MODEL);
    assert_eq!(
        &resource.request.payload[..],
        br#"{"model":"gemini-3.5-flash","input":"hi"}"#
    );
    assert_eq!(resource.options.metadata.requested_model, MODEL);
    assert_eq!(resource.options.metadata.forced_provider, None);
    assert_eq!(resource.options.metadata.auth_selection_model, None);
    assert_eq!(resource.options.source_format, Format::INTERACTIONS);
    assert_eq!(resource.options.alt, "json");
    assert_eq!(bare.request.model, MODEL);
    assert_eq!(
        &bare.request.payload[..],
        br#"{"model":" gemini-3.5-flash "}"#
    );

    let (status, _, body) = send(&app, post(PATH, r#"{"model":"nope"}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        r#"{"error":{"message":"unknown provider for model nope","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
    );
    assert_eq!(dispatcher.calls().len(), 2);
}

// Not upstream's: a non-streaming call's failure is an OpenAI error body,
// and a slow call is kept alive.
#[tokio::test(start_paused = true)]
async fn non_streaming_failures_and_keep_alives() {
    let config = ServerConfig {
        nonstream_keepalive: Some(Duration::from_secs(1)),
        ..ServerConfig::default()
    };
    let (app, _) = server(
        config,
        vec![
            Outcome::Fail(ExecError::upstream(429, "slow down")),
            Outcome::Slow(
                Duration::from_millis(1500),
                open_ferry_core::exec::Response {
                    payload: Bytes::from_static(b"{}"),
                    headers: HeaderMap::new(),
                },
            ),
        ],
    );
    let body = r#"{"model":"gemini-3.5-flash"}"#;
    let (status, headers, got) = send(&app, post(PATH, body)).await;
    assert_eq!(
        (status, got.as_str()),
        (StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN)
    );
    assert_eq!(content_type(&headers), "application/json");
    let (status, _, got) = send(&app, post(PATH, body)).await;
    assert_eq!((status, got.as_str()), (StatusCode::OK, "\n{}"));
}

// TestForwardInteractionsStreamWrapsBareJSONAsSSEData
#[tokio::test]
async fn forward_wraps_bare_json_as_sse_data() {
    assert_eq!(
        written(&[r#"{"type":"interaction.completed"}"#], None).await,
        "data: {\"type\":\"interaction.completed\"}\n\n"
    );
}

// Not upstream's: chunks already framed as SSE go on as they are, ended by
// a blank line, and an error partway ends the stream as `event: error`.
#[tokio::test]
async fn forward_frames_chunks_and_errors() {
    assert_eq!(
        written(
            &[
                "event: interaction.start\ndata: {\"n\":1}\n\n",
                "data: {\"n\":2}",
                " \n data: {\"n\":3}\n",
                "",
            ],
            None
        )
        .await,
        "event: interaction.start\ndata: {\"n\":1}\n\n\
         data: {\"n\":2}\n\n \n data: {\"n\":3}\n\n\n"
    );
    assert_eq!(
        written(&["{}"], Some(ErrorMessage::new(500, "boom"))).await,
        format!("data: {{}}\n\nevent: error\ndata: {BOOM}\n\n")
    );
    let unavailable = r#"{"error":{"message":"Service Unavailable","type":"server_error","code":"internal_server_error"}}"#;
    assert_eq!(
        written(&[], Some(ErrorMessage::new(503, ""))).await,
        format!("event: error\ndata: {unavailable}\n\n")
    );
    let mut out = BytesMut::new();
    InteractionsWriter.write_keep_alive(&mut out);
    assert_eq!(&out[..], b": keep-alive\n\n");
}

// Not upstream's: a stream through the route, with its headers, a failure
// partway, a failure before the first payload, and one that ends empty.
#[tokio::test]
async fn streams_through_the_route() {
    let mut upstream = HeaderMap::new();
    upstream.insert("x-up", HeaderValue::from_static("1"));
    let (app, dispatcher) = server(
        ServerConfig {
            passthrough_headers: true,
            ..ServerConfig::default()
        },
        vec![
            Outcome::Stream(
                upstream,
                vec![chunk(r#"{"n":1}"#), Err(ExecError::upstream(500, "boom"))],
            ),
            Outcome::Fail(ExecError::upstream(429, "slow down")),
            Outcome::chunks(&[]),
        ],
    );
    let body = r#"{"agent":"agents/test-agent","stream":true}"#;

    let (status, headers, got) = send(&app, post(PATH, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        got,
        format!("data: {{\"n\":1}}\n\nevent: error\ndata: {BOOM}\n\n")
    );
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(headers[header::CONNECTION], "keep-alive");
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert_eq!(headers["x-up"], "1");

    let (status, headers, got) = send(&app, post(PATH, body)).await;
    assert_eq!(
        (status, got.as_str()),
        (StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN)
    );
    assert_eq!(content_type(&headers), "application/json");

    let (status, headers, got) = send(&app, post(PATH, body)).await;
    assert_eq!((status, got.as_str()), (StatusCode::OK, ""));
    assert_eq!(content_type(&headers), "text/event-stream");

    let call = &dispatcher.calls()[0];
    assert_eq!(call.method, "execute_stream");
    assert!(call.options.stream);
    assert_eq!(call.providers, ["gemini-interactions"]);
    assert_eq!(
        call.options.metadata.forced_provider.as_deref(),
        Some("gemini-interactions")
    );
}

// Not upstream's: a stream writes keep-alives at the configured interval.
#[tokio::test(start_paused = true)]
async fn streams_keep_alive() {
    let config = ServerConfig {
        streaming: StreamingConfig {
            keepalive: Some(Duration::from_secs(1)),
            ..StreamingConfig::default()
        },
        ..ServerConfig::default()
    };
    let (app, _) = server(
        config,
        vec![Outcome::Hang(HeaderMap::new(), vec![chunk(r#"{"n":1}"#)])],
    );
    let body = r#"{"model":"gemini-3.5-flash","stream":true}"#;
    let response = app.oneshot(post(PATH, body)).await.unwrap();
    let mut body = response.into_body();
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&frame[..], b"data: {\"n\":1}\n\n");
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&frame[..], b": keep-alive\n\n");
}

/// `fields` in a JSON object, with one more member nested so the object is
/// `depth` levels deep.
fn nested(fields: &str, depth: usize) -> String {
    format!(
        r#"{{{fields},"deep":{}0{}}}"#,
        "[".repeat(depth - 1),
        "]".repeat(depth - 1)
    )
}

// Not upstream's: a body with 128 or more arrays and objects inside one
// another gets a 400 before the call is made, for a model, for an agent, and
// for a stream, which is told by the body; one of 127 goes through. Upstream
// forwards a body of any depth.
#[tokio::test]
async fn bodies_nested_too_deeply_are_refused() {
    let (app, dispatcher) = app(vec![
        Outcome::reply(r#"{"id":"i1"}"#),
        Outcome::reply(r#"{"id":"i2"}"#),
        Outcome::chunks(&[r#"{"n":1}"#]),
    ]);
    let model = format!(r#""model":"{MODEL}","input":"hi""#);
    let agent = format!(r#""agent":"{AGENT}","input":"hi""#);
    let stream = format!(r#""model":"{MODEL}","input":"hi","stream":true"#);
    for (fields, providers) in [
        (&model, vec!["gemini-interactions", "gemini"]),
        (&agent, vec!["gemini-interactions"]),
        (&stream, vec!["gemini-interactions", "gemini"]),
    ] {
        let calls = dispatcher.calls().len();
        let (status, _, body) = send(&app, post(PATH, &nested(fields, 127))).await;
        assert_eq!(status, StatusCode::OK, "{fields}: {body}");
        let calls_after = dispatcher.calls();
        assert_eq!(calls_after.len(), calls + 1, "{fields}");
        assert_eq!(calls_after[calls].providers, providers, "{fields}");

        for depth in [128, 129, 100_000] {
            let (status, headers, body) = send(&app, post(PATH, &nested(fields, depth))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{fields}: {depth}");
            assert_eq!(content_type(&headers), "application/json");
            let error: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(error["error"]["type"], "invalid_request_error", "{body}");
            let message = error["error"]["message"].as_str().unwrap();
            assert!(message.contains("nested more than 127"), "{body}");
        }
        assert_eq!(dispatcher.calls().len(), calls + 1, "{fields}");
    }
}
