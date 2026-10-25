// Ported from CLIProxyAPI gemini_executor_test.go and
// gemini_vertex_executor_test.go, upstream 3f5d3a16 (MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::sync::Arc;
use std::time::Duration;

use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::usage::Usage;
use open_ferry_core::observe::{CallReport, Observation, RequestContext};
use serde_json::{Value, json};

use super::testing::{Mock, Reply, collect, key_auth, request, service_account_auth};
use super::{GeminiExecutor, VertexExecutor, prompt_block_error_message};

const PROVIDERS: [&str; 3] = ["gemini", "vertex_api_key", "vertex_service_account"];

#[test]
fn prompt_block_detection_requires_no_candidates_and_a_nonblank_reason() {
    for body in [
        "",
        "invalid",
        "{}",
        r#"{"promptFeedback":{"blockReason":" "}}"#,
    ] {
        assert_eq!(prompt_block_error_message(body.as_bytes()), None);
    }
    for candidates in [json!([]), Value::Null, json!({})] {
        for wrapped in [false, true] {
            let body = json!({"candidates":candidates,"promptFeedback":{
                "blockReason":" SAFETY ","blockReasonMessage":" blocked "
            }});
            let body = if wrapped {
                json!({"response":body})
            } else {
                body
            };
            assert_eq!(
                prompt_block_error_message(body.to_string().as_bytes()).as_deref(),
                Some("upstream blocked the prompt: SAFETY - blocked")
            );
        }
    }
    for wrapped_candidates in [false, true] {
        for wrapped_feedback in [false, true] {
            let mut body = json!({"response":{}});
            let target = if wrapped_candidates {
                &mut body["response"]
            } else {
                &mut body
            };
            target["candidates"] = json!([{}]);
            let target = if wrapped_feedback {
                &mut body["response"]
            } else {
                &mut body
            };
            target["promptFeedback"] = json!({"blockReason":"SAFETY"});
            assert_eq!(
                prompt_block_error_message(body.to_string().as_bytes()),
                None
            );
        }
    }
}

#[tokio::test]
async fn prompt_blocks_return_502_and_keep_observed_usage() {
    for provider in PROVIDERS {
        for format in [
            Format::OPENAI,
            Format::CLAUDE,
            Format::OPENAI_RESPONSE,
            Format::GEMINI,
        ] {
            for stream in [false, true] {
                for with_message in [false, true] {
                    let mut body = json!({
                        "candidates":[],
                        "promptFeedback":{"blockReason":"SAFETY"},
                        "usageMetadata":{"promptTokenCount":413751,"totalTokenCount":413751},
                        "modelVersion":"gemini-4-argon"
                    });
                    if with_message {
                        body.as_object_mut().unwrap().remove("candidates");
                        body["promptFeedback"]["blockReasonMessage"] =
                            "The prompt is blocked due to safety".into();
                    }
                    let message = if with_message {
                        "upstream blocked the prompt: SAFETY - The prompt is blocked due to safety"
                    } else {
                        "upstream blocked the prompt: SAFETY"
                    };
                    for wrapped in [false, true] {
                        let body = if wrapped {
                            let mut response = body.clone();
                            let usage = response
                                .as_object_mut()
                                .unwrap()
                                .remove("usageMetadata")
                                .unwrap();
                            json!({"response":response,"usageMetadata":usage})
                        } else {
                            body.clone()
                        };
                        run_case(provider, &format, &body, stream, Some(message), 413751, 0).await;
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn candidates_with_prompt_feedback_are_not_prompt_blocks() {
    let body = json!({
        "candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],
        "promptFeedback":{"blockReason":"SAFETY"},
        "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}
    });
    for provider in PROVIDERS {
        for format in [
            Format::OPENAI,
            Format::CLAUDE,
            Format::OPENAI_RESPONSE,
            Format::GEMINI,
        ] {
            for stream in [false, true] {
                run_case(provider, &format, &body, stream, None, 10, 1).await;
            }
        }
    }
}

#[tokio::test]
async fn prompt_blocks_without_usage_still_fail() {
    let body = json!({"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"}});
    for provider in PROVIDERS {
        for stream in [false, true] {
            run_case(
                provider,
                &Format::OPENAI,
                &body,
                stream,
                Some("upstream blocked the prompt: PROHIBITED_CONTENT"),
                0,
                0,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn prompt_block_errors_redact_echoed_credentials() {
    for provider in PROVIDERS {
        let secret = if provider == "vertex_service_account" {
            "local-token"
        } else {
            "test-key"
        };
        let body = json!({
            "promptFeedback":{"blockReason":"SAFETY","blockReasonMessage":format!("echo {secret}")},
            "usageMetadata":{"promptTokenCount":20,"totalTokenCount":20}
        });
        for stream in [false, true] {
            run_case(
                provider,
                &Format::OPENAI,
                &body,
                stream,
                Some("upstream blocked the prompt: SAFETY - echo [redacted]"),
                20,
                0,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn chat_and_claude_truncation_fail_and_keep_observed_usage() {
    let body = json!({
        "candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]}}],
        "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":5,"totalTokenCount":25}
    });
    for provider in PROVIDERS {
        for format in [Format::OPENAI, Format::CLAUDE] {
            for finish in [None, Some(""), Some("   ")] {
                let mut body = body.clone();
                if let Some(finish) = finish {
                    body["candidates"][0]["finishReason"] = finish.into();
                }
                for with_feedback in [false, true] {
                    let mut body = body.clone();
                    if with_feedback {
                        body["promptFeedback"] = json!({"blockReason":"SAFETY"});
                    }
                    run_case(
                        provider,
                        &format,
                        &body,
                        true,
                        Some("upstream stream closed before finishReason"),
                        20,
                        5,
                    )
                    .await;
                }
            }
        }
    }
}

async fn run_case(
    provider: &str,
    format: &Format,
    body: &Value,
    stream: bool,
    expected_error: Option<&str>,
    input: i64,
    output: i64,
) {
    let body = body.to_string();
    let reply = if stream {
        // A prompt block must terminate even when the provider leaves its connection open.
        let reply = Reply::sse(&format!("data: {body}\n\ndata: [DONE]\n\n"));
        if expected_error.is_some_and(|message| message.starts_with("upstream blocked")) {
            reply.hold_open()
        } else {
            reply
        }
    } else {
        Reply::json(&body)
    };
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
            Box::new(VertexExecutor::new("direct").with_service_account_base_url(&mock.url)),
            Arc::new(service_account_auth(
                &format!("{}/token", mock.url),
                "global",
            )),
        ),
    };
    let payload = match format.as_str() {
        "gemini" => r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
        "openai-response" => r#"{"input":"hi"}"#,
        _ => r#"{"messages":[{"role":"user","content":"hi"}],"max_tokens":64}"#,
    };
    let request = request("gemini-4-argon", payload);
    let mut options = Options::new(format.clone());
    options.stream = stream;
    options.original_request = request.payload.clone();
    let mut config = Config::default();
    config.usage_statistics_enabled = true;
    let usage = Usage::new(&config);
    let (records, _subscription) = usage.observe();
    let context = Arc::new(RequestContext::new(http::Method::POST, "/test".into()));
    let tap = usage.tap(&context, &request, &options).unwrap();
    options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
    let report = CallReport::start(&options);
    let label = format!("{provider}/{format:?}/stream={stream}");
    let error = if stream {
        let response = report
            .stream(executor.execute_stream(auth, request, options).await)
            .unwrap();
        let (chunks, error) = tokio::time::timeout(Duration::from_secs(5), collect(response))
            .await
            .unwrap_or_else(|_| panic!("{label}: stream did not end"));
        if expected_error.is_some() {
            assert!(
                !chunks.iter().any(|chunk| chunk.contains("[DONE]")),
                "{label}: failed stream emitted DONE: {chunks:?}"
            );
        }
        if expected_error.is_some_and(|message| message.starts_with("upstream blocked")) {
            assert!(
                chunks.is_empty(),
                "{label}: prompt block emitted success chunks: {chunks:?}"
            );
        }
        error
    } else {
        let result = executor.execute(auth, request, options).await;
        report.finish(&result);
        result.err()
    };
    if let Some(message) = expected_error {
        let error = error.unwrap_or_else(|| panic!("{label}: no error"));
        assert_eq!(error.status, 502, "{label}: {error:?}");
        assert!(error.message.contains(message), "{label}: {error:?}");
        assert!(error.keeps_usage, "{label}");
    } else {
        assert!(error.is_none(), "{label}: {error:?}");
    }
    let record = records
        .try_recv()
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    assert_eq!(
        record.failed,
        expected_error.is_some(),
        "{label}: {record:?}"
    );
    assert_eq!(
        record.status,
        if expected_error.is_some() { 502 } else { 200 },
        "{label}"
    );
    assert_eq!(record.tokens.input.total_tokens, input, "{label}");
    assert_eq!(record.tokens.output.total_tokens, output, "{label}");
    assert_eq!(record.total_tokens, input + output, "{label}");
    assert!(records.try_recv().is_err(), "{label}: duplicate usage");
}
