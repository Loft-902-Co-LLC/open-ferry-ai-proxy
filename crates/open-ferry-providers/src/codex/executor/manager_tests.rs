// Ported from CLIProxyAPI test/codex_quota_failover_test.go (TestCodexTerminalQuotaCoolsAccountAcrossModels, TestCodexModelLevelCoolingPreservesSiblingModel) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The executor under the credential manager, against a mock Codex server
//! on 127.0.0.1 that tells two API keys apart: how a usage limit cools a
//! credential, with and without `codex.model-level-cooling`, and how an
//! overload that stream bootstrap buffering holds back reaches the manager.
//!
//! Deviations from upstream:
//! - Upstream's `websocket-*` rows register its WebSocket executor, which
//!   takes every call over the WebSocket. Here the executor takes a call
//!   there only for a client on the Responses WebSocket and a credential
//!   with websockets on, so those rows have both.
//! - Upstream registers the models in its global registry; here each test
//!   has its own.
//! - `bootstrap_overload_fails_over_to_another_credential` isn't upstream's.
//!   It checks what upstream's buffering tests leave to the manager: that
//!   an overload held back before the stream starts makes the manager try
//!   the next credential, and that without buffering it reaches the client.

use std::sync::{Mutex, PoisonError};

use axum::Router;
use chrono::{TimeDelta, Utc};
use futures_util::StreamExt as _;
use open_ferry_core::auth::Status;
use open_ferry_core::config::CodexConfig;
use open_ferry_core::exec::Dispatcher;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;

use super::bootstrap_tests::OVERLOAD_EVENT;
use super::*;
use crate::codex::websocket::mock::{Answer, Server};

const CREATED: &str = r#"{"type":"response.created","response":{"id":"quota-test-response"}}"#;
const QUOTA: &str = r#"{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":3600}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"quota-test-success","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;

/// The credentials' API keys and priorities: the first is picked first.
const CREDENTIALS: [(&str, &str); 2] = [("quota-high", "4"), ("quota-low", "3")];

/// How Codex answers, as the rows of upstream's tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    /// Over HTTP, with server-sent events.
    Sse,
    /// Over the Responses WebSocket, ending a usage limit with an `error`
    /// event.
    WebsocketError,
    /// Over the Responses WebSocket, ending a usage limit with
    /// `response.failed`.
    WebsocketFailed,
}

impl Transport {
    const ALL: [Self; 3] = [Self::Sse, Self::WebsocketError, Self::WebsocketFailed];

    fn name(self) -> &'static str {
        match self {
            Self::Sse => "sse",
            Self::WebsocketError => "websocket-error",
            Self::WebsocketFailed => "websocket-failed",
        }
    }

    /// The event a usage limit ends the answer with.
    fn quota(self) -> String {
        match self {
            Self::WebsocketFailed => {
                format!(r#"{{"type":"response.failed","response":{{"error":{QUOTA}}}}}"#)
            }
            Self::Sse | Self::WebsocketError => {
                format!(r#"{{"type":"error","status":429,"error":{QUOTA}}}"#)
            }
        }
    }
}

/// The ID of the credential with `key`.
fn credential_id(key: &str, transport: Transport) -> String {
    format!("{key}-{}", transport.name())
}

/// The API key a request's `Authorization` carries.
fn bearer(authorization: Option<&str>) -> String {
    authorization
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_owned()
}

/// A mock Codex that answers `CREATED`, then the event `terminal` picks
/// given the API key and the request body, over `transport`. Returns its
/// URL and the API key of each request or handshake, in order.
async fn serve<T>(transport: Transport, terminal: T) -> (String, Arc<Mutex<Vec<String>>>)
where
    T: Fn(&str, &str) -> String + Clone + Send + Sync + 'static,
{
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&attempts);
    if transport != Transport::Sse {
        let server = Server::start(move |_| {
            let (terminal, recorder) = (terminal.clone(), Arc::clone(&recorder));
            Answer::accept(move |mut peer| {
                let (terminal, recorder) = (terminal.clone(), Arc::clone(&recorder));
                async move {
                    let account = bearer(peer.handshake().header("authorization"));
                    recorder
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(account.clone());
                    let Some(body) = peer.recv().await else {
                        return;
                    };
                    peer.send(CREATED).await;
                    peer.send(&terminal(&account, &body)).await;
                }
            })
        })
        .await;
        return (server.url, attempts);
    }
    let app = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let (terminal, recorder) = (terminal.clone(), Arc::clone(&recorder));
        async move {
            let account = bearer(
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
            );
            let event = terminal(&account, &String::from_utf8_lossy(&body));
            recorder
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(account);
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(format!(
                    "data: {CREATED}\n\ndata: {event}\n\n"
                )))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, attempts)
}

/// A manager with no retries, the Codex executor following `codex`, and
/// [`CREDENTIALS`] for `url`, each serving `models`, with websockets on
/// for a WebSocket `transport`.
fn start_manager(codex: CodexConfig, url: &str, models: &[&str], transport: Transport) -> Manager {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, None);
    let mut config = Config::default();
    config.codex = codex;
    manager.register_executor(Arc::new(
        CodexExecutor::new("direct").with_config(Arc::new(config)),
    ));
    let models: Vec<ModelInfo> = models
        .iter()
        .map(|id| ModelInfo {
            id: (*id).to_owned(),
            ..ModelInfo::default()
        })
        .collect();
    for (key, priority) in CREDENTIALS {
        let id = credential_id(key, transport);
        registry.register_client(&id, "codex", &models);
        let mut auth = Auth {
            id,
            provider: "codex".into(),
            status: Status::Active,
            ..Auth::default()
        };
        auth.attributes.insert("priority".into(), priority.into());
        auth.attributes.insert("base_url".into(), url.into());
        auth.attributes.insert("api_key".into(), key.into());
        if transport != Transport::Sse {
            auth.attributes.insert("websockets".into(), "true".into());
        }
        auth.metadata
            .insert("disable_cooling".into(), Value::Bool(false));
        manager.register(auth).unwrap();
    }
    manager
}

/// Streams a Responses request for `model` through `manager`, from a client
/// on the Responses WebSocket for a WebSocket `transport`: the payload, and
/// the last error.
async fn run(manager: &Manager, model: &str, transport: Transport) -> (String, Option<ExecError>) {
    let request = Request {
        model: model.into(),
        payload: Bytes::from(format!(r#"{{"model":"{model}","input":"hello"}}"#)),
    };
    let options = Options {
        stream: true,
        downstream_websocket: transport != Transport::Sse,
        ..Options::new(Format::from("openai-response".to_owned()))
    };
    let response = manager
        .execute_stream(&["codex".to_owned()], request, options)
        .await
        .unwrap_or_else(|error| {
            panic!("the stream must start before the terminal error: {error:?}")
        });
    let mut chunks = response.chunks;
    let mut payload = String::new();
    let mut error = None;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => payload.push_str(&String::from_utf8_lossy(&chunk)),
            Err(failure) => error = Some(failure),
        }
    }
    (payload, error)
}

fn attempts(recorded: &Mutex<Vec<String>>) -> Vec<String> {
    recorded
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The first credential's cooldown reason, and when it ends.
fn high_recovers_at(manager: &Manager, transport: Transport) -> (String, chrono::DateTime<Utc>) {
    let high = manager
        .get(&credential_id(CREDENTIALS[0].0, transport))
        .unwrap();
    let recover = high
        .quota
        .next_recover_at
        .expect("the credential has no recovery time");
    (high.quota.reason.clone(), recover)
}

// TestCodexTerminalQuotaCoolsAccountAcrossModels: without model-level
// cooling a usage limit cools the whole credential, so the sibling model
// goes to the other one.
#[tokio::test]
async fn terminal_quota_cools_account_across_models() {
    const MODEL: &str = "gpt-5.4";
    const SIBLING: &str = "gpt-5.4-mini";
    for transport in Transport::ALL {
        let row = transport.name();
        let (url, recorded) = serve(transport, move |account, _| {
            if account == "quota-high" {
                transport.quota()
            } else {
                COMPLETED.to_owned()
            }
        })
        .await;
        let manager = start_manager(CodexConfig::default(), &url, &[MODEL, SIBLING], transport);

        let before = Utc::now();
        let (payload, error) = run(&manager, MODEL, transport).await;
        let error = error
            .unwrap_or_else(|| panic!("{row}: expected a terminal quota error after {payload}"));
        assert!(payload.contains("response.created"), "{row}: {payload}");
        assert!(error.credential_scoped, "{row}: {error:?}");
        let (reason, recover) = high_recovers_at(&manager, transport);
        assert_eq!(reason, "credential_quota", "{row}");
        assert!(recover >= before + TimeDelta::hours(1), "{row}: {recover}");

        let (payload, error) = run(&manager, SIBLING, transport).await;
        assert!(error.is_none(), "{row}: {error:?}");
        assert!(payload.contains("response.completed"), "{row}: {payload}");
        assert_eq!(attempts(&recorded), ["quota-high", "quota-low"], "{row}");
    }
}

// TestCodexModelLevelCoolingPreservesSiblingModel: with model-level cooling
// a usage limit cools only the model, so the sibling model stays on the
// same credential.
#[tokio::test]
async fn model_level_cooling_preserves_sibling_model() {
    const MODEL: &str = "gpt-5.3-codex-spark";
    const SIBLING: &str = "gpt-5.6-sol";
    for transport in Transport::ALL {
        let row = transport.name();
        let (url, recorded) = serve(transport, move |account, body| {
            if account == "quota-high" && body.contains(MODEL) {
                transport.quota()
            } else {
                COMPLETED.to_owned()
            }
        })
        .await;
        let codex = CodexConfig {
            model_level_cooling: true,
            ..CodexConfig::default()
        };
        let manager = start_manager(codex, &url, &[MODEL, SIBLING], transport);

        let before = Utc::now();
        let (payload, error) = run(&manager, MODEL, transport).await;
        let error = error
            .unwrap_or_else(|| panic!("{row}: expected a terminal quota error after {payload}"));
        assert!(payload.contains("response.created"), "{row}: {payload}");
        assert!(
            !error.credential_scoped,
            "{row}: model-level cooling must not scope the error to the credential: {error:?}"
        );
        let (reason, recover) = high_recovers_at(&manager, transport);
        assert_eq!(
            reason, "quota",
            "{row}: model-level cooling must not set credential_quota"
        );
        assert!(recover >= before + TimeDelta::hours(1), "{row}: {recover}");

        let (payload, error) = run(&manager, SIBLING, transport).await;
        assert!(
            error.is_none(),
            "{row}: the sibling model failed: {error:?}"
        );
        assert!(payload.contains("response.completed"), "{row}: {payload}");
        assert_eq!(attempts(&recorded), ["quota-high", "quota-high"], "{row}");
    }
}

// Not upstream's: an overload held back by bootstrap buffering fails the
// first credential's call before its stream starts, and the manager tries
// the next one; without buffering the stream has started, and the
// overload reaches the client.
#[tokio::test]
async fn bootstrap_overload_fails_over_to_another_credential() {
    const MODEL: &str = "gpt-5.6-terra";
    let terminal = |account: &str, _: &str| {
        if account == "quota-high" {
            OVERLOAD_EVENT.to_owned()
        } else {
            COMPLETED.to_owned()
        }
    };

    let (url, recorded) = serve(Transport::Sse, terminal).await;
    let codex = CodexConfig {
        stream_bootstrap_buffering: true,
        ..CodexConfig::default()
    };
    let manager = start_manager(codex, &url, &[MODEL], Transport::Sse);
    let (payload, error) = run(&manager, MODEL, Transport::Sse).await;
    assert!(
        error.is_none(),
        "the overload must not reach the client: {error:?}"
    );
    assert_eq!(payload.matches("response.created").count(), 1, "{payload}");
    assert!(payload.contains("quota-test-success"), "{payload}");
    assert_eq!(attempts(&recorded), ["quota-high", "quota-low"]);

    let (url, recorded) = serve(Transport::Sse, terminal).await;
    let manager = start_manager(CodexConfig::default(), &url, &[MODEL], Transport::Sse);
    let (payload, error) = run(&manager, MODEL, Transport::Sse).await;
    let error = error.unwrap_or_else(|| panic!("expected an in-stream overload after {payload}"));
    assert_eq!(error.status, 502);
    assert!(payload.contains("response.created"), "{payload}");
    assert_eq!(attempts(&recorded), ["quota-high"]);
}
