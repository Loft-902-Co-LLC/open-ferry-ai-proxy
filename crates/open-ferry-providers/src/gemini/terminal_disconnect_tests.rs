// Ported from CLIProxyAPI internal/runtime/executor/
// gemini_executor_terminal_disconnect_test.go
// (TestGeminiStream_TerminalDisconnectUsage) (v8.0.42, MIT).
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
    no_content: bool,
}

impl Case {
    fn body(self) -> String {
        let usage = json!({"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15});
        let mut chunk = json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"OK"}]}}],
            "traceId": format!("terminal-disconnect-{}", self.name),
        });
        if self.no_content {
            chunk["candidates"][0]
                .as_object_mut()
                .unwrap()
                .remove("content");
        }
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
        no_content: false,
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

#[tokio::test]
async fn no_content_terminal_delivery_and_disconnect_usage() {
    for provider in PROVIDERS {
        for finish in [
            "STOP",
            "MAX_TOKENS",
            "SAFETY",
            "RECITATION",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "SPII",
            "IMAGE_SAFETY",
            "MALFORMED_RESPONSE",
            "MALFORMED_FUNCTION_CALL",
        ] {
            let case = Case {
                name: "no_content_eof",
                finish,
                cancel_on: "",
                split_usage: true,
                read_error: false,
                failed: false,
                no_content: true,
            };
            run(provider, "gemini-2.5-flash", &Format::GEMINI, case).await;
            run(provider, "gemini-2.5-flash", &Format::OPENAI_RESPONSE, case).await;
            run(
                provider,
                "gemini-2.5-flash",
                &Format::GEMINI,
                Case {
                    name: "no_content_terminal_cancel",
                    cancel_on: "finishReason",
                    split_usage: false,
                    ..case
                },
            )
            .await;
        }
    }
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
    // Claude needs EOF for message_stop; Interactions completes on finish plus usage.
    let needs_eof = *format == Format::CLAUDE;
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
                let final_usage =
                    json!({"total_input_tokens":21,"total_output_tokens":8,"total_tokens":29});
                let mut data = json!({"interaction":{"id":"native", "status":"completed", "usage":final_usage}});
                if matches!(kind, "event_type" | "type") {
                    data[kind] = name.into();
                }
                let mut frames = format!(
                    "event: interaction.created\ndata: {{\"event_type\":\"interaction.created\",\"interaction\":{{\"id\":\"native\",\"status\":\"in_progress\",\"usage\":{usage}}}}}\n\nevent: {name}\ndata: {data}\n\n"
                );
                if before_terminal {
                    // No terminal or later counts have been read on this connection.
                    frames.truncate(frames.find("\n\n").unwrap() + 2);
                }
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
                assert_eq!(event.provider, "vertex", "{label}");
                assert_eq!(
                    event.tokens.input.total_tokens,
                    if before_terminal { 11 } else { 21 },
                    "{label}"
                );
                assert_eq!(
                    event.tokens.output.total_tokens,
                    if before_terminal { 4 } else { 8 },
                    "{label}"
                );
                assert_eq!(
                    event.total_tokens,
                    if before_terminal { 15 } else { 29 },
                    "{label}"
                );
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

#[tokio::test]
async fn vertex_bridge_interactions_stream_content_and_usage() {
    vertex_interactions_content_and_usage(false, true).await;
}

#[tokio::test]
async fn vertex_bridge_interactions_execute_content_and_usage() {
    vertex_interactions_content_and_usage(false, false).await;
}

#[tokio::test]
async fn vertex_native_interactions_stream_usage() {
    vertex_interactions_content_and_usage(true, true).await;
}

#[tokio::test]
async fn vertex_native_interactions_execute_usage() {
    vertex_interactions_content_and_usage(true, false).await;
}

async fn vertex_interactions_content_and_usage(native: bool, stream: bool) {
    for provider in ["vertex_api_key", "vertex_service_account"] {
        let gemini = json!({
            "responseId":"bridge_content",
            "candidates":[{"content":{"role":"model","parts":[
                {"text":"bridge text"},
                {"thought":true,"text":"bridge thought"},
                {"functionCall":{"id":"call_weather","name":"weather","args":{"city":"Chicago"}}}
            ]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15}
        });
        let native_body = json!({"id":"native_counts","status":"completed",
            "usage":{"total_input_tokens":11,"total_output_tokens":4,"total_tokens":15}});
        let body = if native { native_body } else { gemini };
        let reply = if stream {
            if native {
                Reply::sse(&format!(
                    "event: interaction.completed\ndata: {{\"event_type\":\"interaction.completed\",\"interaction\":{body}}}\n\n"
                ))
            } else {
                // Completion must arrive without EOF, including all translated parts.
                Reply::sse(&format!("event: message\ndata: {body}\n\n")).hold_open()
            }
        } else {
            Reply::json(&body.to_string())
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
        let mut auth = Auth::clone(&auth);
        if native {
            auth.metadata.insert("interactions".into(), true.into());
        }
        let request = request("gemini-2.5-flash", r#"{"input":"hello"}"#);
        let mut options = stream_options(&Format::INTERACTIONS);
        options.original_request = request.payload.clone();
        let mut config = Config::default();
        config.usage_statistics_enabled = true;
        let usage = Usage::new(&config);
        let (records, _subscription) = usage.observe();
        let context = Arc::new(RequestContext::new(http::Method::POST, "/test".into()));
        let tap = usage.tap(&context, &request, &options).unwrap();
        options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
        let report = CallReport::start(&options);
        let mut events = Vec::new();
        let response_body = if stream {
            let mut response = report
                .stream(
                    executor
                        .execute_stream(Arc::new(auth), request, options)
                        .await,
                )
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(chunk) = response.chunks.next().await {
                    let chunk = chunk.unwrap();
                    let text = std::str::from_utf8(&chunk).unwrap();
                    for line in text.lines().filter_map(|line| line.strip_prefix("data: ")) {
                        if let Ok(event) = serde_json::from_str::<serde_json::Value>(line) {
                            events.push(event);
                        }
                    }
                    if !native && text.contains("event: interaction.completed\n") {
                        break;
                    }
                }
            })
            .await
            .expect("completion must arrive before EOF for the held-open bridge");
            drop(response);
            events
                .iter()
                .find(|event| event["event_type"] == "interaction.completed")
                .unwrap()["interaction"]
                .clone()
        } else {
            let result = executor.execute(Arc::new(auth), request, options).await;
            report.finish(&result);
            serde_json::from_slice::<serde_json::Value>(&result.unwrap().payload).unwrap()
        };
        assert_eq!(response_body["usage"]["total_tokens"], 15);
        if !native && stream {
            assert!(
                events
                    .iter()
                    .any(|event| event["delta"]["text"] == "bridge text")
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["delta"]["type"] == "thought_summary"
                        && event["delta"]["content"]["text"] == "bridge thought")
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["step"]["type"] == "function_call"
                        && event["step"]["name"] == "weather"
                        && event["step"]["id"] == "call_weather")
            );
            let args = events
                .iter()
                .find_map(|event| event["delta"]["arguments"].as_str())
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(args).unwrap(),
                json!({"city":"Chicago"})
            );
            assert_eq!(response_body["usage"]["total_input_tokens"], 11);
            assert_eq!(response_body["usage"]["total_output_tokens"], 4);
        } else if !native {
            assert_eq!(
                response_body["steps"][0]["content"][0]["text"],
                "bridge text"
            );
            assert_eq!(
                response_body["steps"][1]["content"][0]["text"],
                "bridge thought"
            );
            assert_eq!(response_body["steps"][2]["name"], "weather");
            assert_eq!(
                response_body["steps"][2]["arguments"],
                json!({"city":"Chicago"})
            );
        }
        let event = records.try_recv().unwrap();
        assert_eq!(event.provider, "vertex");
        assert!(
            !event.failed,
            "{provider}/native={native}/stream={stream}: {event:?}"
        );
        assert_eq!(event.status, 200);
        assert_eq!(event.tokens.input.total_tokens, 11);
        assert_eq!(event.tokens.output.total_tokens, 4);
        assert_eq!(event.total_tokens, 15);
        assert!(records.try_recv().is_err());
        assert_eq!(mock.last().path.contains("/interactions"), native);
    }
}

// Accepted HTTP 200 without a terminal must retain usage without cooling or
// replaying either credential, including with an explicit 502 retry rule.
#[tokio::test]
async fn accepted_truncation_is_one_attempt_without_cooldown_and_keeps_usage() {
    use open_ferry_core::exec::Dispatcher;
    use open_ferry_core::manager::{Manager, Settings};
    use open_ferry_core::models::ModelInfo;
    use open_ferry_core::registry::ModelRegistry;
    for provider in PROVIDERS {
        for format in [Format::OPENAI, Format::CLAUDE] {
            let reply = Reply::sse(
                "data: {\"service_tier\":\"flex\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}],\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":4,\"totalTokenCount\":15}}\n\n",
            );
            let mock = Mock::answering(move |seen| {
                if seen.path == "/token" {
                    Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
                } else {
                    reply.clone()
                }
            })
            .await;
            let (exec, auth) = executor(provider, &mock);
            let registry = Arc::new(ModelRegistry::new());
            let manager = Manager::new(
                Settings {
                    request_retry: 2,
                    ..Default::default()
                },
                registry.clone(),
                None,
            );
            let provider_name = exec.id().to_owned();
            manager.register_executor(Arc::from(exec));
            for id in ["a", "b"] {
                let mut candidate = (*auth).clone();
                candidate.id = id.into();
                candidate.metadata.insert("request_scoped_errors".into(), json!([{"status":502,"match":["terminal","finishReason"],"action":"continue-and-cooldown"}]));
                registry.register_client(
                    id,
                    &provider_name,
                    &[ModelInfo {
                        id: "gemini-2.5-flash".into(),
                        ..Default::default()
                    }],
                );
                manager.register(candidate).unwrap();
            }
            let request = request(
                "gemini-2.5-flash",
                r#"{"messages":[{"role":"user","content":"hello"}],"max_tokens":32,"service_tier":"priority"}"#,
            );
            let mut options = stream_options(&format);
            options.original_request = request.payload.clone();
            let mut config = Config::default();
            config.usage_statistics_enabled = true;
            let usage = Usage::new(&config);
            open_ferry_core::observe::usage::reconfigure(&usage, None, &config, true);
            let (records, _subscription) = usage.observe();
            let context = Arc::new(RequestContext::new(http::Method::POST, "/test".into()));
            let tap = usage.tap(&context, &request, &options).unwrap();
            options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
            let result = manager
                .execute_stream(std::slice::from_ref(&provider_name), request, options)
                .await;
            let mut response = result.unwrap();
            let mut error = None;
            while let Some(chunk) = response.chunks.next().await {
                if let Err(failure) = chunk {
                    assert!(error.replace(failure).is_none());
                }
            }
            drop(response);
            let error = error.expect("missing terminal must fail");
            assert_eq!(error.http_status(), 502);
            assert_eq!(error.failure_code(), Some("stream_truncated"));
            assert!(error.stops_replay() && error.keeps_usage);
            assert_eq!(
                mock.requests()
                    .iter()
                    .filter(|r| r.path != "/token")
                    .count(),
                1
            );
            for id in ["a", "b"] {
                let live = manager.get(id).unwrap();
                assert!(
                    !live.unavailable && live.model_states.is_empty(),
                    "{provider}/{format:?}: cooled credential"
                );
            }
            let record = records.try_recv().unwrap();
            assert!(record.failed);
            assert_eq!(record.status, 502);
            assert_eq!(record.total_tokens, 15);
            assert_eq!(record.tokens.input.total_tokens, 11);
            assert_eq!(record.tokens.output.total_tokens, 4);
            let queued: Vec<serde_json::Value> = usage
                .pop_oldest(10)
                .iter()
                .map(|bytes| serde_json::from_slice(bytes).unwrap())
                .filter(|record: &serde_json::Value| record.get("fail").is_some())
                .collect();
            assert_eq!(queued.len(), 1);
            let record = &queued[0];
            assert_eq!(record["service_tier"], "priority");
            assert_eq!(record["response_service_tier"], "flex");
            assert_eq!(record["fail"]["code"], "stream_truncated");
            assert!(records.try_recv().is_err());
        }
    }
}

// be929f07 provider handoff. Control lines must never start an interaction;
// release each chunk independently and keep the upstream connection open.
#[tokio::test]
async fn vertex_generate_content_interactions_sse_framing() {
    use bytes::Bytes;
    for provider in ["vertex_api_key", "vertex_service_account"] {
        for variant in [
            "combined",
            "split",
            "whitespace",
            "prefixed_done",
            "duplicate_done",
        ] {
            let (reply, sender) = Reply::controlled();
            let mock = Mock::answering(move |seen| {
                if seen.path == "/token" {
                    Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
                } else {
                    reply.clone()
                }
            })
            .await;
            let (exec, auth) = executor(provider, &mock);
            let req = request("gemini-2.5-flash", r#"{"input":"hello"}"#);
            let mut response = exec
                .execute_stream(auth, req, stream_options(&Format::INTERACTIONS))
                .await
                .unwrap();
            for control in [
                ": keepalive\n",
                "event: message\n",
                "id: bridge-1\n",
                "retry: 1000\n",
                "  : keepalive\r\n",
                "\n",
            ] {
                sender.send(Bytes::from_static(control.as_bytes())).unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), response.chunks.next())
                        .await
                        .is_err(),
                    "{provider}/{variant}/{control}: control line produced output"
                );
            }
            let mut content = json!({"responseId":"framing", "candidates":[{"content":{"role":"model","parts":[
                {"text":"bridge text"}, {"thought":true,"text":"bridge thought"},
                {"functionCall":{"id":"call_weather","name":"weather","args":{"city":"Chicago"}}}
            ]}, "finishReason":"STOP"}]});
            let usage =
                json!({"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15});
            if variant != "split" {
                content["usageMetadata"] = usage.clone();
            }
            let prefix = if variant == "whitespace" {
                "  data:  "
            } else {
                "data: "
            };
            sender
                .send(Bytes::from(format!("{prefix}{content}\r\n\n")))
                .unwrap();
            let mut output = String::new();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !output.contains("function_call") {
                    output.push_str(
                        std::str::from_utf8(&response.chunks.next().await.unwrap().unwrap())
                            .unwrap(),
                    );
                }
            })
            .await
            .unwrap();
            if variant == "split" {
                assert!(!output.contains("interaction.completed"));
                sender
                    .send(Bytes::from(format!(
                        "data: {}\n\n",
                        json!({"usageMetadata":usage})
                    )))
                    .unwrap();
            }
            let done = if variant == "prefixed_done" {
                "  data: [DONE]\r\n\n"
            } else {
                "[DONE]\n"
            };
            sender.send(Bytes::from_static(done.as_bytes())).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !output.contains("[DONE]") {
                    output.push_str(
                        std::str::from_utf8(&response.chunks.next().await.unwrap().unwrap())
                            .unwrap(),
                    );
                }
            })
            .await
            .unwrap();
            assert!(
                output.contains("bridge text")
                    && output.contains("bridge thought")
                    && output.contains("call_weather"),
                "{output}"
            );
            assert_eq!(
                output.matches("event: interaction.completed\n").count(),
                1,
                "{output}"
            );
            assert!(output.contains("\"total_tokens\":15"), "{output}");
            assert_eq!(output.matches("[DONE]").count(), 1);
            if variant == "duplicate_done" {
                sender
                    .send(Bytes::from_static(b"data: [DONE]\n\n"))
                    .unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), response.chunks.next())
                    .await
                    .is_err(),
                "duplicate completion or DONE"
            );
            assert!(
                mock.requests()
                    .iter()
                    .filter(|r| r.path != "/token")
                    .all(|r| r.path.ends_with(":streamGenerateContent"))
            );
        }
    }
}

#[tokio::test]
async fn native_interactions_endpoint_scopes_vertex_signatures() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut bytes = vec![0xa5; 405];
    bytes[..5].copy_from_slice(&[1, 0x8f, 0x3d, 0x6b, 0x5f]);
    let signature = STANDARD.encode(bytes);
    for provider in PROVIDERS {
        let mock = Mock::answering(|seen| {
            if seen.path == "/token" {
                Reply::json(r#"{"access_token":"local-token","expires_in":3600}"#)
            } else {
                Reply::json(r#"{"id":"native","status":"completed","output":[]}"#)
            }
        })
        .await;
        let (mut exec, auth) = executor(provider, &mock);
        if provider == "gemini" {
            exec = Box::new(super::InteractionsExecutor::new("direct"));
        }
        let mut auth = (*auth).clone();
        if provider == "gemini" {
            auth.provider = "gemini-interactions".into();
        }
        auth.metadata.insert("interactions".into(), true.into());
        let body = json!({"input":[
            {"type":"thought","signature":signature,"summary":[{"type":"text","text":"reason"}]},
            {"type":"text","text":"next"}
        ]});
        exec.execute(
            Arc::new(auth),
            request("gemini-2.5-flash", &body.to_string()),
            stream_options(&Format::INTERACTIONS),
        )
        .await
        .unwrap();
        let sent = mock.last().json();
        let parts = sent["input"].as_array().unwrap();
        if provider == "gemini" {
            assert_eq!(parts.len(), 1);
        } else {
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0]["signature"], signature);
        }
    }
}
