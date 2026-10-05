// Ported from CLIProxyAPI internal/translator/claude/interactions/interactions_claude_test.go
// (the response tests) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests after them are new; the expected output of
// the stream and the collected stream comes from upstream.

use serde_json::{Value, json};

use super::*;

fn stream(model: &str, chunks: &[&str]) -> Vec<String> {
    let mut translator = ClaudeToInteractionsStream::new(model);
    chunks
        .iter()
        .flat_map(|chunk| translator.translate(chunk.as_bytes()))
        .collect()
}

/// `findClaudeInteractionsEventPayload`: the first `data:` payload whose
/// `event_type` or `type` is `event_type`.
fn find_event(frames: &[String], event_type: &str) -> Option<Value> {
    frames
        .iter()
        .flat_map(|frame| frame.lines())
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|payload| payload["event_type"] == event_type || payload["type"] == event_type)
}

fn non_stream(model: &str, body: &str) -> Value {
    convert_claude_response_to_interactions_non_stream(model, body.as_bytes())
}

// TestConvertClaudeResponseToInteractionsNonStream
#[test]
fn response_to_interactions_non_stream() {
    let out = non_stream(
        "claude-test",
        r#"{"id":"msg_1","model":"claude-test","content":[{"type":"thinking","thinking":"reasoning"},{"type":"text","text":"ok"},{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}],"usage":{"input_tokens":3,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":4,"thinking_tokens":5}}"#,
    );
    assert_eq!(out["steps"][0]["type"], "thought", "{out}");
    assert_eq!(out["steps"][1]["content"][0]["text"], "ok", "{out}");
    assert_eq!(out["steps"][2]["call_id"], "toolu_1", "{out}");
    assert_eq!(out["usage"]["total_tokens"], 5, "{out}");
    assert_eq!(out["usage"]["total_cached_tokens"], 5, "{out}");
}

// TestConvertClaudeSSEToInteractionsNonStream
#[test]
fn sse_to_interactions_non_stream() {
    let out = non_stream(
        "claude-test",
        concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":3,"output_tokens":0}}}"#,
            "\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            "\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n",
            r#"data: {"type":"message_delta","usage":{"output_tokens":2}}"#,
        ),
    );
    assert_eq!(out["steps"][0]["content"][0]["text"], "ok", "{out}");
    assert_eq!(out["usage"]["total_tokens"], 5, "{out}");
}

// TestConvertClaudeResponseToInteractionsStreamMergesUsageAndStatus
#[test]
fn stream_merges_usage_and_status() {
    let events = stream(
        "claude-test",
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":3,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","usage":{"output_tokens":2}}"#,
        ],
    );
    assert!(
        find_event(&events, "interaction.status_update").is_some(),
        "{events:?}"
    );
    let payload = find_event(&events, "interaction.completed").unwrap();
    let usage = &payload["interaction"]["usage"];
    assert_eq!(usage["total_input_tokens"], 3, "{payload}");
    assert_eq!(usage["total_output_tokens"], 2, "{payload}");
    assert_eq!(usage["total_tokens"], 5, "{payload}");
}

// TestConvertClaudeResponseToInteractionsStream
#[test]
fn response_to_interactions_stream() {
    let events = stream(
        "claude-test",
        &[
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ],
    );
    let payload = find_event(&events, "step.delta").unwrap();
    assert_eq!(payload["delta"]["text"], "ok", "{payload}");
}

/// Splits each frame into its event and data (JSON if it parses), checking
/// it is one `event:` line and one `data:` line ending in a blank line, and
/// replaces the generated interaction ID and timestamps with `ID` and `T`.
fn masked(frames: &[String]) -> Vec<(String, Value)> {
    frames
        .iter()
        .map(|frame| {
            let body = frame.strip_suffix("\n\n").unwrap();
            let (event, data) = body.split_once('\n').unwrap();
            let event = event.strip_prefix("event: ").unwrap();
            let data = data.strip_prefix("data: ").unwrap();
            let mut data =
                serde_json::from_str(data).unwrap_or_else(|_| Value::String(data.to_owned()));
            for pointer in ["/interaction/id", "/interaction_id"] {
                if let Some(id) = data.pointer_mut(pointer) {
                    assert!(id.as_str().unwrap().starts_with("interaction_"), "{frame}");
                    *id = "ID".into();
                }
            }
            for key in ["created", "updated"] {
                if let Some(time) = data.pointer_mut(&format!("/interaction/{key}")) {
                    let text = time.as_str().unwrap();
                    assert!(text.len() == 20 && text.ends_with('Z'), "{frame}");
                    *time = "T".into();
                }
            }
            (event.to_owned(), data)
        })
        .collect()
}

// Not upstream's: every event of a stream with a tool call, a thinking block
// that never started, a delta whose block index isn't the step's (which
// restarts the step), and lines that give nothing.
#[test]
fn stream_events() {
    let frames = stream(
        "m",
        &[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"signature_delta","signature":"s"}}"#,
            r#"data: {"type":"message_delta","usage":{"output_tokens":2}}"#,
            r#"data: {"type":"message_stop"}"#,
            "event: ping",
            "[DONE]",
            "data: [DONE]",
        ],
    );
    let events = masked(&frames);
    let expected = [
        (
            "interaction.created",
            json!({"interaction": {"id": "ID", "status": "in_progress", "object": "interaction", "model": "m"}, "event_type": "interaction.created"}),
        ),
        (
            "interaction.status_update",
            json!({"interaction_id": "ID", "status": "in_progress", "event_type": "interaction.status_update"}),
        ),
        (
            "step.start",
            json!({"index": 0, "step": {"type": "function_call", "name": "f", "id": "t1", "call_id": "t1", "arguments": {}}, "event_type": "step.start"}),
        ),
        (
            "step.delta",
            json!({"index": 0, "delta": {"arguments": "{\"a\"", "type": "arguments_delta"}, "event_type": "step.delta"}),
        ),
        ("step.stop", json!({"index": 0, "event_type": "step.stop"})),
        (
            "step.start",
            json!({"index": 1, "step": {"type": "thought"}, "event_type": "step.start"}),
        ),
        (
            "step.delta",
            json!({"index": 1, "delta": {"type": "thought_summary", "content": {"type": "text", "text": "hm"}}, "event_type": "step.delta"}),
        ),
        ("step.stop", json!({"index": 1, "event_type": "step.stop"})),
        (
            "step.start",
            json!({"index": 2, "step": {"type": "thought"}, "event_type": "step.start"}),
        ),
        ("step.stop", json!({"index": 2, "event_type": "step.stop"})),
        (
            "interaction.completed",
            json!({
                "interaction": {
                    "id": "ID",
                    "status": "completed",
                    "usage": {"output_tokens": 2, "total_output_tokens": 2, "total_tokens": 2},
                    "created": "T",
                    "updated": "T",
                    "service_tier": "standard",
                    "object": "interaction",
                    "model": "m",
                },
                "event_type": "interaction.completed",
            }),
        ),
        ("done", json!("[DONE]")),
    ]
    .map(|(event, data)| (event.to_owned(), data));
    assert_eq!(events, expected);
}

// Not upstream's: a whole Claude stream collected into one interaction, with
// thinking in two parts, tool input split across deltas around spaces, a
// start's input followed by a delta (no longer JSON), and a stop for a block
// that never started.
#[test]
fn sse_non_stream_collects_blocks() {
    let body = [
        r#"data: {"type":"message_start","message":{"id":"msg_9","model":"claude-y","usage":{"input_tokens":4,"cache_read_input_tokens":2}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"a"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"b"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"f","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" {\"x\":"}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"[1]} "}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"g","input":{"k":"v"}}}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"x"}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
        r#"data: {"type":"content_block_stop","index":7}"#,
        r#"data: {"type":"message_delta","usage":{"output_tokens":3,"thinking_tokens":1}}"#,
        "data: [DONE]",
    ]
    .join("\n");
    assert_eq!(
        non_stream("m", &body),
        json!({
            "id": "msg_9",
            "object": "interaction",
            "status": "completed",
            "model": "claude-y",
            "steps": [
                {"type": "thought", "content": [{"type": "text", "text": "ab"}]},
                {"type": "function_call", "name": "f", "arguments": {"x": [1]}, "id": "t", "call_id": "t"},
                {"type": "function_call", "name": "g", "arguments": {}},
                {"type": "model_output", "content": [{"type": "text", "text": ""}]},
            ],
            "usage": {
                "input_tokens": 4,
                "total_input_tokens": 4,
                "output_tokens": 3,
                "total_output_tokens": 3,
                "total_tokens": 7,
                "cached_tokens": 2,
                "total_cached_tokens": 2,
                "reasoning_tokens": 1,
                "total_thought_tokens": 1,
            },
        })
    );
}

// Not upstream's: Go's time.RFC3339 in UTC, across a leap day.
#[test]
fn rfc3339_formats_utc_seconds() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
}

// Not upstream's: a tool call's input keeps each number as written, as
// upstream copies it, whether the message is whole or streamed in pieces, and
// a message id sent as the number -0 stays "-0", as gjson's String() gives
// it (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let out = non_stream(
        "m",
        &format!(
            r#"{{"id":"msg_1","type":"message","role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"f","input":{spelled}}}],"stop_reason":"tool_use"}}"#
        ),
    );
    assert_eq!(out["steps"][0]["arguments"].to_string(), spelled);

    let pieces = serde_json::to_string(spelled).unwrap();
    let out = non_stream(
        "m",
        &format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\",\"input\":{{}}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{pieces}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n"
        ),
    );
    assert_eq!(out["steps"][0]["arguments"].to_string(), spelled);

    let out = non_stream(
        "m",
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":-0,\"model\":\"m\"}}\n\n",
    );
    assert_eq!(out["id"], "-0", "{out}");
    let frames = stream(
        "m",
        &[
            r#"data: {"type":"message_start","message":{"id":-0,"model":"m","usage":{"input_tokens":1}}}"#,
        ],
    );
    let created = find_event(&frames, "interaction.created").expect("interaction.created");
    assert_eq!(created["interaction"]["id"], "-0");
}
