// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_integration_test.go
// (TestApplyPatchActualProviderErrorAndEOF,
// TestApplyPatchResponsesInvalidTerminalUsage,
// TestApplyPatchHTTPGatewayErrorMatrix, TestApplyPatchSDKOriginalRequestFallback,
// TestApplyPatchFailureStopsConsumptionAndNextAttemptIsFresh,
// TestApplyPatchXAIWebsocketFailureMatrix,
// TestApplyPatchInteractionsSourceFailureIsSealed) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `apply_patch_integration_test.go`: an invalid or unfinished call to
//! `apply_patch` from each provider fails the call with the clean 502, with
//! one failed usage record, whether the client streams or not, and through
//! the gateway.
//!
//! Deviations from upstream: see the parent module.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::Provider::{
    Claude, ClaudeOauth, CustomCompat, Gemini, GeminiInteractions, Meta, Vertex, Xai,
};
use super::*;
use crate::codex::bootstrap_tests::{Writer, serve};
use crate::codex::websocket::mock::Server;
use crate::json::str_at;
use crate::xai::websocket_tests::{auth_as, ws_options};

/// A Responses stream's first event.
const CREATED: &str = r#"{"type":"response.created","response":{"id":"r"}}"#;

/// A `response.completed` whose call to `apply_patch` has no input.
const EMPTY_ARGS_COMPLETED: &str = r#"{"type":"response.completed","response":{"output":[{"type":"function_call","name":"apply_patch","arguments":"{}"}],"usage":{"input_tokens":5,"output_tokens":3}}}"#;

/// The start of a call to `apply_patch` that never ends.
const PATCH_ITEM_ADDED: &str = r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"c","type":"function_call","name":"apply_patch","arguments":""}}"#;

// TestApplyPatchActualProviderErrorAndEOF: without the Kimi and Antigravity
// rows. The `scanner` mode's body fails after it is sent (see the parent
// module).
#[tokio::test]
async fn actual_provider_error_and_eof() {
    let mut cases = Vec::new();
    for provider in [
        CustomCompat,
        Claude,
        ClaudeOauth,
        Gemini,
        GeminiInteractions,
        Vertex,
    ] {
        for mode in ["nonstream", "stream", "eof", "empty", "scanner"] {
            cases.push(case(
                format!("{}/{mode}", provider.name()),
                provider_error(provider, mode),
            ));
        }
    }
    subtests(cases).await;
}

/// A case of [`actual_provider_error_and_eof`].
async fn provider_error(provider: Provider, mode: &'static str) {
    let mock = Mock::answering(move |seen| {
        let name = if provider.is_claude() {
            str_at(&seen.json(), "tools.0.name")
        } else {
            "apply_patch".to_owned()
        };
        let fixture_mode = match mode {
            "scanner" => "eof",
            "nonstream" if provider.is_claude() => "stream",
            mode => mode,
        };
        let body = fixture(provider, fixture_mode, &name);
        match mode {
            "nonstream" => Reply::json(&body),
            "scanner" => Reply::sse(&body).cut_off(),
            _ => Reply::sse(&body),
        }
    })
    .await;
    let executor = provider.executor();
    let auth = Arc::new(provider.auth("task6", &mock.url));
    let usage = Usage::new();
    let request = request(provider.model(), PATCH_REQUEST);
    let mut options = options_for(&request, mode != "nonstream");
    usage.observe(&request, &mut options);
    if mode == "nonstream" {
        assert_patch_failure(execute(&*executor, auth, request, options).await);
    } else {
        let (chunks, error) = stream(&*executor, auth, request, options).await;
        assert_failed_stream(&chunks, error);
    }
    usage.assert_one_failure();
}

// TestApplyPatchResponsesInvalidTerminalUsage: without the Kimi row.
#[tokio::test]
async fn responses_invalid_terminal_usage() {
    let completed = r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"c","name":"apply_patch","arguments":"{}"}],"usage":{"input_tokens":5,"output_tokens":3}}}"#;
    let body = sse(&[CREATED.to_owned(), completed.to_owned()]);
    let mut cases = Vec::new();
    for provider in [Xai, Meta] {
        let body = body.clone();
        cases.push(case(provider.name().to_owned(), async move {
            let mock = Mock::start(Reply::sse(&body)).await;
            let executor = provider.executor();
            let auth = Arc::new(provider.auth("task6-invalid-terminal", &mock.url));
            let usage = Usage::new();
            let request = request(provider.model(), PATCH_REQUEST);
            let mut options = options_for(&request, true);
            usage.observe(&request, &mut options);
            let (chunks, error) = stream(&*executor, auth, request, options).await;
            assert_failed_stream(&chunks, error);
            usage.assert_one_failure();
        }));
    }
    subtests(cases).await;
}

// TestApplyPatchHTTPGatewayErrorMatrix: without the Antigravity, Devin and
// Kimi rows, through the credential manager (see the parent module).
#[tokio::test]
async fn http_gateway_error_matrix() {
    let mut cases = Vec::new();
    for provider in [
        CustomCompat,
        Claude,
        ClaudeOauth,
        Gemini,
        GeminiInteractions,
        Vertex,
        Xai,
        Meta,
    ] {
        for mode in ["nonstream", "stream", "eof"] {
            cases.push(case(
                format!("{}/{mode}", provider.name()),
                gateway_error(provider, mode),
            ));
        }
    }
    subtests(cases).await;
}

/// A case of [`http_gateway_error_matrix`].
async fn gateway_error(provider: Provider, mode: &'static str) {
    let mock = Mock::answering(move |seen| {
        if matches!(provider, Xai | Meta) {
            let last = if mode == "eof" {
                PATCH_ITEM_ADDED
            } else {
                EMPTY_ARGS_COMPLETED
            };
            return Reply::sse(&sse(&[CREATED.to_owned(), last.to_owned()]));
        }
        let mut name = "apply_patch".to_owned();
        let mut actual_mode = mode;
        if provider.is_claude() {
            name = str_at(&seen.json(), "tools.0.name");
            if mode == "nonstream" {
                actual_mode = "stream";
            }
        }
        let body = fixture(provider, actual_mode, &name);
        if actual_mode == "nonstream" {
            Reply::json(&body)
        } else {
            Reply::sse(&body)
        }
    })
    .await;
    let auth = provider.auth(&format!("task6-http-{}{mode}", provider.name()), &mock.url);
    let usage = Usage::new();
    if mode == "nonstream" {
        assert_patch_failure(gateway_execute(provider.executor(), auth, &usage).await);
    } else {
        let (chunks, error) = gateway_stream(provider.executor(), auth, &usage).await;
        assert_failed_stream(&chunks, error);
    }
    usage.assert_one_failure();
}

// TestApplyPatchSDKOriginalRequestFallback: a call without the client's
// request reads the tools from the request it was given.
#[tokio::test]
async fn sdk_original_request_fallback() {
    let mut cases = Vec::new();
    for provider in [CustomCompat, Gemini, GeminiInteractions, Vertex] {
        cases.push(case(provider.name().to_owned(), async move {
            let mock =
                Mock::start(Reply::json(&fixture(provider, "nonstream", "apply_patch"))).await;
            let executor = provider.executor();
            let auth = Arc::new(provider.auth("", &mock.url));
            let result = execute(
                &*executor,
                auth,
                request("gemini-3.1-pro-preview", PATCH_REQUEST),
                options(false),
            )
            .await;
            assert_patch_failure(result);
        }));
    }
    subtests(cases).await;
}

// TestApplyPatchFailureStopsConsumptionAndNextAttemptIsFresh
#[tokio::test]
async fn failure_stops_consumption_and_next_attempt_is_fresh() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (stopped, mut stopped_rx) = mpsc::unbounded_channel::<()>();
    let url = serve(200, move |writer: Writer| {
        let calls = Arc::clone(&calls);
        let stopped = stopped.clone();
        async move {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                writer.write(&fixture(CustomCompat, "stream", "apply_patch"));
                writer.closed().await;
                let _ = stopped.send(());
                return;
            }
            writer.write(&fixture(CustomCompat, "nonstream", "apply_patch").replace(
                r#"\"input\":7,\"secret\":\"RAW_SECRET\""#,
                r#"\"input\":\"valid patch\""#,
            ));
        }
    })
    .await;
    let executor = CustomCompat.executor();
    let auth = Arc::new(CustomCompat.auth("", &url));
    let request = request("patch-model", PATCH_REQUEST);

    let (chunks, error) = stream(
        &*executor,
        Arc::clone(&auth),
        request.clone(),
        options_for(&request, true),
    )
    .await;
    assert_failed_stream(&chunks, error);
    tokio::time::timeout(Duration::from_secs(3), stopped_rx.recv())
        .await
        .expect("the failed attempt kept consuming the upstream");

    let response = execute(
        &*executor,
        auth,
        request.clone(),
        options_for(&request, false),
    )
    .await
    .unwrap_or_else(|error| panic!("the next attempt failed: {error:?}"));
    let payload: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(
        str_at(&payload, "output.0.input"),
        "valid patch",
        "{payload}"
    );
}

// TestApplyPatchXAIWebsocketFailureMatrix: the `raw=true` cases. The
// `raw=false` ones go over HTTP here (see the parent module).
#[tokio::test]
async fn xai_websocket_failure_matrix() {
    let invalid_completed = r#"{"type":"response.completed","response":{"output":[{"type":"function_call","name":"apply_patch","arguments":"{\"input\":7,\"secret\":\"RAW_SECRET\"}"}],"usage":{"input_tokens":5,"output_tokens":3}}}"#;
    let item_added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"c","name":"apply_patch","arguments":""}}"#;
    let mut cases = Vec::new();
    for (mode, last) in [("invalid", invalid_completed), ("eof", item_added)] {
        cases.push(case(format!("{mode}/raw=true"), async move {
            let server = Server::once(&[CREATED, last]).await;
            let executor = crate::xai::websocket_tests::executor();
            let auth = auth_as(&format!("task6-ws-{mode}"), "test", &server.url);
            let usage = Usage::new();
            let request = request("grok-4", PATCH_REQUEST);
            let mut options = ws_options("");
            usage.observe(&request, &mut options);
            let (chunks, error) = stream(&executor, auth, request, options).await;
            assert_failed_stream(&chunks, error);
            usage.assert_one_failure();
        }));
    }
    subtests(cases).await;
}

// TestApplyPatchInteractionsSourceFailureIsSealed
#[tokio::test]
async fn interactions_source_failure_is_sealed() {
    let mut cases = Vec::new();
    for streaming in [false, true] {
        cases.push(case(format!("stream={streaming}"), async move {
            let reply = if streaming {
                Reply::sse(&sse(&[
                    r#"{"event_type":"interaction.created","interaction":{"id":"r"}}"#.to_owned(),
                    r#"{"event_type":"interaction.failed","interaction":{"id":"r"},"error":{"message":"RAW_SECRET"}}"#.to_owned(),
                    r#"{"event_type":"interaction.completed","interaction":{"id":"r","usage":{"total_input_tokens":5}}}"#.to_owned(),
                    "[DONE]".to_owned(),
                ]))
            } else {
                Reply::json(r#"{"id":"r","status":"failed","error":{"message":"RAW_SECRET"}}"#)
            };
            let mock = Mock::start(reply).await;
            let executor = GeminiInteractions.executor();
            let auth = Arc::new(GeminiInteractions.auth("task6-source-failed", &mock.url));
            let usage = Usage::new();
            let request = request(GeminiInteractions.model(), PATCH_REQUEST);
            let mut options = options(streaming);
            usage.observe(&request, &mut options);
            if streaming {
                let (chunks, error) = stream(&*executor, auth, request, options).await;
                assert_failed_stream(&chunks, error);
            } else {
                assert_patch_failure(execute(&*executor, auth, request, options).await);
            }
            usage.assert_one_failure();
        }));
    }
    subtests(cases).await;
}
