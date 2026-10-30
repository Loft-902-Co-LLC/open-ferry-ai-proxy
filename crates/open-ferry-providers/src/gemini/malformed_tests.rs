// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor_test.go
// and gemini_vertex_executor_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Malformed calls fail all Gemini/Vertex Claude paths. The call's taps record
//! failure as soon as the error event is emitted, including on stream drop.
//! Usage and conductor integration are owned by the manager in this port;
//! these tests check what reaches its taps and the terminal stream error.

use super::testing::{Mock, Reply, collect, key_auth, request, service_account_auth};
use super::{GeminiExecutor, VertexExecutor};
use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::CallReport;
use open_ferry_core::observe::usage::Usage;
use open_ferry_core::observe::{Observation, RequestContext, Tap};
use serde_json::Value;
use std::sync::{Arc, Mutex};

const MALFORMED: &str = r#"{"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#;
const USAGE: &str = r#"{"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":10,"thoughtsTokenCount":313,"totalTokenCount":373}}"#;
const PAYLOAD: &str = r#"{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}"#;
const PROVIDERS: [&str; 3] = ["gemini", "vertex_api_key", "vertex_service_account"];

fn setup(provider: &str, mock: &Mock) -> (Box<dyn ProviderExecutor>, Arc<Auth>) {
    match provider {
        "gemini" => (
            Box::new(GeminiExecutor::new("direct")),
            key_auth("gemini", "test-key", &mock.url),
        ),
        "vertex_api_key" => (
            Box::new(VertexExecutor::new("direct")),
            key_auth("vertex", "test-key", &mock.url),
        ),
        "vertex_service_account" => (
            Box::new(VertexExecutor::new("direct").with_service_account_base_url(mock.url.clone())),
            Arc::new(service_account_auth(
                &format!("{}/token", mock.url),
                "global",
            )),
        ),
        _ => unreachable!(),
    }
}
async fn server(reply: Reply) -> Mock {
    Mock::answering(move |seen| {
        if seen.path == "/token" {
            Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
        } else {
            reply.clone()
        }
    })
    .await
}

// Ported from TestGeminiExecutorClaudeMalformedFunctionCallReturnsRetryableError
// and TestGeminiVertexExecutorClaudeMalformedFunctionCallReturnsRetryableError.
#[tokio::test]
async fn malformed_function_call_returns_retryable_error() {
    for provider in PROVIDERS {
        for streaming in [false, true] {
            let reply = if streaming {
                Reply::sse(&format!("data: {MALFORMED}\n\ndata: {USAGE}\n\n"))
            } else {
                Reply::json(MALFORMED)
            };
            let mock = server(reply).await;
            let (executor, auth) = setup(provider, &mock);
            let mut options = Options::new(Format::CLAUDE);
            options.stream = streaming;
            let request = request("gemini-2.5-flash", PAYLOAD);
            let error = if streaming {
                let (chunks, error) = collect(
                    executor
                        .execute_stream(auth, request, options)
                        .await
                        .unwrap(),
                )
                .await;
                let output = chunks.concat();
                assert_eq!(
                    output.matches("event: error\n").count(),
                    1,
                    "{provider}: {output}"
                );
                assert!(!output.contains("event: message_stop"));
                error.unwrap()
            } else {
                executor
                    .execute(auth, request, options)
                    .await
                    .err()
                    .unwrap()
            };
            assert_eq!(error.status, 502, "{provider}: {error:?}");
            assert!(error.to_string().contains("MALFORMED_FUNCTION_CALL"));
            assert!(error.keeps_usage, "{provider}");
        }
    }
}

#[derive(Default)]
struct Recorder {
    failed: Mutex<bool>,
    chunks: Mutex<Vec<u8>>,
}
impl Tap for Recorder {
    fn attempt_error(&self, message: &str) {
        if message.contains("MALFORMED_FUNCTION_CALL") {
            *self.failed.lock().unwrap() = true;
        }
    }
    fn chunk(&self, chunk: &Bytes) {
        self.chunks.lock().unwrap().extend_from_slice(chunk);
    }
}

// Ported from TestGeminiExecutorClaudeMalformedFunctionCallSplitUsageAndConductorFailure,
// TestGeminiExecutorClaudeMalformedFunctionCallCancelAfterErrorPreservesUsageAndConductorFailure,
// TestGeminiVertexExecutorClaudeMalformedFunctionCallRecordsFailureUsage and
// TestGeminiVertexExecutorClaudeMalformedFunctionCallCancelAfterErrorRecordsFailureUsage.
#[tokio::test]
async fn malformed_stream_retains_failure_and_split_usage_even_on_drop() {
    for provider in PROVIDERS {
        for (cancel, split) in [(false, true), (true, false), (true, true)] {
            let body = if split {
                format!("data: {MALFORMED}\n\ndata: {USAGE}\n\n")
            } else {
                let mut malformed: Value = serde_json::from_str(MALFORMED).unwrap();
                malformed["usageMetadata"] =
                    serde_json::from_str::<Value>(USAGE).unwrap()["usageMetadata"].clone();
                format!("data: {malformed}\n\n")
            };
            let mock = server(Reply::sse(&body).hold_open()).await;
            let (executor, auth) = setup(provider, &mock);
            let recorder = Arc::new(Recorder::default());
            let mut options = Options::new(Format::CLAUDE);
            options.stream = true;
            let request = request("gemini-2.5-flash", PAYLOAD);
            let mut config = Config::default();
            config.usage_statistics_enabled = true;
            let usage = Usage::new(&config);
            let (records, _observation) = usage.observe();
            let context = Arc::new(RequestContext::new(
                http::Method::POST,
                "/v1/messages".into(),
            ));
            let tap = usage.tap(&context, &request, &options).unwrap();
            options.observation = Some(Arc::new(Observation::new(
                context,
                vec![recorder.clone(), tap],
            )));
            let report = CallReport::start(&options);
            let mut response = report
                .stream(executor.execute_stream(auth, request, options).await)
                .unwrap();
            if cancel {
                loop {
                    let chunk = response.chunks.next().await.unwrap().unwrap();
                    if String::from_utf8_lossy(&chunk).contains("event: error") {
                        break;
                    }
                }
                drop(response);
            } else {
                let (_, error) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), collect(response))
                        .await
                        .unwrap();
                assert_eq!(error.unwrap().status, 502);
                assert!(String::from_utf8_lossy(&recorder.chunks.lock().unwrap()).contains("373"));
            }
            assert!(
                *recorder.failed.lock().unwrap(),
                "{provider}/cancel={cancel}"
            );
            let event = records.try_recv().unwrap();
            assert!(event.failed, "{provider}/cancel={cancel}: {event:?}");
            assert_eq!(event.status, 502);
            assert_eq!(event.total_tokens, 373);
        }
    }
}

// Test{Gemini,GeminiVertex}ExecutorClaudeStreamTruncationReportsError and
// TestGeminiAndVertexClaudeStreamWithFinishSignalCompletes.
#[tokio::test]
async fn claude_stream_requires_gemini_finish_signal() {
    for provider in PROVIDERS {
        for (tail, fails) in [
            ("", true),
            (r#"{"candidates":[{"finishReason":"STOP"}]}"#, false),
            (r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#, true),
        ] {
            let body = format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"hi\"}}]}}}}]}}\n\ndata: {USAGE}\n\ndata: {tail}\n\n"
            );
            let mock = server(Reply::sse(&body)).await;
            let (executor, auth) = setup(provider, &mock);
            let options = Options {
                stream: true,
                ..Options::new(Format::CLAUDE)
            };
            let response = executor
                .execute_stream(auth, request("gemini-2.5-flash", PAYLOAD), options)
                .await
                .unwrap();
            let (chunks, error) = collect(response).await;
            assert_eq!(
                error.is_some(),
                fails,
                "{provider} {tail}: {chunks:?} {error:?}"
            );
            if let Some(error) = error {
                assert_eq!(error.status, 502);
                assert!(error.keeps_usage);
            }
        }
    }
}
