// Ported from CLIProxyAPI sdk/api/handlers/gemini/interactions_handlers_test.go
// (TestInteractionsAgentUsesNativeInteractionsEndpoint) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v1beta/interactions` through the credential manager and the real
//! `gemini-interactions` executor, against a mock Interactions API on a
//! loopback port.
//!
//! Changed from upstream:
//! - The request goes through the router, so it carries a client key, which
//!   upstream's call of the handler needs no key for.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use bytes::Bytes;
use http::{HeaderMap, StatusCode, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_providers::gemini::InteractionsExecutor;
use serde_json::{Value, json};

use super::{AGENT, PATH, content_type, post, send};
use crate::config::ServerConfig;
use crate::router;
use crate::state::AppState;
use crate::testing::FakeCatalog;

/// The model an `agent` call's credential is picked by.
const SELECTION_MODEL: &str = "gemini-2.5-flash";
/// The credential's API key, which only the mock should see.
const CREDENTIAL_KEY: &str = "test-key";
/// The mock's answer to a call that doesn't stream (upstream's).
const INTERACTION: &str = r#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[{"type":"model_output","content":[{"text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// A mock Interactions API answering every request with `status`,
/// `content_type` and `body`.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(status: u16, content_type: &'static str, body: &'static str) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, received: Bytes| {
            let recorder = Arc::clone(&recorder);
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Seen {
                        path: uri.path().to_owned(),
                        headers,
                        body: received,
                    });
                axum::response::Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    /// The only request the mock received.
    fn only(&self) -> Seen {
        let seen = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let [request] = seen.as_slice() else {
            panic!("requests: {seen:?}");
        };
        request.clone()
    }
}

/// The router, with the client key `sk-test`, over a manager with the
/// `gemini-interactions` executor and one of its credentials, an API key
/// calling `mock`, registered for `gemini-2.5-flash` alone. The catalog
/// routes `gemini-2.5-flash` to `gemini-interactions`.
fn proxy(mock: &Mock) -> Router {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(Settings::default(), registry.clone(), None));
    manager.register_executor(Arc::new(InteractionsExecutor::new("direct")));
    let Value::Object(metadata) = json!({"email": "interactions-agent@example.com"}) else {
        unreachable!("an object");
    };
    let mut auth = Auth {
        id: "interactions-agent-native-auth".into(),
        provider: "gemini-interactions".into(),
        metadata,
        ..Auth::default()
    };
    auth.attributes
        .insert("api_key".into(), CREDENTIAL_KEY.into());
    auth.attributes.insert("base_url".into(), mock.url.clone());
    manager.register_unsaved(auth).unwrap();
    registry.register_client(
        "interactions-agent-native-auth",
        "gemini-interactions",
        &[ModelInfo {
            id: SELECTION_MODEL.into(),
            ..ModelInfo::default()
        }],
    );
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..ServerConfig::default()
    };
    let catalog = FakeCatalog::new().serve(SELECTION_MODEL, &["gemini-interactions"]);
    router(AppState::new(config, manager, Arc::new(catalog)))
}

// TestInteractionsAgentUsesNativeInteractionsEndpoint. Also checks the key
// the call carries upstream: the credential's, not the client's.
#[tokio::test]
async fn an_agent_uses_the_native_interactions_endpoint() {
    let mock = Mock::start(200, "application/json", INTERACTION).await;
    let app = proxy(&mock);
    let (status, _, body) = send(
        &app,
        post(PATH, r#"{"agent":"agents/test-agent","input":"hi"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let seen = mock.only();
    assert_eq!(seen.path, "/v1beta/interactions");
    assert_eq!(seen.json()["agent"], AGENT, "{:?}", seen.body);
    assert_eq!(seen.header("x-goog-api-key"), Some(CREDENTIAL_KEY));
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["id"], "interaction_1", "{body}");
}

// Not upstream's: an agent's stream reaches the client frame for frame, as
// the API sent it.
#[tokio::test]
async fn an_agent_stream_passes_the_frames_through() {
    let frames = concat!(
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"interaction_1\"}}\n\n",
        "event: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"interaction_1\",\"status\":\"completed\"}}\n\n",
    );
    let mock = Mock::start(200, "text/event-stream", frames).await;
    let app = proxy(&mock);
    let request = r#"{"agent":"agents/test-agent","input":"hi","stream":true}"#;
    let (status, headers, body) = send(&app, post(PATH, request)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type(&headers), "text/event-stream");
    assert_eq!(body, frames);

    let seen = mock.only();
    assert_eq!(seen.path, "/v1beta/interactions");
    assert_eq!(seen.json()["agent"], AGENT);
    assert_eq!(seen.json()["stream"], true);
}

// Not upstream's: a model named as a resource reaches the API by its bare
// name.
#[tokio::test]
async fn a_model_resource_name_reaches_the_api_bare() {
    let mock = Mock::start(200, "application/json", INTERACTION).await;
    let app = proxy(&mock);
    let request = r#"{"model":"models/gemini-2.5-flash","input":"hi"}"#;
    let (status, _, body) = send(&app, post(PATH, request)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let seen = mock.only();
    assert_eq!(seen.path, "/v1beta/interactions");
    assert_eq!(seen.json()["model"], SELECTION_MODEL);
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["id"], "interaction_1", "{body}");
}

// Not upstream's: the API's error comes back with its status.
#[tokio::test]
async fn an_agent_error_comes_back_with_its_status() {
    let error = r#"{"error":{"code":404,"message":"agent not found","status":"NOT_FOUND"}}"#;
    let mock = Mock::start(404, "application/json", error).await;
    let app = proxy(&mock);
    let (status, _, body) = send(&app, post(PATH, r#"{"agent":"agents/missing"}"#)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("agent not found"), "{body}");
    assert_eq!(mock.only().json()["agent"], "agents/missing");
}
