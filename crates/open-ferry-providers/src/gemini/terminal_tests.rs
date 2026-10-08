// Ported from CLIProxyAPI internal/runtime/executor/issue6258_terminal_test.go
// (TestIssue6258ExecutorResponsesSplitUsage,
// TestIssue6258ExecutorReadErrorHasNoTerminal,
// TestIssue6258ExecutorCleanEOFControl) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Issue 6258: a Gemini or Vertex AI stream whose connection fails gives the
//! Responses client its error and no terminal event, while one that ends
//! cleanly gets exactly one, with the usage that came after the finish
//! reason, for the Gemini executor, Vertex AI with an API key and Vertex AI
//! with a service account.
//!
//! Changed:
//! - Without the Antigravity rows, which aren't ported.
//! - The failing connection is cut off after the body, where upstream
//!   promises 128 more bytes than it sends; either way the read fails, and
//!   the read error here isn't Go's `io.ErrUnexpectedEOF`.
//! - `TestIssue6258ExecutorCancellationHasNoTerminal` isn't ported: a
//!   dropped stream stops at once and yields nothing more (see the parent
//!   module), where upstream checks its context.

use std::sync::Arc;

use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{Format, Options, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use serde_json::Value;

use super::testing::{
    Mock, PATCH_REQUEST, PATCH_RESPONSE, Reply, Seen, assert_patch_output, collect, key_auth,
    request, service_account_auth, stream_options,
};
use super::{GeminiExecutor, VertexExecutor};

/// Upstream's `issue6258ExecutorContent`.
const CONTENT: &str =
    r#"{"responseId":"executor-6258","candidates":[{"content":{"parts":[{"text":"answer"}]}}]}"#;

/// Upstream's `issue6258ExecutorUsage`.
const USAGE: &str = r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10,"thoughtsTokenCount":50,"totalTokenCount":160}}"#;

/// Upstream's `issue6258Providers`, without Antigravity.
const PROVIDERS: [&str; 3] = ["gemini", "vertex_api_key", "vertex_service_account"];

/// The terminal events of a Responses stream.
const TERMINALS: [&str; 3] = [
    "response.completed",
    "response.incomplete",
    "response.failed",
];

/// Upstream's `issue6258SSE`, without Antigravity's envelope.
fn sse(body: &str) -> String {
    format!("data: {body}\n\n")
}

/// Upstream's `issue6258TokenResponse`: a token for a service account's
/// request to `/token`, else `reply`.
fn token_or(seen: &Seen, reply: &Reply) -> Reply {
    if seen.path == "/token" {
        Reply::json(r#"{"access_token":"local-token","token_type":"Bearer","expires_in":3600}"#)
    } else {
        reply.clone()
    }
}

/// Upstream's `issue6258Executor` and `issue6258StartStream`: streams
/// `payload` for a Responses client through `provider`, served by `mock`.
async fn start(provider: &str, mock: &Mock, payload: &str) -> StreamResponse {
    let (executor, auth): (Box<dyn ProviderExecutor>, Arc<Auth>) = match provider {
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
        other => panic!("unknown provider {other:?}"),
    };
    let request = request("gemini-3.7-flash", payload);
    let options = Options {
        original_request: request.payload.clone(),
        ..stream_options(&Format::OPENAI_RESPONSE)
    };
    executor
        .execute_stream(auth, request, options)
        .await
        .unwrap_or_else(|error| panic!("{provider}: execute_stream: {error:?}"))
}

/// Upstream's `issue6258Events`: the typed events in `chunks`.
fn events(chunks: &[String]) -> Vec<Value> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.split('\n'))
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .filter(|event| event.get("type").is_some())
        .collect()
}

// TestIssue6258ExecutorResponsesSplitUsage.
#[tokio::test]
async fn responses_split_usage() {
    for provider in PROVIDERS {
        for trace in [false, true] {
            let body: String = [
                CONTENT,
                r#"{"candidates":[{"finishReason":"STOP"}]}"#,
                USAGE,
            ]
            .iter()
            .map(|body| {
                if trace {
                    let body = body.strip_suffix('}').unwrap_or(body);
                    sse(&format!(r#"{body},"traceId":"trace-6258"}}"#))
                } else {
                    sse(body)
                }
            })
            .collect();
            let reply = Reply::sse(&body);
            let mock = Mock::answering(move |seen| token_or(seen, &reply)).await;
            let stream = start(provider, &mock, r#"{"input":"hello","stream":true}"#).await;
            let (chunks, error) = collect(stream).await;
            let case = format!("{provider}/trace={trace}");
            assert!(
                error.is_none(),
                "{case}: the clean stream failed: {error:?}"
            );
            let response = terminal(&case, &chunks);
            for (path, want) in [
                ("input_tokens", 100),
                ("output_tokens", 60),
                ("total_tokens", 160),
                ("output_tokens_details.reasoning_tokens", 50),
            ] {
                let got = crate::json::get(&response["usage"], path);
                assert_eq!(
                    got.and_then(Value::as_i64),
                    Some(want),
                    "{case}: terminal usage.{path}; usage={}",
                    response["usage"]
                );
            }
        }
    }
}

// TestIssue6258ExecutorReadErrorHasNoTerminal.
#[tokio::test]
async fn read_error_has_no_terminal() {
    for provider in PROVIDERS {
        let reply = Reply::sse(&sse(CONTENT)).cut_off();
        let mock = Mock::answering(move |seen| token_or(seen, &reply)).await;
        let stream = start(provider, &mock, r#"{"input":"hello","stream":true}"#).await;
        let (chunks, error) = collect(stream).await;
        assert!(error.is_some(), "{provider}: no read error: {chunks:?}");
        let events = events(&chunks);
        let deltas = events
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .count();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| TERMINALS.iter().any(|kind| event["type"] == *kind))
            .collect();
        assert!(deltas > 0, "{provider}: no deltas: {chunks:?}");
        assert!(
            terminals.is_empty(),
            "{provider}: the read-error stream synthesized {terminals:?}"
        );
    }
}

// TestIssue6258ExecutorCleanEOFControl.
#[tokio::test]
async fn clean_eof_has_one_terminal() {
    for provider in PROVIDERS {
        for patch in [false, true] {
            let (body, payload) = if patch {
                // A valid source STOP must not become an invalid patch.
                (PATCH_RESPONSE, PATCH_REQUEST)
            } else {
                (CONTENT, r#"{"input":"hello","stream":true}"#)
            };
            let reply = Reply::sse(&sse(body));
            let mock = Mock::answering(move |seen| token_or(seen, &reply)).await;
            let (chunks, error) = collect(start(provider, &mock, payload).await).await;
            let case = format!("{provider}/apply_patch={patch}");
            assert!(
                error.is_none(),
                "{case}: the clean stream failed: {error:?}"
            );
            let response = terminal(&case, &chunks);
            if patch {
                assert_patch_output(&response);
            } else {
                assert_eq!(
                    response["output"][0]["content"][0]["text"], "answer",
                    "{case}: the clean EOF lost the partial output: {response}"
                );
            }
        }
    }
}

/// Upstream's `issue6258ExecutorTerminal`: the response of the one terminal
/// event of a clean stream, which completed it and finished each of its
/// items once.
fn terminal(case: &str, chunks: &[String]) -> Value {
    let events = events(chunks);
    let terminals: Vec<_> = events
        .iter()
        .filter(|event| TERMINALS.iter().any(|kind| event["type"] == *kind))
        .collect();
    let [terminal] = terminals.as_slice() else {
        panic!(
            "{case}: {} terminal events, want one: {chunks:?}",
            terminals.len()
        );
    };
    let response = terminal["response"].clone();
    assert_eq!(response["status"], "completed", "{case}: {response}");
    for item in response["output"].as_array().into_iter().flatten() {
        let done = events
            .iter()
            .filter(|event| {
                event["type"] == "response.output_item.done" && event["item"]["id"] == item["id"]
            })
            .count();
        assert_eq!(done, 1, "{case}: item done count for {item}");
    }
    response
}
