//! A `compaction_trigger` on the WebSocket, answered over HTTP with the
//! session's history, ported from upstream's
//! `xai_websockets_executor_test.go`.

use std::time::SystemTime;

use open_ferry_core::executor::ProviderExecutor as _;
use serde_json::{Value, json};

use super::super::message::validate_compaction;
use super::{auth_as, executor, refused, streamed, ws_options};
use crate::codex::websocket::mock::{Answer, Server};
use crate::codex::xai_replay_cache::tests::grok_content;
use crate::json::{exists, str_at};

/// The JSON events of SSE chunks.
fn sse_events(chunks: &[String]) -> Vec<Value> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.lines())
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect()
}

/// Whether `body` has an input item of `item_type`.
fn has_item(body: &Value, item_type: &str) -> bool {
    body["input"]
        .as_array()
        .is_some_and(|input| input.iter().any(|item| item["type"] == item_type))
}

/// An answer of the compact call: `body` as JSON.
fn compact_answer(body: &str) -> Answer {
    Answer::Refuse {
        status: 200,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_owned(),
    }
}

/// The `n`th body the server read over plain HTTP, as JSON.
fn compact_body(server: &Server, n: usize) -> Value {
    let record = server.record();
    serde_json::from_str(
        record
            .bodies
            .get(n)
            .unwrap_or_else(|| panic!("no body {n}: {record:?}")),
    )
    .unwrap()
}

// TestXAIWebsocketsExecuteStreamCompactionTriggerUsesHTTPCompactWithRecordedContext.
// The mock answers its first connection as the WebSocket and the next as
// the compact call.
#[tokio::test]
async fn compaction_trigger_uses_http_compact_with_recorded_context() {
    let encrypted = grok_content(41);
    let compact = json!({"id": "resp_compact", "model": "grok-4.3", "output": [{"type": "compaction", "encrypted_content": encrypted}], "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}}).to_string();
    let server = Server::start(move |n| {
        if n > 0 {
            return compact_answer(&compact);
        }
        Answer::accept(|mut peer| async move {
            for (id, output, text) in [
                ("resp-real", "out-1", "first answer"),
                ("resp-after-compact", "out-2", "second answer"),
            ] {
                if peer.recv().await.is_none() {
                    return;
                }
                let completed = json!({"type": "response.completed", "response": {"id": id, "output": [{"type": "message", "id": output, "role": "assistant", "content": text}], "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}}});
                peer.send(&completed.to_string()).await;
            }
            peer.hold().await;
        })
    })
    .await;
    let executor = executor();
    let auth = auth_as("xai-auth-compaction", super::API_KEY, &server.url);
    let session = "xai-compaction-session";

    streamed(
        &executor,
        &auth,
        r#"{"model":"grok-4.3","stream":true,"input":[{"type":"message","id":"msg-1","role":"user","content":"first"}]}"#,
        ws_options(session),
    )
    .await;
    let first = super::message(&server, 0);
    assert_eq!(str_at(&first, "type"), "response.create", "{first}");
    assert_eq!(first["input"].as_array().unwrap().len(), 1, "{first}");
    assert!(!exists(&first, "stream"), "{first}");

    let chunks = streamed(
        &executor,
        &auth,
        r#"{"model":"grok-4.3","stream":true,"previous_response_id":"resp-real-xai-1","input":[{"type":"compaction_trigger"}]}"#,
        ws_options(session),
    )
    .await;
    let events = sse_events(&chunks);
    let completed = events
        .iter()
        .rfind(|event| event["type"] == "response.completed")
        .unwrap_or_else(|| panic!("no response.completed: {chunks:?}"));
    assert_eq!(
        str_at(completed, "response.id"),
        "resp_compact",
        "{completed}"
    );
    assert_eq!(
        str_at(completed, "response.output.0.type"),
        "compaction",
        "{completed}"
    );
    let body = compact_body(&server, 0);
    assert!(!has_item(&body, "compaction_trigger"), "{body}");
    let ids: Vec<String> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| str_at(item, "id"))
        .collect();
    assert_eq!(ids, ["msg-1", "out-1"], "{body}");
    assert_eq!(str_at(&body, "previous_response_id"), "", "{body}");
    assert_eq!(
        server.record().handshakes[1].path,
        "/responses/compact",
        "the compact call went elsewhere"
    );

    streamed(
        &executor,
        &auth,
        r#"{"model":"grok-4.3","stream":true,"previous_response_id":"resp_compact","input":[{"type":"message","id":"msg-2","role":"user","content":"second"}]}"#,
        ws_options(session),
    )
    .await;
    let next = super::message(&server, 1);
    assert_eq!(str_at(&next, "previous_response_id"), "", "{next}");
    let input = next["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "{next}");
    assert_eq!(input[0]["type"], "compaction", "{next}");
    assert_eq!(input[0]["encrypted_content"], encrypted, "{next}");
    assert_eq!(input[1]["id"], "msg-2", "{next}");
    // The WebSocket was dialled once; the compact call was the other
    // connection.
    assert_eq!(server.record().handshakes.len(), 2);
    executor.close_execution_session(session);
}

// TestXAIWebsocketsCompactionTriggerFreshSessionFallback
#[tokio::test]
async fn compaction_trigger_fresh_session_fallback() {
    let server = Server::start(|_| {
        compact_answer(
            r#"{"id":"resp_compact_fresh","output":[{"id":"cmp-1","type":"compaction","encrypted_content":"ZW5jcnlwdGVk"}]}"#,
        )
    })
    .await;
    let executor = executor();
    let auth = auth_as("xai-auth-compaction-fresh", super::API_KEY, &server.url);

    // A new session whose input has history and the trigger (and a
    // previous_response_id to drop).
    streamed(
        &executor,
        &auth,
        r#"{"model":"grok-4.3","stream":true,"previous_response_id":"resp-should-be-dropped","input":[{"type":"message","id":"msg-fresh-1","role":"user","content":"hello"},{"type":"compaction_trigger"}]}"#,
        ws_options("xai-compaction-fresh-input-session"),
    )
    .await;
    let body = compact_body(&server, 0);
    assert!(!has_item(&body, "compaction_trigger"), "{body}");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "{body}");
    assert_eq!(input[0]["id"], "msg-fresh-1", "{body}");
    assert_eq!(str_at(&body, "previous_response_id"), "", "{body}");

    // A new session with only the trigger and a previous_response_id.
    streamed(
        &executor,
        &auth,
        r#"{"model":"grok-4.3","stream":true,"previous_response_id":"resp-prev-123","input":[{"type":"compaction_trigger"}]}"#,
        ws_options("xai-compaction-fresh-prev-session"),
    )
    .await;
    let body = compact_body(&server, 1);
    assert!(!has_item(&body, "compaction_trigger"), "{body}");
    assert_eq!(
        str_at(&body, "previous_response_id"),
        "resp-prev-123",
        "{body}"
    );

    // A new session with nothing but the trigger.
    let error = refused(
        &executor,
        &auth,
        r#"{"model":"grok-4.3","stream":true,"input":[{"type":"compaction_trigger"}]}"#,
        ws_options("xai-compaction-fresh-empty-session"),
    )
    .await;
    assert_eq!(error.status, 400, "{error:?}");
    assert!(
        error
            .message
            .contains("xai websocket compaction context is empty"),
        "{error:?}"
    );
    assert_eq!(server.record().bodies.len(), 2);
    assert!(
        server
            .record()
            .handshakes
            .iter()
            .all(|h| h.path == "/responses/compact")
    );
    for session in [
        "xai-compaction-fresh-input-session",
        "xai-compaction-fresh-prev-session",
        "xai-compaction-fresh-empty-session",
    ] {
        executor.close_execution_session(session);
    }
}

// Not upstream's: a trigger outside any session (and without a prompt
// cache key) has no history to compact, and an answer without a
// compaction fails the call with a 502.
#[tokio::test]
async fn compaction_trigger_needs_context_and_a_compaction() {
    let server = Server::start(|_| compact_answer(r#"{"id":"resp_compact","output":[]}"#)).await;
    let executor = executor();
    let auth = auth_as("xai-auth-compaction-bad", super::API_KEY, &server.url);
    let trigger = r#"{"model":"grok-4.3","input":[{"type":"message","id":"msg-1","role":"user","content":"hi"},{"type":"compaction_trigger"}]}"#;

    let error = refused(&executor, &auth, trigger, ws_options("")).await;
    assert_eq!(error.status, 400, "{error:?}");
    assert!(
        error
            .message
            .contains("xai websocket compaction context is unavailable"),
        "{error:?}"
    );
    assert!(server.record().handshakes.is_empty());

    let error = refused(&executor, &auth, trigger, ws_options("bad-compaction")).await;
    assert_eq!(error.status, 502, "{error:?}");
    assert!(
        error.message.contains("missing compacted state"),
        "{error:?}"
    );
    assert_eq!(server.record().bodies.len(), 1);
    executor.close_execution_session("bad-compaction");
}

// TestValidateXAIWebsocketCompactionResponse
#[test]
fn validate_compaction_response() {
    let (id, item) = validate_compaction(
        br#"{"id":"resp_compact","output":[{"type":"compaction","encrypted_content":"opaque-state"}]}"#,
        SystemTime::now(),
    )
    .unwrap();
    assert_eq!(id, "resp_compact");
    assert_eq!(item["encrypted_content"], "opaque-state", "{item}");

    for payload in [
        "",
        "{}",
        r#"{"id":"resp_empty","output":[]}"#,
        r#"{"id":123,"output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
        r#"{"id":"resp_object","output":{"0":{"type":"compaction","encrypted_content":"opaque"}}}"#,
        r#"{"id":"resp_numeric_state","output":[{"type":"compaction","encrypted_content":123}]}"#,
        r#"{"id":"resp_missing_state","output":[{"type":"compaction"}]}"#,
    ] {
        assert!(
            validate_compaction(payload.as_bytes(), SystemTime::now()).is_err(),
            "an invalid compaction was taken: {payload}"
        );
    }
}
