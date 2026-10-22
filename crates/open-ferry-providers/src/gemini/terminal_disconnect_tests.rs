// Ported from CLIProxyAPI internal/runtime/executor/
// gemini_executor_terminal_disconnect_test.go
// (TestGeminiStream_TerminalDisconnectUsage) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Usage outcomes through the real translator and the manager's report.
//! Extended to Vertex API keys and service accounts, early drops with a
//! terminal still queued, and Interactions plus the four other client formats. The existing
//! mock cuts off the connection instead of promising an oversized body;
//! either produces a genuine read error, independent of cancellation.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt as _, StreamExt as _};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::usage::Usage;
use open_ferry_core::observe::{CallReport, Observation, RequestContext};
use serde_json::json;

use super::testing::{
    Mock, PATCH_REQUEST, PATCH_RESPONSE, Reply, key_auth, request, service_account_auth,
    stream_options,
};
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
    let mut failures = Vec::new();
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
                check(
                    &mut failures,
                    provider,
                    model,
                    &Format::OPENAI_RESPONSE,
                    case,
                )
                .await;
            }
        }
        for format in [
            Format::GEMINI,
            Format::OPENAI,
            Format::CLAUDE,
            Format::INTERACTIONS,
        ] {
            // Each translator has a different terminal syntax; the completed
            // chunk is observed only when it leaves the reported stream.
            let cancel_on = match format.as_str() {
                "gemini" => "finishReason",
                "openai" => "finish_reason\":\"stop",
                "interactions" => "interaction.completed",
                _ => "message_stop",
            };
            check(
                &mut failures,
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
        check(
            &mut failures,
            provider,
            "gemini-2.5-flash",
            &Format::INTERACTIONS,
            Case {
                name: "interactions_before_terminal_cancel",
                cancel_on: "interaction.created",
                failed: true,
                ..completed
            },
        )
        .await;
        check(
            &mut failures,
            provider,
            "gemini-2.5-flash",
            &Format::GEMINI,
            Case {
                name: "cancel_on_usage_after_terminal",
                split_usage: true,
                cancel_on: "promptTokenCount",
                ..completed
            },
        )
        .await;
    }
    assert!(
        failures.is_empty(),
        "{} failing rows:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

async fn check(
    failures: &mut Vec<String>,
    provider: &str,
    model: &str,
    format: &Format,
    case: Case,
) {
    if let Err(panic) = AssertUnwindSafe(run(provider, model, format, case))
        .catch_unwind()
        .await
    {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        failures.push(format!(
            "{provider}/{model}/{format:?}/{}: {message}",
            case.name
        ));
    }
}

async fn run(provider: &str, model: &str, format: &Format, case: Case) {
    let label = format!("{provider}/{model}/{format:?}/{}", case.name);
    let mut reply = Reply::sse(&case.body());
    // Claude needs EOF for message_stop. Vertex's Gemini-to-Interactions
    // bridge passes raw SSE lines to a JSON-only translator, so its terminal
    // likewise comes only from the final [DONE]. Test that existing behavior
    // by allowing EOF and dropping before polling the translated stream end.
    let needs_eof =
        *format == Format::CLAUDE || (*format == Format::INTERACTIONS && provider != "gemini");
    if !case.cancel_on.is_empty() && !needs_eof {
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
    let (executor, auth) = executor(provider, &mock);
    let payload = match format.as_str() {
        "gemini" => r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
        "openai" | "claude" => {
            r#"{"messages":[{"role":"user","content":"hello"}],"max_tokens":32}"#
        }
        "interactions" => r#"{"input":"Reply only OK.","stream":true}"#,
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

fn executor(provider: &str, mock: &Mock) -> (Box<dyn ProviderExecutor>, Arc<Auth>) {
    match provider {
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
    }
}

// Native Vertex frames carry their event name separately from the data. Both
// credential kinds must distinguish an early drop from a returned completion.
#[tokio::test]
async fn vertex_native_interactions_terminal_disconnect_usage() {
    for provider in ["vertex_api_key", "vertex_service_account"] {
        for kind in [
            "event_type",
            "type",
            "sse_only",
            "legacy_finish",
            "interaction.failed",
            "response.failed",
            "error",
        ] {
            for before_terminal in [false, true] {
                let failed = before_terminal
                    || matches!(kind, "interaction.failed" | "response.failed" | "error");
                let name = if kind == "legacy_finish" {
                    "finish"
                } else if matches!(kind, "interaction.failed" | "response.failed" | "error") {
                    kind
                } else {
                    "interaction.completed"
                };
                let usage =
                    json!({"total_input_tokens":11,"total_output_tokens":4,"total_tokens":15});
                let mut data =
                    json!({"interaction":{"id":"native", "status":"completed", "usage":usage}});
                if matches!(kind, "event_type" | "type") {
                    data[kind] = name.into();
                }
                let frames = format!(
                    "event: interaction.created\ndata: {{\"event_type\":\"interaction.created\",\"interaction\":{{\"id\":\"native\",\"status\":\"in_progress\",\"usage\":{usage}}}}}\n\nevent: {name}\ndata: {data}\n\n"
                );
                let mock = Mock::answering(move |seen| {
                    if seen.path == "/token" {
                        Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
                    } else {
                        Reply::sse(&frames).hold_open()
                    }
                })
                .await;
                let (executor, auth) = executor(provider, &mock);
                let mut auth = Auth::clone(&auth);
                auth.metadata.insert("interactions".into(), true.into());
                let request = request("gemini-2.5-flash", r#"{"input":"hello","stream":true}"#);
                let mut options = stream_options(&Format::INTERACTIONS);
                let mut config = Config::default();
                config.usage_statistics_enabled = true;
                let usage = Usage::new(&config);
                let (records, _subscription) = usage.observe();
                let context = Arc::new(RequestContext::new(http::Method::POST, "/test".into()));
                let tap = usage.tap(&context, &request, &options).unwrap();
                options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
                let report = CallReport::start(&options);
                let mut response = report
                    .stream(
                        executor
                            .execute_stream(Arc::new(auth), request, options)
                            .await,
                    )
                    .unwrap();
                let label = format!("{provider}/{kind}/before_terminal={before_terminal}");
                tokio::time::timeout(Duration::from_secs(5), async {
                    let initial = response.chunks.next().await.unwrap().unwrap();
                    assert!(
                        String::from_utf8_lossy(&initial).contains("interaction.created"),
                        "{label}"
                    );
                    if !before_terminal {
                        let terminal = response.chunks.next().await.unwrap().unwrap();
                        assert!(
                            String::from_utf8_lossy(&terminal)
                                .contains(&format!("event: {name}\n")),
                            "{label}"
                        );
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{label}: no native frame"));
                drop(response);
                let event = records.try_recv().unwrap();
                assert_eq!(event.failed, failed, "{label}: {event:?}");
                assert_eq!(event.status, if failed { 499 } else { 200 }, "{label}");
                // Existing Mode::GeminiStream reads Gemini usageMetadata only;
                // native Interactions usage parsing is a separate pre-existing gap.
                assert_eq!(event.total_tokens, 0, "{label}");
                assert!(records.try_recv().is_err(), "{label}: duplicate record");
                assert!(mock.last().path.ends_with("/interactions"), "{label}");
            }
        }
    }
}

// A queued apply_patch failure wins over drop, even immediately after its
// response.failed frame is returned and while upstream remains open.
#[tokio::test]
async fn response_failed_then_drop_is_failure() {
    for provider in PROVIDERS {
        let mut chunk: serde_json::Value = serde_json::from_str(PATCH_RESPONSE).unwrap();
        chunk["candidates"][0]["content"]["parts"][0]["functionCall"]["args"]["input"] = json!(7);
        let frames = format!("data: {chunk}\n\n");
        let mock = Mock::answering(move |seen| {
            if seen.path == "/token" {
                Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
            } else {
                Reply::sse(&frames).hold_open()
            }
        })
        .await;
        let (executor, auth) = executor(provider, &mock);
        let request = request("gemini-2.5-flash", PATCH_REQUEST);
        let mut options = Options {
            original_request: request.payload.clone(),
            ..stream_options(&Format::OPENAI_RESPONSE)
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
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let chunk = response
                    .chunks
                    .next()
                    .await
                    .expect("response.failed must be returned before the error")
                    .unwrap();
                if String::from_utf8_lossy(&chunk).contains("event: response.failed\n") {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{provider}: no response.failed frame"));
        drop(response);
        let event = records.try_recv().unwrap();
        assert!(event.failed, "{provider}: {event:?}");
        assert_eq!(event.status, 502, "{provider}");
        assert!(records.try_recv().is_err(), "{provider}: duplicate record");
    }
}
