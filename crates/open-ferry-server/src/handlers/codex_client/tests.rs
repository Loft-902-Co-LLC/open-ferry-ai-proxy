// Ported from CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_multi_agent_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses boundary's preparation, alone and through the routes.
//!
//! Changed from upstream:
//! - The checks of the prepared marker
//!   (`CodexMultiAgentV2ToolsPreparedContextKey`) are left out, as there is
//!   no marker.
//! - The routes are checked against a scripted dispatcher, where upstream
//!   registers a capturing executor with an auth manager and the model
//!   registry.
//!
//! Added: compact rewrites orphan delegation but not the tools, and nothing
//! is touched while the settings are off.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use open_ferry_core::models::ModelInfo;
use open_ferry_translate::codex_client::multi_agent_v2::SPAWN_AGENT_MODELS_HEADING;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

use super::prepare;
use crate::config::ServerConfig;
use crate::router;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

const CODEX_CLI: &str = "codex_cli_rs/0.144.1";
const MODEL: &str = "responses-multi-agent-test-model";
const COMPLETED: &str =
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"output\":[]}}\n\n";

/// A config with the multi-agent v2 setting and orphan delegation as given.
fn config(optimize: bool, orphans: bool) -> ServerConfig {
    let mut config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        codex_orphan_delegation: orphans,
        ..ServerConfig::default()
    };
    config.codex_client.optimize_multi_agent_v2 = optimize;
    config
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for &(name, value) in pairs {
        headers.insert(name, HeaderValue::from_str(value).unwrap());
    }
    headers
}

fn parse(raw: &[u8]) -> Value {
    serde_json::from_slice(raw).unwrap()
}

/// A Codex sub-agent's input: a delegation output without its call, then a
/// user message.
fn orphan_input() -> Value {
    json!([
        {
            "type": "function_call_output",
            "name": "create_thread",
            "namespace": "codex_app",
            "output": "<codex_delegation>msg</codex_delegation>"
        },
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "continue"}]
        }
    ])
}

/// The collaboration namespace with `spawn_agent`'s `message` encrypted.
fn collaboration_tools() -> Value {
    json!([{"type":"namespace","name":"collaboration","tools":[
        {"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}
    ]}])
}

// TestPrepareCodexMultiAgentV2ToolsAtResponsesBoundary
#[test]
fn readies_collaboration_tools_at_the_boundary() {
    let payload = br#"{
        "tools":[{"type":"namespace","name":"collaboration","tools":[
            {"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}},
            {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}
        ]}]
    }"#;
    let got = prepare(
        &config(true, false),
        &FakeCatalog::new(),
        &headers(&[("user-agent", CODEX_CLI)]),
        payload,
        true,
    )
    .unwrap();
    let got = parse(&got);
    assert_eq!(got["tools"][0]["name"], "collaboration");
    for tool in 0..2 {
        let message = &got["tools"][0]["tools"][tool]["parameters"]["properties"]["message"];
        assert_eq!(message, &json!({}), "tool {tool}");
    }
}

/// Not upstream's: sjson edits the body in place, so a rewritten body keeps
/// each of the client's numbers as written.
#[test]
fn keeps_the_clients_numbers() {
    let payload = br#"{"temperature":-0,"max":1E20,"tools":[{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true},"n":{"default":1e5}}}}]}"#;
    let got = prepare(
        &config(true, false),
        &FakeCatalog::new(),
        &headers(&[("user-agent", CODEX_CLI)]),
        payload,
        true,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(got).unwrap(),
        r#"{"temperature":-0,"max":1E20,"tools":[{"type":"function","name":"send_message","parameters":{"properties":{"message":{},"n":{"default":1e5}}}}]}"#
    );
}

#[test]
fn lists_the_models_for_spawn_agent() {
    let catalog = FakeCatalog::new().models(vec![ModelInfo {
        id: "boundary-model".into(),
        description: "Boundary model.".into(),
        ..ModelInfo::default()
    }]);
    let payload = json!({"tools": collaboration_tools()}).to_string();
    let codex = headers(&[("user-agent", CODEX_CLI)]);
    let got = prepare(
        &config(true, false),
        &catalog,
        &codex,
        payload.as_bytes(),
        true,
    )
    .unwrap();
    let description = parse(&got)["tools"][0]["tools"][0]["description"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        description.starts_with(&format!(
            "{SPAWN_AGENT_MODELS_HEADING}\n- `boundary-model`: Boundary model."
        )),
        "{description}"
    );
    assert!(description.ends_with("\nSpawns an agent."), "{description}");

    // Not for `responses/compact`.
    assert_eq!(
        prepare(
            &config(true, false),
            &catalog,
            &codex,
            payload.as_bytes(),
            false
        ),
        None
    );
}

// TestPrepareCodexMultiAgentV2ToolsAtResponsesBoundarySkipsOtherClients
#[test]
fn leaves_other_clients_alone() {
    let payload = br#"{"tools":[{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}]}"#;
    let got = prepare(
        &config(true, false),
        &FakeCatalog::new(),
        &headers(&[("user-agent", "curl/8.7.1")]),
        payload,
        true,
    );
    assert_eq!(got, None);
}

// TestClientMultiAgentPreparationDoesNotWaitForOAuthCredential.
#[test]
fn readies_tools_and_orphans_together() {
    let payload = br#"{"input":[{"type":"function_call_output","name":"create_thread","namespace":"codex_app","output":"<codex_delegation>task</codex_delegation>"}],"tools":[{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}]}"#;
    let got = prepare(
        &config(true, true),
        &FakeCatalog::new(),
        &headers(&[
            ("user-agent", CODEX_CLI),
            ("x-openai-subagent", "collab_spawn"),
        ]),
        payload,
        true,
    )
    .unwrap();
    let got = parse(&got);
    assert_eq!(
        got["tools"][0]["parameters"]["properties"]["message"],
        json!({})
    );
    assert_eq!(got["input"][0]["type"], "message");
}

#[test]
fn leaves_the_body_alone_while_the_settings_are_off() {
    let payload = json!({"input": orphan_input(), "tools": collaboration_tools()}).to_string();
    let codex = headers(&[
        ("user-agent", CODEX_CLI),
        ("x-openai-subagent", "collab_spawn"),
    ]);
    assert_eq!(
        prepare(
            &config(false, false),
            &FakeCatalog::new(),
            &codex,
            payload.as_bytes(),
            true
        ),
        None
    );
    // Neither applies to a body that isn't an object.
    assert_eq!(
        prepare(
            &config(true, true),
            &FakeCatalog::new(),
            &codex,
            b"[]",
            true
        ),
        None
    );
    assert_eq!(
        prepare(&config(true, true), &FakeCatalog::new(), &codex, b"{", true),
        None
    );
    // Nor to one with nothing to change.
    assert_eq!(
        prepare(
            &config(true, true),
            &FakeCatalog::new(),
            &codex,
            br#"{"input":"hi"}"#,
            true
        ),
        None
    );
}

/// A server that serves [`MODEL`] through `codex` with `config`, and lists
/// it with the description "Test model.".
fn app(config: ServerConfig, outcomes: Vec<Outcome>) -> (Router, Arc<FakeDispatcher>) {
    let catalog = FakeCatalog::new()
        .serve(MODEL, &["codex"])
        .models(vec![ModelInfo {
            id: MODEL.into(),
            description: "Test model.".into(),
            ..ModelInfo::default()
        }]);
    let dispatcher = FakeDispatcher::new(outcomes);
    (router(state(config, catalog, &dispatcher)), dispatcher)
}

/// POSTs `body` to `uri` with the test key and `extra` headers, and gives
/// the response's status.
async fn post(app: &Router, uri: &str, body: &Value, extra: &[(&str, &str)]) -> StatusCode {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer sk-test")
        .header(header::CONTENT_TYPE, "application/json");
    for &(name, value) in extra {
        request = request.header(name, value);
    }
    let request = request.body(Body::from(body.to_string())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    status
}

// TestResponsesPreparesCodexMultiAgentV2ToolsForHTTPAndSSE, which also
// checks the model list here.
#[tokio::test]
async fn responses_ready_tools_for_http_and_sse() {
    for stream in [false, true] {
        let outcome = if stream {
            Outcome::chunks(&[COMPLETED])
        } else {
            Outcome::reply(r#"{"id":"resp-1","output":[]}"#)
        };
        let (app, dispatcher) = app(config(true, false), vec![outcome]);
        let body = json!({"model": MODEL, "stream": stream, "tools": collaboration_tools()});
        post(&app, "/v1/responses", &body, &[("user-agent", CODEX_CLI)]).await;

        let calls = dispatcher.calls();
        assert_eq!(calls.len(), 1, "stream={stream}");
        let captured = parse(&calls[0].request.payload);
        let tool = &captured["tools"][0];
        assert_eq!(tool["name"], "collaboration", "stream={stream}");
        assert_eq!(
            tool["tools"][0]["parameters"]["properties"]["message"],
            json!({}),
            "stream={stream}"
        );
        let description = tool["tools"][0]["description"].as_str().unwrap();
        let want = format!("- `{MODEL}`: Test model.");
        assert!(
            description.contains(&want),
            "stream={stream}: {description}"
        );
    }
}

// TestResponsesOrphanCodexDelegationCompatibility
#[tokio::test]
async fn responses_rewrite_orphan_delegation_for_sub_agents() {
    let (app, dispatcher) = app(
        config(false, true),
        vec![Outcome::reply("{}"), Outcome::reply("{}")],
    );
    let body = json!({"model": MODEL, "stream": false, "input": orphan_input()});
    post(
        &app,
        "/v1/responses",
        &body,
        &[("x-openai-subagent", "collab_spawn")],
    )
    .await;
    let captured = parse(&dispatcher.calls()[0].request.payload);
    assert_eq!(captured["input"][0]["type"], "message");
    assert_eq!(captured["input"][0]["role"], "user");
    assert_eq!(
        captured["input"][0]["content"][0]["text"],
        "Tool output from codex_app__create_thread:\n<codex_delegation>msg</codex_delegation>"
    );

    // Without the header, the output stays as it was.
    post(&app, "/v1/responses", &body, &[]).await;
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(&calls[1].request.payload[..], body.to_string().as_bytes());
}

#[tokio::test]
async fn compact_rewrites_orphan_delegation_but_not_tools() {
    let (app, dispatcher) = app(config(true, true), vec![Outcome::reply("{}")]);
    let body = json!({"model": MODEL, "input": orphan_input(), "tools": collaboration_tools()});
    post(
        &app,
        "/v1/responses/compact",
        &body,
        &[
            ("user-agent", CODEX_CLI),
            ("x-openai-subagent", "collab_spawn"),
        ],
    )
    .await;
    let call = &dispatcher.calls()[0];
    assert_eq!(call.options.alt, "responses/compact");
    let captured = parse(&call.request.payload);
    assert_eq!(captured["input"][0]["type"], "message");
    assert_eq!(captured["tools"], collaboration_tools());
}

// TestResponsesWebsocketPreparesCodexMultiAgentV2Tools
#[tokio::test]
async fn websocket_readies_tools() {
    let catalog = FakeCatalog::new().serve(MODEL, &["codex"]);
    let dispatcher = FakeDispatcher::new([Outcome::chunks(&[COMPLETED])]);
    let app = router(state(config(true, false), catalog, &dispatcher));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut request = format!("ws://{addr}/v1/responses")
        .into_client_request()
        .unwrap();
    let request_headers = request.headers_mut();
    request_headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    request_headers.insert("user-agent", HeaderValue::from_static(CODEX_CLI));
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();

    let create = json!({
        "type": "response.create",
        "model": MODEL,
        "input": [],
        "tools": collaboration_tools()
    });
    ws.send(tungstenite::Message::Text(create.to_string().into()))
        .await
        .unwrap();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .expect("socket ended")
            .expect("socket failed");
        if matches!(message, tungstenite::Message::Text(_)) {
            break;
        }
    }

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    let captured = parse(&calls[0].request.payload);
    let tool = &captured["tools"][0];
    assert_eq!(tool["name"], "collaboration");
    assert_eq!(
        tool["tools"][0]["parameters"]["properties"]["message"],
        json!({})
    );
}
