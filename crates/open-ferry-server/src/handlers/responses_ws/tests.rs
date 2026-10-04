// Ported from CLIProxyAPI
// sdk/api/handlers/openai/openai_responses_websocket_test.go and
// sdk/api/handlers/openai/openai_responses_websocket_requests_memory_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses WebSocket's parts against upstream's cases, then whole
//! sessions over a real socket against a scripted dispatcher.
//!
//! Deviations from upstream:
//! - Tests of allocations and retained memory, the client's own WebSocket
//!   timeline in the request log, home runtimes, plugins, provider routes,
//!   model routers, disconnect subscriptions and the writer's lock aren't
//!   ported, as what they test isn't.
//! - Upstream's tests of errors that arrive through a disconnect
//!   subscription run here as errors in the call's stream.
//! - A `str` can't hold bytes that aren't UTF-8, so the close reason cases
//!   with them are left out.
//! - Credentials are told apart by what the dispatcher says of them, where
//!   upstream registers them with an auth manager and the model registry.
//! - The compact request between turns of the transcript reset case is left
//!   out, as `/v1/responses/compact` belongs to the HTTP handler.
//! - The memory test that checks generated transcripts against a reference
//!   merge isn't ported, as the reference merge isn't.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::ws::Message;
use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use open_ferry_core::exec::{ErrorKind, ExecError, WebsocketAuth, WebsocketSupport, WsClose};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

use super::forward::{
    ForwardOptions, Forwarded, OutputItems, canonical_header_key, completed_output_from_payload,
    error_payload, forward, payloads_from_chunk, record_pending_call_ids,
    restore_completion_output, should_expose, should_release_pinned, should_replay_pinned_failure,
};
use super::repair::{
    ServerToolCaches, ToolCache, ToolCacheTurn, ToolCaches, is_complete_tool_call,
    prepare_fallback_turn, record_tool_calls_from_payload, repair,
};
use super::requests::{
    DecodeError, LOCAL_SUMMARY_PREFIX, Normalized, has_local_compaction_summary,
    input_contains_full_transcript, merge_input, normalize, should_replace_transcript,
};
use super::session::{
    Upstream, is_lite_request, native_passthrough_allowed, previous_response_not_found,
    replay_required, requires_current_upstream,
};
use super::writer::{Closed, Conn, close_frame_for, truncate_close_reason};
use super::{check_handshake, is_valid_challenge_key, token_list_contains};
use crate::auth::{Principal, PrincipalTags};
use crate::config::ServerConfig;
use crate::errors::ErrorMessage;
use crate::json::{self, Val};
use crate::state::AppState;
use crate::status::status_text;
use crate::testing::{FakeCatalog, FakeDispatcher, Outcome};

/// `raw` as JSON.
fn parse(raw: &[u8]) -> Value {
    serde_json::from_slice(raw)
        .unwrap_or_else(|err| panic!("{err}: {}", String::from_utf8_lossy(raw)))
}

/// The IDs of `items`, empty for an item without one.
fn item_ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {items}"))
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The IDs of a request's input items.
fn input_ids(request: &[u8]) -> Vec<String> {
    item_ids(&parse(request)["input"])
}

/// `text` as a JSON string.
fn json_str(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

/// A Codex local compaction summary, as a JSON string.
fn summary(body: &str) -> String {
    json_str(&format!("{LOCAL_SUMMARY_PREFIX}{body}"))
}

/// `normalizeResponsesWebsocketRequest`.
fn normalize_default(
    raw: &str,
    last_request: &str,
    last_output: &str,
) -> Result<Normalized, ErrorMessage> {
    normalize_mode(raw, last_request, last_output, true, true)
}

/// `normalizeResponsesWebsocketRequestWithMode`.
fn normalize_mode(
    raw: &str,
    last_request: &str,
    last_output: &str,
    allow_incremental: bool,
    allow_compaction_bypass: bool,
) -> Result<Normalized, ErrorMessage> {
    normalize(
        raw.as_bytes(),
        last_request.as_bytes(),
        last_output.as_bytes(),
        "",
        &[],
        allow_incremental,
        allow_compaction_bypass,
    )
}

/// An upstream WebSocket's close for a message too big, with `reason`.
fn close_too_big(reason: &str) -> ErrorMessage {
    let mut error = ExecError::new(
        ErrorKind::Upstream,
        format!("websocket: close 1009 (message too big): {reason}"),
    );
    error.ws_close = Some(WsClose::MessageTooBig(reason.to_owned()));
    ErrorMessage::from_exec(error)
}

/// The JSON body of a 413 for a message too big.
const MESSAGE_TOO_BIG_BODY: &str =
    r#"{"error":{"message":"upstream websocket message too big","code":"message_too_big"}}"#;

#[test]
fn replay_close_requires_typed_signal() {
    assert_eq!(
        close_frame_for(&replay_required()),
        Some((1012, "upstream requires HTTP replay".to_owned()))
    );
    let spoofed = ErrorMessage::from_exec(ExecError::upstream(
        426,
        r#"{"error":{"code":"upstream_http_replay_required"}}"#,
    ));
    assert_eq!(close_frame_for(&spoofed), None);
}

#[test]
fn request_requires_current_upstream() {
    for (payload, want) in [
        (
            r#"{"type":"response.create","previous_response_id":"resp-1","input":[]}"#,
            true,
        ),
        (r#"{"type":"response.append","input":[]}"#, true),
        (r#"{"type":"response.create","input":[]}"#, false),
    ] {
        assert_eq!(
            requires_current_upstream(payload.as_bytes()),
            want,
            "{payload}"
        );
    }
}

#[test]
fn native_passthrough_requires_immediately_previous_auth() {
    assert!(native_passthrough_allowed(
        Upstream::Websocket,
        true,
        "auth-a",
        "auth-a"
    ));
    assert!(!native_passthrough_allowed(
        Upstream::Websocket,
        true,
        "auth-a",
        "auth-b"
    ));
    assert!(!native_passthrough_allowed(
        Upstream::Http,
        true,
        "auth-a",
        "auth-a"
    ));
}

#[test]
fn close_for_upstream_error_mirrors_message_too_big() {
    let cases = [
        (
            close_too_big("message too big"),
            "message too big".to_owned(),
        ),
        (
            ErrorMessage::from_exec(ExecError::upstream(413, MESSAGE_TOO_BIG_BODY)),
            "upstream websocket message too big".to_owned(),
        ),
        (close_too_big(&"🙂".repeat(31)), "🙂".repeat(30)),
    ];
    for (error, reason) in cases {
        assert_eq!(close_frame_for(&error), Some((1009, reason)));
    }
}

#[tokio::test]
async fn close_for_upstream_error_sends_the_close_frame() {
    let socket = FakeSocket::default();
    let mut conn = Conn::new(socket.clone());
    assert!(
        conn.close_for_upstream_error(&close_too_big("message too big"))
            .await
    );
    assert_eq!(
        close_of(&socket.sent()),
        Some((1009, "message too big".to_owned()))
    );
    assert!(conn.write(b"{}").await.is_err());

    let socket = FakeSocket::default();
    let mut conn = Conn::new(socket.clone());
    assert!(
        !conn
            .close_for_upstream_error(&ErrorMessage::new(500, "boom"))
            .await
    );
    assert!(socket.sent().is_empty());
}

#[test]
fn truncate_websocket_close_reason() {
    assert_eq!(truncate_close_reason("message too big", 0), "");
    assert_eq!(
        truncate_close_reason("message too big", 123),
        "message too big"
    );
    assert_eq!(
        truncate_close_reason(&"x".repeat(1 << 20), 123),
        "x".repeat(123)
    );
    assert_eq!(truncate_close_reason("ab🙂cd", 5), "ab");
}

#[test]
fn normalize_request_create() {
    let raw = r#"{"type":"response.create","model":"test-model","stream":false,"input":[{"type":"message","id":"msg-1"}]}"#;
    let (normalized, last) = normalize_default(raw, "", "").unwrap();
    let doc = parse(&normalized);
    assert!(doc.get("type").is_none(), "{doc}");
    assert_eq!(doc["stream"], true);
    assert_eq!(doc["model"], "test-model");
    assert_eq!(last, normalized);
}

const LAST_REQUEST: &str =
    r#"{"model":"test-model","stream":true,"input":[{"type":"message","id":"msg-1"}]}"#;

const LAST_REQUEST_WITH_INSTRUCTIONS: &str = r#"{"model":"test-model","stream":true,"instructions":"be helpful","input":[{"type":"message","id":"msg-1"}]}"#;

const CALL_THEN_MESSAGE: &str = r#"[
    {"type":"function_call","id":"fc-1","call_id":"call-1"},
    {"type":"message","id":"assistant-1"}
]"#;

#[test]
fn normalize_request_create_with_history() {
    let raw = r#"{"type":"response.create","input":[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]}"#;
    let (normalized, next) = normalize_default(raw, LAST_REQUEST, CALL_THEN_MESSAGE).unwrap();
    let doc = parse(&normalized);
    assert!(doc.get("type").is_none(), "{doc}");
    assert_eq!(doc["model"], "test-model");
    assert_eq!(
        input_ids(&normalized),
        ["msg-1", "fc-1", "assistant-1", "tool-out-1"]
    );
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_with_previous_response_id_incremental() {
    let raw = r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]}"#;
    let (normalized, next) = normalize_mode(
        raw,
        LAST_REQUEST_WITH_INSTRUCTIONS,
        CALL_THEN_MESSAGE,
        true,
        false,
    )
    .unwrap();
    let doc = parse(&normalized);
    assert!(doc.get("type").is_none(), "{doc}");
    assert_eq!(doc["previous_response_id"], "resp-1");
    assert_eq!(input_ids(&normalized), ["tool-out-1"]);
    assert_eq!(doc["model"], "test-model");
    assert_eq!(doc["instructions"], "be helpful");
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_injects_previous_response_id_for_incremental() {
    let raw = r#"{"type":"response.create","input":[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]}"#;
    let (normalized, next) = normalize(
        raw.as_bytes(),
        LAST_REQUEST_WITH_INSTRUCTIONS.as_bytes(),
        CALL_THEN_MESSAGE.as_bytes(),
        "resp-1",
        &[],
        true,
        false,
    )
    .unwrap();
    let doc = parse(&normalized);
    assert_eq!(doc["previous_response_id"], "resp-1");
    assert_eq!(input_ids(&normalized), ["tool-out-1"]);
    assert_eq!(doc["model"], "test-model");
    assert_eq!(doc["instructions"], "be helpful");
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_injects_previous_response_id_when_pending_output_is_present() {
    let raw = r#"{"type":"response.create","input":[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]}"#;
    let (normalized, _) = normalize(
        raw.as_bytes(),
        LAST_REQUEST_WITH_INSTRUCTIONS.as_bytes(),
        b"[]",
        "resp-1",
        &["call-1".to_owned()],
        true,
        false,
    )
    .unwrap();
    assert_eq!(parse(&normalized)["previous_response_id"], "resp-1");
    assert_eq!(input_ids(&normalized), ["tool-out-1"]);
}

#[test]
fn normalize_request_skips_previous_response_id_when_pending_output_is_missing() {
    let raw = r#"{"type":"response.create","input":[{"type":"message","role":"user","id":"summary-1","content":"compacted summary"}]}"#;
    let (normalized, next) = normalize(
        raw.as_bytes(),
        LAST_REQUEST_WITH_INSTRUCTIONS.as_bytes(),
        br#"[{"type":"function_call","id":"fc-1","call_id":"call-1"}]"#,
        "resp-1",
        &["call-1".to_owned()],
        true,
        false,
    )
    .unwrap();
    assert!(parse(&normalized).get("previous_response_id").is_none());
    assert_eq!(input_ids(&normalized), ["summary-1"]);
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_replaces_codex_local_compaction_transcript() {
    let last_request = r#"{"model":"gpt-5.6-sol","stream":true,"instructions":"be helpful","input":[
        {"type":"message","role":"user","id":"old-user","content":[{"type":"input_text","text":"old prompt"}]},
        {"type":"function_call_output","id":"old-tool-output","call_id":"old-call","output":"old result"}
    ]}"#;
    let last_output = r#"[
        {"type":"function_call","id":"old-tool-call","call_id":"old-call","name":"lookup","arguments":"{}"},
        {"type":"message","role":"assistant","id":"old-assistant","content":[{"type":"output_text","text":"old answer"}]}
    ]"#;
    let raw = r#"{"type":"response.create","input":[
        {"type":"additional_tools","role":"developer","tools":[]},
        {"role":"developer","id":"initial-context","content":"workspace context"},
        {"type":"message","role":"user","id":"compacted-user","content":[{"type":"input_text","text":"retained context"}]},
        {"role":"user","id":"local-summary","content":SUMMARY},
        {"type":"message","role":"developer","id":"turn-context","content":[{"type":"input_text","text":"current workspace context"}]},
        {"role":"user","id":"incoming-user","content":"continue the task"}
    ],"parallel_tool_calls":true,"client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":"true"}}"#
        .replace("SUMMARY", &summary("\nThe compacted summary."));

    let (normalized, next) = normalize_mode(&raw, last_request, last_output, false, false).unwrap();
    let doc = parse(&normalized);
    assert!(doc.get("previous_response_id").is_none(), "{doc}");
    assert_eq!(
        json::get(&normalized, "input").unwrap().raw,
        json::get(raw.as_bytes(), "input").unwrap().raw
    );
    assert_eq!(
        input_ids(&normalized),
        [
            "",
            "initial-context",
            "compacted-user",
            "local-summary",
            "turn-context",
            "incoming-user"
        ]
    );
    assert_eq!(doc["input"][0]["type"], "additional_tools");
    assert_eq!(doc["input"][0]["role"], "developer");
    assert_eq!(doc["input"][0]["tools"], Value::Array(Vec::new()));
    let text = String::from_utf8_lossy(&normalized);
    for stale in [
        "old-user",
        "old-tool-output",
        "old-tool-call",
        "old-assistant",
    ] {
        assert!(!text.contains(stale), "{stale} in {text}");
    }
    assert_eq!(doc["model"], "gpt-5.6-sol");
    assert_eq!(doc["instructions"], "be helpful");
    assert_eq!(doc["stream"], true);
    assert_eq!(doc["parallel_tool_calls"], true);
    assert_eq!(
        doc["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
        "true"
    );
    assert_eq!(next, normalized);
}

#[test]
fn should_replace_transcript_codex_local_compaction_semantics() {
    let compacted = r#"[
        {"type":"message","role":"developer","content":[{"type":"input_text","text":"initial context"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"retained context"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":SUMMARY}]}
    ]"#
    .replace("SUMMARY", &summary("\nSummary body."));
    let compacted = Val::parse(compacted.as_bytes()).unwrap();
    assert!(should_replace_transcript(
        br#"{"type":"response.create"}"#,
        compacted
    ));
    for request in [
        r#"{"type":"response.create","previous_response_id":"resp-1"}"#,
        r#"{"type":"response.create","previous_response_id":""}"#,
        r#"{"type":"response.create","previous_response_id":null}"#,
        r#"{"type":"response.append"}"#,
    ] {
        assert!(
            !should_replace_transcript(request.as_bytes(), compacted),
            "{request}"
        );
    }

    let ordinary = br#"[
        {"type":"message","role":"developer","content":"Please summarize future messages."},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"Please create a compacted summary of this text."}]}
    ]"#;
    assert!(!should_replace_transcript(
        br#"{"type":"response.create"}"#,
        Val::parse(ordinary).unwrap()
    ));
}

#[test]
fn codex_local_compaction_summary_content_shapes() {
    let body = summary("\nSummary body.");
    let cases = [
        ("string content", "user", body.clone(), true),
        (
            "multiple input text parts",
            "user",
            format!(
                r#"[{{"type":"input_text","text":{}}},{{"type":"input_text","text":{}}}]"#,
                json_str(LOCAL_SUMMARY_PREFIX),
                json_str("\nSummary body.")
            ),
            true,
        ),
        (
            "non-text part before summary",
            "user",
            format!(
                r#"[{{"type":"input_image","image_url":"data:image/png;base64,AA=="}},{{"type":"input_text","text":{body}}}]"#
            ),
            true,
        ),
        ("bare prefix", "user", json_str(LOCAL_SUMMARY_PREFIX), false),
        (
            "prefix followed by space",
            "user",
            summary(" Summary body."),
            false,
        ),
        (
            "summary after ordinary text",
            "user",
            format!(
                r#"[{{"type":"input_text","text":"ordinary text"}},{{"type":"input_text","text":{body}}}]"#
            ),
            false,
        ),
        ("developer summary", "developer", body.clone(), false),
    ];
    for (name, role, content, want) in cases {
        let input = format!(r#"[{{"type":"message","role":"{role}","content":{content}}}]"#);
        assert_eq!(
            has_local_compaction_summary(Val::parse(input.as_bytes()).unwrap()),
            want,
            "{name}"
        );
    }
}

#[test]
fn codex_local_compaction_summary_additional_tools_constraints() {
    let item = format!(
        r#"{{"role":"user","content":{}}}"#,
        summary("\nSummary body.")
    );
    let cases = [
        (
            "Responses Lite tools first",
            r#"[{"type":"additional_tools","role":"developer","tools":[{"type":"custom","name":"exec"}]},ITEM]"#,
            true,
        ),
        (
            "tools after message",
            r#"[ITEM,{"type":"additional_tools","role":"developer","tools":[{"type":"custom","name":"exec"}]}]"#,
            false,
        ),
        (
            "tools with user role",
            r#"[{"type":"additional_tools","role":"user","tools":[{"type":"custom","name":"exec"}]},ITEM]"#,
            false,
        ),
        (
            "tools missing array",
            r#"[{"type":"additional_tools","role":"developer"},ITEM]"#,
            false,
        ),
        (
            "tools not array",
            r#"[{"type":"additional_tools","role":"developer","tools":{}},ITEM]"#,
            false,
        ),
        (
            "tools empty",
            r#"[{"type":"additional_tools","role":"developer","tools":[]},ITEM]"#,
            true,
        ),
        (
            "malformed tool",
            r#"[{"type":"additional_tools","role":"developer","tools":[null]},ITEM]"#,
            false,
        ),
        (
            "arbitrary input item",
            r#"[{"type":"unknown","role":"developer"},ITEM]"#,
            false,
        ),
    ];
    for (name, input, want) in cases {
        let input = input.replace("ITEM", &item);
        assert_eq!(
            has_local_compaction_summary(Val::parse(input.as_bytes()).unwrap()),
            want,
            "{name}"
        );
    }
}

#[test]
fn codex_local_compaction_summary_rejects_ordinary_history_items() {
    let cases = [
        (r#"{"type":"reasoning","id":"reasoning-1"}"#, false),
        (
            r#"{"type":"message","role":"assistant","id":"assistant-1"}"#,
            true,
        ),
        (r#"{"type":"function_call","call_id":"call-1"}"#, true),
        (
            r#"{"type":"function_call_output","call_id":"call-1"}"#,
            false,
        ),
        (r#"{"type":"custom_tool_call","call_id":"call-1"}"#, true),
        (
            r#"{"type":"custom_tool_call_output","call_id":"call-1"}"#,
            false,
        ),
    ];
    for (history, want_replace) in cases {
        let input = format!(
            r#"[{history},{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}]"#,
            summary("\nSummary body.")
        );
        let input = Val::parse(input.as_bytes()).unwrap();
        assert!(!has_local_compaction_summary(input), "{history}");
        assert_eq!(
            should_replace_transcript(br#"{"type":"response.create"}"#, input),
            want_replace,
            "{history}"
        );
    }
}

#[test]
fn normalize_request_with_previous_response_id_merged_when_incremental_disabled() {
    let raw = r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]}"#;
    let (normalized, next) =
        normalize_mode(raw, LAST_REQUEST, CALL_THEN_MESSAGE, false, false).unwrap();
    assert!(parse(&normalized).get("previous_response_id").is_none());
    assert_eq!(
        input_ids(&normalized),
        ["msg-1", "fc-1", "assistant-1", "tool-out-1"]
    );
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_append() {
    let last_output = r#"[
        {"type":"message","id":"assistant-1"},
        {"type":"function_call_output","id":"tool-out-1"}
    ]"#;
    let raw = r#"{"type":"response.append","input":[{"type":"message","id":"msg-2"},{"type":"message","id":"msg-3"}]}"#;
    let (normalized, next) = normalize_default(raw, LAST_REQUEST, last_output).unwrap();
    assert_eq!(
        input_ids(&normalized),
        ["msg-1", "assistant-1", "tool-out-1", "msg-2", "msg-3"]
    );
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_append_without_create() {
    let error = normalize_default(r#"{"type":"response.append","input":[]}"#, "", "").unwrap_err();
    assert_eq!(error.status, 400);
}

#[test]
fn json_payloads_from_chunk() {
    let chunk = concat!(
        "event: response.created\n\n",
        r#"data: {"type":"response.created","response":{"id":"resp-1"}}"#,
        "\n\ndata: [DONE]\n"
    );
    let payloads = payloads_from_chunk(chunk.as_bytes());
    assert_eq!(payloads.len(), 1);
    assert_eq!(parse(&payloads[0])["type"], "response.created");
}

#[test]
fn json_payloads_from_plain_json_chunk() {
    let payloads =
        payloads_from_chunk(br#"{"type":"response.completed","response":{"id":"resp-1"}}"#);
    assert_eq!(payloads.len(), 1);
    assert_eq!(parse(&payloads[0])["type"], "response.completed");
}

/// Output items by index.
fn indexed(items: &[&str]) -> OutputItems {
    let mut outputs = OutputItems::default();
    for (index, item) in (0..).zip(items) {
        outputs.by_index.insert(index, item.as_bytes().to_vec());
    }
    outputs
}

#[test]
fn response_completed_output_from_payload() {
    let payload = br#"{"type":"response.completed","response":{"id":"resp-1","output":[{"type":"message","id":"out-1"}]}}"#;
    let output = completed_output_from_payload(payload, &OutputItems::default());
    assert_eq!(item_ids(&parse(&output)), ["out-1"]);
}

#[test]
fn response_completed_output_from_payload_drops_incomplete_collected_tool_calls() {
    let payload = br#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#;
    let outputs = indexed(&[
        r#"{"type":"message","id":"msg-1"}"#,
        r#"{"type":"function_call","call_id":"call-1","name":"exec"}"#,
        r#"{"type":"custom_tool_call","call_id":"call-2","name":"exec","input":"pwd"}"#,
    ]);
    let output = parse(&completed_output_from_payload(payload, &outputs));
    assert_eq!(output.as_array().unwrap().len(), 2, "{output}");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["id"], "msg-1");
    assert_eq!(output[1]["type"], "custom_tool_call");
    assert_eq!(output[1]["call_id"], "call-2");
}

#[test]
fn restore_completion_output_preserves_non_empty_output() {
    let payload = br#"{"type":"response.completed","response":{"id":"resp-1","output":[{"type":"message","id":"out-1"}]}}"#;
    let outputs = indexed(&[r#"{"type":"function_call","id":"call-1","call_id":"call-1"}"#]);
    assert_eq!(restore_completion_output(payload, &outputs), payload);
}

#[test]
fn restore_completion_output_reconciles_conflicting_tool_call() {
    let payload = br#"{"type":"response.completed","response":{"id":"resp-1","output":[{"type":"message","id":"msg-1"},{"type":"function_call","call_id":"call-1","name":"exec"}]}}"#;
    let outputs = indexed(&[
        r#"{"type":"custom_tool_call","id":"ctc-1","call_id":"call-1","name":"exec","input":"pwd","status":"completed"}"#,
    ]);
    let restored = restore_completion_output(payload, &outputs);
    let output = &parse(&restored)["response"]["output"];
    assert_eq!(output.as_array().unwrap().len(), 2, "{output}");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["id"], "msg-1");
    assert_eq!(output[1]["type"], "custom_tool_call");
    assert_eq!(output[1]["call_id"], "call-1");
    assert_eq!(output[1]["input"], "pwd");

    let last_request = br#"{"model":"gpt-test","stream":true,"input":[{"type":"message","id":"user-1","role":"user","content":"run pwd"}]}"#;
    let next_request = br#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"custom_tool_call_output","call_id":"call-1","output":"ok"}]}"#;
    let completed = json::get(&restored, "response.output").unwrap().raw;
    let (normalized, _) = normalize(
        next_request,
        last_request,
        completed,
        "resp-1",
        &["call-1".to_owned()],
        false,
        false,
    )
    .unwrap();
    let doc = parse(&normalized);
    assert!(doc.get("previous_response_id").is_none(), "{doc}");
    let input = doc["input"].as_array().unwrap();
    assert_eq!(input.len(), 4, "{doc}");
    assert_eq!(input[2]["type"], "custom_tool_call");
    assert_eq!(input[2]["input"], "pwd");
    assert_eq!(input[3]["type"], "custom_tool_call_output");
    assert_eq!(input[3]["call_id"], "call-1");

    let mut cache = ToolCache::default();
    let done = br#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","id":"ctc-1","call_id":"call-1","name":"exec","input":"pwd","status":"completed"}}"#;
    record_tool_calls_from_payload(&mut cache, "session-1", done);
    record_tool_calls_from_payload(&mut cache, "session-1", &restored);
    let cached = parse(cache.get("session-1", "call-1").expect("cached call"));
    assert_eq!(cached["type"], "custom_tool_call");
    assert_eq!(cached["input"], "pwd");
}

#[test]
fn restore_completion_output_ignores_incomplete_collected_tool_call() {
    let payload = br#"{"type":"response.completed","response":{"id":"resp-1","output":[{"type":"function_call","call_id":"call-1","name":"exec"}]}}"#;
    let outputs = indexed(&[r#"{"type":"custom_tool_call","call_id":"call-1","name":"exec"}"#]);
    assert_eq!(restore_completion_output(payload, &outputs), payload);
}

#[test]
fn is_complete_tool_call_requires_string_fields() {
    for (item, want) in [
        (
            r#"{"type":"function_call","call_id":123,"name":"exec","arguments":"{}"}"#,
            false,
        ),
        (
            r#"{"type":"function_call","call_id":"call-1","name":true,"arguments":"{}"}"#,
            false,
        ),
        (
            r#"{"type":"function_call","call_id":"call-1","name":"exec","arguments":123}"#,
            false,
        ),
        (
            r#"{"type":"custom_tool_call","call_id":"call-1","name":"exec","input":{}}"#,
            false,
        ),
        (
            r#"{"type":"function_call","call_id":"call-1","name":"exec","arguments":""}"#,
            true,
        ),
        (
            r#"{"type":"custom_tool_call","call_id":"call-1","name":"exec","input":""}"#,
            true,
        ),
    ] {
        assert_eq!(
            is_complete_tool_call(Val::parse(item.as_bytes())),
            want,
            "{item}"
        );
    }
}

/// A socket that keeps what is sent to it and has nothing to read.
#[derive(Clone, Default)]
struct FakeSocket {
    sent: Arc<Mutex<Vec<Message>>>,
    fail_ping: bool,
}

impl FakeSocket {
    fn sent(&self) -> Vec<Message> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Sink<Message> for FakeSocket {
    type Error = axum::Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
        if self.fail_ping && matches!(message, Message::Ping(_)) {
            return Err(axum::Error::new(std::io::Error::other("ping failed")));
        }
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl Stream for FakeSocket {
    type Item = Result<Message, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

/// The code and reason of the last close frame in `sent`.
fn close_of(sent: &[Message]) -> Option<(u16, String)> {
    sent.iter().rev().find_map(|message| match message {
        Message::Close(Some(frame)) => Some((frame.code, frame.reason.to_string())),
        _ => None,
    })
}

/// The text messages in `sent`, as JSON.
fn texts(sent: &[Message]) -> Vec<Value> {
    sent.iter()
        .filter_map(|message| match message {
            Message::Text(text) => Some(parse(text.as_str().as_bytes())),
            _ => None,
        })
        .collect()
}

/// How many pings are in `sent`.
fn pings(sent: &[Message]) -> usize {
    sent.iter()
        .filter(|message| matches!(message, Message::Ping(_)))
        .count()
}

/// `json` as an SSE event.
fn sse(json: &str) -> String {
    format!("data: {json}\n\n")
}

#[tokio::test]
async fn writer_write_ping() {
    let socket = FakeSocket::default();
    let mut conn = Conn::new(socket.clone());
    assert_eq!(conn.ping().await, Ok(()));
    assert_eq!(pings(&socket.sent()), 1);
    assert!(conn.close_without_error());
    assert_eq!(conn.ping().await, Err(Closed));
}

/// Each item as its type and its call ID, or its ID without one.
fn kinds(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {items}"))
        .iter()
        .map(|item| {
            let key = item["call_id"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .unwrap_or_default();
            format!("{} {key}", item["type"].as_str().unwrap_or_default())
        })
        .collect()
}

/// `repairResponsesWebsocketToolCallsWithCaches` for `session-1`.
fn repair_with(caches: &mut ToolCaches, raw: &str) -> Value {
    parse(&repair(caches, "session-1", raw.as_bytes(), true, None))
}

/// The tool call and output types, with the name upstream's cases give each.
const TOOL_KINDS: [(&str, &str, &str); 2] = [
    ("function_call", "function_call_output", "tool"),
    ("custom_tool_call", "custom_tool_call_output", "apply_patch"),
];

#[test]
fn repair_tool_calls_inserts_cached_output() {
    for (call, output, name) in TOOL_KINDS {
        let mut caches = ToolCaches::default();
        let warm = format!(
            r#"{{"previous_response_id":"resp-1","input":[{{"type":"{output}","call_id":"call-1","output":"ok"}}]}}"#
        );
        assert_eq!(
            repair_with(&mut caches, &warm)["input"][0]["call_id"],
            "call-1"
        );
        let raw = format!(
            r#"{{"input":[{{"type":"{call}","call_id":"call-1","name":"{name}"}},{{"type":"message","id":"msg-1"}}]}}"#
        );
        assert_eq!(
            kinds(&repair_with(&mut caches, &raw)["input"]),
            [
                format!("{call} call-1"),
                format!("{output} call-1"),
                "message msg-1".to_owned()
            ]
        );
    }
}

#[test]
fn repair_tool_calls_dedupes_input_items_by_id() {
    let raw = br#"{"input":[{"type":"message","id":"msg-1","content":"old"},{"type":"message","id":"msg-1","content":"new"}]}"#;
    for session_key in ["dedupe-session", ""] {
        let repaired = parse(&repair(
            &mut ToolCaches::default(),
            session_key,
            raw,
            true,
            None,
        ));
        let input = repaired["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "{session_key:?}: {repaired}");
        assert_eq!(input[0]["content"], "new");
    }
}

#[test]
fn repair_tool_calls_drops_orphan_call() {
    for (call, _, name) in TOOL_KINDS {
        let raw = format!(
            r#"{{"input":[{{"type":"{call}","call_id":"call-1","name":"{name}"}},{{"type":"message","id":"msg-1"}}]}}"#
        );
        assert_eq!(
            kinds(&repair_with(&mut ToolCaches::default(), &raw)["input"]),
            ["message msg-1"]
        );
    }
}

#[test]
fn repair_tool_calls_inserts_cached_call_for_orphan_output() {
    for (call, output, name) in TOOL_KINDS {
        let mut caches = ToolCaches::default();
        let cached = format!(r#"{{"type":"{call}","call_id":"call-1","name":"{name}"}}"#);
        caches
            .calls
            .record("session-1", "call-1", cached.as_bytes());
        let raw = format!(
            r#"{{"input":[{{"type":"{output}","call_id":"call-1","output":"ok"}},{{"type":"message","id":"msg-1"}}]}}"#
        );
        assert_eq!(
            kinds(&repair_with(&mut caches, &raw)["input"]),
            [
                format!("{call} call-1"),
                format!("{output} call-1"),
                "message msg-1".to_owned()
            ]
        );
    }
}

#[test]
fn repair_tool_calls_keeps_previous_response_output_incremental() {
    for (call, output, name) in TOOL_KINDS {
        let mut caches = ToolCaches::default();
        let cached =
            format!(r#"{{"type":"{call}","id":"fc-1","call_id":"call-1","name":"{name}"}}"#);
        caches
            .calls
            .record("session-1", "call-1", cached.as_bytes());
        let raw = format!(
            r#"{{"previous_response_id":"resp-latest","input":[{{"type":"{output}","call_id":"call-1","id":"tool-out-1","output":"ok"}},{{"type":"message","id":"msg-1"}}]}}"#
        );
        let repaired = repair_with(&mut caches, &raw);
        assert_eq!(repaired["previous_response_id"], "resp-latest");
        assert_eq!(
            kinds(&repaired["input"]),
            [format!("{output} call-1"), "message msg-1".to_owned()]
        );
    }
}

#[test]
fn repair_tool_calls_keeps_previous_response_call_incremental() {
    let mut caches = ToolCaches::default();
    caches.outputs.record(
        "session-1",
        "call-1",
        br#"{"type":"function_call_output","call_id":"call-1","id":"tool-out-1","output":"ok"}"#,
    );
    let raw = r#"{"previous_response_id":"resp-latest","input":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"tool"},{"type":"message","id":"msg-1"}]}"#;
    let repaired = repair_with(&mut caches, raw);
    assert_eq!(repaired["previous_response_id"], "resp-latest");
    assert_eq!(
        kinds(&repaired["input"]),
        ["function_call call-1", "message msg-1"]
    );
}

#[test]
fn repair_tool_calls_drops_orphan_output_when_call_missing() {
    for (_, output, _) in TOOL_KINDS {
        let raw = format!(
            r#"{{"input":[{{"type":"{output}","call_id":"call-1","output":"ok"}},{{"type":"message","id":"msg-1"}}]}}"#
        );
        assert_eq!(
            kinds(&repair_with(&mut ToolCaches::default(), &raw)["input"]),
            ["message msg-1"]
        );
    }
}

/// The request a cached `call-1` output pairs with.
const NEXT_CALL: &[u8] = br#"{"input":[{"type":"function_call","id":"fc-next","call_id":"call-1","name":"lookup","arguments":"{}"}]}"#;

#[test]
fn tool_cache_turn_commits_only_on_success() {
    let key = "tool-cache-turn-commit-session";
    let mut caches = ToolCaches::default();
    let (_, turn) = prepare_fallback_turn(
        &mut caches,
        key,
        br#"{"input":[{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"cached result"}]}"#,
    );
    let before = parse(&repair(&mut caches, key, NEXT_CALL, false, None));
    turn.expect("turn").commit(&mut caches);
    let after = parse(&repair(&mut caches, key, NEXT_CALL, false, None));
    assert_eq!(before["input"], Value::Array(Vec::new()), "{before}");
    let input = after["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "{after}");
    assert_eq!(input[1]["output"], "cached result");
}

#[test]
fn tool_cache_retain_prevents_overlapping_release_deletion() {
    let key = "tool-cache-overlapping-retain-session";
    let mut caches = ToolCaches::default();
    caches.retain(key);
    caches.retain(key);
    let (_, turn) = prepare_fallback_turn(
        &mut caches,
        key,
        br#"{"input":[{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"kept"}]}"#,
    );
    turn.expect("turn").commit(&mut caches);
    caches.release(key);
    assert!(caches.outputs.get(key, "call-1").is_some());
    caches.release(key);
    assert!(caches.outputs.get(key, "call-1").is_none());
}

/// A request with `call-1` and its output, as Alice sends it in review3.
const CALL_AND_OUTPUT: &[u8] = br#"{"input":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"secret","arguments":"{\"password\":\"alice-private\"}"},{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"alice private result"}]}"#;

/// A request with only an output for `call-1`.
const ORPHAN_OUTPUT: &[u8] = br#"{"input":[{"type":"function_call_output","id":"fco-2","call_id":"call-1","output":"attacker"}]}"#;

#[test]
fn tool_caches_are_kept_per_principal() {
    let tags = PrincipalTags::default();
    let (alice, bob) = (tags.of("sk-alice"), tags.of("sk-bob"));
    let server = ServerToolCaches::default();
    for principal in [alice, bob, Principal::ANONYMOUS] {
        server.lock(principal).retain("shared");
    }
    let (_, turn) = prepare_fallback_turn(&mut server.lock(alice), "shared", CALL_AND_OUTPUT);
    turn.expect("turn").commit(&mut server.lock(alice));

    for principal in [bob, Principal::ANONYMOUS] {
        let repaired = parse(&repair(
            &mut server.lock(principal),
            "shared",
            ORPHAN_OUTPUT,
            false,
            None,
        ));
        assert_eq!(repaired["input"], Value::Array(Vec::new()), "{repaired}");
    }
    let repaired = parse(&repair(
        &mut server.lock(alice),
        "shared",
        ORPHAN_OUTPUT,
        false,
        None,
    ));
    assert_eq!(item_ids(&repaired["input"]), ["fc-1", "fco-2"]);

    // A principal's caches go once its last session is released.
    server.lock(alice).release("shared");
    let repaired = parse(&repair(
        &mut server.lock(alice),
        "shared",
        ORPHAN_OUTPUT,
        false,
        None,
    ));
    assert_eq!(repaired["input"], Value::Array(Vec::new()), "{repaired}");
}

/// How many items Go's `encoding/json` decodes as the input: the last
/// `input` key's, matched ignoring case.
fn effective_input_len(payload: &[u8]) -> usize {
    Val::parse(payload)
        .expect("JSON")
        .members()
        .into_iter()
        .rev()
        .find(|(key, _)| json::fold_eq(key, "input"))
        .map_or(0, |(_, input)| input.array().len())
}

/// Commits `turn`, and says whether it kept anything for `call-1`.
fn turn_recorded(session_key: &str, turn: Option<ToolCacheTurn>) -> bool {
    let mut caches = ToolCaches::default();
    turn.expect("turn").commit(&mut caches);
    caches.calls.get(session_key, "call-1").is_some()
        || caches.outputs.get(session_key, "call-1").is_some()
}

#[test]
fn tool_cache_scan_preserves_json_request_semantics() {
    let left_alone = [
        (
            "trailing-data-session",
            r#"{"input":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{}"}]} trailing"#,
        ),
        (
            "duplicate-input-session",
            r#"{"input":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{}"}],"input":[{"type":"message","id":"message-1","role":"user","content":"hello"}]}"#,
        ),
        (
            "invalid-duplicate-input-session",
            r#"{"input":{},"input":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{}"},{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"ok"}]}"#,
        ),
    ];
    for (key, payload) in left_alone {
        let (repaired, turn) =
            prepare_fallback_turn(&mut ToolCaches::default(), key, payload.as_bytes());
        assert_eq!(repaired, payload.as_bytes(), "{key}");
        assert!(!turn_recorded(key, turn), "{key}");
    }

    let repaired = [
        (
            "duplicate-case-input-session",
            r#"{"input":[{"type":"message","id":"shadowed","role":"user","content":"ignore"}],"INPUT":[{"type":"function_call_output","id":"fco-1","call_id":"missing-call","output":"orphan"}]}"#,
        ),
        (
            "duplicate-exact-input-session",
            r#"{"input":[{"type":"message","id":"shadowed","role":"user","content":"ignore"}],"input":[{"type":"function_call_output","id":"fco-1","call_id":"missing-call","output":"orphan"}]}"#,
        ),
        (
            "duplicate-previous-response-session",
            r#"{"previous_response_id":"resp-first","previous_response_id":null,"input":[{"type":"function_call_output","id":"fco-1","call_id":"missing-call","output":"orphan"}]}"#,
        ),
    ];
    for (key, payload) in repaired {
        let (repaired, _) =
            prepare_fallback_turn(&mut ToolCaches::default(), key, payload.as_bytes());
        assert_eq!(
            effective_input_len(&repaired),
            0,
            "{key}: {}",
            String::from_utf8_lossy(&repaired)
        );
    }
}

#[test]
fn record_tool_calls_ignores_incomplete_call() {
    let payload = br#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call-1","name":"exec"}}"#;
    let mut cache = ToolCache::default();
    let mut pending = BTreeSet::new();
    record_tool_calls_from_payload(&mut cache, "session-1", payload);
    record_pending_call_ids(&mut pending, payload);
    assert!(cache.get("session-1", "call-1").is_none());
    assert!(pending.is_empty(), "{pending:?}");
}

#[test]
fn record_tool_calls_from_payload_with_cache() {
    for (payload, kind) in [
        (
            r#"{"type":"response.completed","response":{"id":"resp-1","output":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"tool","arguments":"{}"}]}}"#,
            "function_call",
        ),
        (
            r#"{"type":"response.completed","response":{"id":"resp-1","output":[{"type":"custom_tool_call","id":"ctc-1","call_id":"call-1","name":"apply_patch","input":"*** Begin Patch"}]}}"#,
            "custom_tool_call",
        ),
        (
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","id":"ctc-1","call_id":"call-1","name":"apply_patch","input":"*** Begin Patch"}}"#,
            "custom_tool_call",
        ),
    ] {
        let mut cache = ToolCache::default();
        record_tool_calls_from_payload(&mut cache, "session-1", payload.as_bytes());
        let cached = cache
            .get("session-1", "call-1")
            .unwrap_or_else(|| panic!("nothing cached from {payload}"));
        let cached = parse(cached);
        assert_eq!(cached["type"], kind);
        assert_eq!(cached["call_id"], "call-1");
    }
}

#[test]
fn record_pending_tool_call_ids_drops_satisfied_calls() {
    let mut pending = BTreeSet::new();
    record_pending_call_ids(
        &mut pending,
        br#"{"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call-1","id":"fc-1"},{"type":"function_call_output","call_id":"call-1","id":"out-1"},{"type":"custom_tool_call","call_id":"call-2","id":"ctc-1"},{"type":"custom_tool_call_output","call_id":"call-2","id":"custom-out-1"}]}}"#,
    );
    assert!(pending.is_empty(), "{pending:?}");
}

/// Forwards `items` to `socket` as a turn under `session_key`, and gives how
/// it ended with the error it ended on.
async fn forward_items(
    socket: &FakeSocket,
    session_key: &str,
    items: Vec<Result<Bytes, ErrorMessage>>,
    preserve_completion_output: bool,
) -> (Forwarded, Option<ErrorMessage>) {
    let seen = Mutex::new(None);
    let suppress = |error: &ErrorMessage| {
        *seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.clone());
        false
    };
    let mut conn = Conn::new(socket.clone());
    let caches = ServerToolCaches::default();
    let forwarded = forward(
        &mut conn,
        stream::iter(items).boxed(),
        ForwardOptions {
            caches: &caches,
            principal: Principal::ANONYMOUS,
            session_key,
            preserve_completion_output,
            turn: None,
            suppress_error: &suppress,
            keepalive: None,
            context: None,
        },
    )
    .await;
    let seen = seen.into_inner().unwrap_or_else(PoisonError::into_inner);
    (forwarded, seen)
}

/// The output, response ID and pending calls of a completed turn.
fn completed(forwarded: Forwarded) -> (Vec<u8>, String, Vec<String>) {
    match forwarded {
        Forwarded::Completed {
            output,
            response_id,
            pending_call_ids,
        } => (output, response_id, pending_call_ids),
        other => panic!("not completed: {other:?}"),
    }
}

#[tokio::test]
async fn forward_restores_and_forwards_completed_output() {
    let completion = r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#;
    for preserve in [false, true] {
        let socket = FakeSocket::default();
        let items = vec![
            Ok(Bytes::from_static(
                br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"call-1","call_id":"call-1","name":"lookup","arguments":"{}"}}"#,
            )),
            Ok(Bytes::from(sse(completion))),
        ];
        let key = format!("forward-completed-{preserve}");
        let (forwarded, error) = forward_items(&socket, &key, items, preserve).await;
        assert!(error.is_none(), "{error:?}");
        let (output, response_id, pending) = completed(forwarded);
        assert_eq!(parse(&output)[0]["id"], "call-1");
        assert_eq!(response_id, "resp-1");
        assert_eq!(pending, ["call-1"]);

        let sent = socket.sent();
        let events = texts(&sent);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"], "response.output_item.done");
        assert_eq!(events[1]["type"], "response.completed");
        if preserve {
            let Message::Text(text) = &sent[1] else {
                panic!("{:?}", sent[1]);
            };
            assert_eq!(text.as_str(), completion);
        } else {
            assert_eq!(events[1]["response"]["output"][0]["id"], "call-1");
        }
    }
}

#[tokio::test]
async fn forward_treats_response_done_as_terminal_without_rewriting() {
    let socket = FakeSocket::default();
    let items = vec![Ok(Bytes::from_static(
        br#"{"type":"response.done","response":{"id":"resp-1","output":[{"type":"message","id":"out-1"}]}}"#,
    ))];
    let (forwarded, _) = forward_items(&socket, "forward-done", items, false).await;
    let (output, response_id, pending) = completed(forwarded);
    assert_eq!(parse(&output)[0]["id"], "out-1");
    assert_eq!(response_id, "resp-1");
    assert!(pending.is_empty(), "{pending:?}");
    assert_eq!(texts(&socket.sent())[0]["type"], "response.done");
}

#[tokio::test]
async fn forward_treats_error_payload_as_terminal() {
    let socket = FakeSocket::default();
    let items = vec![Ok(Bytes::from_static(
        br#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"invalid request"}}"#,
    ))];
    let (forwarded, error) = forward_items(&socket, "forward-error", items, false).await;
    assert!(matches!(forwarded, Forwarded::Closed), "{forwarded:?}");
    let error = error.expect("an error");
    assert_eq!(error.status, 400);
    assert!(error.text.contains("invalid request"), "{}", error.text);
    let events = texts(&socket.sent());
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["type"], "error");
}

#[tokio::test]
async fn forward_mirrors_message_too_big() {
    let payload: Result<Bytes, ErrorMessage> = Ok(Bytes::from_static(
        br#"{"type":"error","status":413,"error":{"message":"upstream websocket message too big","code":"message_too_big"}}"#,
    ));
    let mapped = Err(ErrorMessage::from_exec(ExecError::upstream(
        413,
        MESSAGE_TOO_BIG_BODY,
    )));
    for (name, item) in [("payload", payload), ("mapped", mapped)] {
        let socket = FakeSocket::default();
        let (forwarded, error) = forward_items(&socket, "forward-too-big", vec![item], false).await;
        assert!(matches!(forwarded, Forwarded::Closed), "{name}");
        assert_eq!(error.expect("an error").status, 413, "{name}");
        let sent = socket.sent();
        assert_eq!(
            close_of(&sent),
            Some((1009, "upstream websocket message too big".to_owned())),
            "{name}"
        );
        assert!(texts(&sent).is_empty(), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn forward_emits_periodic_pings() {
    let socket = FakeSocket::default();
    let mut conn = Conn::new(socket.clone());
    let items = stream::once(async {
        tokio::time::sleep(Duration::from_secs(25)).await;
        Ok(Bytes::from_static(
            br#"{"type":"response.done","response":{"id":"resp-ping-1","output":[]}}"#,
        ))
    })
    .boxed();
    let caches = ServerToolCaches::default();
    let forwarded = forward(
        &mut conn,
        items,
        ForwardOptions {
            caches: &caches,
            principal: Principal::ANONYMOUS,
            session_key: "session-keepalive-test",
            preserve_completion_output: false,
            turn: None,
            suppress_error: &|_: &ErrorMessage| false,
            keepalive: Some(Duration::from_secs(10)),
            context: None,
        },
    )
    .await;
    assert_eq!(completed(forwarded).1, "resp-ping-1");
    let sent = socket.sent();
    assert_eq!(pings(&sent), 2);
    assert_eq!(texts(&sent)[0]["type"], "response.done");
}

#[tokio::test(start_paused = true)]
async fn forward_ping_write_failure_aborts_session() {
    let socket = FakeSocket {
        fail_ping: true,
        ..FakeSocket::default()
    };
    let mut conn = Conn::new(socket.clone());
    let caches = ServerToolCaches::default();
    let forwarded = forward(
        &mut conn,
        stream::pending().boxed(),
        ForwardOptions {
            caches: &caches,
            principal: Principal::ANONYMOUS,
            session_key: "session-ping-fail",
            preserve_completion_output: false,
            turn: None,
            suppress_error: &|_: &ErrorMessage| false,
            keepalive: Some(Duration::from_millis(10)),
            context: None,
        },
    )
    .await;
    assert!(matches!(forwarded, Forwarded::Closed), "{forwarded:?}");
    assert!(socket.sent().is_empty());
}

#[test]
fn should_expose_upstream_error() {
    for (status, want) in [
        (400, true),
        (409, true),
        (413, true),
        (422, true),
        (401, false),
        (408, false),
        (429, false),
        (500, false),
    ] {
        let error = ErrorMessage::new(status, status_text(status));
        assert_eq!(should_expose(&error), want, "{status}");
    }
}

#[test]
fn upstream_error_body_drives_exposure() {
    for (status, body, want) in [
        (400, "bad request", true),
        (409, "conflict", true),
        (413, "too large", true),
        (422, "unprocessable", true),
        (
            502,
            r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"flagged"}}"#,
            true,
        ),
        (
            500,
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"too long"}}"#,
            true,
        ),
        (401, "invalid token", false),
        (402, "insufficient credits", false),
        (403, "forbidden", false),
        (429, "usage limit reached", false),
        (408, "timeout", false),
        (502, "bad gateway", false),
        (
            500,
            r#"{"error":{"message":"websocket: close 1006 (abnormal closure): unexpected EOF","type":"server_error","code":"internal_server_error"}}"#,
            false,
        ),
        (0, "", false),
    ] {
        let error = ErrorMessage::new(status, body);
        assert_eq!(should_expose(&error), want, "{status} {body}");
    }
}

#[test]
fn pinned_auth_failure_replay_and_release() {
    for (status, want) in [(401, true), (429, true), (403, false), (503, false)] {
        let error = ErrorMessage::new(status, "");
        assert_eq!(should_replay_pinned_failure(&error), want, "{status}");
    }
    for (status, text, want) in [
        (408, "stream closed before response.completed", true),
        (503, "websocket bootstrap failed", true),
        (400, "invalid request", false),
        (400, "previous_response_not_found", true),
        (
            500,
            "empty_stream: upstream stream closed before first payload",
            true,
        ),
    ] {
        let error = ErrorMessage::new(status, text);
        assert_eq!(should_release_pinned(&error), want, "{status} {text}");
    }
}

#[test]
fn normalize_request_treats_transcript_replacement_as_reset() {
    let last = r#"{"model":"test-model","stream":true,"input":[{"type":"message","id":"msg-1"},{"type":"function_call","id":"fc-1","call_id":"call-1"},{"type":"function_call_output","id":"tool-out-1","call_id":"call-1"},{"type":"message","id":"assistant-1","role":"assistant"}]}"#;
    let output = r#"[{"type":"message","id":"assistant-1","role":"assistant"}]"#;
    let raw = r#"{"type":"response.create","input":[{"type":"function_call","id":"fc-compact","call_id":"call-1","name":"tool"},{"type":"message","id":"msg-2"}]}"#;
    let (normalized, next) = normalize_default(raw, last, output).unwrap();
    assert!(parse(&normalized).get("previous_response_id").is_none());
    assert_eq!(input_ids(&normalized), ["fc-compact", "msg-2"]);
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_treats_custom_tool_transcript_replacement_as_reset() {
    let last = r#"{"model":"test-model","stream":true,"input":[{"type":"message","id":"msg-1"},{"type":"custom_tool_call","id":"ctc-1","call_id":"call-1","name":"apply_patch"},{"type":"custom_tool_call_output","id":"tool-out-1","call_id":"call-1"},{"type":"message","id":"assistant-1","role":"assistant"}]}"#;
    let output = r#"[{"type":"message","id":"assistant-1","role":"assistant"}]"#;
    let raw = r#"{"type":"response.create","input":[{"type":"custom_tool_call","id":"ctc-compact","call_id":"call-1","name":"apply_patch"},{"type":"custom_tool_call_output","id":"tool-out-compact","call_id":"call-1"},{"type":"message","id":"msg-2"}]}"#;
    let (normalized, next) = normalize_default(raw, last, output).unwrap();
    assert!(parse(&normalized).get("previous_response_id").is_none());
    assert_eq!(
        input_ids(&normalized),
        ["ctc-compact", "tool-out-compact", "msg-2"]
    );
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_does_not_treat_developer_message_as_replacement() {
    let output = r#"[{"type":"message","id":"assistant-1","role":"assistant"}]"#;
    let raw = r#"{"type":"response.create","input":[{"type":"message","id":"dev-1","role":"developer"},{"type":"message","id":"msg-2"}]}"#;
    let (normalized, next) = normalize_default(raw, LAST_REQUEST, output).unwrap();
    assert_eq!(
        input_ids(&normalized),
        ["msg-1", "assistant-1", "dev-1", "msg-2"]
    );
    assert_eq!(next, normalized);
}

#[test]
fn normalize_request_drops_duplicate_tool_calls_by_call_id() {
    for (call, output, name) in TOOL_KINDS {
        let last = format!(
            r#"{{"model":"test-model","stream":true,"input":[{{"type":"{call}","id":"fc-1","call_id":"call-1"}},{{"type":"{output}","id":"tool-out-1","call_id":"call-1"}}]}}"#
        );
        let last_output =
            format!(r#"[{{"type":"{call}","id":"fc-1","call_id":"call-1","name":"{name}"}}]"#);
        let raw = r#"{"type":"response.create","input":[{"type":"message","id":"msg-2"}]}"#;
        let (normalized, _) = normalize_default(raw, &last, &last_output).unwrap();
        assert_eq!(
            input_ids(&normalized),
            ["fc-1", "tool-out-1", "msg-2"],
            "{call}"
        );
    }
}

#[test]
fn normalize_request_drops_duplicate_input_items_by_id() {
    let last = r#"{"model":"test-model","stream":true,"input":[{"type":"message","id":"msg-1","role":"user"}]}"#;
    let output = r#"[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"tool"}]"#;
    let raw = r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"function_call","id":"fc-1","call_id":"call-2","name":"tool"},{"type":"function_call_output","id":"tool-out-1","call_id":"call-2"}]}"#;
    let (normalized, _) = normalize_mode(raw, last, output, false, true).unwrap();
    let input = &parse(&normalized)["input"];
    assert_eq!(item_ids(input), ["msg-1", "fc-1", "tool-out-1"]);
    assert_eq!(input[1]["call_id"], "call-2");
}

/// `dedupeResponsesWebsocketInputItemsByID`.
fn dedupe_by_id(payload: &str) -> Value {
    parse(&repair(
        &mut ToolCaches::default(),
        "",
        payload.as_bytes(),
        false,
        None,
    ))
}

#[test]
fn dedupe_input_items_by_id_after_repair() {
    let deduped = dedupe_by_id(
        r#"{"input":[{"type":"custom_tool_call","id":"ctc-1","call_id":"call-1","name":"tool"},{"type":"custom_tool_call","id":"ctc-1","call_id":"call-2","name":"tool"},{"type":"custom_tool_call_output","id":"tool-out-1","call_id":"call-2"}]}"#,
    );
    let input = &deduped["input"];
    assert_eq!(item_ids(input), ["ctc-1", "tool-out-1"]);
    assert_eq!(input[0]["call_id"], "call-2");
}

#[test]
fn dedupe_input_items_by_id_keeps_referenced_tool_call() {
    let deduped = dedupe_by_id(
        r#"{"input":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"exec_command"},{"type":"function_call","id":"fc-1","call_id":"call-2","name":"exec_command"},{"type":"function_call_output","id":"fco-1","call_id":"call-1"}]}"#,
    );
    let input = &deduped["input"];
    assert_eq!(item_ids(input), ["fc-1", "fco-1"]);
    assert_eq!(input[0]["call_id"], "call-1");
    assert_eq!(input[1]["call_id"], "call-1");
}

/// Whether the JSON array `raw` holds a whole transcript.
fn full_transcript(raw: &str) -> bool {
    input_contains_full_transcript(Val::parse(raw.as_bytes()).unwrap())
}

#[test]
fn input_contains_full_transcript_cases() {
    assert!(!full_transcript(
        r#"[{"type":"message","role":"user","content":"hello"},{"type":"message","role":"assistant","content":"hi there"}]"#
    ));
    for kind in ["compaction", "compaction_summary"] {
        assert!(
            full_transcript(&format!(
                r#"[{{"type":"message","role":"user","content":"hello"}},{{"type":"{kind}","encrypted_content":"summary"}}]"#
            )),
            "{kind}"
        );
    }
    for raw in [
        r#"[{"type":"function_call_output","call_id":"call-1","output":"result"}]"#,
        r#"[{"type":"message","role":"user","content":"next question"}]"#,
        "[]",
    ] {
        assert!(!full_transcript(raw), "{raw}");
    }
}

const COMPACT_LAST_REQUEST: &str = r#"{"model":"gpt-5.4","stream":true,"input":[
    {"type":"message","role":"user","id":"msg-1","content":"original long prompt"},
    {"type":"message","role":"assistant","id":"msg-2","content":"original long response"},
    {"type":"function_call","id":"fc-1","call_id":"call-old","name":"bash","arguments":"{}"},
    {"type":"function_call_output","id":"fco-1","call_id":"call-old","output":"old result"}
]}"#;

const COMPACT_LAST_OUTPUT: &str = r#"[
    {"type":"message","role":"assistant","id":"msg-3","content":"another assistant reply"},
    {"type":"function_call","id":"fc-2","call_id":"call-stale","name":"read","arguments":"{}"}
]"#;

const COMPACT_REQUEST: &str = r#"{"type":"response.create","input":[
    {"type":"message","role":"user","id":"msg-1c","content":"compacted user msg"},
    {"type":"compaction","encrypted_content":"conversation summary"}
]}"#;

#[test]
fn normalize_subsequent_request_compact_skips_merge() {
    let (normalized, _) =
        normalize_default(COMPACT_REQUEST, COMPACT_LAST_REQUEST, COMPACT_LAST_OUTPUT).unwrap();
    let input = &parse(&normalized)["input"];
    assert_eq!(input.as_array().unwrap().len(), 2, "{input}");
    assert_eq!(input[0]["id"], "msg-1c");
    assert_eq!(input[1]["type"], "compaction");
}

#[test]
fn normalize_subsequent_request_compact_merges_when_compaction_replay_unsupported() {
    let (normalized, _) = normalize_mode(
        COMPACT_REQUEST,
        COMPACT_LAST_REQUEST,
        COMPACT_LAST_OUTPUT,
        false,
        false,
    )
    .unwrap();
    let input = &parse(&normalized)["input"];
    assert_eq!(
        item_ids(input),
        ["msg-1", "msg-2", "fc-1", "fco-1", "msg-3", "fc-2", "msg-1c"]
    );
    for item in input.as_array().unwrap() {
        assert!(
            item["type"] != "compaction" && item["type"] != "compaction_summary",
            "{item}"
        );
    }
}

#[test]
fn normalize_subsequent_request_reasoning_continuation_with_previous_response_id() {
    let last = r#"{"model":"gpt-5.6-terra","stream":true,"input":[{"type":"message","role":"user","id":"old-user","content":"long history"}]}"#;
    let output = r#"[{"type":"function_call","id":"old-call","call_id":"old-call","name":"lookup","arguments":"{}"}]"#;
    for kind in ["response.create", "response.append"] {
        let raw = format!(
            r#"{{"type":"{kind}","previous_response_id":"resp-1","input":[
                {{"type":"reasoning","id":"reasoning-1","summary":[]}},
                {{"type":"function_call_output","id":"output-1","call_id":"old-call","output":"result"}}
            ]}}"#
        );
        let (normalized, _) = normalize_default(&raw, last, output).unwrap();
        assert_eq!(
            parse(&normalized)["previous_response_id"],
            "resp-1",
            "{kind}"
        );
        assert_eq!(
            input_ids(&normalized),
            ["reasoning-1", "output-1"],
            "{kind}"
        );
    }
}

#[test]
fn output_collector_restores_completed_output() {
    let mut outputs = OutputItems::default();
    for payload in [
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"reply-1","role":"assistant"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"summary-1","summary":[]}}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"call-1","call_id":"call-1","name":"exec","arguments":"{}"}}"#,
    ] {
        outputs.collect(payload.as_bytes());
    }
    let output = completed_output_from_payload(
        br#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#,
        &outputs,
    );
    assert_eq!(
        item_ids(&parse(&output)),
        ["summary-1", "reply-1", "call-1"]
    );
}

#[test]
fn normalize_subsequent_request_drops_consumed_compaction_trigger() {
    let last = r#"{"model":"gpt-5.6-sol","stream":true,"input":[
        {"type":"message","role":"user","id":"msg-old","content":"old prompt"}
    ]}"#;
    let trigger = r#"{"type":"response.create","previous_response_id":"resp-before-compact","input":[
        {"type":"message","role":"user","id":"msg-tool-output","content":"done"},
        {"type":"compaction_trigger"}
    ]}"#;
    let (_, state) = normalize_mode(trigger, last, "", false, false).unwrap();
    let state = String::from_utf8(state).unwrap();
    let output = r#"[{"type":"compaction","id":"cmp-1","encrypted_content":"opaque"}]"#;
    let replay = r#"{"type":"response.create","input":[
        {"type":"message","role":"developer","id":"msg-new-context","content":"new context"},
        {"type":"compaction","id":"cmp-1","encrypted_content":"opaque"},
        {"type":"message","role":"user","id":"msg-next","content":"continue"}
    ]}"#;
    let (normalized, _) = normalize_mode(replay, &state, output, false, false).unwrap();
    for item in parse(&normalized)["input"].as_array().unwrap() {
        assert_ne!(item["type"], "compaction_trigger", "{item}");
    }
}

#[test]
fn normalize_subsequent_request_incremental_input_still_merges() {
    let last = r#"{"model":"gpt-5.4","stream":true,"input":[{"type":"message","role":"user","id":"msg-1","content":"hello"}]}"#;
    let output = r#"[
        {"type":"message","role":"assistant","id":"msg-2","content":"let me check"},
        {"type":"function_call","id":"fc-1","call_id":"call-1","name":"bash","arguments":"{}"}
    ]"#;
    let raw = r#"{"type":"response.create","input":[{"type":"function_call_output","call_id":"call-1","id":"fco-1","output":"done"}]}"#;
    let (normalized, _) = normalize_default(raw, last, output).unwrap();
    assert_eq!(input_ids(&normalized), ["msg-1", "msg-2", "fc-1", "fco-1"]);
}

#[test]
fn normalize_subsequent_request_assistant_input_triggers_transcript_replacement() {
    let last = r#"{"model":"gpt-5.4","stream":true,"input":[{"type":"message","role":"user","id":"msg-1","content":"hello"}]}"#;
    let output = r#"[
        {"type":"message","role":"assistant","id":"msg-2","content":"prior assistant"},
        {"type":"function_call","id":"fc-1","call_id":"call-1","name":"bash","arguments":"{}"}
    ]"#;
    let raw = r#"{"type":"response.append","input":[{"type":"message","role":"assistant","id":"msg-3","content":"patched assistant turn"}]}"#;
    let (normalized, _) = normalize_default(raw, last, output).unwrap();
    assert_eq!(input_ids(&normalized), ["msg-3"]);
}

#[test]
fn merge_input_matches_compatibility_scenarios() {
    let cases = [
        (
            "messages and paired tool call",
            r#"{"model":"gpt-5.4","input":[{"type":"message","id":"msg-1","role":"user","content":"hello"}]}"#,
            r#"[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{}"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"done"}]"#,
            r#"[{"type":"message","id":"msg-1","role":"user","content":"hello"},{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{}"},{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"done"}]"#,
        ),
        (
            "duplicate function call keeps first",
            r#"{"input":[{"type":"function_call","id":"fc-first","call_id":"call-1","name":"first","arguments":"{}"}]}"#,
            r#"[{"type":"function_call","id":"fc-second","call_id":"call-1","name":"second","arguments":"{}"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"done"}]"#,
            r#"[{"type":"function_call","id":"fc-first","call_id":"call-1","name":"first","arguments":"{}"},{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"done"}]"#,
        ),
        (
            "duplicate id keeps item referenced by output",
            r#"{"input":[{"type":"function_call","id":"fc-1","call_id":"call-kept","name":"first","arguments":"{}"}]}"#,
            r#"[{"type":"function_call","id":"fc-1","call_id":"call-other","name":"second","arguments":"{}"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
            r#"[{"type":"function_call","id":"fc-1","call_id":"call-kept","name":"first","arguments":"{}"},{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
        ),
        (
            "raw JSON values and escaping",
            r#"{"input":[ {"type":"message","id":"msg-1","content":"<tag> & @BS@u263a"}, true ]}"#,
            r#"[null, 42, "line@BS@nvalue"]"#,
            r#"[{"id":"last","nested":{"value":[1,2,3]}}]"#,
            r#"[{"type":"message","id":"msg-1","content":"<tag> & @BS@u263a"},true,null,42,"line@BS@nvalue",{"id":"last","nested":{"value":[1,2,3]}}]"#,
        ),
        (
            "large numbers retain exact JSON values",
            r#"{"input":[9007199254740993,{"id":"n","value":9223372036854775807}]}"#,
            "[18446744073709551615]",
            r#"[{"id":"decimal","value":1.0000000000000000001}]"#,
            r#"[9007199254740993,{"id":"n","value":9223372036854775807},18446744073709551615,{"id":"decimal","value":1.0000000000000000001}]"#,
        ),
        (
            "invalid response output remains ignored",
            r#"{"input":[{"id":"first"}]}"#,
            r#"[{"id":"#,
            r#"[{"id":"last"}]"#,
            r#"[{"id":"first"},{"id":"last"}]"#,
        ),
        (
            "missing previous input and null append",
            r#"{"model":"gpt-5.4"}"#,
            r#"[{"id":"response"}]"#,
            "null",
            r#"[{"id":"response"}]"#,
        ),
        (
            "null previous request",
            "null",
            "[]",
            r#"[{"id":"last"}]"#,
            r#"[{"id":"last"}]"#,
        ),
        (
            "duplicate metadata keys follow encoding json",
            r#"{"input":[{"type":"message","type":"function_call","id":"first","id":"fc-1","call_id":"call-other","call_id":"call-kept"}]}"#,
            r#"[{"type":"function_call","id":"fc-2","call_id":"call-kept"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
            r#"[{"type":"function_call","id":"fc-1","call_id":"call-kept"},{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
        ),
        (
            "case insensitive metadata dedupes function calls",
            r#"{"input":[{"Type":"function_call","ID":"fc-old","CALL_ID":"call-1","name":"first"}]}"#,
            r#"[{"type":"function_call","id":"fc-new","call_id":"call-1","name":"second"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"done"}]"#,
            r#"[{"Type":"function_call","ID":"fc-old","CALL_ID":"call-1","name":"first"},{"type":"function_call_output","id":"fco-1","call_id":"call-1","output":"done"}]"#,
        ),
        (
            "case insensitive metadata keeps referenced duplicate id",
            r#"{"input":[{"Type":"function_call","Id":"fc-1","Call_Id":"call-kept","name":"first"}]}"#,
            r#"[{"type":"function_call","id":"fc-1","call_id":"call-other","name":"second"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
            r#"[{"Type":"function_call","Id":"fc-1","Call_Id":"call-kept","name":"first"},{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
        ),
        (
            "mixed case duplicate metadata keeps last values",
            r#"{"input":[{"type":"message","TYPE":"function_call","id":"first","ID":"fc-1","call_id":"call-other","CALL_ID":"call-kept"}]}"#,
            r#"[{"type":"function_call","id":"fc-2","call_id":"call-kept"}]"#,
            r#"[{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
            r#"[{"type":"message","TYPE":"function_call","id":"first","ID":"fc-1","call_id":"call-other","CALL_ID":"call-kept"},{"type":"function_call_output","id":"fco-1","call_id":"call-kept","output":"done"}]"#,
        ),
        (
            "duplicate previous input keeps last array",
            r#"{"input":[{"id":"old"}],"input":[{"id":"new"}]}"#,
            "[]",
            "[]",
            r#"[{"id":"new"}]"#,
        ),
        (
            "previous input field matching is case insensitive",
            r#"{"Input":[{"id":"old"}],"INPUT":[{"id":"new"}]}"#,
            "[]",
            "[]",
            r#"[{"id":"new"}]"#,
        ),
        (
            "last duplicate null clears previous input",
            r#"{"input":[{"id":"old"}],"input":null}"#,
            r#"[{"id":"response"}]"#,
            "[]",
            r#"[{"id":"response"}]"#,
        ),
    ];
    let unescape = |text: &str| text.replace("@BS@", r"\\");
    for (name, last, output, append, want) in cases {
        let (last, output, append, want) = (
            unescape(last),
            unescape(output),
            unescape(append),
            unescape(want),
        );
        let merged = merge_input(last.as_bytes(), output.as_bytes(), append.as_bytes())
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(parse(&merged), parse(want.as_bytes()), "{name}");
        if name.starts_with("large numbers") {
            let merged = String::from_utf8(merged).unwrap();
            for number in [
                "9007199254740993",
                "18446744073709551615",
                "1.0000000000000000001",
            ] {
                assert!(merged.contains(number), "{merged}");
            }
        }
    }
}

#[test]
fn merge_input_returns_compatible_errors() {
    let previous = "invalid previous request input";
    let cases = [
        (
            "invalid previous request",
            r#"{"input":"#,
            "[]",
            previous,
            true,
        ),
        (
            "non-array previous input",
            r#"{"input":{"id":"item"}}"#,
            "[]",
            previous,
            false,
        ),
        (
            "invalid appended input",
            r#"{"input":[]}"#,
            r#"[{"id":"#,
            "invalid request input",
            true,
        ),
        (
            "non-array appended input",
            r#"{"input":[]}"#,
            r#"{"id":"item"}"#,
            "invalid request input",
            false,
        ),
        ("array previous request", "[]", "[]", previous, false),
        (
            "last duplicate previous input is non-array",
            r#"{"input":[],"input":{"id":"item"}}"#,
            "[]",
            previous,
            false,
        ),
        (
            "earlier non-array previous input remains invalid",
            r#"{"input":{"id":"item"},"input":[]}"#,
            "[]",
            previous,
            false,
        ),
    ];
    for (name, last, append, prefix, syntax) in cases {
        let err = merge_input(last.as_bytes(), b"", append.as_bytes()).expect_err(name);
        assert!(
            err.to_string().starts_with(&format!("{prefix}: ")),
            "{name}: {err}"
        );
        assert_eq!(
            matches!(err.cause(), DecodeError::Syntax),
            syntax,
            "{name}: {err}"
        );
    }
}

#[test]
fn lite_request_from_header_or_client_metadata() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-openai-internal-codex-responses-lite",
        HeaderValue::from_static("TRUE"),
    );
    assert!(is_lite_request(b"{}", &headers));
    for (value, want) in [
        ("true", true),
        (r#""true""#, true),
        (r#""false""#, false),
        ("1", false),
    ] {
        let payload = format!(
            r#"{{"client_metadata":{{"ws_request_header_x_openai_internal_codex_responses_lite":{value}}}}}"#
        );
        assert_eq!(
            is_lite_request(payload.as_bytes(), &HeaderMap::new()),
            want,
            "{value}"
        );
    }
}

/// The headers of a valid handshake.
fn handshake_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "connection",
        HeaderValue::from_static("keep-alive, Upgrade"),
    );
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
    );
    headers
}

#[test]
fn handshake_checks_follow_gorilla() {
    assert_eq!(check_handshake(&Method::GET, &handshake_headers()), Ok(()));
    assert_eq!(
        check_handshake(&Method::POST, &handshake_headers()),
        Err(405)
    );
    let mut headers = handshake_headers();
    headers.remove("upgrade");
    assert_eq!(check_handshake(&Method::POST, &headers), Err(400));
    let mut headers = handshake_headers();
    headers.insert("sec-websocket-version", HeaderValue::from_static("8"));
    assert_eq!(check_handshake(&Method::GET, &headers), Err(400));
    let mut headers = handshake_headers();
    headers.insert("sec-websocket-key", HeaderValue::from_static("short"));
    assert_eq!(check_handshake(&Method::GET, &headers), Err(400));
}

#[test]
fn token_lists_match_whole_tokens() {
    let mut headers = HeaderMap::new();
    headers.append("connection", HeaderValue::from_static("keep-alive"));
    headers.append("connection", HeaderValue::from_static(" Upgrade , close"));
    assert!(token_list_contains(&headers, "connection", "upgrade"));
    assert!(token_list_contains(&headers, "connection", "close"));
    assert!(!token_list_contains(&headers, "connection", "keep"));
    headers.insert("upgrade", HeaderValue::from_static("websocket/13"));
    assert!(!token_list_contains(&headers, "upgrade", "websocket"));
}

#[test]
fn challenge_key_is_base64_for_sixteen_bytes() {
    assert!(is_valid_challenge_key(b"dGhlIHNhbXBsZSBub25jZQ=="));
    assert!(!is_valid_challenge_key(b"dGhlIHNhbXBsZSBub25jZQ="));
    assert!(!is_valid_challenge_key(b"dGhlIHNhbXBsZSBub25jZ-=="));
}

#[test]
fn error_payload_carries_status_code_and_headers() {
    let mut error = previous_response_not_found();
    error
        .addon
        .insert("x-request-id", HeaderValue::from_static("req-1"));
    let payload = parse(&error_payload(&error));
    assert_eq!(payload["type"], "error");
    assert_eq!(payload["status"], 409);
    assert_eq!(payload["error"]["code"], "previous_response_not_found");
    assert_eq!(payload["headers"]["X-Request-Id"], "req-1");
    assert_eq!(canonical_header_key("x-request-id"), "X-Request-Id");
    assert_eq!(canonical_header_key("CONTENT-type"), "Content-Type");
}

/// A client's end of a socket.
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// `test-model`, served by a provider without upstream WebSockets.
fn test_catalog() -> FakeCatalog {
    FakeCatalog::new().serve("test-model", &["test-provider"])
}

/// Serves the router on a free port with `catalog`, `outcomes` and the key
/// `sk-test`: the socket's URL, and the dispatcher.
async fn serve(catalog: FakeCatalog, outcomes: Vec<Outcome>) -> (String, Arc<FakeDispatcher>) {
    let (url, dispatcher, _) = serve_keys(catalog, outcomes, &["sk-test"]).await;
    (url, dispatcher)
}

/// Serves the router on a free port with `catalog`, `outcomes` and `keys`:
/// the socket's URL, the dispatcher, and the server's state.
async fn serve_keys(
    catalog: FakeCatalog,
    outcomes: Vec<Outcome>,
    keys: &[&str],
) -> (String, Arc<FakeDispatcher>, AppState) {
    let dispatcher = FakeDispatcher::new(outcomes);
    let config = ServerConfig {
        api_keys: keys.iter().map(|&key| key.to_owned()).collect(),
        ..ServerConfig::default()
    };
    let state = crate::testing::state(config, catalog, &dispatcher);
    let app = crate::router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("ws://{addr}/v1/responses"), dispatcher, state)
}

/// Opens a socket to `url` with the test key and `headers`, and gives the
/// handshake's response too.
async fn connect_with(
    url: &str,
    headers: &[(&'static str, &str)],
) -> (Client, tungstenite::handshake::client::Response) {
    connect_key(url, Some("sk-test"), headers).await
}

/// Opens a socket to `url` with `key`, if any, and `headers`, and gives the
/// handshake's response too.
async fn connect_key(
    url: &str,
    key: Option<&str>,
    headers: &[(&'static str, &str)],
) -> (Client, tungstenite::handshake::client::Response) {
    let mut request = url.into_client_request().unwrap();
    let request_headers = request.headers_mut();
    if let Some(key) = key {
        let bearer = HeaderValue::from_str(&format!("Bearer {key}")).unwrap();
        request_headers.insert("authorization", bearer);
    }
    for &(name, value) in headers {
        request_headers.insert(name, HeaderValue::from_str(value).unwrap());
    }
    tokio_tungstenite::connect_async(request).await.unwrap()
}

/// Opens a socket to `url` with the test key and `headers`.
async fn connect(url: &str, headers: &[(&'static str, &str)]) -> Client {
    connect_with(url, headers).await.0
}

/// Sends `payload` as a text message.
async fn send(ws: &mut Client, payload: &str) {
    ws.send(tungstenite::Message::Text(payload.to_owned().into()))
        .await
        .unwrap();
}

/// The next message, as JSON, skipping pings and pongs.
async fn recv(ws: &mut Client) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .expect("socket ended")
            .expect("socket failed");
        match message {
            tungstenite::Message::Text(text) => return parse(text.as_str().as_bytes()),
            tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The messages left before the socket ends, as JSON, and its close
/// frame's code and reason, or `None` when it just ends.
async fn rest(ws: &mut Client) -> (Vec<Value>, Option<(u16, String)>) {
    let mut messages = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out");
        match next {
            None | Some(Err(_)) => return (messages, None),
            Some(Ok(tungstenite::Message::Close(frame))) => {
                let close =
                    frame.map(|frame| (u16::from(frame.code), frame.reason.as_str().to_owned()));
                return (messages, close);
            }
            Some(Ok(tungstenite::Message::Text(text))) => {
                messages.push(parse(text.as_str().as_bytes()));
            }
            Some(Ok(_)) => {}
        }
    }
}

/// How the socket ends, with nothing more sent before it.
async fn recv_end(ws: &mut Client) -> Option<(u16, String)> {
    let (messages, close) = rest(ws).await;
    assert!(messages.is_empty(), "{messages:?}");
    close
}

/// A stream that completes response `id` with `output`.
fn completes(id: &str, output: &str) -> Outcome {
    let event =
        format!(r#"{{"type":"response.completed","response":{{"id":"{id}","output":{output}}}}}"#);
    Outcome::chunks(&[&sse(&event)])
}

/// A stream that completes response `id` with the message `item`.
fn completes_with(id: &str, item: &str) -> Outcome {
    completes(id, &format!(r#"[{{"type":"message","id":"{item}"}}]"#))
}

/// What the dispatcher says when every credential for the model is
/// `provider`'s, with websockets on or off.
fn support_for(
    provider: &'static str,
    websockets: bool,
) -> impl Fn(&[String], &str, Option<&str>) -> WebsocketSupport + Send + Sync + 'static {
    move |_: &[String], _: &str, auth_id: Option<&str>| WebsocketSupport {
        upstream_passthrough: websockets && matches!(provider, "codex" | "xai"),
        compaction_replay: provider == "codex",
        auth: auth_id.map(|_| WebsocketAuth {
            provider: provider.to_owned(),
            serves_model: true,
            websockets,
        }),
    }
}

/// `value` as a string, or empty when it isn't one.
fn text_of(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// The payload of the dispatcher's call `index`.
fn payload_of(dispatcher: &FakeDispatcher, index: usize) -> Value {
    parse(&dispatcher.calls()[index].request.payload)
}

/// Waits until `done` holds.
async fn eventually(mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out");
}

#[tokio::test]
async fn handshake_errors_answer_as_gorilla_does() {
    let dispatcher = FakeDispatcher::new(Vec::new());
    let config = ServerConfig {
        api_keys: vec!["sk-test".into()],
        ..ServerConfig::default()
    };
    let app = crate::router(crate::testing::state(config, test_catalog(), &dispatcher));
    let request = |headers: HeaderMap, key: bool| {
        let mut builder = http::Request::builder()
            .method(Method::GET)
            .uri("/v1/responses");
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        if key {
            builder = builder.header("authorization", "Bearer sk-test");
        }
        builder.body(axum::body::Body::empty()).unwrap()
    };

    let response = app
        .clone()
        .oneshot(request(HeaderMap::new(), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let headers = response.headers();
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(headers["sec-websocket-version"], "13");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body, "Bad Request\n");

    // A real handshake, but nothing to upgrade.
    let response = app
        .clone()
        .oneshot(request(handshake_headers(), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let response = app
        .oneshot(request(handshake_headers(), false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn codex_path_serves_the_socket_and_echoes_turn_state() {
    let (url, dispatcher) = serve(test_catalog(), vec![completes_with("resp-1", "out-1")]).await;
    let url = url.replace("/v1/responses", "/backend-api/codex/responses");
    let (mut ws, response) = connect_with(&url, &[("x-codex-turn-state", "turn-state-1")]).await;
    assert_eq!(response.headers()["x-codex-turn-state"], "turn-state-1");
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    assert_eq!(dispatcher.calls().len(), 1);
}

#[tokio::test]
async fn merges_transcript_for_non_passthrough_upstream() {
    let (url, dispatcher) = serve(
        FakeCatalog::new().serve("test-model", &["codex"]),
        vec![
            completes_with("resp-1", "out-1"),
            completes_with("resp-2", "out-2"),
        ],
    )
    .await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    let event = recv(&mut ws).await;
    assert_eq!(event["type"], "response.completed");
    assert_eq!(event["response"]["id"], "resp-1");
    send(
        &mut ws,
        r#"{"type":"response.create","input":[{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    let first = parse(&calls[0].request.payload);
    assert!(first.get("type").is_none(), "{first}");
    assert_eq!(first["stream"], true);
    assert_eq!(calls[0].method, "execute_stream");
    assert_eq!(calls[0].providers, ["codex"]);
    assert_eq!(calls[0].request.model, "test-model");
    assert!(calls[0].options.stream && calls[0].options.downstream_websocket);
    assert_eq!(calls[0].options.metadata.pinned_auth_id, None);
    let session = calls[0]
        .options
        .metadata
        .execution_session_id
        .clone()
        .expect("an execution session");
    assert_eq!(
        calls[1].options.metadata.execution_session_id.as_deref(),
        Some(session.as_str())
    );
    let second = parse(&calls[1].request.payload);
    assert!(second.get("previous_response_id").is_none(), "{second}");
    assert_eq!(item_ids(&second["input"]), ["msg-1", "out-1", "msg-2"]);

    ws.close(None).await.unwrap();
    eventually(|| dispatcher.closed_sessions().contains(&session)).await;
}

#[tokio::test]
async fn does_not_inject_previous_response_id_when_pending_tool_output_missing() {
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![
            completes(
                "resp-1",
                r#"[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"tool"}]"#,
            ),
            completes_with("resp-2", "assistant-1"),
        ],
    )
    .await;
    let mut ws = connect(&url, &[]).await;
    for request in [
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
        r#"{"type":"response.create","input":[{"type":"message","role":"user","id":"summary-1","content":"compacted summary"}]}"#,
    ] {
        send(&mut ws, request).await;
        assert_eq!(recv(&mut ws).await["type"], "response.completed");
    }
    let second = payload_of(&dispatcher, 1);
    assert!(second.get("previous_response_id").is_none(), "{second}");
    assert_eq!(item_ids(&second["input"]), ["msg-1", "fc-1", "summary-1"]);
}

/// Reads a warm-up's answer and gives its ID, checking it was made up here.
async fn prewarm_answer(ws: &mut Client) -> String {
    let created = recv(ws).await;
    assert_eq!(created["type"], "response.created", "{created}");
    let id = text_of(&created["response"]["id"]).to_owned();
    assert!(id.starts_with("resp_prewarm_"), "{created}");
    let completed = recv(ws).await;
    assert_eq!(completed["type"], "response.completed", "{completed}");
    assert_eq!(completed["response"]["id"], id.as_str());
    assert_eq!(
        completed["response"]["output"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(completed["response"]["usage"]["total_tokens"], 0);
    id
}

#[tokio::test]
async fn prewarm_handled_locally_for_sse_upstream() {
    let (url, dispatcher) = serve(test_catalog(), vec![completes_with("resp-1", "out-1")]).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","generate":false}"#,
    )
    .await;
    let created = recv(&mut ws).await;
    assert_eq!(created["type"], "response.created");
    assert_eq!(created["response"]["model"], "test-model");
    let id = text_of(&created["response"]["id"]).to_owned();
    assert!(id.starts_with("resp_prewarm_"), "{created}");
    let completed = recv(&mut ws).await;
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["id"], id.as_str());
    assert_eq!(completed["response"]["usage"]["total_tokens"], 0);
    assert!(dispatcher.calls().is_empty());

    send(
        &mut ws,
        &format!(
            r#"{{"type":"response.create","previous_response_id":"{id}","input":[{{"type":"message","id":"msg-1"}}]}}"#
        ),
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    assert_eq!(dispatcher.calls().len(), 1);
    let forwarded = payload_of(&dispatcher, 0);
    assert!(
        forwarded.get("previous_response_id").is_none(),
        "{forwarded}"
    );
    assert!(forwarded.get("generate").is_none(), "{forwarded}");
    assert_eq!(forwarded["model"], "test-model");
    assert_eq!(item_ids(&forwarded["input"]), ["msg-1"]);
}

/// Runs a turn, then the warm-up `prewarm`, then a turn that follows it
/// with `followup_input`: the IDs of the input that last turn sent.
async fn mid_connection_prewarm(prewarm: &str, followup_input: &str) -> Vec<String> {
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![
            completes_with("resp-1", "out-1"),
            completes_with("resp-2", "out-2"),
        ],
    )
    .await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-initial"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    send(&mut ws, prewarm).await;
    let id = prewarm_answer(&mut ws).await;
    assert_eq!(dispatcher.calls().len(), 1);
    send(
        &mut ws,
        &format!(
            r#"{{"type":"response.create","previous_response_id":"{id}","input":{followup_input}}}"#
        ),
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    assert_eq!(dispatcher.calls().len(), 2);
    let forwarded = payload_of(&dispatcher, 1);
    assert!(
        forwarded.get("previous_response_id").is_none(),
        "{forwarded}"
    );
    assert!(forwarded.get("generate").is_none(), "{forwarded}");
    item_ids(&forwarded["input"])
}

#[tokio::test]
async fn mid_connection_prewarm_handled_locally_for_sse_upstream() {
    let ids = mid_connection_prewarm(
        r#"{"type":"response.create","model":"test-model","generate":false,"input":[{"type":"message","id":"msg-prewarm"}]}"#,
        r#"[{"type":"message","id":"msg-followup"}]"#,
    )
    .await;
    for id in ["msg-prewarm", "msg-followup"] {
        assert!(ids.iter().any(|got| got == id), "{ids:?}");
    }
    for id in ["msg-initial", "out-1"] {
        assert!(!ids.iter().any(|got| got == id), "{ids:?}");
    }
}

#[tokio::test]
async fn mid_connection_prewarm_empty_input_followup_for_sse_upstream() {
    let ids = mid_connection_prewarm(
        r#"{"type":"response.create","model":"test-model","generate":false,"input":[{"type":"message","id":"msg-prewarm"}]}"#,
        "[]",
    )
    .await;
    assert_eq!(ids, ["msg-prewarm"]);
}

#[tokio::test]
async fn mid_connection_incremental_prewarm_handled_locally_for_sse_upstream() {
    let ids = mid_connection_prewarm(
        r#"{"type":"response.create","model":"test-model","previous_response_id":"resp-1","generate":false,"input":[{"type":"message","id":"msg-prewarm-incremental"}]}"#,
        r#"[{"type":"message","id":"msg-followup"}]"#,
    )
    .await;
    for id in ["msg-initial", "msg-prewarm-incremental", "msg-followup"] {
        assert!(ids.iter().any(|got| got == id), "{ids:?}");
    }
}

/// A case of `prewarm_preserves_compacted_followup`.
#[derive(Default)]
struct PrewarmCase {
    name: &'static str,
    input: &'static str,
    parent: bool,
    fail_first: bool,
    wrong_parent: bool,
    invalid_first: bool,
    invalid_type: bool,
    omit_model: bool,
    want_prefix: bool,
}

/// A Codex warm-up with tools and instructions.
const WARMUP: &str = r#"{"type":"response.create","model":"prewarm-prefix-model","instructions":"legacy base","generate":false,"input":[{"type":"additional_tools","id":"warm-tools","role":"developer","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]},{"type":"message","id":"warm-base","role":"developer","content":"base instructions"}]}"#;

/// Runs a case of `prewarm_preserves_compacted_followup`.
async fn prewarm_case(case: PrewarmCase) {
    let name = case.name;
    let mut outcomes = Vec::new();
    if case.fail_first {
        outcomes.push(Outcome::Stream(
            HeaderMap::new(),
            vec![Err(ExecError::upstream(400, "retry diagnostic"))],
        ));
    }
    outcomes.push(completes_with("resp-upstream", "out-1"));
    outcomes.push(completes_with("resp-upstream", "out-1"));
    let (url, dispatcher) = serve(
        FakeCatalog::new().serve("prewarm-prefix-model", &["codex"]),
        outcomes,
    )
    .await;
    dispatcher.websocket(support_for("codex", false));
    let session = format!("prewarm-prefix-{name}");
    let headers = [("session_id", session.as_str())];
    let mut ws = connect(&url, &headers).await;
    send(&mut ws, WARMUP).await;
    let mut parent = prewarm_answer(&mut ws).await;
    assert!(dispatcher.calls().is_empty(), "{name}");

    let mut parent_field = String::new();
    if case.parent {
        if case.wrong_parent {
            "resp_prewarm_unrelated".clone_into(&mut parent);
        }
        parent_field = format!(r#","previous_response_id":"{parent}""#);
    }
    let model_field = if case.omit_model {
        ""
    } else {
        r#","model":"prewarm-prefix-model""#
    };
    let followup = format!(
        r#"{{"type":"response.create"{model_field}{parent_field},"input":{},"client_metadata":{{"source":"automation_heartbeat","keep":"unchanged"}}}}"#,
        case.input
    );
    if case.invalid_type {
        send(
            &mut ws,
            &format!(r#"{{"type":"unsupported","previous_response_id":"{parent}","input":[]}}"#),
        )
        .await;
        assert_eq!(recv(&mut ws).await["type"], "error", "{name}");
        assert!(dispatcher.calls().is_empty(), "{name}");
    }
    if case.invalid_first {
        let invalid = if case.parent {
            format!(
                r#"{{"type":"response.create","previous_response_id":"{parent}","input":{{}}}}"#
            )
        } else {
            r#"{"type":"response.create","input":{}}"#.to_owned()
        };
        send(&mut ws, &invalid).await;
        assert_eq!(recv(&mut ws).await["type"], "error", "{name}");
        assert!(dispatcher.calls().is_empty(), "{name}");
    }
    send(&mut ws, &followup).await;
    let mut result = recv(&mut ws).await;
    if case.wrong_parent {
        assert_eq!(result["type"], "error", "{name}");
        assert!(dispatcher.calls().is_empty(), "{name}");
        return;
    }
    if case.fail_first {
        assert_eq!(result["type"], "error", "{name}");
        // Codex reconnects and warms up again before retrying the request.
        ws = connect(&url, &headers).await;
        send(&mut ws, WARMUP).await;
        let new_parent = prewarm_answer(&mut ws).await;
        send(&mut ws, &followup.replace(&parent, &new_parent)).await;
        result = recv(&mut ws).await;
    }
    assert_eq!(result["type"], "response.completed", "{name}: {result}");

    let want = parse(case.input.as_bytes());
    let want = want.as_array().unwrap();
    for call in dispatcher.calls() {
        let forwarded = parse(&call.request.payload);
        assert_eq!(forwarded["model"], "prewarm-prefix-model", "{name}");
        assert_eq!(forwarded["instructions"], "legacy base", "{name}");
        let mut input = forwarded["input"].as_array().unwrap().as_slice();
        if case.want_prefix {
            assert_eq!(
                item_ids(&forwarded["input"])[..2],
                ["warm-tools", "warm-base"],
                "{name}"
            );
            input = &input[2..];
        }
        assert_eq!(input, want.as_slice(), "{name}");
        assert!(
            forwarded.get("previous_response_id").is_none(),
            "{name}: {forwarded}"
        );
        assert!(forwarded.get("generate").is_none(), "{name}: {forwarded}");
        assert_eq!(forwarded["client_metadata"]["keep"], "unchanged", "{name}");
    }
    if name == "compacted_delta" {
        send(
            &mut ws,
            r#"{"type":"response.create","model":"prewarm-prefix-model","previous_response_id":"resp-upstream","input":[{"type":"function_call_output","name":"automation_update","output":"next heartbeat"}]}"#,
        )
        .await;
        assert_eq!(recv(&mut ws).await["type"], "response.completed");
        let calls = dispatcher.calls();
        let last = parse(&calls.last().unwrap().request.payload);
        let input = last["input"].as_array().unwrap();
        let tools = input
            .iter()
            .filter(|item| item["type"] == "additional_tools")
            .count();
        assert_eq!(tools, 1, "{last}");
        assert_eq!(input.last().unwrap()["output"], "next heartbeat", "{last}");
    }
}

#[tokio::test]
async fn prewarm_preserves_compacted_followup() {
    let compacted = r#"[{"type":"compaction","encrypted_content":"opaque-checkpoint"},{"type":"function_call_output","name":"automation_update","output":"current heartbeat"}]"#;
    let ordinary = r#"[{"type":"function_call_output","name":"automation_update","output":"current heartbeat"}]"#;
    let tools_only = r#"[{"type":"additional_tools","role":"developer","tools":[]}]"#;
    let cases = [
        PrewarmCase {
            name: "compacted_delta",
            input: compacted,
            parent: true,
            want_prefix: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "ordinary_delta",
            input: ordinary,
            parent: true,
            want_prefix: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "failed_attempt_reconnect",
            input: compacted,
            parent: true,
            fail_first: true,
            want_prefix: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "invalid_delta_retry",
            input: compacted,
            parent: true,
            invalid_first: true,
            want_prefix: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "invalid_type_retry",
            input: ordinary,
            parent: true,
            invalid_type: true,
            want_prefix: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "replacement_inherits_defaults",
            input: tools_only,
            omit_model: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "invalid_replacement_retry",
            input: tools_only,
            invalid_first: true,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "replacement_empty_tools",
            input: r#"[{"type":"additional_tools","role":"developer","tools":[]},{"type":"message","role":"user","content":"replacement"}]"#,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "replacement_new_tools",
            input: r#"[{"type":"additional_tools","role":"developer","tools":[{"type":"function","name":"replacement_tool"}]},{"type":"message","role":"user","content":"replacement"}]"#,
            ..PrewarmCase::default()
        },
        PrewarmCase {
            name: "unrelated_parent",
            input: r#"[{"type":"message","role":"user","content":"not this warmup"}]"#,
            parent: true,
            wrong_parent: true,
            ..PrewarmCase::default()
        },
    ];
    for case in cases {
        prewarm_case(case).await;
    }
}

#[tokio::test]
async fn rejects_unknown_previous_response_on_new_socket() {
    let (url, dispatcher) = serve(
        FakeCatalog::new().serve("grok", &["xai"]),
        vec![completes_with("resp-1", "out-1")],
    )
    .await;
    dispatcher.websocket(support_for("xai", true));
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"grok","previous_response_id":"resp-old","input":[{"type":"message","id":"msg-2","role":"user","content":"second"}]}"#,
    )
    .await;
    let error = recv(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 409);
    assert_eq!(error["error"]["code"], "previous_response_not_found");
    assert!(dispatcher.calls().is_empty());

    send(
        &mut ws,
        r#"{"type":"response.create","model":"grok","input":[{"type":"message","id":"msg-1"},{"type":"message","id":"out-1","role":"assistant"},{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(item_ids(&payload_of(&dispatcher, 0)["input"]).len(), 3);
}

#[tokio::test]
async fn closes_after_non_retryable_client_error() {
    let body = r#"{"error":{"message":"No tool call found for function call output with call_id failed-call.","type":"invalid_request_error","param":"input"}}"#;
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![
            completes_with("resp-1", "out-1"),
            Outcome::Fail(ExecError::upstream(400, body)),
        ],
    )
    .await;
    let mut ws = connect(&url, &[("session-id", "client-error-session")]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    send(
        &mut ws,
        r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"function_call","id":"fc-failed","call_id":"failed-call","name":"failed_tool","arguments":"{}"},{"type":"function_call_output","id":"fco-failed","call_id":"failed-call","output":"must-not-survive"}]}"#,
    )
    .await;
    let error = recv(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 400);
    assert_eq!(recv_end(&mut ws).await, None);
    assert_eq!(dispatcher.calls().len(), 2);
}

/// The upstream's plain-text 404 for an item it didn't store.
const ITEM_NOT_PERSISTED: &str = "Item with id 'rs_0b5f3eb6f51f175c0169ca74e4a85881998539920821603a74' not found. Items are not persisted when `store` is set to false. Try again with `store` set to true, or remove this item from your input.";

#[tokio::test]
async fn exposes_item_not_persisted_and_recovers_on_reconnect() {
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![
            completes_with("resp-1", "out-1"),
            Outcome::Fail(ExecError::upstream(404, ITEM_NOT_PERSISTED)),
            completes_with("resp-2", "out-2"),
        ],
    )
    .await;
    let headers = [("session-id", "item-miss-session")];
    let mut ws = connect(&url, &headers).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    send(
        &mut ws,
        r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"reasoning","id":"rs_0b5f3eb6f51f175c0169ca74e4a85881998539920821603a74"},{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    let error = recv(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 404);
    assert!(
        text_of(&error["error"]["message"]).contains("Items are not persisted"),
        "{error}"
    );
    assert_eq!(recv_end(&mut ws).await, None);

    let mut ws = connect(&url, &headers).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"},{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 3);
    let rebuilt = parse(&calls[2].request.payload);
    assert!(rebuilt.get("previous_response_id").is_none(), "{rebuilt}");
    assert_eq!(item_ids(&rebuilt["input"]), ["msg-1", "msg-2"]);
    assert!(!rebuilt.to_string().contains("rs_0b5f3eb6"), "{rebuilt}");
}

#[tokio::test]
async fn hides_non_client_upstream_errors() {
    let cases = [
        ExecError::new(
            ErrorKind::Upstream,
            "websocket: close 1006 (abnormal closure): unexpected EOF",
        ),
        ExecError::new(
            ErrorKind::Upstream,
            "read tcp 198.18.0.1:53030->145.223.58.12:6281: i/o timeout",
        ),
        ExecError::upstream(
            429,
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#,
        ),
        ExecError::upstream(
            401,
            r#"{"error":{"type":"authentication_error","message":"Invalid token"}}"#,
        ),
    ];
    for error in cases {
        let text = error.to_string();
        let (url, _) = serve(test_catalog(), vec![Outcome::Fail(error)]).await;
        let mut ws = connect(&url, &[]).await;
        send(
            &mut ws,
            r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
        )
        .await;
        assert_eq!(recv_end(&mut ws).await, None, "{text}");
    }
}

#[tokio::test]
async fn exposes_cyber_policy_regardless_of_status() {
    let body = r#"{"error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk.","param":null}}"#;
    for status in [400, 502, 500] {
        let (url, _) = serve(
            test_catalog(),
            vec![Outcome::Fail(ExecError::upstream(status, body))],
        )
        .await;
        let mut ws = connect(&url, &[]).await;
        send(
            &mut ws,
            r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
        )
        .await;
        let error = recv(&mut ws).await;
        assert_eq!(error["type"], "error", "{status}");
        assert_eq!(error["status"], status, "{status}");
        assert_eq!(error["error"]["code"], "cyber_policy", "{status}");
        assert!(
            text_of(&error["error"]["message"]).contains("cybersecurity risk"),
            "{error}"
        );
        assert_eq!(recv_end(&mut ws).await, None, "{status}");
    }
}

#[tokio::test]
async fn exposes_terminal_oauth_error() {
    let error = ExecError::upstream(
        503,
        r#"token refresh failed with status 401: {"error":{"message":"Refresh credential has already been consumed; sign in again.","type":"invalid_request_error","code":"refresh_token_reused"}}"#,
    )
    .with_terminal_auth();
    let (url, _) = serve(test_catalog(), vec![Outcome::Fail(error)]).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    let error = recv(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 503);
    assert_eq!(error["error"]["type"], "authentication_error");
    assert_eq!(error["error"]["code"], "upstream_authentication_required");
    assert_eq!(error["error"]["retryable"], false);
    assert!(
        text_of(&error["error"]["message"]).contains("refresh_token_reused"),
        "{error}"
    );
    assert_eq!(recv_end(&mut ws).await, None);
}

#[tokio::test]
async fn mirrors_upstream_message_too_big() {
    let mut error = ExecError::new(
        ErrorKind::Upstream,
        "websocket: close 1009 (message too big): message too big",
    );
    error.ws_close = Some(WsClose::MessageTooBig("message too big".to_owned()));
    let (url, _) = serve(test_catalog(), vec![Outcome::Fail(error)]).await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    assert_eq!(
        recv_end(&mut ws).await,
        Some((1009, "message too big".to_owned()))
    );
}

#[tokio::test]
async fn xai_passthrough_keeps_native_request_and_closes_when_eligibility_changes() {
    let (url, dispatcher) = serve(
        FakeCatalog::new().serve("grok", &["xai"]),
        vec![
            Outcome::via(&["auth-x"], completes_with("resp-1", "out-1")),
            Outcome::via(&["auth-x"], completes_with("resp-2", "out-2")),
            completes_with("resp-3", "out-3"),
            completes_with("resp-4", "out-4"),
        ],
    )
    .await;
    dispatcher.websocket(support_for("xai", true));
    let mut ws = connect(&url, &[]).await;
    for request in [
        r#"{"type":"response.create","model":"grok","input":[{"type":"message","id":"msg-1","role":"user","content":"first"}]}"#,
        r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"message","id":"msg-2","role":"user","content":"second"}]}"#,
    ] {
        send(&mut ws, request).await;
        assert_eq!(recv(&mut ws).await["type"], "response.completed");
    }
    let calls = dispatcher.calls();
    assert_eq!(calls[0].options.metadata.pinned_auth_id, None);
    assert_eq!(
        calls[1].options.metadata.pinned_auth_id.as_deref(),
        Some("auth-x")
    );
    let second = parse(&calls[1].request.payload);
    assert_eq!(second["type"], "response.create");
    assert_eq!(second["model"], "grok");
    assert_eq!(second["previous_response_id"], "resp-1");
    assert_eq!(item_ids(&second["input"]), ["msg-2"]);

    // The credential loses its websockets, so a delta has nowhere to go.
    dispatcher.websocket(support_for("xai", false));
    send(
        &mut ws,
        r#"{"type":"response.create","previous_response_id":"resp-2","input":[{"type":"message","id":"msg-3"}]}"#,
    )
    .await;
    assert_eq!(
        recv_end(&mut ws).await,
        Some((1012, "upstream requires HTTP replay".to_owned()))
    );
    assert_eq!(dispatcher.calls().len(), 2);

    let mut ws = connect(&url, &[]).await;
    for request in [
        r#"{"type":"response.create","model":"grok","input":[{"type":"message","id":"msg-1"},{"type":"message","id":"out-1"},{"type":"message","id":"msg-2"},{"type":"message","id":"out-2"},{"type":"message","id":"msg-3"}]}"#,
        r#"{"type":"response.create","previous_response_id":"resp-3","input":[{"type":"message","id":"msg-4"}]}"#,
    ] {
        send(&mut ws, request).await;
        assert_eq!(recv(&mut ws).await["type"], "response.completed");
    }
    assert_eq!(dispatcher.calls().len(), 4);
    let delta = payload_of(&dispatcher, 3);
    assert!(delta.get("previous_response_id").is_none(), "{delta}");
    assert_eq!(
        item_ids(&delta["input"]),
        [
            "msg-1", "out-1", "msg-2", "out-2", "msg-3", "out-3", "msg-4"
        ]
    );
}

/// Runs a turn on `auth-a`'s upstream WebSocket, then a delta whose stream
/// is `second`: the server's URL and dispatcher, and the socket.
async fn pinned_delta(second: Outcome) -> (String, Arc<FakeDispatcher>, Client) {
    let (url, dispatcher) = serve(
        FakeCatalog::new().serve("test-model", &["xai"]),
        vec![
            Outcome::via(&["auth-a"], completes_with("resp-auth-a-1", "out-auth-a-1")),
            Outcome::via(&["auth-a"], second),
            Outcome::via(&["auth-b"], completes_with("resp-auth-b-1", "out-auth-b-1")),
        ],
    )
    .await;
    dispatcher.websocket(support_for("xai", true));
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    send(
        &mut ws,
        r#"{"type":"response.create","previous_response_id":"resp-auth-a-1","input":[{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    (url, dispatcher, ws)
}

/// Replays the whole conversation on a new socket, and checks it went out
/// as one request with no pin.
async fn replay_in_full(url: &str, dispatcher: &FakeDispatcher) {
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].options.metadata.pinned_auth_id.as_deref(),
        Some("auth-a")
    );
    let mut ws = connect(url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"},{"type":"message","id":"out-auth-a-1"},{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].options.metadata.pinned_auth_id, None);
    let replay = parse(&calls[2].request.payload);
    assert!(replay.get("previous_response_id").is_none(), "{replay}");
    assert_eq!(item_ids(&replay["input"]).len(), 3);
}

#[tokio::test]
async fn replays_immediately_after_pinned_auth_failure() {
    for (status, body) in [
        (
            401,
            r#"{"error":{"type":"authentication_error","message":"Invalid token"}}"#,
        ),
        (
            429,
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"}}"#,
        ),
    ] {
        let failure = Outcome::Stream(
            HeaderMap::new(),
            vec![Err(ExecError::upstream(status, body))],
        );
        let (url, dispatcher, mut ws) = pinned_delta(failure).await;
        assert_eq!(
            recv_end(&mut ws).await,
            Some((1012, "upstream requires HTTP replay".to_owned())),
            "{status}"
        );
        replay_in_full(&url, &dispatcher).await;
    }
}

#[tokio::test]
async fn releases_pinned_auth_after_premature_close() {
    let partial = sse(
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"partial"}}"#,
    );
    let (url, dispatcher, mut ws) = pinned_delta(Outcome::chunks(&[&partial])).await;
    let (messages, _) = rest(&mut ws).await;
    assert!(
        messages.iter().all(|message| message["type"] != "error"),
        "{messages:?}"
    );
    replay_in_full(&url, &dispatcher).await;
}

#[tokio::test]
async fn compaction_resets_turn_state_on_transcript_replacement() {
    let cases = [
        (
            r#"[{"type":"function_call_output","call_id":"call-1","id":"tool-out-1"}]"#,
            r#"[{"type":"function_call","id":"fc-compact","call_id":"call-1","name":"tool"},{"type":"message","id":"msg-2"}]"#,
            vec!["fc-compact", "msg-2"],
        ),
        (
            r#"[{"type":"custom_tool_call_output","call_id":"call-1","id":"tool-out-1"}]"#,
            r#"[{"type":"custom_tool_call","id":"ctc-compact","call_id":"call-1","name":"apply_patch"},{"type":"custom_tool_call_output","id":"tool-out-compact","call_id":"call-1"},{"type":"message","id":"msg-2"}]"#,
            vec!["ctc-compact", "tool-out-compact", "msg-2"],
        ),
    ];
    for (tool_output, replacement, want) in cases {
        let (url, dispatcher) = serve(
            test_catalog(),
            vec![
                completes(
                    "resp-1",
                    r#"[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"tool"}]"#,
                ),
                completes_with("resp-2", "assistant-1"),
                completes_with("resp-3", "assistant-2"),
            ],
        )
        .await;
        let mut ws = connect(&url, &[]).await;
        for input in [
            r#"[{"type":"message","id":"msg-1"}]"#,
            tool_output,
            replacement,
        ] {
            send(
                &mut ws,
                &format!(r#"{{"type":"response.create","model":"test-model","input":{input}}}"#),
            )
            .await;
            assert_eq!(recv(&mut ws).await["type"], "response.completed");
        }
        let merged = payload_of(&dispatcher, 2);
        assert_eq!(item_ids(&merged["input"]), want, "{merged}");
        assert_eq!(merged["input"][0]["call_id"], "call-1");
    }
}

/// Runs a turn, a compaction, and then `turns`, all on one credential that
/// has no upstream WebSocket.
async fn observed_compaction(turns: &[String]) -> Arc<FakeDispatcher> {
    let mut outcomes = vec![
        Outcome::via(
            &["auth-sse"],
            completes(
                "resp-1",
                r#"[{"type":"message","role":"assistant","id":"old-assistant"}]"#,
            ),
        ),
        Outcome::via(
            &["auth-sse"],
            completes(
                "resp-2",
                r#"[{"type":"compaction","id":"cmp-1","encrypted_content":"opaque"}]"#,
            ),
        ),
    ];
    for index in 3..turns.len() + 3 {
        outcomes.push(Outcome::via(
            &["auth-sse"],
            completes_with(&format!("resp-{index}"), &format!("assistant-{index}")),
        ));
    }
    let (url, dispatcher) = serve(
        FakeCatalog::new()
            .serve("test-model", &["test-provider"])
            .serve("other-model", &["test-provider"]),
        outcomes,
    )
    .await;
    dispatcher.websocket(support_for("test-provider", false));
    let mut ws = connect(&url, &[]).await;
    let start = [
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","role":"user","id":"old-user"}]}"#.to_owned(),
        r#"{"type":"response.create","input":[{"type":"compaction_trigger"}]}"#.to_owned(),
    ];
    for request in start.iter().chain(turns) {
        send(&mut ws, request).await;
        assert_eq!(
            recv(&mut ws).await["type"],
            "response.completed",
            "{request}"
        );
    }
    dispatcher
}

/// A turn that carries the compacted transcript and the user message `id`,
/// naming `model` unless it is empty.
fn compacted_turn(model: &str, id: &str) -> String {
    let model = if model.is_empty() {
        String::new()
    } else {
        format!(r#","model":"{model}""#)
    };
    format!(
        r#"{{"type":"response.create"{model},"input":[{{"type":"compaction","id":"cmp-1","encrypted_content":"opaque"}},{{"type":"message","role":"user","id":"{id}"}}]}}"#
    )
}

/// Checks that `input` is the compaction, then the user message `id`.
fn assert_compacted(input: &Value, id: &str) {
    assert_eq!(item_ids(input), ["cmp-1", id], "{input}");
    assert_eq!(input[0]["type"], "compaction");
    assert_eq!(input[0]["encrypted_content"], "opaque");
    assert_eq!(input[1]["type"], "message");
    assert_eq!(input[1]["role"], "user");
}

#[tokio::test]
async fn uses_observed_compaction_response_for_replay() {
    let dispatcher = observed_compaction(&[compacted_turn("", "new-user")]).await;
    let calls = dispatcher.calls();
    assert_compacted(&parse(&calls[2].request.payload)["input"], "new-user");
    assert_eq!(
        calls[2].options.metadata.pinned_auth_id.as_deref(),
        Some("auth-sse")
    );

    let dispatcher = observed_compaction(&[compacted_turn("other-model", "new-user")]).await;
    assert_eq!(
        item_ids(&payload_of(&dispatcher, 2)["input"]),
        ["old-user", "old-assistant", "cmp-1", "new-user"]
    );
}

#[tokio::test]
async fn retains_observed_compaction_across_subsequent_turns() {
    let dispatcher = observed_compaction(&[
        compacted_turn("", "turn-3-user"),
        compacted_turn("", "turn-4-user"),
    ])
    .await;
    assert_compacted(&payload_of(&dispatcher, 3)["input"], "turn-4-user");
}

/// Alice's request in review3: `call_1` and its output, with a secret in
/// the call's arguments.
const ALICE_TURN: &str = r#"{"type":"response.create","model":"test-model","input":[{"type":"function_call","id":"fc-1","call_id":"call_1","name":"secret","arguments":"{\"password\":\"alice-private\"}"},{"type":"function_call_output","id":"fco-1","call_id":"call_1","output":"alice private result"}]}"#;

/// The follow-up in review3: only an output for `call_1`.
const ORPHAN_TURN: &str = r#"{"type":"response.create","model":"test-model","input":[{"type":"function_call_output","id":"fco-2","call_id":"call_1","output":"attacker"}]}"#;

/// Connects as `key`, if any, with `Session-Id: shared`, sends Alice's turn
/// and waits until the server has cached its call. Gives the socket, which
/// must stay open to keep the session's caches.
async fn alice_turn(url: &str, state: &AppState, key: Option<&str>) -> Client {
    let (mut ws, _) = connect_key(url, key, &[("session-id", "shared")]).await;
    send(&mut ws, ALICE_TURN).await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    let principal = key.map_or(Principal::ANONYMOUS, |key| state.principal_tags().of(key));
    eventually(|| {
        state
            .tool_caches()
            .lock(principal)
            .calls
            .get("shared", "call_1")
            .is_some()
    })
    .await;
    ws
}

/// Sends the orphan output as `key`, if any, with `Session-Id: shared`, and
/// gives the input the dispatcher's call `index` was sent.
async fn orphan_turn(
    url: &str,
    dispatcher: &FakeDispatcher,
    key: Option<&str>,
    index: usize,
) -> Value {
    let (mut ws, _) = connect_key(url, key, &[("session-id", "shared")]).await;
    send(&mut ws, ORPHAN_TURN).await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    payload_of(dispatcher, index)["input"].clone()
}

#[tokio::test]
async fn tool_caches_do_not_cross_api_keys() {
    let (url, dispatcher, state) = serve_keys(
        test_catalog(),
        vec![completes("resp-a", "[]"), completes("resp-b", "[]")],
        &["sk-alice", "sk-bob"],
    )
    .await;
    let _alice = alice_turn(&url, &state, Some("sk-alice")).await;
    let input = orphan_turn(&url, &dispatcher, Some("sk-bob"), 1).await;
    assert_eq!(input, Value::Array(Vec::new()));
    let sent = String::from_utf8_lossy(&dispatcher.calls()[1].request.payload).into_owned();
    assert!(!sent.contains("alice-private"), "{sent}");
}

#[tokio::test]
async fn tool_caches_repair_within_one_api_key() {
    let (url, dispatcher, state) = serve_keys(
        test_catalog(),
        vec![completes("resp-a", "[]"), completes("resp-b", "[]")],
        &["sk-alice", "sk-bob"],
    )
    .await;
    let _alice = alice_turn(&url, &state, Some("sk-alice")).await;
    let input = orphan_turn(&url, &dispatcher, Some("sk-alice"), 1).await;
    assert_eq!(item_ids(&input), ["fc-1", "fco-2"]);
    assert_eq!(input[0]["arguments"], r#"{"password":"alice-private"}"#);
}

#[tokio::test]
async fn tool_caches_are_shared_without_api_keys() {
    let (url, dispatcher, state) = serve_keys(
        test_catalog(),
        vec![completes("resp-a", "[]"), completes("resp-b", "[]")],
        &[],
    )
    .await;
    let _first = alice_turn(&url, &state, None).await;
    let input = orphan_turn(&url, &dispatcher, None, 1).await;
    assert_eq!(item_ids(&input), ["fc-1", "fco-2"]);
}

// An event that grows past the limit ends the turn, and the call is dropped
// while the provider is still sending.
#[tokio::test]
async fn ends_the_turn_when_an_event_grows_past_the_limit() {
    let mib = Bytes::from(vec![b'x'; 1 << 20]);
    let mut chunks = vec![
        Ok(Bytes::from(sse(
            r#"{"type":"response.created","response":{"id":"resp-1"}}"#,
        ))),
        Ok(Bytes::from_static(b"data: {\"delta\":\"")),
    ];
    chunks.extend((0..=crate::sse_check::MAX_EVENT_BYTES >> 20).map(|_| Ok(mib.clone())));
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![Outcome::Hang(HeaderMap::new(), chunks)],
    )
    .await;
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"test-model","input":[{"type":"message","id":"msg-1"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.created");
    // The 502 is the server's own, which the client isn't told: the socket
    // just ends.
    let end = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            match ws.next().await {
                Some(Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_))) => {}
                end => return end,
            }
        }
    })
    .await
    .expect("the turn never ended");
    assert!(matches!(end, None | Some(Err(_))), "{end:?}");
    eventually(|| dispatcher.live_streams() == 0).await;
}

/// A `response.create` for `test-model` with `extra` members and one more
/// nested so the request is `depth` levels deep.
fn nested_create(extra: &str, depth: usize) -> String {
    format!(
        r#"{{"type":"response.create","model":"test-model"{extra},"deep":{}0{}}}"#,
        "[".repeat(depth - 1),
        "]".repeat(depth - 1)
    )
}

// Not upstream's: a request with 128 or more arrays and objects inside one
// another is answered with a 400 error event, whatever it asks for, and the
// session goes on as if it hadn't been sent: nothing is called, a warm-up
// isn't answered, and the transcript is as it was. One of 127 is a turn.
// Upstream forwards a request of any depth.
#[tokio::test]
async fn refuses_requests_nested_too_deeply() {
    let (url, dispatcher) = serve(
        test_catalog(),
        vec![
            completes_with("resp-1", "out-1"),
            completes_with("resp-2", "out-2"),
        ],
    )
    .await;
    let mut ws = connect(&url, &[]).await;
    let refused = |error: &Value| {
        assert_eq!(error["type"], "error", "{error}");
        assert_eq!(error["status"], 400, "{error}");
        assert_eq!(error["error"]["type"], "invalid_request_error", "{error}");
        let message = text_of(&error["error"]["message"]);
        assert!(message.contains("nested more than 127"), "{error}");
    };
    let message = r#","input":[{"type":"message","id":"msg-1"}]"#;
    // A turn, a warm-up, and a delta on a response there isn't.
    for (extra, depth) in [
        (message, 128),
        (r#","generate":false"#, 128),
        (r#","previous_response_id":"resp-1""#, 129),
        (message, 100_000),
    ] {
        send(&mut ws, &nested_create(extra, depth)).await;
        refused(&recv(&mut ws).await);
        assert!(dispatcher.calls().is_empty(), "{extra}: {depth}");
    }

    send(&mut ws, &nested_create(message, 127)).await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    assert_eq!(dispatcher.calls().len(), 1);

    send(
        &mut ws,
        &nested_create(r#","previous_response_id":"resp-1""#, 128),
    )
    .await;
    refused(&recv(&mut ws).await);
    assert_eq!(dispatcher.calls().len(), 1);

    send(
        &mut ws,
        r#"{"type":"response.create","previous_response_id":"resp-1","input":[{"type":"message","id":"msg-2"}]}"#,
    )
    .await;
    assert_eq!(recv(&mut ws).await["type"], "response.completed");
    assert_eq!(dispatcher.calls().len(), 2);
    let forwarded = payload_of(&dispatcher, 1);
    assert_eq!(item_ids(&forwarded["input"]), ["msg-1", "out-1", "msg-2"]);
}
