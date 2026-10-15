// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_auth_test.go
// (TestCodexWebsocketsExecutorUseSIWCExecuteAndStream) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::codex::request::{Context, Kind};
use crate::secret_echo::Logs;
use open_ferry_core::config::{AnyValue, PayloadModelRule, PayloadRule};
use open_ferry_core::exec::{InputFrame, WebsocketInput};

mod live;
mod live_messages;

const TOKEN: &str = "siwc-websocket-test-token";
const DEVICE: &str = "siwc-device-never-transmitted-61a0e56d-c76b-469d-88ca-c2e2cdaf5801";
const API_KEY: &str = "siwc-unused-api-key-never-transmitted";
const BODY: &str = r#"{"input":"hello","instructions":"  ","stream":true,"background":true,"store":true,"temperature":0.7,"max_output_tokens":1024,"max_completion_tokens":1024,"prompt_cache_retention":"24h","prompt_cache_options":{"ttl":"30m"},"prompt_cache_key":"body-only-cache-key"}"#;
const FORBIDDEN: &[&str] = &[
    "originator",
    "chatgpt-account-id",
    "x-codex-routing-hint",
    "conversation_id",
    "conversation-id",
    "x-codex-window-id",
    "x-codex-turn-state",
    "x-codex-turn-metadata",
    "x-codex-beta-features",
    "version",
    "thread-id",
    "thread_id",
    "x-openai-internal-codex-responses-lite",
    "x-openai-client-request-time",
];

fn credential(base: &str) -> Auth {
    let mut auth = auth(base);
    auth.attributes.insert("api_key".into(), API_KEY.into());
    auth.metadata.insert("use_siwc".into(), json!(true));
    auth.metadata
        .insert("access_token".into(), json!("unused-outer-token"));
    auth.metadata
        .insert("account_id".into(), json!("unused-account"));
    auth.metadata.insert(
        "siwc".into(),
        json!({"access_token":TOKEN,"device_id":DEVICE}),
    );
    for name in FORBIDDEN.iter().copied().chain([
        "user-agent",
        "authorization",
        "session_id",
        "x-client-request-id",
    ]) {
        auth.attributes
            .insert(format!("header:{name}"), "credential-attempt".into());
    }
    auth.attributes
        .insert("header:X-Team".into(), "$X-Tenant".into());
    auth
}

fn attempted_options(session: &str, ids: bool) -> Options {
    let mut opts = ws_options(session);
    opts.source_format = Format::OPENAI_RESPONSE;
    for name in FORBIDDEN {
        opts.headers.insert(
            name.parse::<http::HeaderName>().unwrap(),
            HeaderValue::from_static("caller-attempt"),
        );
    }
    opts.headers.insert(
        "user-agent",
        HeaderValue::from_static("codex_cli_rs/1 (Mac OS)"),
    );
    opts.headers.insert(
        "x-responsesapi-include-timing-metrics",
        HeaderValue::from_static("caller-metrics-attempt"),
    );
    opts.headers
        .insert("x-tenant", HeaderValue::from_static("team-blue"));
    if ids {
        opts.headers
            .insert("session_id", HeaderValue::from_static("caller-session"));
        opts.headers.insert(
            "x-client-request-id",
            HeaderValue::from_static("caller-request"),
        );
    }
    opts
}

fn assert_headers(head: &super::super::mock::Handshake, token: &str, ids: bool) {
    assert_eq!(head.path, "/v1/responses");
    assert_eq!(
        head.header("user-agent"),
        Some(crate::codex::siwc::user_agent().as_str())
    );
    assert!(
        head.header("authorization") == Some(format!("Bearer {token}").as_str()),
        "SIWC bearer was not selected"
    );
    assert_eq!(head.header("x-team"), Some("team-blue"));
    for name in FORBIDDEN {
        assert!(head.header(name).is_none(), "forbidden SIWC header {name}");
    }
    assert_eq!(head.header("session_id"), ids.then_some("caller-session"));
    assert_eq!(
        head.header("x-client-request-id"),
        ids.then_some("caller-request")
    );
    let headers = format!("{:?}", head.headers);
    assert!(!headers.contains(DEVICE));
    assert!(!headers.contains(API_KEY));
}

fn assert_body(text: &str) {
    let body: Value = serde_json::from_str(text).unwrap();
    assert_eq!(body["type"], "response.create");
    assert_eq!(body["store"], false);
    assert_eq!(body["prompt_cache_key"], "body-only-cache-key");
    for field in [
        "stream",
        "background",
        "prompt_cache_retention",
        "prompt_cache_options",
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "instructions",
        "tools",
    ] {
        assert!(!exists(&body, field), "SIWC body retained {field}");
    }
    assert!(!text.contains(DEVICE));
}

// Upstream's execute/stream test, with independent caller IDs and final identity enforcement.
#[tokio::test]
async fn siwc_execute_and_stream_use_fixed_identity_and_websocket_body() {
    for streaming in [false, true] {
        for ids in [false, true] {
            let server = Server::once(&[COMPLETED]).await;
            let credential = credential(&format!("{}/v1", server.url));
            let mut config = Config::default();
            config.codex.signin_with_chatgpt_identifier = "config-attempt".into();
            // Codex header defaults expose no user-agent override in this port.
            config.codex_header_defaults.beta_features = "config-beta-attempt".into();
            let executor = executor_with(config);
            let mut opts = attempted_options("", ids);
            if ids {
                opts.headers.insert(
                    "openai-beta",
                    HeaderValue::from_static("caller=1; responses_websockets=caller"),
                );
            } else {
                opts.headers
                    .insert("openai-beta", HeaderValue::from_static("unrelated-beta"));
            }
            let req = request("gpt-5-codex", BODY);
            if streaming {
                let (_, error) = collect(
                    executor
                        .execute_stream(Arc::new(credential), req, opts)
                        .await
                        .unwrap(),
                )
                .await;
                assert!(error.is_none());
            } else {
                executor
                    .execute(Arc::new(credential), req, opts)
                    .await
                    .unwrap();
            }
            let record = server.record();
            assert_eq!(record.handshakes.len(), 1);
            assert!(record.bodies.is_empty(), "no HTTP request");
            let head = &record.handshakes[0];
            assert_headers(head, TOKEN, ids);
            // Upstream omits caller metrics, but permits credential overrides.
            assert!(
                head.header("x-responsesapi-include-timing-metrics")
                    .is_none()
            );
            assert_eq!(
                head.header("openai-beta"),
                Some(if ids {
                    "caller=1; responses_websockets=caller"
                } else {
                    BETA_HEADER_VALUE
                })
            );
            assert_eq!(record.messages.len(), 1);
            assert_body(&record.messages[0]);
        }
    }
}

#[tokio::test]
async fn siwc_custom_protocol_headers_and_hyphenated_session_precedence() {
    for streaming in [false, true] {
        for both in [false, true] {
            let server = Server::once(&[COMPLETED]).await;
            let mut auth = credential(&format!("{}/v1", server.url));
            auth.attributes
                .insert("header:OpenAI-Beta".into(), "credential-beta".into());
            auth.attributes.insert(
                "header:x-responsesapi-include-timing-metrics".into(),
                "credential-metrics".into(),
            );
            let mut opts = attempted_options("", true);
            opts.headers
                .insert("session-id", HeaderValue::from_static("caller-session"));
            if both {
                opts.headers
                    .insert("session_id", HeaderValue::from_static("underscore-loses"));
            } else {
                opts.headers.remove("session_id");
            }
            let executor = executor();
            let req = request("gpt-5-codex", BODY);
            if streaming {
                assert!(
                    collect(
                        executor
                            .execute_stream(Arc::new(auth), req, opts)
                            .await
                            .unwrap()
                    )
                    .await
                    .1
                    .is_none()
                );
            } else {
                executor.execute(Arc::new(auth), req, opts).await.unwrap();
            }
            let record = server.record();
            let head = &record.handshakes[0];
            assert_headers(head, TOKEN, true);
            assert_eq!(head.header("openai-beta"), Some("credential-beta"));
            assert_eq!(
                head.header("x-responsesapi-include-timing-metrics"),
                Some("credential-metrics")
            );
            assert!(head.header("session-id").is_none());
        }
    }
}

#[tokio::test]
async fn siwc_payload_override_lands_last_on_execute_and_stream() {
    for streaming in [false, true] {
        let server = Server::once(&[COMPLETED]).await;
        let auth = credential(&format!("{}/v1", server.url));
        let mut config = Config::default();
        config.payload.r#override.push(PayloadRule {
            models: vec![PayloadModelRule {
                name: "gpt-5-codex".into(),
                protocol: "codex".into(),
                ..Default::default()
            }],
            params: vec![("temperature".into(), AnyValue::Float(0.25))],
        });
        let executor = executor_with(config);
        let req = request("gpt-5-codex", BODY);
        let opts = attempted_options("", false);
        if streaming {
            assert!(
                collect(
                    executor
                        .execute_stream(Arc::new(auth), req, opts)
                        .await
                        .unwrap()
                )
                .await
                .1
                .is_none()
            );
        } else {
            executor.execute(Arc::new(auth), req, opts).await.unwrap();
        }
        let record = server.record();
        assert_headers(&record.handshakes[0], TOKEN, false);
        let body: Value = serde_json::from_str(&record.messages[0]).unwrap();
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["store"], false);
        assert!(!exists(&body, "stream"));
        assert!(!record.messages[0].contains(DEVICE));
    }
}

#[test]
fn siwc_prepare_preserves_instructions_and_payload_overrides_and_default_url() {
    let auth = credential("");
    let mut config = Config::default();
    config.payload.r#override.push(PayloadRule {
        models: vec![PayloadModelRule {
            name: "gpt-5-codex".into(),
            protocol: "codex".into(),
            ..Default::default()
        }],
        params: vec![("temperature".into(), AnyValue::Float(0.25))],
    });
    for kind in [Kind::Execute, Kind::Stream] {
        let prepared = request::prepare(
            kind,
            Context {
                auth: Some(&auth),
                config: Some(&config),
                models: None,
            },
            &auth,
            "https://chatgpt.com/backend-api/codex",
            &request(
                "gpt-5-codex",
                r#"{"input":"hello","instructions":" caller instructions ","temperature":1}"#,
            ),
            &attempted_options("", false),
        )
        .unwrap();
        assert_eq!(prepared.url, "wss://api.openai.com/v1/responses");
        assert_eq!(prepared.body["temperature"], 0.25);
        assert_eq!(prepared.body["instructions"], " caller instructions ");
        assert!(!exists(&prepared.body, "tools"));
        assert!(!exists(&prepared.body, "stream"));
        for name in ["session_id", "x-client-request-id"] {
            let mut opts = attempted_options("", false);
            opts.headers
                .insert(name, HeaderValue::from_static("only-caller-id"));
            let headers = request::build_headers(&auth, &opts.headers, false).unwrap();
            assert_eq!(headers[name], "only-caller-id");
            let other = if name == "session_id" {
                "x-client-request-id"
            } else {
                "session_id"
            };
            assert!(headers.get(other).is_none());
        }
    }
}

#[tokio::test]
async fn siwc_missing_token_fails_before_preparation_or_any_connection() {
    let server = Server::once(&[COMPLETED]).await;
    let executor = executor();
    for missing_partition in [false, true] {
        let mut auth = credential(&format!("{}/v1", server.url));
        if missing_partition {
            auth.metadata.remove("siwc");
        } else {
            auth.metadata["siwc"]["access_token"] = json!("");
        }
        for streaming in [false, true] {
            let req = request("gpt-5-codex", BODY);
            let opts = attempted_options("missing", false);
            let error = if streaming {
                executor
                    .execute_stream(Arc::new(auth.clone()), req, opts)
                    .await
                    .err()
                    .unwrap()
            } else {
                executor
                    .execute(Arc::new(auth.clone()), req, opts)
                    .await
                    .err()
                    .unwrap()
            };
            assert_eq!(error.status, 401);
            assert!(error.message.contains("siwc access_token is missing"));
        }
        let error = request::prepare(
            Kind::Stream,
            executor.context(&auth),
            &auth,
            executor.base_url(),
            &request("gpt-5-codex", BODY),
            &attempted_options("", false),
        )
        .err()
        .unwrap();
        assert_eq!(error.status, 401);
    }
    assert_eq!(executor.websockets().len(), 0);
    assert!(server.record().handshakes.is_empty());
    assert!(server.record().bodies.is_empty());
}

#[tokio::test]
async fn siwc_rotation_and_mode_switch_reconnect_even_with_equal_token_text() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = executor();
    let mut auth = credential(&format!("{}/v1", server.url));
    for (index, token) in [TOKEN, "rotated-siwc-test-token", "rotated-siwc-test-token"]
        .into_iter()
        .enumerate()
    {
        auth.metadata["siwc"]["access_token"] = json!(token);
        if index == 2 {
            auth.metadata.insert("use_siwc".into(), json!(false));
            auth.attributes.insert("api_key".into(), token.into());
            auth.attributes.remove("header:authorization");
        }
        executor
            .execute(
                Arc::new(auth.clone()),
                request("gpt-5-codex", BODY),
                attempted_options("rotation", false),
            )
            .await
            .unwrap();
        assert_eq!(server.record().handshakes.len(), index + 1);
        assert!(
            server.record().handshakes[index].header("authorization")
                == Some(format!("Bearer {token}").as_str()),
            "current token was not sent"
        );
    }
    executor.close_execution_session("rotation");
}

#[tokio::test]
async fn siwc_duplex_second_create_applies_body_rules_on_kept_connection() {
    let server = Server::turns(&[
        r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#,
        COMPLETED,
    ])
    .await;
    let mut config = Config::default();
    config.codex.response_steering = true;
    let executor = executor_with(config);
    let (tx, rx) = mpsc::channel(4);
    let mut opts = attempted_options("siwc-duplex", false);
    opts.websocket_input = Some(WebsocketInput::new(rx));
    let response = executor
        .execute_stream(
            Arc::new(credential(&format!("{}/v1", server.url))),
            request("gpt-5-codex", BODY),
            opts,
        )
        .await
        .unwrap();
    let mut chunks = response.chunks;
    for expected in ["response.created", "response.completed"] {
        let chunk = within("first duplex events", chunks.next())
            .await
            .unwrap()
            .unwrap();
        let event: Value = serde_json::from_slice(&chunk).unwrap();
        assert_eq!(event["type"], expected);
    }
    let mut next: Value = serde_json::from_str(BODY).unwrap();
    next["type"] = json!("response.create");
    tx.send(InputFrame::Payload(Bytes::from(next.to_string())))
        .await
        .unwrap();
    for expected in ["response.created", "response.completed"] {
        let chunk = within("second duplex events", chunks.next())
            .await
            .unwrap()
            .unwrap();
        let event: Value = serde_json::from_slice(&chunk).unwrap();
        assert_eq!(event["type"], expected);
    }
    let record = server.record();
    assert_eq!(record.handshakes.len(), 1);
    assert_headers(&record.handshakes[0], TOKEN, false);
    assert_eq!(record.messages.len(), 2);
    for message in &record.messages {
        assert_body(message);
    }
    drop(tx);
    drop(chunks);
    executor.close_execution_session("siwc-duplex");
}

#[tokio::test]
async fn siwc_426_fallback_uses_the_siwc_http_route() {
    for streaming in [false, true] {
        let server = Server::start(|index| {
            if index == 0 {
                Answer::refuse(426, "upgrade required")
            } else {
                Answer::Refuse {
                    status: 200,
                    headers: vec![("Content-Type", "text/event-stream".into())],
                    body: format!("data: {COMPLETED}\n\n"),
                }
            }
        })
        .await;
        let executor = executor();
        let auth = credential(&format!("{}/v1", server.url));
        let mut opts = attempted_options("", true);
        opts.downstream_websocket = false;
        let req = request("gpt-5-codex", BODY);
        if streaming {
            let response = super::super::stream::execute_stream(&executor, &auth, req, opts)
                .await
                .unwrap();
            assert!(collect(response).await.1.is_none());
        } else {
            super::super::execute::execute(&executor, &auth, &req, &opts)
                .await
                .unwrap();
        }
        let record = server.record();
        assert_eq!(record.handshakes.len(), 2);
        let http = &record.handshakes[1];
        assert_eq!(http.method, "POST");
        assert_headers(http, TOKEN, true);
        let body: Value = serde_json::from_str(&record.bodies[0]).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(!exists(&body, "temperature"));
        assert!(!exists(&body, "tools"));
        assert!(!record.bodies[0].contains(DEVICE));
    }
}

#[tokio::test]
async fn siwc_secrets_are_redacted_from_messages_errors_and_connection_logs() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.send(&format!(
                    r#"{{"type":"response.output_text.delta","delta":"{TOKEN}"}}"#
                ))
                .await;
                peer.close(1008, &format!("invalid token {TOKEN}")).await;
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = executor();
    let response = executor
        .execute_stream(
            Arc::new(credential(&format!("{}/v1", server.url))),
            request("gpt-5-codex", BODY),
            attempted_options("siwc-secrets", false),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    let error = error.unwrap();
    assert!(!error.message.contains(TOKEN));
    assert!(error.message.contains(crate::redact::REDACTED));
    assert!(chunks.iter().all(|text| !text.contains(TOKEN)));
    assert!(
        chunks
            .iter()
            .any(|text| text.contains(crate::redact::REDACTED))
    );
    server.wait_closed(1).await;
    let text = logs.text();
    assert!(text.contains("codex websockets: upstream disconnected"));
    assert!(text.contains(crate::redact::REDACTED));
    assert!(!text.contains(TOKEN));
    assert!(!text.contains(DEVICE));
    executor.close_execution_session("siwc-secrets");
}
