// Ported from CLIProxyAPI test/codex_quota_failover_test.go (TestCodexTerminalQuotaCoolsAccountAcrossModels, TestCodexModelLevelCoolingPreservesSiblingModel) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The executor under the credential manager, against a mock Codex server
//! on 127.0.0.1 that tells two API keys apart: how a usage limit cools a
//! credential, with and without `codex.model-level-cooling`.
//!
//! Deviations from upstream:
//! - The `websocket-error` and `websocket-failed` rows of both tests are
//!   dropped: the Responses WebSocket upstream isn't ported.
//! - Upstream registers the models in its global registry; here each test
//!   has its own.

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

use super::*;

const CREATED: &str = r#"{"type":"response.created","response":{"id":"quota-test-response"}}"#;
const QUOTA: &str = r#"{"type":"usage_limit_reached","message":"You've hit your usage limit.","resets_in_seconds":3600}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"quota-test-success","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;

/// The credentials, each with its API key and priority: the first is
/// picked first.
const CREDENTIALS: [(&str, &str, &str); 2] = [
    ("quota-high-sse", "quota-high", "4"),
    ("quota-low-sse", "quota-low", "3"),
];

/// Picks the event Codex ends its answer with, given the API key and the
/// request body.
type Terminal = fn(&str, &str) -> String;

/// A mock Codex server that answers `data: CREATED`, then the event
/// `terminal` picks. Returns its URL and the API key of each request, in
/// order.
async fn serve(terminal: Terminal) -> (String, Arc<Mutex<Vec<String>>>) {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&attempts);
    let app = Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let recorder = Arc::clone(&recorder);
        async move {
            let account = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_owned();
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
/// [`CREDENTIALS`] for `url`, each serving `models`.
fn start_manager(codex: CodexConfig, url: &str, models: &[&str]) -> Manager {
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
    for (id, key, priority) in CREDENTIALS {
        registry.register_client(id, "codex", &models);
        let mut auth = Auth {
            id: id.into(),
            provider: "codex".into(),
            status: Status::Active,
            ..Auth::default()
        };
        auth.attributes.insert("priority".into(), priority.into());
        auth.attributes.insert("base_url".into(), url.into());
        auth.attributes.insert("api_key".into(), key.into());
        auth.metadata
            .insert("disable_cooling".into(), Value::Bool(false));
        manager.register(auth).unwrap();
    }
    manager
}

/// Streams a Responses request for `model` through `manager`: the payload,
/// and the last error.
async fn run(manager: &Manager, model: &str) -> (String, Option<ExecError>) {
    let request = Request {
        model: model.into(),
        payload: Bytes::from(format!(r#"{{"model":"{model}","input":"hello"}}"#)),
    };
    let options = Options {
        stream: true,
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

/// When the first credential's cooldown ends.
fn high_recovers_at(manager: &Manager) -> (String, chrono::DateTime<Utc>) {
    let high = manager.get(CREDENTIALS[0].0).unwrap();
    let recover = high
        .quota
        .next_recover_at
        .expect("the credential has no recovery time");
    (high.quota.reason.clone(), recover)
}

// TestCodexTerminalQuotaCoolsAccountAcrossModels, the SSE row: without
// model-level cooling a usage limit cools the whole credential, so the
// sibling model goes to the other one.
#[tokio::test]
async fn terminal_quota_cools_account_across_models() {
    const MODEL: &str = "gpt-5.4";
    const SIBLING: &str = "gpt-5.4-mini";
    let (url, recorded) = serve(|account, _| {
        if account == "quota-high" {
            format!(r#"{{"type":"error","status":429,"error":{QUOTA}}}"#)
        } else {
            COMPLETED.to_owned()
        }
    })
    .await;
    let manager = start_manager(CodexConfig::default(), &url, &[MODEL, SIBLING]);

    let before = Utc::now();
    let (payload, error) = run(&manager, MODEL).await;
    let error = error.unwrap_or_else(|| panic!("expected a terminal quota error after {payload}"));
    assert!(payload.contains("response.created"), "{payload}");
    assert!(error.credential_scoped, "{error:?}");
    let (reason, recover) = high_recovers_at(&manager);
    assert_eq!(reason, "credential_quota");
    assert!(recover >= before + TimeDelta::hours(1), "{recover}");

    let (payload, error) = run(&manager, SIBLING).await;
    assert!(error.is_none(), "{error:?}");
    assert!(payload.contains("response.completed"), "{payload}");
    assert_eq!(attempts(&recorded), ["quota-high", "quota-low"]);
}

// TestCodexModelLevelCoolingPreservesSiblingModel, the SSE row: with
// model-level cooling a usage limit cools only the model, so the sibling
// model stays on the same credential.
#[tokio::test]
async fn model_level_cooling_preserves_sibling_model() {
    const MODEL: &str = "gpt-5.3-codex-spark";
    const SIBLING: &str = "gpt-5.6-sol";
    let (url, recorded) = serve(|account, body| {
        if account == "quota-high" && body.contains(MODEL) {
            format!(r#"{{"type":"error","status":429,"error":{QUOTA}}}"#)
        } else {
            COMPLETED.to_owned()
        }
    })
    .await;
    let codex = CodexConfig {
        model_level_cooling: true,
        ..CodexConfig::default()
    };
    let manager = start_manager(codex, &url, &[MODEL, SIBLING]);

    let before = Utc::now();
    let (payload, error) = run(&manager, MODEL).await;
    let error = error.unwrap_or_else(|| panic!("expected a terminal quota error after {payload}"));
    assert!(payload.contains("response.created"), "{payload}");
    assert!(
        !error.credential_scoped,
        "model-level cooling must not scope the error to the credential: {error:?}"
    );
    let (reason, recover) = high_recovers_at(&manager);
    assert_eq!(
        reason, "quota",
        "model-level cooling must not set credential_quota"
    );
    assert!(recover >= before + TimeDelta::hours(1), "{recover}");

    let (payload, error) = run(&manager, SIBLING).await;
    assert!(error.is_none(), "the sibling model failed: {error:?}");
    assert!(payload.contains("response.completed"), "{payload}");
    assert_eq!(attempts(&recorded), ["quota-high", "quota-high"]);
}
