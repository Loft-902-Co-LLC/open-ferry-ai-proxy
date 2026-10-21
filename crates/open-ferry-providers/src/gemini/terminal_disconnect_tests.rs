// Ported from CLIProxyAPI internal/runtime/executor/
// gemini_executor_terminal_disconnect_test.go
// (TestGeminiStream_TerminalDisconnectUsage) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Usage outcomes through the real translator and the manager's report.
//! Extended to Vertex API keys and service accounts, early drops with a
//! terminal still queued, and all four served client formats. The existing
//! mock cuts off the connection instead of promising an oversized body;
//! either produces a genuine read error, independent of cancellation.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::usage::Usage;
use open_ferry_core::observe::{CallReport, Observation, RequestContext};
use serde_json::json;

use super::testing::{Mock, Reply, key_auth, request, service_account_auth, stream_options};
use super::{GeminiExecutor, VertexExecutor};

const PROVIDERS: [&str; 3] = ["gemini", "vertex_api_key", "vertex_service_account"];

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    finish: &'static str,
    cancel_on: &'static str,
    split_usage: bool,
    read_error: bool,
    failed: bool,
}

impl Case {
    fn body(self) -> String {
        let usage = json!({"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15});
        let mut chunk = json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"OK"}]}}],
            "traceId": format!("terminal-disconnect-{}", self.name),
        });
        if !self.finish.is_empty() {
            chunk["candidates"][0]["finishReason"] = self.finish.into();
        }
        if !self.split_usage {
            chunk["usageMetadata"] = usage.clone();
        }
        let mut body = format!("data: {chunk}\n\n");
        if self.split_usage {
            let tail = json!({"usageMetadata":usage,"traceId":chunk["traceId"]});
            body.push_str(&format!("data: {tail}\n\n"));
        }
        body
    }
}

// TestGeminiStream_TerminalDisconnectUsage, extended to both Vertex paths.
#[tokio::test]
async fn terminal_disconnect_usage() {
    let completed = Case {
        name: "completed_cancel",
        finish: "STOP",
        cancel_on: "response.completed",
        split_usage: false,
        read_error: false,
        failed: false,
    };
    for provider in PROVIDERS {
        for model in ["claude-opus-4-6", "gemini-3.8-flash-high"] {
            for case in [
                completed,
                Case {
                    name: "incomplete_cancel",
                    finish: "MAX_TOKENS",
                    cancel_on: "response.incomplete",
                    ..completed
                },
                Case {
                    name: "usage_tail_cancel",
                    split_usage: true,
                    ..completed
                },
                Case {
                    name: "completed_eof",
                    cancel_on: "",
                    ..completed
                },
                Case {
                    name: "usage_tail_eof",
                    cancel_on: "",
                    split_usage: true,
                    ..completed
                },
                Case {
                    name: "before_completion_cancel",
                    finish: "",
                    cancel_on: "response.output_text.delta",
                    failed: true,
                    ..completed
                },
                // Raw STOP has been read, but its translated terminal is still
                // queued. Drop's ready-item drain must not count as delivery.
                Case {
                    name: "before_queued_terminal_cancel",
                    cancel_on: "response.output_text.delta",
                    failed: true,
                    ..completed
                },
                Case {
                    name: "before_usage_tail_cancel",
                    cancel_on: "response.output_text.delta",
                    split_usage: true,
                    failed: true,
                    ..completed
                },
                Case {
                    name: "after_completion_read_error",
                    cancel_on: "",
                    read_error: true,
                    failed: true,
                    ..completed
                },
                Case {
                    name: "before_completion_read_error",
                    finish: "",
                    cancel_on: "",
                    read_error: true,
                    failed: true,
                    ..completed
                },
            ] {
                run(provider, model, &Format::OPENAI_RESPONSE, case).await;
            }
        }
        for format in [Format::GEMINI, Format::OPENAI, Format::CLAUDE] {
            // Each translator has a different terminal syntax; the completed
            // chunk is observed only when it leaves the reported stream.
            let cancel_on = match format.as_str() {
                "gemini" => "finishReason",
                "openai" => "finish_reason\":\"stop",
                _ => "message_stop",
            };
            run(
                provider,
                "gemini-2.5-flash",
                &format,
                Case {
                    cancel_on,
                    ..completed
                },
            )
            .await;
        }
    }
}

async fn run(provider: &str, model: &str, format: &Format, case: Case) {
    let label = format!("{provider}/{model}/{format:?}/{}", case.name);
    let mut reply = Reply::sse(&case.body());
    // Claude emits message_stop only when EOF finalizes its translator;
    // drop after that returned chunk, before polling the stream's end.
    if !case.cancel_on.is_empty() && *format != Format::CLAUDE {
        reply = reply.hold_open();
    }
    if case.read_error {
        reply = reply.cut_off();
    }
    let mock = Mock::answering(move |seen| {
        if seen.path == "/token" {
            Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
        } else {
            reply.clone()
        }
    })
    .await;
    let (executor, auth): (Box<dyn ProviderExecutor>, Arc<Auth>) = match provider {
        "gemini" => (
            Box::new(GeminiExecutor::new("direct")),
            key_auth("gemini", "test-key", &mock.url),
        ),
        "vertex_api_key" => (
            Box::new(VertexExecutor::new("direct")),
            key_auth("vertex", "test-key", &mock.url),
        ),
        _ => (
            Box::new(VertexExecutor::new("direct").with_service_account_base_url(mock.url.clone())),
            Arc::new(service_account_auth(
                &format!("{}/token", mock.url),
                "global",
            )),
        ),
    };
    let payload = match format.as_str() {
        "gemini" => r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
        "openai" | "claude" => {
            r#"{"messages":[{"role":"user","content":"hello"}],"max_tokens":32}"#
        }
        _ => r#"{"input":"Reply only OK.","stream":true,"max_output_tokens":32}"#,
    };
    let request = request(model, payload);
    let mut options = Options {
        original_request: request.payload.clone(),
        ..stream_options(format)
    };
    let mut config = Config::default();
    config.usage_statistics_enabled = true;
    let usage = Usage::new(&config);
    let (records, _subscription) = usage.observe();
    let context = Arc::new(RequestContext::new(http::Method::POST, "/test".into()));
    let tap = usage.tap(&context, &request, &options).unwrap();
    options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
    let report = CallReport::start(&options);
    let mut response = report
        .stream(executor.execute_stream(auth, request, options).await)
        .unwrap();
    let mut canceled = false;
    let mut terminal_received = false;
    let mut stream_error = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = response.chunks.next().await {
            match chunk {
                Ok(chunk) => {
                    let text = String::from_utf8_lossy(&chunk);
                    terminal_received |= text.contains("event: response.completed\n")
                        || text.contains("event: response.incomplete\n");
                    if !case.cancel_on.is_empty() && text.contains(case.cancel_on) {
                        canceled = true;
                        break;
                    }
                }
                Err(error) => stream_error = Some(error),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}: stream did not finish"));
    drop(response);
    assert_eq!(canceled, !case.cancel_on.is_empty(), "{label}");
    if *format == Format::OPENAI_RESPONSE && !case.finish.is_empty() && (!canceled || !case.failed)
    {
        assert!(terminal_received, "{label}: no terminal event received");
    }
    assert_eq!(
        stream_error.is_some(),
        case.read_error,
        "{label}: {stream_error:?}"
    );
    let event = records
        .try_recv()
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    assert_eq!(event.failed, case.failed, "{label}: {event:?}");
    let status = if case.read_error {
        assert!(stream_error.as_ref().unwrap().keeps_usage, "{label}");
        500
    } else if case.failed {
        499
    } else {
        200
    };
    assert_eq!(event.status, status, "{label}");
    assert_eq!(event.tokens.input.total_tokens, 11, "{label}");
    assert_eq!(event.tokens.output.total_tokens, 4, "{label}");
    assert_eq!(event.total_tokens, 15, "{label}");
    assert!(
        records.try_recv().is_err(),
        "{label}: duplicate usage record"
    );
}
