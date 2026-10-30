//! Upstream gemini_missing_terminal_upstream_repro_test.go (aa4130e6),
//! extended to both Vertex sign-in paths and OpenAI Chat output.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt as _, StreamExt as _};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Dispatcher, Format};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::observe::usage::Usage;
use open_ferry_core::observe::{Observation, RequestContext};
use open_ferry_core::registry::ModelRegistry;
use serde_json::json;

use super::terminal_disconnect_tests::executor;
use super::testing::{Mock, Reply, request, stream_options};

const MODEL: &str = "gemini-3.1-pro-preview";

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    status: u16,
    terminal: bool,
    content: bool,
    calls: usize,
    cooled: usize,
    error: Option<&'static str>,
}

// An accepted generation is never resubmitted, even when it produces no public
// chunks. A rejected HTTP request still fails over and cools each attempted pair.
#[tokio::test]
async fn missing_terminal_upstream_repro() {
    let mut failures = Vec::new();
    for provider in ["gemini", "vertex_api_key", "vertex_service_account"] {
        for format in [Format::CLAUDE, Format::OPENAI] {
            for case in [
                Case {
                    name: "normal_terminal",
                    status: 200,
                    terminal: true,
                    content: true,
                    calls: 1,
                    cooled: 0,
                    error: None,
                },
                Case {
                    name: "partial_then_missing_terminal",
                    status: 200,
                    terminal: false,
                    content: true,
                    calls: 1,
                    cooled: 0,
                    error: Some("before finishReason"),
                },
                Case {
                    name: "empty_200_missing_terminal",
                    status: 200,
                    terminal: false,
                    content: false,
                    calls: 1,
                    cooled: 0,
                    error: Some("before finishReason"),
                },
                Case {
                    name: "real_http_502_control",
                    status: 502,
                    terminal: false,
                    content: false,
                    calls: 2,
                    cooled: 2,
                    error: Some("mock upstream rejection"),
                },
            ] {
                if let Err(panic) = AssertUnwindSafe(run(provider, &format, case))
                    .catch_unwind()
                    .await
                {
                    let message = panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic");
                    failures.push(format!("{provider}/{format:?}/{}: {message}", case.name));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "independent missing-terminal cases failed: {failures:#?}"
    );
}

async fn run(provider: &str, format: &Format, case: Case) {
    let reply = if case.status != 200 {
        Reply::error(
            case.status,
            r#"{"error":{"code":502,"message":"mock upstream rejection","status":"UNAVAILABLE"}}"#,
        )
    } else if case.content {
        let mut content = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"partial answer"}]}}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"totalTokenCount":13}});
        if case.terminal {
            content["candidates"][0]["finishReason"] = "STOP".into();
            content["candidates"][0]["content"]["parts"][0]["text"] = "complete answer".into();
        }
        Reply::sse(&format!("data: {content}\n\n"))
    } else {
        Reply::sse("")
    };
    let mock = Mock::answering(move |seen| {
        if seen.path == "/token" {
            Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
        } else {
            reply.clone()
        }
    })
    .await;
    let (executor, auth) = executor(provider, &mock);
    let models = Arc::new(ModelRegistry::new());
    let manager = Manager::new(
        Settings {
            request_retry: 0,
            max_retry_interval: Duration::ZERO,
            ..Default::default()
        },
        models.clone(),
        None,
    );
    let provider_name = executor.id().to_owned();
    manager.register_executor(Arc::from(executor));
    for id in ["a", "b"] {
        let mut auth = (*auth).clone();
        auth.id = id.into();
        models.register_client(
            id,
            &provider_name,
            &[ModelInfo {
                id: MODEL.into(),
                ..Default::default()
            }],
        );
        manager.register(auth).unwrap();
    }
    let request = request(
        MODEL,
        r#"{"model":"gemini-3.1-pro-preview","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"Say hello"}]}"#,
    );
    let mut options = stream_options(format);
    options.original_request = request.payload.clone();
    let mut config = Config::default();
    config.usage_statistics_enabled = true;
    let usage = Usage::new(&config);
    let (records, _subscription) = usage.observe();
    let context = Arc::new(RequestContext::new(http::Method::POST, "/test".into()));
    let tap = usage.tap(&context, &request, &options).unwrap();
    options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
    let error = tokio::time::timeout(Duration::from_secs(5), async {
        match manager
            .execute_stream(std::slice::from_ref(&provider_name), request, options)
            .await
        {
            Err(error) => Some(error),
            Ok(mut response) => {
                let mut error = None;
                while let Some(chunk) = response.chunks.next().await {
                    if let Err(failure) = chunk {
                        assert!(error.replace(failure).is_none());
                    }
                }
                error
            }
        }
    })
    .await
    .expect("mock stream must terminate");
    let calls = mock
        .requests()
        .into_iter()
        .filter(|seen| seen.path != "/token")
        .collect::<Vec<_>>();
    assert!(
        calls
            .iter()
            .all(|seen| seen.path.ends_with(":streamGenerateContent"))
    );
    let cooled = ["a", "b"]
        .into_iter()
        .filter(|id| {
            manager
                .get(id)
                .unwrap()
                .model_states
                .get(MODEL)
                .is_some_and(|state| {
                    state.unavailable
                        && state
                            .next_retry_after
                            .is_some_and(|until| until > chrono::Utc::now())
                })
        })
        .count();
    // These assertions precede error classification so removing the typed
    // truncation error must demonstrate actual replay/cooldown policy failure.
    assert_eq!(calls.len(), case.calls, "generation POST count");
    assert_eq!(cooled, case.cooled, "cooled credential/model pairs");
    assert_eq!(error.is_some(), case.error.is_some());
    if let Some(text) = case.error {
        let error = error.unwrap();
        assert_eq!(error.http_status(), 502);
        assert!(
            error.to_string().contains(text),
            "unexpected mock error: {error}"
        );
        if case.status == 200 {
            assert_eq!(error.failure_code(), Some("stream_truncated"));
            assert!(error.stops_replay() && error.keeps_usage);
        } else {
            assert_eq!(error.failure_code(), None);
            assert!(!error.stops_replay());
        }
    }
    let records: Vec<_> = records.try_iter().collect();
    assert_eq!(
        records.len(),
        case.calls,
        "one usage record per upstream attempt"
    );
    for record in records {
        assert_eq!(record.failed, case.error.is_some());
        assert_eq!(record.status, if case.error.is_some() { 502 } else { 200 });
        assert_eq!(record.total_tokens, if case.content { 13 } else { 0 });
        assert_eq!(
            record.tokens.input.total_tokens,
            if case.content { 10 } else { 0 }
        );
        assert_eq!(
            record.tokens.output.total_tokens,
            if case.content { 3 } else { 0 }
        );
    }
}
