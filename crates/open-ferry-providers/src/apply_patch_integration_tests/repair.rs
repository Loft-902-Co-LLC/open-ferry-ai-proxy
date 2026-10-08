// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_repair_test.go
// (TestApplyPatchRepairResponsesSourceTerminalHTTP,
// TestApplyPatchRepairXAIWebsocketSourceTerminal,
// TestApplyPatchRepairXAIWebsocketPersistentFailureRetry,
// TestApplyPatchRepairResponsesSourceTerminalNonStream,
// TestApplyPatchRepairXAIWebsocketCompactionUsage,
// TestApplyPatchRepairOrdinaryEmptyHTTPPassthrough) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `apply_patch_repair_test.go`: a Responses source whose call to
//! `apply_patch` is whole but whose response never completes fails cleanly,
//! once; one that completes is passed on; a WebSocket that failed is not
//! used again; and the usage of a compaction on the WebSocket.
//!
//! Deviations from upstream: see the parent module.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::sync::{Notify, mpsc};

use super::Provider::{Meta, Xai};
use super::*;
use crate::codex::websocket::mock::{Answer, Server};
use crate::json::str_at;
use crate::xai::websocket_tests::{auth_as, has_conn, ws_options};

/// The modes of a source that ends without a terminal event.
const UNFINISHED: [&str; 4] = ["args-eof", "item-eof", "empty", "done"];

/// The modes of a source that ends with one.
const TERMINAL: [&str; 3] = ["response.completed", "response.incomplete", "response.done"];

/// A Responses source in `mode` whose call to `apply_patch` is whole
/// (`task6RepairSource`): `args-eof`, its arguments' `done` and no more;
/// `item-eof`, its item's `done` and no more; `empty`, nothing; `done`, the
/// arguments' `done` and `[DONE]`; a `response.*` mode, the item's `done`
/// and that terminal event.
fn repair_source(mode: &str) -> Vec<String> {
    if mode == "empty" {
        return Vec::new();
    }
    let item = r#"{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"valid patch\"}"}"#;
    let mut events = vec![r#"{"type":"response.created","response":{"id":"r"}}"#.to_owned()];
    if mode == "args-eof" || mode == "done" {
        events.extend([
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}"#.to_owned(),
            r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","arguments":"{\"input\":\"valid patch\"}"}"#.to_owned(),
        ]);
    } else {
        events.push(format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{item}}}"#
        ));
    }
    if mode == "done" {
        events.push("[DONE]".to_owned());
    } else if mode.starts_with("response.") {
        events.push(format!(
            r#"{{"type":{},"response":{{"id":"r","output":[{item}],"usage":{{"input_tokens":5,"output_tokens":3}}}}}}"#,
            quote(mode)
        ));
    }
    events
}

/// The type of a chunk's event (`task6RepairChunkType`).
fn chunk_type(chunk: &str) -> String {
    let chunk = chunk.trim();
    let chunk = chunk.strip_prefix("data:").map_or(chunk, str::trim);
    serde_json::from_str::<Value>(chunk)
        .map(|event| str_at(&event, "type"))
        .unwrap_or_default()
}

/// Asserts what a WebSocket stream gave (`task6RepairReadStream`): for a
/// `failure`, one `response.failed`, the clean 502 and no completion or
/// `[DONE]`; otherwise one completion with the valid patch and no failure.
/// Never the invalid arguments.
fn assert_repair_stream(chunks: &[String], error: Option<ExecError>, failure: bool) {
    let mut failed = 0;
    let mut completed = 0;
    for chunk in chunks {
        match chunk_type(chunk).as_str() {
            "response.failed" => failed += 1,
            "response.completed" | "response.done" | "response.incomplete" => completed += 1,
            _ => {}
        }
        assert!(
            !chunk.contains("RAW_SECRET") && !(failure && chunk.contains("[DONE]")),
            "invalid source leaked: {chunk}"
        );
    }
    let valid_patch = chunks.iter().any(|chunk| chunk.contains("valid patch"));
    if failure {
        let error = error.unwrap_or_else(|| panic!("no error: {chunks:?}"));
        assert_patch_error(&error);
        assert_eq!((failed, completed), (1, 0), "{chunks:?}");
    } else {
        assert!(error.is_none(), "{error:?}");
        assert_eq!((failed, completed), (0, 1), "{chunks:?}");
        assert!(valid_patch, "{chunks:?}");
    }
}

// TestApplyPatchRepairResponsesSourceTerminalHTTP: without the Kimi rows,
// and the gateway through the credential manager (see the parent module).
#[tokio::test]
async fn responses_source_terminal_http() {
    let mut cases = Vec::new();
    for provider in [Xai, Meta] {
        for mode in UNFINISHED.into_iter().chain(TERMINAL) {
            cases.push(case(
                format!("{}/{mode}", provider.name()),
                source_terminal_http(provider, mode),
            ));
        }
    }
    subtests(cases).await;
}

/// A case of [`responses_source_terminal_http`].
async fn source_terminal_http(provider: Provider, mode: &'static str) {
    let terminal = mode.starts_with("response.");
    let mut body = sse(&repair_source(mode));
    if terminal {
        body.push_str("data: [DONE]\n\ndata: [DONE]\n\n");
    }
    let mock = Mock::start(Reply::sse(&body)).await;
    let executor = provider.executor();
    let id = format!("{}/{mode}", provider.name());
    let auth = provider.auth(&id, &mock.url);
    let request = request(provider.model(), PATCH_REQUEST);
    if !terminal {
        let usage = Usage::new();
        let mut options = options(true);
        usage.observe(&request, &mut options);
        let (chunks, error) = stream(&*executor, Arc::new(auth.clone()), request, options).await;
        assert_failed_stream(&chunks, error);
        usage.assert_one_failure();

        let usage = Usage::new();
        let mut auth = auth;
        auth.id.push_str("-gateway");
        let (chunks, error) = gateway_stream(executor, auth, &usage).await;
        assert_failed_stream(&chunks, error);
        usage.assert_one_failure();
        return;
    }
    let (chunks, error) = stream(&*executor, Arc::new(auth), request, options(true)).await;
    assert!(error.is_none(), "{error:?}");
    let output = chunks.concat();
    assert!(!output.contains("response.failed"), "{output}");
    assert!(output.contains("valid patch"), "{output}");
    assert_eq!(output.matches("[DONE]").count(), 1, "{output}");
}

// TestApplyPatchRepairXAIWebsocketSourceTerminal: the `raw=true` cases. The
// `raw=false` ones go over HTTP here (see the parent module).
#[tokio::test]
async fn xai_websocket_source_terminal() {
    let mut cases = Vec::new();
    for mode in UNFINISHED.into_iter().chain(TERMINAL) {
        cases.push(case(format!("{mode}/raw=true"), async move {
            let events = repair_source(mode);
            let frames: Vec<&str> = events.iter().map(String::as_str).collect();
            let server = Server::once(&frames).await;
            let executor = crate::xai::websocket_tests::executor();
            let auth = auth_as(&format!("{mode}/raw=true"), "test", &server.url);
            let failure = !mode.starts_with("response.");
            let usage = Usage::new();
            let request = request("grok-4", PATCH_REQUEST);
            let mut options = ws_options("");
            usage.observe(&request, &mut options);
            let (chunks, error) = stream(&executor, auth, request, options).await;
            assert_repair_stream(&chunks, error, failure);
            if failure {
                usage.assert_one_failure();
            }
        }));
    }
    subtests(cases).await;
}

// TestApplyPatchRepairXAIWebsocketPersistentFailureRetry: the `bridge`
// branch with `raw=true` and `detach=false`, without the lifecycle's check
// (see the parent module). The first connection gets invalid arguments and
// is left open; the retry, started while the first call still has its
// error to give, must get a fresh connection.
#[tokio::test]
async fn xai_websocket_persistent_failure_retry() {
    let (seen, mut seen_rx) = mpsc::unbounded_channel::<&'static str>();
    let release = Arc::new(Notify::new());
    let server = {
        let release = Arc::clone(&release);
        Server::start(move |n| {
            let seen = seen.clone();
            let release = Arc::clone(&release);
            Answer::accept(move |mut peer| {
                let seen = seen.clone();
                let release = Arc::clone(&release);
                async move {
                    if peer.recv().await.is_none() {
                        return;
                    }
                    if n == 0 {
                        peer.send(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}"#).await;
                        peer.send(r#"{"type":"response.function_call_arguments.delta","item_id":"a","output_index":0,"delta":"{\"input\":7,\"secret\":\"RAW_SECRET\"}"}"#).await;
                        // Left open: a retry on the same socket would get
                        // stale output.
                        if peer.recv().await.is_some() {
                            peer.send(r#"{"type":"response.completed","response":{"id":"stale","output":[]}}"#).await;
                            let _ = seen.send("a second request on the failed socket");
                        } else {
                            let _ = seen.send("old closed");
                        }
                        return;
                    }
                    let _ = seen.send("fresh request");
                    release.notified().await;
                    for event in repair_source("response.completed") {
                        peer.send(&event).await;
                    }
                    peer.hold().await;
                }
            })
        })
        .await
    };
    let executor = Arc::new(crate::xai::websocket_tests::executor());
    let session = "task6-repair-persistent-failure";
    let auth = auth_as("task6-repair-persistent-failure", "test", &server.url);
    let usage = Usage::new();
    let first_request = request("grok-4", PATCH_REQUEST);
    let mut options = ws_options(session);
    usage.observe(&first_request, &mut options);
    let first = start_stream(&*executor, Arc::clone(&auth), first_request, options).await;
    let mut chunks = first.chunks;

    // The failure frame comes before the request lock is released.
    let mut failed = false;
    while let Some(chunk) = within("a chunk of the first call", chunks.next()).await {
        let chunk = chunk.unwrap_or_else(|error| panic!("an error before the failure: {error:?}"));
        let chunk = String::from_utf8_lossy(&chunk).into_owned();
        assert!(
            !chunk.contains("RAW_SECRET") && chunk_type(&chunk) != "response.completed",
            "the failed attempt leaked or completed: {chunk}"
        );
        if chunk_type(&chunk) == "response.failed" {
            assert!(
                !has_conn(&executor, session),
                "the failure came before the persistent connection was dropped"
            );
            failed = true;
            break;
        }
    }
    assert!(failed, "no clean failure frame");

    // The retry starts while the first call has its clean error to give.
    let second = tokio::spawn({
        let executor = Arc::clone(&executor);
        let auth = Arc::clone(&auth);
        async move {
            executor
                .execute_stream(auth, request("grok-4", PATCH_REQUEST), ws_options(session))
                .await
        }
    });
    let mut errors = 0;
    while let Some(chunk) = within("the rest of the first call", chunks.next()).await {
        match chunk {
            Ok(chunk) => panic!("extra failure payload: {}", String::from_utf8_lossy(&chunk)),
            Err(error) => {
                errors += 1;
                assert_patch_error(&error);
            }
        }
    }
    assert_eq!(errors, 1, "clean errors");
    usage.assert_one_failure();

    let mut saw = Vec::new();
    while saw.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(3), seen_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("watchdog: only {saw:?}"))
            .expect("the server is gone");
        saw.push(event);
    }
    saw.sort_unstable();
    assert_eq!(saw, ["fresh request", "old closed"]);

    release.notify_one();
    let second = within("the retry to start", second)
        .await
        .unwrap()
        .unwrap_or_else(|error| panic!("the retry failed: {error:?}"));
    let (chunks, error) = crate::xai::websocket_tests::collect(second).await;
    assert_repair_stream(&chunks, error, false);
    assert_eq!(server.record().handshakes.len(), 2, "connections");
    executor.close_execution_session(session);
}

// TestApplyPatchRepairResponsesSourceTerminalNonStream: without the Kimi
// row, and the gateway through the credential manager (see the parent
// module).
#[tokio::test]
async fn responses_source_terminal_non_stream() {
    let mut cases = Vec::new();
    for provider in [Xai, Meta] {
        for mode in UNFINISHED {
            cases.push(case(format!("{}/{mode}", provider.name()), async move {
                // Upstream's mock sends the SSE body without a content type;
                // none of these executors reads it.
                let mock = Mock::start(Reply::json(&sse(&repair_source(mode)))).await;
                let executor = provider.executor();
                let id = format!("{}/{mode}", provider.name());
                let auth = provider.auth(&id, &mock.url);
                let usage = Usage::new();
                let request = request(provider.model(), PATCH_REQUEST);
                let mut options = options(false);
                usage.observe(&request, &mut options);
                let result = execute(&*executor, Arc::new(auth.clone()), request, options).await;
                assert_patch_failure(result);
                usage.assert_one_failure();

                let usage = Usage::new();
                let mut auth = auth;
                auth.id.push_str("-gateway");
                assert_patch_failure(gateway_execute(executor, auth, &usage).await);
                usage.assert_one_failure();
            }));
        }
    }
    subtests(cases).await;
}

/// A case of [`xai_websocket_compaction_usage`].
#[derive(Clone, Copy)]
struct Compaction {
    name: &'static str,
    /// What the compact call answers.
    body: &'static str,
    http_status: u16,
    /// The status the call fails with, or 0 when it succeeds.
    want_status: u16,
    want_records: usize,
    /// Whether the input is the trigger alone.
    empty_input: bool,
    /// Whether the call is an ordinary HTTP compact call.
    passthrough: bool,
}

// TestApplyPatchRepairXAIWebsocketCompactionUsage: the usage of a
// `compaction_trigger` on the WebSocket, answered with the compact call's
// state. Upstream's record has no status for a call that succeeded; the
// port's JSON record says 200, so only a failure's status is compared.
#[tokio::test]
async fn xai_websocket_compaction_usage() {
    const INVALID_STATE: &str = r#"{"id":"resp_empty","output":[]}"#;
    const VALID_STATE: &str = r#"{"id":"resp_compact","output":[{"type":"compaction","encrypted_content":"opaque-state"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let base = Compaction {
        name: "",
        body: INVALID_STATE,
        http_status: 200,
        want_status: 0,
        want_records: 1,
        empty_input: false,
        passthrough: false,
    };
    let mut cases = Vec::new();
    for compaction in [
        Compaction {
            name: "invalid-state",
            want_status: 502,
            ..base
        },
        Compaction {
            name: "valid-state",
            body: VALID_STATE,
            ..base
        },
        Compaction {
            name: "http-error",
            body: r#"{"error":{"message":"compact rejected"}}"#,
            http_status: 400,
            want_status: 400,
            ..base
        },
        Compaction {
            name: "missing-reporter-empty-context",
            want_status: 400,
            want_records: 0,
            empty_input: true,
            ..base
        },
        Compaction {
            name: "http-compact-passthrough",
            passthrough: true,
            ..base
        },
    ] {
        cases.push(case(
            compaction.name.to_owned(),
            compaction_usage(compaction, VALID_STATE),
        ));
    }
    subtests(cases).await;
}

/// A case of [`xai_websocket_compaction_usage`]; `valid_state` is the
/// answer whose usage counts 3 tokens.
async fn compaction_usage(case: Compaction, valid_state: &'static str) {
    let (status, body) = (case.http_status, case.body);
    let server = Server::start(move |_| Answer::Refuse {
        status,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_owned(),
    })
    .await;
    let executor = crate::xai::websocket_tests::executor();
    let auth = auth_as(case.name, "test", &server.url);
    let payload = if case.empty_input {
        r#"{"model":"grok-4.3","input":[{"type":"compaction_trigger"}]}"#
    } else {
        r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"history"},{"type":"compaction_trigger"}]}"#
    };
    let request = request("grok-4.3", payload);
    let usage = Usage::new();
    let result = if case.passthrough {
        let mut options = options(false);
        options.alt = "responses/compact".into();
        options.metadata.execution_session_id = Some(case.name.into());
        usage.observe(&request, &mut options);
        let result = execute(&executor, auth, request, options).await;
        if let Ok(response) = &result {
            let payload: Value = serde_json::from_slice(&response.payload).unwrap();
            assert_eq!(payload["output"], Value::Array(Vec::new()), "{payload}");
        }
        result.map(drop)
    } else {
        let mut options = ws_options(case.name);
        usage.observe(&request, &mut options);
        let report = CallReport::start(&options);
        let result = within(
            "the compaction to start",
            executor.execute_stream(auth, request, options),
        )
        .await;
        match report.stream(result) {
            Ok(response) => {
                let (chunks, error) = crate::xai::websocket_tests::collect(response).await;
                assert!(error.is_none(), "compact stream chunk error: {error:?}");
                let completed = chunks
                    .iter()
                    .flat_map(|chunk| chunk_events(chunk))
                    .rfind(|event| event["type"] == "response.completed")
                    .unwrap_or(Value::Null);
                assert_eq!(
                    case.want_status, 0,
                    "unexpected compact success: {chunks:?}"
                );
                assert_eq!(
                    str_at(&completed, "response.output.0.encrypted_content"),
                    "opaque-state",
                    "{chunks:?}"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    };
    match result {
        Ok(()) => assert_eq!(case.want_status, 0, "the compaction succeeded"),
        Err(error) => assert_eq!(error.status, case.want_status, "{error:?}"),
    }

    let record = server.record();
    if case.empty_input {
        assert!(
            record.handshakes.is_empty(),
            "unexpected compact request: {record:?}"
        );
    } else {
        let request = record
            .handshakes
            .first()
            .unwrap_or_else(|| panic!("the compaction made no HTTP request"));
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", "/responses/compact")
        );
        let body = record
            .bodies
            .first()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(!body.contains("compaction_trigger"), "{body}");
    }

    let records = usage.records();
    assert_eq!(
        records.len(),
        case.want_records,
        "usage records: {records:?}"
    );
    for record in &records {
        assert_eq!(record["failed"], case.want_status != 0, "{record}");
        if case.want_status != 0 {
            assert_eq!(record["fail"]["status_code"], case.want_status, "{record}");
        }
        if case.body == valid_state {
            assert_eq!(record["tokens"]["total_tokens"], 3, "{record}");
        }
    }
}

// TestApplyPatchRepairOrdinaryEmptyHTTPPassthrough: without the Kimi row. A
// call that declares `apply_patch` as an ordinary function and gets an
// empty answer fails as it would without the bridge.
#[tokio::test]
async fn ordinary_empty_http_passthrough() {
    let mut cases = Vec::new();
    for provider in [Xai, Meta] {
        cases.push(case(provider.name().to_owned(), async move {
            let mock = Mock::start(Reply::json("")).await;
            let executor = provider.executor();
            let auth = Arc::new(provider.auth("", &mock.url));
            let result = execute(
                &*executor,
                auth,
                request(
                    "grok-4",
                    r#"{"input":"ordinary","tools":[{"type":"function","name":"apply_patch"}]}"#,
                ),
                options(false),
            )
            .await;
            let error = result.expect_err("the call succeeded");
            assert_eq!(error.status, 408, "{error:?}");
        }));
    }
    subtests(cases).await;
}
