// Ported from CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_steering_test.go,
// sdk/api/handlers/openai/openai_responses_steering_auth_test.go,
// sdk/api/handlers/openai/openai_responses_steering_error_test.go,
// sdk/api/handlers/openai/openai_responses_steering_integration_test.go and
// sdk/api/handlers/openai/openai_responses_steering_validation_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Response steering through the whole router: a client's Responses
//! WebSocket, the auth manager and the Codex executor, against a mock Codex
//! on 127.0.0.1 that answers with the events of upstream's cases.
//!
//! Deviations from upstream:
//! - `TestResponsesWebsocketClosesOnIdleCodexDisconnect`'s case with
//!   steering off isn't ported: that socket closes through a disconnect
//!   subscription, which isn't ported.
//! - Models are registered with a registry of the test's own and routed by
//!   a fake catalog, where upstream's tests use the global registry.
//! - A socket the proxy closes without a close frame reads as ended, where
//!   gorilla's client reads a 1006 close error.
//! - The error recovery case with a stale OAuth scope sets the setting
//!   under `oauth.providers` instead, as the list of OAuth-only settings
//!   belongs to the config module; that `for_api_key` keeps the setting
//!   with such an entry is checked there
//!   (`client_codex_optimize_multi_agent_v2_historical_paths`), and the
//!   Codex executor reads its own config rather than `for_api_key`'s.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use open_ferry_core::auth::{Auth, Status};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{WebsocketAuth, WebsocketSupport};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_providers::codex::CodexExecutor;
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::{
    Client, completes, connect, eventually, normalize_default, parse, recv, rest, send, sse,
    test_catalog,
};
use crate::config::ServerConfig;
use crate::state::AppState;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome};

/// How long the mock waits for a frame (upstream's read deadlines).
const UPSTREAM_WAIT: Duration = Duration::from_secs(8);

/// How long the client waits for a message.
const CLIENT_WAIT: Duration = Duration::from_secs(10);

/// How long the mock may take to see its connection close.
const CLEANUP_WAIT: Duration = Duration::from_secs(3);

/// A mock Codex on 127.0.0.1, each connection of which runs a script.
struct Upstream {
    /// Its base URL.
    url: String,
    connections: Arc<AtomicUsize>,
    /// The frames the scripts counted.
    frames: Arc<AtomicUsize>,
    /// Whether each connection's script finished without failing.
    done: mpsc::UnboundedReceiver<bool>,
}

impl Upstream {
    /// A mock whose every connection runs `script`.
    async fn start<F, Fut>(script: F) -> Self
    where
        F: Fn(Peer) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(AtomicUsize::new(0));
        let (done_tx, done) = mpsc::unbounded_channel();
        let (count, counted) = (Arc::clone(&connections), Arc::clone(&frames));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let (script, frames, done) =
                    (script.clone(), Arc::clone(&counted), done_tx.clone());
                tokio::spawn(async move {
                    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                        let _ = done.send(false);
                        return;
                    };
                    let finished = tokio::spawn(script(Peer { ws, frames })).await;
                    let _ = done.send(finished.is_ok());
                });
            }
        });
        Self {
            url,
            connections,
            frames,
            done,
        }
    }

    /// Waits for a connection's script to finish, and checks it didn't
    /// fail.
    async fn finished(&mut self) {
        let ok = tokio::time::timeout(CLEANUP_WAIT, self.done.recv())
            .await
            .expect("upstream cleanup stalled");
        assert_eq!(ok, Some(true), "the mock's script failed");
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn frames(&self) -> usize {
        self.frames.load(Ordering::SeqCst)
    }
}

/// The mock's end of a connection.
struct Peer {
    ws: WebSocketStream<TcpStream>,
    frames: Arc<AtomicUsize>,
}

impl Peer {
    /// The next text frame, counted, or `None` once the connection ends or
    /// nothing comes in time.
    async fn read(&mut self) -> Option<String> {
        let text = self.next().await?;
        self.frames.fetch_add(1, Ordering::SeqCst);
        Some(text)
    }

    /// The next text frame, not counted.
    async fn next(&mut self) -> Option<String> {
        loop {
            match tokio::time::timeout(UPSTREAM_WAIT, self.ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => return Some(text.as_str().to_owned()),
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
                _ => return None,
            }
        }
    }

    async fn write(&mut self, text: &str) {
        self.ws
            .send(Message::Text(text.to_owned().into()))
            .await
            .unwrap();
    }
}

/// A config with `codex.response-steering` as `enabled`.
fn steering(enabled: bool) -> Config {
    let mut config = Config::default();
    config.codex.response_steering = enabled;
    config
}

/// A Codex credential for `base_url` with websockets on: an API key, or an
/// access token when `oauth`.
fn credential(id: &str, base_url: &str, oauth: bool) -> Auth {
    let mut auth = Auth {
        id: id.to_owned(),
        provider: "codex".into(),
        status: Status::Active,
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("websockets".into(), "true".into());
    if oauth {
        auth.metadata
            .insert("access_token".into(), Value::String("test-token".into()));
    } else {
        auth.attributes.insert("api_key".into(), "test-key".into());
    }
    auth.metadata
        .insert("disable_cooling".into(), Value::Bool(false));
    auth
}

/// The whole router with `config`, over an auth manager whose Codex
/// executor follows `config` too, with `credential` serving `model`: the
/// socket's URL, and the manager.
async fn proxy(config: &Config, credential: Auth, model: &str) -> (String, Arc<Manager>) {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(Settings::default(), registry.clone(), None));
    manager.register_executor(Arc::new(
        CodexExecutor::new("direct").with_config(Arc::new(config.clone())),
    ));
    let models = [ModelInfo {
        id: model.to_owned(),
        ..ModelInfo::default()
    }];
    registry.register_client(&credential.id, "codex", &models);
    manager.register_unsaved(credential).unwrap();
    let server = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..ServerConfig::from(config)
    };
    let catalog = FakeCatalog::new().serve(model, &["codex"]);
    let state = AppState::new(server, manager.clone(), Arc::new(catalog));
    let app = crate::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("ws://{addr}/v1/responses"), manager)
}

/// The next text message, or `None` once the socket ends.
async fn next_text(ws: &mut Client) -> Option<String> {
    loop {
        let next = tokio::time::timeout(CLIENT_WAIT, ws.next())
            .await
            .expect("the socket neither sent nor closed");
        match next {
            Some(Ok(Message::Text(text))) => return Some(text.as_str().to_owned()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return None,
        }
    }
}

/// The string at `path` (a JSON pointer) of `text`, or empty.
fn at(text: &str, path: &str) -> String {
    parse(text.as_bytes())
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

// TestResponsesSteerRejectedWhenDisabled: the session's own requests turn
// response.steer away; only a Codex duplex stream takes it.
#[test]
fn steer_rejected_when_disabled() {
    let error = normalize_default(
        r#"{"type":"response.steer","previous_response_id":"resp-1","input":"Keep the scope small."}"#,
        r#"{"model":"gpt-6-astra","stream":true,"input":[]}"#,
        "[]",
    )
    .unwrap_err();
    assert!(
        error
            .text
            .contains("unsupported websocket request type: response.steer"),
        "{}",
        error.text
    );
}

// TestResponsesSteerInFlightWebSocket
#[tokio::test]
async fn steer_in_flight() {
    let received = Arc::new(Notify::new());
    let signal = Arc::clone(&received);
    let _upstream = Upstream::start(move |mut peer| {
        let signal = Arc::clone(&signal);
        async move {
            let create = peer.read().await.unwrap();
            assert_eq!(at(&create, "/type"), "response.create", "{create}");
            peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
                .await;
            let steer = peer.read().await.unwrap();
            assert_eq!(at(&steer, "/type"), "response.steer", "{steer}");
            signal.notify_one();
            peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#).await;
            peer.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
                .await;
        }
    })
    .await;
    let model = "steering-red-model";
    let auth = credential("steering-red-test", &_upstream.url, false);
    let (url, _manager) = proxy(&steering(true), auth, model).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        &format!(r#"{{"type":"response.create","model":"{model}","input":[]}}"#),
    )
    .await;
    let created = next_text(&mut ws).await.unwrap();
    assert_eq!(at(&created, "/type"), "response.created", "{created}");
    send(
        &mut ws,
        r#"{"type":"response.steer","previous_response_id":"r1","input":"Focus on networking"}"#,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), received.notified())
        .await
        .expect("upstream never received response.steer frame during in-flight generation");
}

// TestResponsesSteeringDisabledAccountCannotSendAnotherFrame
#[tokio::test]
async fn disabled_account_cannot_send_another_frame() {
    let mut upstream = Upstream::start(|mut peer| async move {
        peer.read().await.unwrap();
        peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
            .await;
        peer.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        peer.read().await;
    })
    .await;
    let id = "steering-disable-test";
    let model = "steering-disable-model";
    let (url, manager) = proxy(&steering(true), credential(id, &upstream.url, false), model).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        &format!(r#"{{"type":"response.create","model":"{model}","input":[]}}"#),
    )
    .await;
    for _ in 0..2 {
        next_text(&mut ws).await.unwrap();
    }
    let mut disabled = (*manager.get(id).unwrap()).clone();
    disabled.disabled = true;
    disabled.status = Status::Disabled;
    manager.update_unsaved(disabled).unwrap();
    send(
        &mut ws,
        r#"{"type":"response.steer","previous_response_id":"r1","input":"must not be sent"}"#,
    )
    .await;
    // The socket ends without a word (gorilla's 1006), where a session
    // without steering would close it asking for a replay.
    assert_eq!(
        rest(&mut ws).await,
        (Vec::new(), None),
        "disabled account connection remained usable"
    );
    upstream.finished().await;
    assert_eq!(upstream.frames(), 1, "the disabled credential sent more");
}

// TestResponsesSteeringErrorRecoveryIntegration: only an error event after
// a response has started, with steering on, leaves the socket open.
#[tokio::test]
async fn error_recovery() {
    const REJECTION: &str = r#"{"type":"error","status":400,"event_id":"rejected-create","error":{"type":"invalid_request_error","message":"Correct the request"}}"#;
    for (name, enabled, initial, upstream_close, stale_oauth_scope) in [
        ("later_error_corrected_create", true, false, false, false),
        ("initial_error_remains_terminal", true, true, false, false),
        (
            "disabled_error_remains_terminal",
            false,
            false,
            false,
            false,
        ),
        ("later_error_then_upstream_close", true, false, true, false),
        (
            "stale_oauth_scope_does_not_disable_shared_api_key_steering",
            true,
            false,
            false,
            true,
        ),
    ] {
        let recoverable = enabled && !initial && !upstream_close;
        let mut upstream = Upstream::start(move |mut peer| async move {
            peer.read().await.unwrap();
            if !initial {
                peer.write(r#"{"type":"response.created","response":{"id":"first","output":[]}}"#)
                    .await;
                peer.write(
                    r#"{"type":"response.completed","response":{"id":"first","output":[]}}"#,
                )
                .await;
                peer.read().await.unwrap();
            }
            peer.write(REJECTION).await;
            if upstream_close {
                return;
            }
            if recoverable {
                let corrected = peer.read().await.unwrap();
                assert_eq!(at(&corrected, "/instructions"), "CORRECTED", "{corrected}");
                peer.write(
                    r#"{"type":"response.created","response":{"id":"corrected","output":[]}}"#,
                )
                .await;
                peer.write(r#"{"type":"response.completed","response":{"id":"corrected","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"RECOVERED"}]}]}}"#).await;
            }
            peer.next().await;
        })
        .await;
        let config = if stale_oauth_scope {
            // The setting under `oauth.providers`, where a v8 document puts
            // it; it still applies to an API key, as it is shared. A stale
            // OAuth-only entry for it can't be set from here (see the
            // module's deviations).
            let config =
                Config::parse("oauth: {providers: {codex: {response-steering: true}}}").unwrap();
            assert!(config.codex.response_steering, "{name}");
            config
        } else {
            steering(enabled)
        };
        let model = "steering-error-model";
        let auth = credential(&format!("steering-error-{name}"), &upstream.url, false);
        let (url, _manager) = proxy(&config, auth, model).await;
        let mut ws = connect(&url, &[]).await;
        let create = |instructions: &str| {
            format!(
                r#"{{"type":"response.create","model":"{model}","instructions":"{instructions}","input":[]}}"#
            )
        };
        send(&mut ws, &create("INITIAL")).await;
        let (mut errors, mut completed) = (0, false);
        loop {
            let Some(text) = next_text(&mut ws).await else {
                assert!(
                    !recoverable,
                    "{name}: socket closed before corrected create completed"
                );
                break;
            };
            match at(&text, "/type").as_str() {
                "response.completed" if at(&text, "/response/id") == "first" => {
                    send(&mut ws, &create("REJECTED")).await;
                }
                "response.completed" => {
                    completed = at(&text, "/response/output/0/content/0/text") == "RECOVERED";
                    let _ = ws.close(None).await;
                    break;
                }
                "error" => {
                    errors += 1;
                    if enabled && !initial {
                        assert_eq!(text, REJECTION, "{name}: recoverable error payload changed");
                    }
                    if recoverable {
                        send(&mut ws, &create("CORRECTED")).await;
                    }
                }
                _ => {}
            }
        }
        assert_eq!((errors, completed), (1, recoverable), "{name}");
        upstream.finished().await;
        let frames = if initial {
            1
        } else if recoverable {
            3
        } else {
            2
        };
        assert_eq!(
            (upstream.connections(), upstream.frames()),
            (1, frames),
            "{name}"
        );
    }
}

// TestResponsesWebsocketClosesOnIdleCodexDisconnect, the cases with
// steering on: the duplex stream ends with the upstream connection.
#[tokio::test]
async fn idle_codex_disconnect_closes_socket() {
    for (name, yaml, oauth) in [
        (
            "legacy_enabled_api_key",
            "codex: {response-steering: true}\n",
            false,
        ),
        (
            "v8_enabled_api_key",
            "oauth: {providers: {codex: {response-steering: true}}}\n",
            false,
        ),
        (
            "v8_enabled_oauth",
            "oauth: {providers: {codex: {response-steering: true}}}\n",
            true,
        ),
        (
            "upstream_enabled_api_key",
            "upstream: {codex: {response-steering: true}}\n",
            false,
        ),
        (
            "upstream_enabled_oauth",
            "upstream: {codex: {response-steering: true}}\n",
            true,
        ),
    ] {
        let close = Arc::new(Notify::new());
        let signal = Arc::clone(&close);
        let mut upstream = Upstream::start(move |mut peer| {
            let signal = Arc::clone(&signal);
            async move {
                peer.read().await.unwrap();
                for payload in [
                    r#"{"type":"response.created","response":{"id":"first","output":[]}}"#,
                    r#"{"type":"response.completed","response":{"id":"first","output":[]}}"#,
                ] {
                    peer.write(payload).await;
                }
                // Close only after the client has the completed response.
                signal.notified().await;
                let frame = CloseFrame {
                    code: CloseCode::Away,
                    reason: "idle upstream disconnect".into(),
                };
                peer.ws.send(Message::Close(Some(frame))).await.unwrap();
            }
        })
        .await;
        let config = Config::parse(yaml).unwrap();
        assert!(config.codex.response_steering, "{name}");
        let model = "idle-disconnect-model";
        let auth = credential(&format!("idle-disconnect-{name}"), &upstream.url, oauth);
        let (url, _manager) = proxy(&config, auth, model).await;
        let mut ws = connect(&url, &[]).await;
        send(
            &mut ws,
            &format!(r#"{{"type":"response.create","model":"{model}","input":[]}}"#),
        )
        .await;
        loop {
            let text = next_text(&mut ws)
                .await
                .unwrap_or_else(|| panic!("{name}: read response before upstream close"));
            if at(&text, "/type") == "response.completed" {
                break;
            }
        }
        close.notify_one();
        assert_eq!(
            next_text(&mut ws).await,
            None,
            "{name}: expected downstream close after idle upstream disconnect"
        );
        upstream.finished().await;
    }
}

// TestResponsesSteeringFullDuplexIntegration
#[tokio::test]
async fn full_duplex_integration() {
    const CONTROL1: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"one"}"#;
    const CONTROL2: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"two"}"#;
    for scenario in [
        "successor",
        "tool_pending",
        "disconnect_accepted",
        "disconnect_pending",
    ] {
        let pending_case = scenario == "tool_pending" || scenario == "disconnect_pending";
        let mut upstream = Upstream::start(move |mut peer| async move {
            let create = peer.read().await.unwrap();
            assert_eq!(at(&create, "/type"), "response.create", "missing initial create");
            peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
                .await;
            assert_eq!(peer.read().await.unwrap(), CONTROL1, "first steer altered");
            assert_eq!(peer.read().await.unwrap(), CONTROL2, "second steer altered");
            peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#).await;
            peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"r1"}}"#).await;
            if scenario == "disconnect_accepted" {
                return;
            }
            if pending_case {
                peer.write(r#"{"type":"response.completed","response":{"id":"r1","output":[{"type":"function_call","call_id":"call1","name":"lookup","arguments":"{}"}]}}"#).await;
                peer.write(r#"{"type":"response.steer.pending","steer":{"id":"s1","previous_response_id":"r1"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"call1","name":"lookup"}]}"#).await;
                if scenario == "disconnect_pending" {
                    return;
                }
                let create = peer.read().await.unwrap();
                assert!(
                    at(&create, "/type") == "response.create"
                        && at(&create, "/input/0/call_id") == "call1",
                    "required input lost: {create}"
                );
            } else {
                peer.write(r#"{"type":"response.incomplete","response":{"id":"r1","incomplete_details":{"reason":"steered"},"output":[]}}"#).await;
            }
            peer.write(r#"{"type":"response.created","response":{"id":"r2"}}"#)
                .await;
            peer.write(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"STEER_OK"}]}}"#).await;
            peer.write(r#"{"type":"response.completed","response":{"id":"r2","output":[]}}"#)
                .await;
            peer.next().await;
        })
        .await;
        let model = "steering-test-model";
        let auth = credential(
            &format!("steering-integration-{scenario}"),
            &upstream.url,
            false,
        );
        let (url, _manager) = proxy(&steering(true), auth, model).await;
        let mut ws = connect(&url, &[]).await;
        send(
            &mut ws,
            &format!(r#"{{"type":"response.create","model":"{model}","input":[]}}"#),
        )
        .await;
        let (mut completed, mut accepted, mut pending_seen) = (false, 0, false);
        loop {
            let Some(text) = next_text(&mut ws).await else {
                assert!(
                    scenario.starts_with("disconnect_"),
                    "{scenario}: early close"
                );
                break;
            };
            let kind = at(&text, "/type");
            let id = at(&text, "/response/id");
            if kind == "response.created" && id == "r1" {
                send(&mut ws, CONTROL1).await;
                send(&mut ws, CONTROL2).await;
            }
            if kind == "response.steer.accepted" {
                accepted += 1;
            }
            if kind == "response.steer.pending" {
                pending_seen = true;
                if scenario == "tool_pending" {
                    send(&mut ws, r#"{"type":"response.create","previous_response_id":"r1","input":[{"type":"function_call_output","call_id":"call1","output":"found"}]}"#).await;
                }
            }
            if kind == "response.completed" && id == "r2" {
                completed = at(&text, "/response/output/0/content/0/text") == "STEER_OK";
                let _ = ws.close(None).await;
                break;
            }
        }
        assert_eq!(
            accepted, 2,
            "{scenario}: expected both acknowledgements before termination"
        );
        assert!(
            !pending_case || pending_seen,
            "{scenario}: connection ended before the pending event was forwarded"
        );
        assert!(
            scenario.starts_with("disconnect_") || completed,
            "{scenario}: successor did not complete"
        );
        upstream.finished().await;
        assert_eq!(upstream.connections(), 1, "{scenario}: reconnect or replay");
    }
}

// TestResponsesSteeringLocalValidationRecovery
#[tokio::test]
async fn local_validation_recovery() {
    let mut upstream = Upstream::start(|mut peer| async move {
        peer.read().await.unwrap();
        peer.write(r#"{"type":"response.created","response":{"id":"first","output":[]}}"#)
            .await;
        let steer = peer.read().await.unwrap();
        assert_eq!(
            at(&steer, "/type"),
            "response.steer",
            "expected corrected steering"
        );
        peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"corrected","previous_response_id":"first"}}"#).await;
        peer.write(r#"{"type":"response.completed","response":{"id":"first","output":[]}}"#)
            .await;
        peer.write(r#"{"type":"response.created","response":{"id":"steered","previous_response_id":"first","output":[]}}"#).await;
        peer.write(r#"{"type":"response.completed","response":{"id":"steered","output":[]}}"#)
            .await;
        let create = peer.read().await.unwrap();
        assert_eq!(
            at(&create, "/type"),
            "response.create",
            "expected corrected create"
        );
        peer.write(r#"{"type":"response.created","response":{"id":"second","output":[]}}"#)
            .await;
        peer.write(r#"{"type":"response.completed","response":{"id":"second","output":[]}}"#)
            .await;
        peer.next().await;
    })
    .await;
    let (id, model) = (
        "steering-local-validation",
        "steering-local-validation-model",
    );
    let (url, manager) = proxy(&steering(true), credential(id, &upstream.url, false), model).await;
    let mut ws = connect(&url, &[]).await;
    let create = format!(r#"{{"type":"response.create","model":"{model}","input":[]}}"#);
    send(&mut ws, &create).await;
    // An invalid frame queued before the first response starts: its error
    // still comes after response.created.
    send(&mut ws, "{").await;
    let (mut errors, mut accepted, mut completed) = (0, false, false);
    while !completed {
        let text = next_text(&mut ws)
            .await
            .expect("local validation closed the established socket");
        match at(&text, "/type").as_str() {
            "response.created" => {
                assert!(
                    at(&text, "/response/id") != "first" || errors == 0,
                    "local error preceded bootstrap"
                );
            }
            "error" => {
                errors += 1;
                let event = parse(text.as_bytes());
                assert!(
                    event["status"] == 400 && event["error"]["type"] == "invalid_request_error",
                    "invalid error envelope: {text}"
                );
                let message = at(&text, "/error/message");
                match errors {
                    1 => {
                        assert!(
                            message.contains("JSON"),
                            "missing JSON validation message: {text}"
                        );
                        send(&mut ws, r#"{"type":"unsupported.request"}"#).await;
                    }
                    2 => {
                        assert!(
                            message.contains("unsupported"),
                            "missing type validation message: {text}"
                        );
                        send(&mut ws, r#"{"type":"response.steer","steering_id":"corrected","input":[{"role":"user","content":[{"type":"input_text","text":"continue"}]}]}"#).await;
                    }
                    _ => panic!("unexpected error: {text}"),
                }
            }
            "response.steer.accepted" => accepted = true,
            "response.completed" => match at(&text, "/response/id").as_str() {
                "first" => send(&mut ws, &create).await,
                "second" => completed = true,
                _ => {}
            },
            _ => {}
        }
    }
    assert_eq!((errors, accepted), (2, true));
    let _ = ws.close(None).await;
    upstream.finished().await;
    assert_eq!((upstream.connections(), upstream.frames()), (1, 3));
    let current = manager.get(id).unwrap();
    assert!(
        !current.unavailable && current.last_error.is_none(),
        "local validation cooled healthy credential: {:?}",
        current.last_error
    );
}

/// Has the dispatcher say every credential is `test-provider`'s without
/// websockets, `off` disabled, and that it doesn't know `gone`.
fn known_credentials(dispatcher: &FakeDispatcher) {
    dispatcher.websocket(
        |_: &[String], _: &str, auth_id: Option<&str>| WebsocketSupport {
            auth: auth_id.filter(|&id| id != "gone").map(|id| WebsocketAuth {
                provider: "test-provider".into(),
                serves_model: true,
                websockets: false,
                disabled: id == "off",
            }),
            ..WebsocketSupport::default()
        },
    );
}

/// Serves the router with steering on over `dispatcher`, which serves
/// `test-model`: the socket's URL.
async fn serve_steering(dispatcher: &Arc<FakeDispatcher>) -> String {
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        codex_response_steering: true,
        ..ServerConfig::default()
    };
    let state = crate::testing::state(config, test_catalog(), dispatcher);
    let app = crate::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("ws://{addr}/v1/responses")
}

// Added: with steering on, a turn without a Codex duplex stream ends as it
// does with steering off, and a request sent during it waits for the next
// turn. Every call gets the client's frames, with a check that turns away
// a credential the dispatcher doesn't know or says is disabled.
#[tokio::test]
async fn turns_without_a_duplex_stream_end_as_before() {
    let dispatcher =
        FakeDispatcher::new(vec![completes("resp-1", "[]"), completes("resp-2", "[]")]);
    known_credentials(&dispatcher);
    let url = serve_steering(&dispatcher).await;
    let mut ws = connect(&url, &[]).await;
    let create = r#"{"type":"response.create","model":"test-model","input":[]}"#;
    send(&mut ws, create).await;
    send(&mut ws, create).await;
    for id in ["resp-1", "resp-2"] {
        let event = recv(&mut ws).await;
        assert_eq!(
            (&event["type"], &event["response"]["id"]),
            (&Value::from("response.completed"), &Value::from(id))
        );
    }
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    for call in calls {
        let input = call.options.websocket_input.expect("the client's frames");
        assert!(input.auth_enabled("on"));
        assert!(!input.auth_enabled("off"));
        assert!(!input.auth_enabled("gone"));
    }
}

// Added: with steering on, a client that goes away mid-turn ends the turn
// at once, as upstream's socket context does, rather than when a write
// fails.
#[tokio::test]
async fn a_client_that_goes_ends_the_turn() {
    let created = sse(r#"{"type":"response.created","response":{"id":"resp-1"}}"#);
    let dispatcher = FakeDispatcher::new(vec![Outcome::Hang(
        HeaderMap::new(),
        vec![Ok(Bytes::from(created))],
    )]);
    let url = serve_steering(&dispatcher).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.created");
    assert_eq!(dispatcher.live_streams(), 1);
    ws.close(None).await.unwrap();
    eventually(|| dispatcher.live_streams() == 0).await;
    // The close is answered.
    assert!(rest(&mut ws).await.0.is_empty());
}
