// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_response_test.go
// and the response half of TestBuildClaudeToolNames_CustomToolCollision in
// claude_openai-responses_tool_names_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;
use crate::claude::openai::responses::test_support::*;

type Events = Vec<(String, Value)>;

/// The events `stream` emits for `lines`.
fn feed(stream: &mut ClaudeToOpenAIResponsesStream, lines: &[&str]) -> Events {
    let out: String = lines
        .iter()
        .map(|line| stream.translate_line(line.as_bytes()))
        .collect();
    sse_events(&out)
}

/// The events a fresh stream for `original_request` emits for `lines`. With
/// `Null`, this is also upstream's `translateClaudeResponsesStreamThroughRegistry`.
fn stream(original_request: &Value, lines: &[&str]) -> Events {
    let mut stream =
        ClaudeToOpenAIResponsesStream::new("claude-test", original_request, &Value::Null);
    feed(&mut stream, lines)
}

/// `lines` as one complete response for `original_request`.
fn complete(original_request: &Value, lines: &[&str]) -> Value {
    convert_claude_response_to_openai_responses_non_stream(
        original_request,
        &Value::Null,
        lines.join("\n").as_bytes(),
    )
}

/// A gjson-style dotted path, with array indices.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` reads it.
fn text(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The value at `path` as gjson's `Int()` reads it.
fn int(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
}

/// gjson's `path.#`: the length of the array at `path`.
fn count(value: &Value, path: &str) -> usize {
    at(value, path)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// The data of the last event named `name`, or `Null`.
fn last(events: &Events, name: &str) -> Value {
    events
        .iter()
        .rev()
        .find(|(event, _)| event == name)
        .map_or(Value::Null, |(_, data)| data.clone())
}

/// The data of the last `name` event whose item is of `item_type`.
fn last_item(events: &Events, name: &str, item_type: &str) -> Value {
    events
        .iter()
        .rev()
        .find(|(event, data)| event == name && text(data, "item.type") == item_type)
        .map_or(Value::Null, |(_, data)| data.clone())
}

fn count_events(events: &Events, name: &str) -> usize {
    events.iter().filter(|(event, _)| event == name).count()
}

/// Each `output_item.added` and `.done` as `event:output_index:item_type`.
fn lifecycle(events: &Events) -> Vec<String> {
    events
        .iter()
        .filter(|(event, _)| {
            event == "response.output_item.added" || event == "response.output_item.done"
        })
        .map(|(event, data)| {
            format!(
                "{event}:{}:{}",
                int(data, "output_index"),
                text(data, "item.type")
            )
        })
        .collect()
}

/// The `type` of each item in `output`.
fn output_types(output: &Value) -> Vec<String> {
    output
        .as_array()
        .map(|items| items.iter().map(|item| text(item, "type")).collect())
        .unwrap_or_default()
}

const MESSAGE_START: &str = r#"data: {"type":"message_start","message":{"id":"msg_123","usage":{"input_tokens":1,"output_tokens":0}}}"#;

#[test]
fn created_and_in_progress_name_the_client_request_model() {
    let mut stream = ClaudeToOpenAIResponsesStream::new(
        "fallback-model",
        &json!({"model": "original-claude-model"}),
        &json!({"model": "translated-claude-model"}),
    );
    let events = feed(
        &mut stream,
        &[r#"data: {"type":"message_start","message":{"id":"msg_123"}}"#],
    );
    assert!(events.len() >= 2, "{events:?}");
    let created = last(&events, "response.created");
    assert_eq!(text(&created, "response.model"), "original-claude-model");
    let in_progress = last(&events, "response.in_progress");
    assert_eq!(
        text(&in_progress, "response.model"),
        "original-claude-model"
    );
}

#[test]
fn thinking_signature_becomes_encrypted_content() {
    let signature = "claude_sig_123";
    let signature_delta = format!(
        r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"signature_delta","signature":"{signature}"}}}}"#
    );
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"internal "}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reasoning"}}"#,
            &signature_delta,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let done = last_item(&events, "response.output_item.done", "reasoning");
    assert!(!done.is_null(), "no reasoning output_item.done");
    assert_eq!(text(&done, "item.encrypted_content"), signature);
    assert_eq!(text(&done, "item.summary.0.text"), "internal reasoning");
    let completed = last(&events, "response.completed");
    assert_eq!(
        text(&completed, "response.output.0.encrypted_content"),
        signature
    );
    assert_eq!(
        text(&completed, "response.output.0.summary.0.text"),
        "internal reasoning"
    );
}

#[test]
fn redacted_thinking_becomes_a_marked_reasoning_item() {
    let data = "EroBCkYIBRgCKkA";
    let start = format!(
        r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"redacted_thinking","data":"{data}"}}}}"#
    );
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            &start,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let want = format!("{REDACTED_THINKING_PREFIX}{data}");
    let done = last_item(&events, "response.output_item.done", "reasoning");
    assert!(!done.is_null(), "no reasoning output_item.done");
    assert_eq!(text(&done, "item.encrypted_content"), want);
    let completed = last(&events, "response.completed");
    assert_eq!(
        text(&completed, "response.output.0.encrypted_content"),
        want
    );
    assert_eq!(text(&completed, "response.output.1.type"), "message");
}

#[test]
fn complete_response_marks_redacted_thinking() {
    let data = "EroBCkYIBRgCKkA";
    let start = format!(
        r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"redacted_thinking","data":"{data}"}}}}"#
    );
    let out = complete(
        &Value::Null,
        &[
            MESSAGE_START,
            &start,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(text(&out, "output.0.type"), "reasoning", "{out}");
    assert_eq!(
        text(&out, "output.0.encrypted_content"),
        format!("{REDACTED_THINKING_PREFIX}{data}")
    );
}

#[test]
fn signature_delta_alone_emits_nothing() {
    let events = stream(
        &Value::Null,
        &[
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"claude_sig_123"}}"#,
        ],
    );
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn adjacent_text_blocks_share_a_message_until_another_item() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"**Compare competitors**\n- "}}"#,
            r#"data: {"type":"content_block_stop","index":4}"#,
            r#"data: {"type":"content_block_start","index":5,"content_block":{"type":"server_tool_use","id":"srv_123","name":"web_search","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Qwen3\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":5}"#,
            r#"data: {"type":"content_block_start","index":6,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_123","content":[{"type":"web_search_result","title":"Example","url":"https://example.com"}]}}"#,
            r#"data: {"type":"content_block_stop","index":6}"#,
            r#"data: {"type":"content_block_delta","index":5,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","cited_text":"Qwen 3.7 Max","url":"https://example.com","title":"Example"}}}"#,
            r#"data: {"type":"content_block_start","index":7,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":7,"delta":{"type":"text_delta","text":"Qwen 3.7 Max leads."}}"#,
            r#"data: {"type":"content_block_stop","index":7}"#,
            r#"data: {"type":"message_delta","usage":{"output_tokens":12}}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    for (event, _) in &events {
        assert!(
            !event.starts_with("content_block_") && event != "message_delta",
            "Claude event leaked: {event}"
        );
    }
    assert_eq!(count_events(&events, "response.output_item.added"), 3);
    assert_eq!(count_events(&events, "response.content_part.added"), 2);
    assert_eq!(count_events(&events, "response.output_text.done"), 2);
    assert_eq!(count_events(&events, "response.content_part.done"), 2);
    assert_eq!(count_events(&events, "response.output_item.done"), 3);
    assert_eq!(
        count_events(&events, "response.function_call_arguments.delta"),
        0
    );
    let completed = last(&events, "response.completed");
    assert_eq!(
        text(&completed, "response.output.0.content.0.text"),
        "**Compare competitors**\n- "
    );
    assert_eq!(
        text(&completed, "response.output.1.type"),
        "web_search_call"
    );
    assert_eq!(
        text(&completed, "response.output.2.content.0.text"),
        "Qwen 3.7 Max leads."
    );
    assert_eq!(
        text(&completed, "response.output.2.content.0.annotations.0.type"),
        "web_search_result_location"
    );
}

#[test]
fn message_is_done_before_a_function_call_starts() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking the workspace."}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(
        lifecycle(&events),
        [
            "response.output_item.added:0:message",
            "response.output_item.done:0:message",
            "response.output_item.added:1:function_call",
            "response.output_item.done:1:function_call",
        ]
    );
    let completed = last(&events, "response.completed");
    assert_eq!(count(&completed, "response.output"), 2);
    assert_eq!(text(&completed, "response.output.0.type"), "message");
    assert_eq!(
        text(&completed, "response.output.0.content.0.text"),
        "Checking the workspace."
    );
    assert_eq!(text(&completed, "response.output.1.type"), "function_call");
    assert_eq!(text(&completed, "response.output.1.call_id"), "call_123");
}

#[test]
fn search_reasoning_text_and_tool_get_contiguous_output_indices() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_123","name":"web_search","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Qwen3\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_123","content":[]}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"Inspect first."}}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
            r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"Checking the workspace."}}"#,
            r#"data: {"type":"content_block_stop","index":3}"#,
            r#"data: {"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":4}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let mut seen = HashMap::<&str, usize>::new();
    for (event, data) in &events {
        let (item_type, want) =
            if event == "response.output_item.added" || event == "response.output_item.done" {
                match &*text(data, "item.type") {
                    "web_search_call" => ("web_search_call", 0),
                    "reasoning" => ("reasoning", 1),
                    "message" => ("message", 2),
                    "function_call" => ("function_call", 3),
                    _ => continue,
                }
            } else if event.starts_with("response.reasoning_") {
                ("reasoning", 1)
            } else if event.starts_with("response.output_text.")
                || event.starts_with("response.content_part.")
            {
                ("message", 2)
            } else if event.starts_with("response.function_call_arguments.") {
                ("function_call", 3)
            } else {
                continue;
            };
        assert!(data.get("output_index").is_some(), "{event}: {data}");
        assert_eq!(int(data, "output_index"), want, "{item_type} {event}");
        *seen.entry(item_type).or_default() += 1;
    }
    for item_type in ["reasoning", "message", "function_call"] {
        assert!(seen.contains_key(item_type), "no {item_type} events");
    }
    let completed = last(&events, "response.completed");
    assert_eq!(
        output_types(&completed["response"]["output"]),
        ["web_search_call", "reasoning", "message", "function_call"]
    );
}

#[test]
fn server_tools_leave_no_output_index_gaps() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Searching. "}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srv_123","name":"web_search","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"Qwen3\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_123","content":[]}}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
            r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"Found it."}}"#,
            r#"data: {"type":"content_block_stop","index":3}"#,
            r#"data: {"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":4}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let mut messages_added = 0;
    let mut messages_done = 0;
    for (event, data) in &events {
        let item_type = text(data, "item.type");
        if event == "response.output_item.added" && item_type == "message" {
            messages_added += 1;
        } else if event == "response.output_item.done" && item_type == "message" {
            messages_done += 1;
        } else if (event.starts_with("response.output_item.") && item_type == "function_call")
            || event.starts_with("response.function_call_arguments.")
        {
            assert_eq!(int(data, "output_index"), 3, "{event}");
        }
    }
    assert_eq!((messages_added, messages_done), (2, 2));
    let completed = last(&events, "response.completed");
    assert_eq!(
        output_types(&completed["response"]["output"]),
        ["message", "web_search_call", "message", "function_call"]
    );
}

#[test]
fn text_after_a_function_call_starts_a_new_message() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Before tool."}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"After tool."}}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(
        lifecycle(&events),
        [
            "response.output_item.added:0:message",
            "response.output_item.done:0:message",
            "response.output_item.added:1:function_call",
            "response.output_item.done:1:function_call",
            "response.output_item.added:2:message",
            "response.output_item.done:2:message",
        ]
    );
    let message_ids: Vec<String> = events
        .iter()
        .filter(|(event, data)| {
            event == "response.output_item.added" && text(data, "item.type") == "message"
        })
        .map(|(_, data)| text(data, "item.id"))
        .collect();
    assert_eq!(message_ids.len(), 2);
    assert_ne!(message_ids[0], message_ids[1]);
    let completed = last(&events, "response.completed");
    assert_eq!(
        output_types(&completed["response"]["output"]),
        ["message", "function_call", "message"]
    );
    assert_eq!(
        text(&completed, "response.output.0.content.0.text"),
        "Before tool."
    );
    assert_eq!(
        text(&completed, "response.output.2.content.0.text"),
        "After tool."
    );
}

#[test]
fn message_is_done_before_reasoning_starts() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Visible first."}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Reason later."}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(
        lifecycle(&events),
        [
            "response.output_item.added:0:message",
            "response.output_item.done:0:message",
            "response.output_item.added:1:reasoning",
            "response.output_item.done:1:reasoning",
        ]
    );
    let completed = last(&events, "response.completed");
    assert_eq!(
        output_types(&completed["response"]["output"]),
        ["message", "reasoning"]
    );
}

#[test]
fn each_thinking_block_is_its_own_reasoning_item() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"First reason."}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Second reason."}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Visible response."}}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let reasoning_done: Vec<i64> = events
        .iter()
        .filter(|(event, data)| {
            event == "response.output_item.done" && text(data, "item.type") == "reasoning"
        })
        .map(|(_, data)| int(data, "output_index"))
        .collect();
    assert_eq!(reasoning_done, [0, 1]);
    let completed = last(&events, "response.completed");
    assert_eq!(
        output_types(&completed["response"]["output"]),
        ["reasoning", "reasoning", "message"]
    );
    assert_eq!(
        text(&completed, "response.output.0.summary.0.text"),
        "First reason."
    );
    assert_eq!(
        text(&completed, "response.output.1.summary.0.text"),
        "Second reason."
    );
}

#[test]
fn empty_function_arguments_become_an_empty_object() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_123","name":"exec_command","input":{}}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let done = last_item(&events, "response.output_item.done", "function_call");
    assert_eq!(text(&done, "item.arguments"), "{}");
    let completed = last(&events, "response.completed");
    assert_eq!(text(&completed, "response.output.0.arguments"), "{}");
}

#[test]
fn empty_reasoning_is_kept_in_the_completed_output() {
    let events = stream(
        &Value::Null,
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Visible response."}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let done = last_item(&events, "response.output_item.done", "reasoning");
    assert_eq!(count(&done, "item.summary"), 1);
    let completed = last(&events, "response.completed");
    assert_eq!(
        output_types(&completed["response"]["output"]),
        ["reasoning", "message"]
    );
    assert_eq!(count(&completed, "response.output.0.summary"), 1);
}

#[test]
fn stream_counts_cached_tokens_as_input() {
    let events = stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_123","usage":{"input_tokens":13,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}"#,
            r#"data: {"type":"message_delta","usage":{"output_tokens":4,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let completed = last(&events, "response.completed");
    assert!(!completed.is_null(), "no response.completed");
    assert_eq!(int(&completed, "response.usage.input_tokens"), 22044);
    assert_eq!(
        int(
            &completed,
            "response.usage.input_tokens_details.cached_tokens"
        ),
        22000
    );
    assert_eq!(int(&completed, "response.usage.output_tokens"), 4);
    assert_eq!(int(&completed, "response.usage.total_tokens"), 22048);
}

#[test]
fn complete_response_thinking_signature_becomes_encrypted_content() {
    let signature = "claude_sig_nonstream";
    let signature_delta = format!(
        r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"signature_delta","signature":"{signature}"}}}}"#
    );
    let out = complete(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"nonstream reasoning"}}"#,
            &signature_delta,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(text(&out, "output.0.encrypted_content"), signature);
    assert_eq!(text(&out, "output.0.summary.0.text"), "nonstream reasoning");
}

#[test]
fn complete_response_keeps_content_block_order() {
    let out = complete(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_nonstream_order","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_order","name":"exec_command","input":{}}}"#,
            r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
            r#"data: {"type":"content_block_stop","index":3}"#,
            r#"data: {"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":"more"}}"#,
            r#"data: {"type":"content_block_stop","index":4}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(
        output_types(&out["output"]),
        [
            "message",
            "reasoning",
            "function_call",
            "message",
            "reasoning"
        ]
    );
    assert_eq!(text(&out, "output.0.content.0.text"), "");
    assert_eq!(text(&out, "output.1.summary.0.text"), "plan");
    assert_eq!(text(&out, "output.2.call_id"), "call_order");
    assert_eq!(text(&out, "output.2.arguments"), r#"{"cmd":"pwd"}"#);
    assert_eq!(text(&out, "output.3.content.0.text"), "done");
    assert_eq!(text(&out, "output.4.summary.0.text"), "more");
    assert_eq!(int(&out, "usage.output_tokens_details.reasoning_tokens"), 2);
}

#[test]
fn complete_response_counts_cached_tokens_as_input() {
    let out = complete(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_nonstream","usage":{"input_tokens":13,"output_tokens":1,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}}"#,
            r#"data: {"type":"message_delta","usage":{"output_tokens":4}}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(int(&out, "usage.input_tokens"), 22044);
    assert_eq!(int(&out, "usage.input_tokens_details.cached_tokens"), 22000);
    assert_eq!(int(&out, "usage.output_tokens"), 4);
    assert_eq!(int(&out, "usage.total_tokens"), 22048);
}

#[test]
fn custom_tool_in_an_additional_namespace_gets_its_name_back() {
    let request = json!({
        "model": "gpt-test",
        "input": [{"type": "additional_tools", "role": "developer", "tools": [
            {"type": "namespace", "name": "functions", "tools": [{"type": "custom", "name": "exec"}]}
        ]}]
    });
    let events = stream(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_custom","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom","name":"functions__exec","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let added = last_item(&events, "response.output_item.added", "custom_tool_call");
    let input_done = last(&events, "response.custom_tool_call_input.done");
    let done = last_item(&events, "response.output_item.done", "custom_tool_call");
    let completed = last(&events, "response.completed");
    for event in [&added, &input_done, &done, &completed] {
        assert!(!event.is_null(), "missing custom tool lifecycle event");
    }
    assert_eq!(
        count_events(&events, "response.function_call_arguments.delta")
            + count_events(&events, "response.function_call_arguments.done"),
        0
    );
    for (label, item) in [
        ("added", &added["item"]),
        ("done", &done["item"]),
        ("completed", &completed["response"]["output"][0]),
    ] {
        assert_eq!(text(item, "name"), "exec", "{label}");
        assert_eq!(text(item, "namespace"), "functions", "{label}");
    }
    assert_eq!(text(&input_done, "input"), "pwd");
    assert_eq!(text(&done, "item.input"), "pwd");
    assert_eq!(
        text(&completed, "response.output.0.type"),
        "custom_tool_call"
    );
    assert_eq!(text(&completed, "response.output.0.input"), "pwd");
}

#[test]
fn direct_custom_tool_wins_a_namespace_collision() {
    let request = json!({
        "model": "gpt-test",
        "tools": [
            {"type": "namespace", "name": "n", "tools": [{"type": "function", "name": "x"}]},
            {"type": "custom", "name": "n__x"}
        ]
    });
    let events = stream(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_collision","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_collision","name":"n__x","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let completed = last(&events, "response.completed");
    let out = complete(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_collision_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_collision_nonstream","name":"n__x","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    for item in [&completed["response"]["output"][0], &out["output"][0]] {
        assert_eq!(text(item, "type"), "custom_tool_call", "{item}");
        assert_eq!(text(item, "input"), "pwd");
        assert_eq!(text(item, "name"), "n__x");
        assert!(item.get("namespace").is_none(), "{item}");
    }
}

#[test]
fn complete_response_custom_tool_in_an_additional_namespace_gets_its_name_back() {
    let request = json!({
        "model": "gpt-test",
        "input": [{"type": "additional_tools", "role": "developer", "tools": [
            {"type": "namespace", "name": "functions", "tools": [{"type": "custom", "name": "exec"}]}
        ]}]
    });
    let out = complete(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_custom_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom_nonstream","name":"functions__exec","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"pwd\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(text(&out, "output.0.type"), "custom_tool_call", "{out}");
    assert_eq!(text(&out, "output.0.input"), "pwd");
    assert_eq!(text(&out, "output.0.call_id"), "call_custom_nonstream");
    assert_eq!(text(&out, "output.0.name"), "exec");
    assert_eq!(text(&out, "output.0.namespace"), "functions");
}

#[test]
fn empty_custom_tool_input_is_the_same_streamed_or_not() {
    let request = json!({"model": "gpt-test", "tools": [{"type": "custom", "name": "exec"}]});
    let events = stream(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_custom_empty","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom_empty","name":"exec","input":{}}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let completed = last(&events, "response.completed");
    assert_eq!(text(&completed, "response.output.0.input"), "");
    let out = complete(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_custom_empty_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_custom_empty","name":"exec","input":{}}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(text(&out, "output.0.input"), "");
}

fn node_repl_request() -> Value {
    json!({
        "model": "gpt-test",
        "tools": [{
            "type": "namespace",
            "name": "mcp__node_repl",
            "tools": [{"type": "function", "name": "js", "parameters": {"type": "object", "properties": {}}}]
        }]
    })
}

#[test]
fn namespaced_function_call_gets_its_name_and_namespace_back() {
    let events = stream(
        &node_repl_request(),
        &[
            MESSAGE_START,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_abc","name":"mcp__node_repl__js","input":{}}}"#,
            // Upstream's line, which isn't valid JSON; it is dropped here.
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{"code":"nodeRepl.write('hello')"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    for name in ["response.output_item.added", "response.output_item.done"] {
        let event = last_item(&events, name, "function_call");
        assert!(!event.is_null(), "no function_call {name}");
        assert_eq!(text(&event, "item.name"), "js", "{name}");
        assert_eq!(text(&event, "item.namespace"), "mcp__node_repl", "{name}");
    }
    let completed = last(&events, "response.completed");
    assert!(!completed.is_null(), "no response.completed");
    assert_eq!(text(&completed, "response.output.0.name"), "js");
    assert_eq!(
        text(&completed, "response.output.0.namespace"),
        "mcp__node_repl"
    );
}

#[test]
fn complete_response_namespaced_function_call_gets_its_name_and_namespace_back() {
    let out = complete(
        &node_repl_request(),
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_nonstream","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_abc","name":"mcp__node_repl__js","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"code\":\"nodeRepl.write('hello')\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(text(&out, "output.0.name"), "js", "{out}");
    assert_eq!(text(&out, "output.0.namespace"), "mcp__node_repl");
}

const MAX_TOKENS_DELTA: &str = r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":64000}}"#;

/// The events a fresh stream for model `claude-fable-5-1` emits.
fn fable_stream(original_request: &Value, lines: &[&str]) -> Events {
    let mut stream =
        ClaudeToOpenAIResponsesStream::new("claude-fable-5-1", original_request, &Value::Null);
    feed(&mut stream, lines)
}

#[test]
fn max_tokens_ends_the_stream_incomplete() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_max_tokens","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"unfinished reasoning"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_max_tokens"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(count_events(&events, "response.completed"), 0);
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.status"), "incomplete");
    assert_eq!(
        text(&incomplete, "response.incomplete_details.reason"),
        "max_output_tokens"
    );
    assert_eq!(text(&incomplete, "response.output.0.type"), "reasoning");
    assert_eq!(text(&incomplete, "response.output.0.status"), "incomplete");
    assert_eq!(
        text(&incomplete, "response.output.0.summary.0.text"),
        "unfinished reasoning"
    );
    assert_eq!(int(&incomplete, "response.usage.output_tokens"), 64000);
}

#[test]
fn complete_response_cut_off_by_max_tokens_keeps_partial_text() {
    let out = complete(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_partial","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial answer"}}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(text(&out, "status"), "incomplete", "{out}");
    assert_eq!(text(&out, "incomplete_details.reason"), "max_output_tokens");
    assert_eq!(text(&out, "output.0.status"), "incomplete");
    assert_eq!(text(&out, "output.0.content.0.text"), "partial answer");
}

#[test]
fn max_tokens_mid_tool_call_leaves_finished_text_completed() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_tool_incomplete","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"calling tool"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_inc_1","name":"get_weather","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"San"}}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let func_done = last_item(&events, "response.output_item.done", "function_call");
    assert!(!func_done.is_null(), "no function_call output_item.done");
    assert_eq!(text(&func_done, "item.status"), "incomplete");
    assert_eq!(text(&func_done, "item.arguments"), r#"{"city":"San"#);
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.status"), "incomplete");
    assert_eq!(
        text(&incomplete, "response.incomplete_details.reason"),
        "max_output_tokens"
    );
    assert_eq!(text(&incomplete, "response.output.0.status"), "completed");
    assert_eq!(
        text(&incomplete, "response.output.0.content.0.text"),
        "calling tool"
    );
    assert_eq!(text(&incomplete, "response.output.1.status"), "incomplete");
    assert_eq!(text(&incomplete, "response.output.1.type"), "function_call");
    assert_eq!(
        text(&incomplete, "response.output.1.arguments"),
        r#"{"city":"San"#
    );
}

#[test]
fn max_tokens_after_a_web_search_leaves_it_incomplete() {
    let lines = [
        r#"data: {"type":"message_start","message":{"id":"msg_ws_incomplete","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"golang\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        MAX_TOKENS_DELTA,
        r#"data: {"type":"message_stop"}"#,
    ];
    let events = fable_stream(&Value::Null, &lines);
    let search_done = last_item(&events, "response.output_item.done", "web_search_call");
    assert!(
        !search_done.is_null(),
        "no web_search_call output_item.done"
    );
    assert_eq!(text(&search_done, "item.status"), "incomplete");
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.output.0.status"), "incomplete");

    let out = complete(&Value::Null, &lines);
    assert_eq!(text(&out, "status"), "incomplete", "{out}");
    assert_eq!(text(&out, "output.0.status"), "incomplete");
}

#[test]
fn max_tokens_before_a_thinking_block_stops_leaves_it_incomplete() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_reasoning_nostop","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"partial thought before cut"}}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let reasoning_done = last_item(&events, "response.output_item.done", "reasoning");
    assert!(!reasoning_done.is_null(), "no reasoning output_item.done");
    assert_eq!(text(&reasoning_done, "item.status"), "incomplete");
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.output.0.status"), "incomplete");
    assert_eq!(text(&incomplete, "response.output.0.type"), "reasoning");
    assert_eq!(
        text(&incomplete, "response.output.0.summary.0.text"),
        "partial thought before cut"
    );
}

#[test]
fn max_tokens_after_a_stopped_tool_call_leaves_it_incomplete() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_tool_blockstop","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_stop_1","name":"do_work","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"step\":1}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let func_done = last_item(&events, "response.output_item.done", "function_call");
    assert!(!func_done.is_null(), "no function_call output_item.done");
    assert_eq!(text(&func_done, "item.status"), "incomplete");
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.output.0.status"), "incomplete");
}

#[test]
fn max_tokens_after_web_search_results_leaves_the_search_incomplete() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_ws_res_incomplete","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_2","name":"web_search"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"golang\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_2","content":[{"type":"web_search_result","title":"Go","url":"https://golang.org"}]}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let search_done = last_item(&events, "response.output_item.done", "web_search_call");
    assert!(
        !search_done.is_null(),
        "no web_search_call output_item.done"
    );
    assert_eq!(text(&search_done, "item.status"), "incomplete");
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.output.0.status"), "incomplete");
}

#[test]
fn reasoning_done_events_are_sent_once() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_rs_once","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"full thought"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    assert_eq!(
        count_events(&events, "response.reasoning_summary_text.done"),
        1
    );
    assert_eq!(
        count_events(&events, "response.reasoning_summary_part.done"),
        1
    );
    assert_eq!(count_events(&events, "response.output_item.done"), 1);
}

#[test]
fn truncated_custom_tool_input_is_unwrapped() {
    let events = fable_stream(
        &json!({"tools": [{"type": "custom", "name": "bash"}]}),
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_custom_trunc","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_c1","name":"bash","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"echo \\u4F60"}}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let custom_done = last_item(&events, "response.output_item.done", "custom_tool_call");
    assert!(
        !custom_done.is_null(),
        "no custom_tool_call output_item.done"
    );
    assert_eq!(text(&custom_done, "item.input"), "echo 你");
    assert_eq!(text(&custom_done, "item.status"), "incomplete");
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.output.0.input"), "echo 你");
    assert_eq!(text(&incomplete, "response.output.0.status"), "incomplete");
}

#[test]
fn empty_function_arguments_stay_empty_when_cut_off() {
    let events = fable_stream(
        &Value::Null,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_empty_args_trunc","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_empty","name":"get_info","input":{}}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            MAX_TOKENS_DELTA,
            r#"data: {"type":"message_stop"}"#,
        ],
    );
    let args_done = last(&events, "response.function_call_arguments.done");
    assert!(!args_done.is_null(), "no function_call_arguments.done");
    let func_done = last_item(&events, "response.output_item.done", "function_call");
    assert!(!func_done.is_null(), "no function_call output_item.done");
    assert_eq!(text(&args_done, "arguments"), "");
    assert_eq!(text(&func_done, "item.arguments"), "");
    assert_eq!(text(&func_done, "item.status"), "incomplete");
    let incomplete = last(&events, "response.incomplete");
    assert!(!incomplete.is_null(), "no response.incomplete");
    assert_eq!(text(&incomplete, "response.output.0.arguments"), "");
}

const PATCH_REQUEST: &str = r#"{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}"#;

const PATCH_END: [&str; 2] = [
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
    r#"data: {"type":"message_stop"}"#,
];

const MESSAGE_STOP: &str = r#"data: {"type":"message_stop"}"#;

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("test JSON")
}

/// `text` as a JSON string. Upstream uses Go's `%q`, which writes the same
/// for the text these tests use.
fn quote(text: &str) -> String {
    Value::from(text).to_string()
}

/// `applyPatchClaudeStart`.
fn patch_start(index: i64, id: &str, name: &str) -> String {
    format!(
        r#"data: {{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":{},"name":{},"input":{{}}}}}}"#,
        quote(id),
        quote(name)
    )
}

/// `applyPatchClaudeFragment`.
fn patch_fragment(index: i64, fragment: &str) -> String {
    format!(
        r#"data: {{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
        quote(fragment)
    )
}

/// A block 0 `tool_use` start that carries `input` as raw JSON.
fn patch_snapshot(id: &str, name: &str, input: &str) -> String {
    format!(
        r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}}}"#
    )
}

fn patch_end() -> Vec<String> {
    PATCH_END.map(str::to_owned).to_vec()
}

fn patch_stream(request: &str) -> ClaudeToOpenAIResponsesStream {
    ClaudeToOpenAIResponsesStream::new("test", &parse(request), &Value::Null)
}

/// `applyPatchClaudeEvents`: the data of each event `stream` emits for
/// `line`.
fn data(stream: &mut ClaudeToOpenAIResponsesStream, line: &str) -> Vec<Value> {
    sse_events(&stream.translate_line(line.as_bytes()))
        .into_iter()
        .map(|(_, data)| data)
        .collect()
}

fn data_all(stream: &mut ClaudeToOpenAIResponsesStream, lines: &[String]) -> Vec<Value> {
    lines.iter().flat_map(|line| data(stream, line)).collect()
}

fn kind(event: &Value) -> String {
    text(event, "type")
}

/// The `response` of the last `response.completed` in `events`, or `Null`.
fn completed_response(events: &[Value]) -> Value {
    events
        .iter()
        .rfind(|event| kind(event) == "response.completed")
        .map_or(Value::Null, |event| event["response"].clone())
}

/// The complete response for `lines` joined by newlines, and the error
/// upstream keeps in its state.
fn patch_complete(
    original_request: &str,
    request: &Value,
    lines: &[String],
) -> (Value, Option<ToolInputError>) {
    non_stream(
        &parse(original_request),
        request,
        lines.join("\n").as_bytes(),
    )
}

#[test]
fn apply_patch_previews_decoded_input_before_done() {
    let mut stream = patch_stream(PATCH_REQUEST);
    data(&mut stream, &patch_start(0, "c1", "apply_patch"));
    let preview = data(
        &mut stream,
        &patch_fragment(
            0,
            r#"{"input":"*** Begin Patch\n*** Add File: a.txt\n+hello\n"#,
        ),
    );
    assert_eq!(preview.len(), 1, "{preview:?}");
    assert_eq!(kind(&preview[0]), "response.custom_tool_call_input.delta");
    assert_eq!(
        text(&preview[0], "delta"),
        "*** Begin Patch\n*** Add File: a.txt\n+hello\n"
    );
    assert_eq!(text(&preview[0], "call_id"), "c1");
    assert_eq!(text(&preview[0], "item_id"), "ctc_c1");
    let mut events = preview;
    events.extend(data(&mut stream, &patch_fragment(0, r#"*** End Patch"}"#)));
    events.extend(data_all(&mut stream, &patch_end()));

    let want = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch";
    let mut deltas = String::new();
    let (mut seen_done, mut seen_item, mut seen_completed) = (false, false, false);
    let mut last_sequence = 0;
    for event in &events {
        let sequence = int(event, "sequence_number");
        assert!(sequence > last_sequence, "non-monotonic sequence: {event}");
        last_sequence = sequence;
        match &*kind(event) {
            "response.custom_tool_call_input.delta" => deltas.push_str(&text(event, "delta")),
            "response.custom_tool_call_input.done" => {
                seen_done = true;
                assert_eq!(deltas, want);
                assert_eq!(text(event, "input"), want);
                assert_eq!(text(event, "call_id"), "c1");
            }
            "response.output_item.done" => {
                seen_item = true;
                assert!(seen_done, "item done before input done");
                assert_eq!(text(event, "item.input"), want);
            }
            "response.completed" => {
                seen_completed = true;
                assert!(seen_item, "completed before item done");
                assert_eq!(text(event, "response.output.0.input"), want);
            }
            _ => {}
        }
    }
    assert!(seen_done && seen_item && seen_completed, "{events:?}");
}

#[test]
fn apply_patch_waits_for_identity_and_keeps_interleaved_calls_apart() {
    let mut stream = patch_stream(PATCH_REQUEST);
    data(&mut stream, &patch_start(0, "", ""));
    for event in data(&mut stream, &patch_fragment(0, r#"{"input":"first\n"#)) {
        let kind = kind(&event);
        assert!(
            !kind.contains("arguments.delta") && kind != "response.custom_tool_call_input.delta",
            "emitted before identity: {event}"
        );
    }
    let mut chunks = vec![
        patch_start(0, "c1", "apply_patch"),
        patch_start(1, "c2", "apply_patch"),
        patch_fragment(1, r#"{"input":"second"#),
        patch_fragment(0, r#"tail"}"#),
        patch_fragment(1, r#" tail"}"#),
    ];
    chunks.extend(patch_end());
    let events = data_all(&mut stream, &chunks);
    let mut inputs = HashMap::<String, String>::new();
    let mut indices = HashMap::<String, i64>::new();
    let mut done = HashMap::<String, String>::new();
    for event in &events {
        match &*kind(event) {
            "response.custom_tool_call_input.delta" => {
                let id = text(event, "call_id");
                inputs
                    .entry(id.clone())
                    .or_default()
                    .push_str(&text(event, "delta"));
                indices.insert(id, int(event, "output_index"));
            }
            "response.custom_tool_call_input.done" => {
                done.insert(text(event, "call_id"), text(event, "input"));
            }
            "response.failed" => panic!("interleaved calls failed: {event}"),
            _ => {}
        }
    }
    assert_eq!(inputs["c1"], "first\ntail");
    assert_eq!(inputs["c2"], "second tail");
    assert_eq!(done["c1"], inputs["c1"]);
    assert_eq!(done["c2"], inputs["c2"]);
    assert_ne!(indices["c1"], indices["c2"]);
}

#[test]
fn invalid_apply_patch_arguments_fail_the_response_once() {
    for arguments in [
        r#"plain patch"#,
        r#"{}"#,
        r#"{"input":42}"#,
        r#"{"input":"x","extra":1}"#,
        r#"{"input":"x","input":"y"}"#,
        r#"{"input":"x"} {}"#,
        r#"{"input":"unfinished"#,
        r#"{"input":"bad\q"}"#,
        r#"{"input":"\ud800"}"#,
    ] {
        let mut stream = patch_stream(PATCH_REQUEST);
        let mut events = data(&mut stream, &patch_start(0, "c1", "apply_patch"));
        events.extend(data(&mut stream, &patch_fragment(0, arguments)));
        events.extend(data_all(&mut stream, &patch_end()));
        events.extend(data_all(&mut stream, &patch_end()));
        let mut failures = 0;
        for event in &events {
            match &*kind(event) {
                "response.failed" => {
                    failures += 1;
                    assert_eq!(
                        text(event, "response.error.code"),
                        "invalid_tool_arguments",
                        "{arguments}: {event}"
                    );
                }
                "response.completed"
                | "response.incomplete"
                | "response.custom_tool_call_input.done"
                | "response.output_item.done" => {
                    panic!("{arguments}: invalid arguments succeeded: {event}")
                }
                _ => {}
            }
        }
        assert_eq!(failures, 1, "{arguments}");
        assert!(stream.tool_input_error().is_some(), "{arguments}");
    }
}

#[test]
fn apply_patch_call_takes_the_winning_declaration() {
    for (name, request, upstream, want_type, want_name, namespace) in [
        (
            "function",
            r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
            "apply_patch",
            "function_call",
            "apply_patch",
            "",
        ),
        (
            "function wins",
            r#"{"tools":[{"type":"function","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
            "function_call",
            "apply_patch",
            "",
        ),
        (
            "custom wins",
            r#"{"tools":[{"type":"custom","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"function","name":"apply_patch"}]}]}"#,
            "apply_patch",
            "custom_tool_call",
            "apply_patch",
            "",
        ),
        (
            "namespace",
            r#"{"tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "editor__apply_patch",
            "custom_tool_call",
            "apply_patch",
            "editor",
        ),
        (
            "flat collision",
            r#"{"tools":[{"type":"function","name":"editor__apply_patch"},{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "editor__apply_patch",
            "function_call",
            "editor__apply_patch",
            "",
        ),
        (
            "same source custom first",
            r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"apply_patch"}]}"#,
            "apply_patch",
            "custom_tool_call",
            "apply_patch",
            "",
        ),
        (
            "same source function first",
            r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}]}"#,
            "apply_patch",
            "function_call",
            "apply_patch",
            "",
        ),
        (
            "namespace before flat",
            r#"{"tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"editor__apply_patch"}]}"#,
            "editor__apply_patch",
            "function_call",
            "editor__apply_patch",
            "",
        ),
        (
            "sanitized namespace",
            r#"{"tools":[{"type":"namespace","name":"mcp.editor","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "mcp_editor__apply_patch",
            "custom_tool_call",
            "apply_patch",
            "mcp.editor",
        ),
        (
            "other custom",
            r#"{"tools":[{"type":"custom","name":"edit"}]}"#,
            "edit",
            "custom_tool_call",
            "edit",
            "",
        ),
    ] {
        let mut stream = patch_stream(request);
        data(&mut stream, &patch_start(0, "c1", upstream));
        let args = if want_type == "function_call" || name == "other custom" {
            r#"{"not_input":42}"#
        } else {
            r#"{"input":"x"}"#
        };
        let mut events = data(&mut stream, &patch_fragment(0, args));
        events.extend(data_all(&mut stream, &patch_end()));
        let response = completed_response(&events);
        let item = &response["output"][0];
        assert_eq!(text(item, "type"), want_type, "{name}: {item}");
        assert_eq!(text(item, "name"), want_name, "{name}: {item}");
        assert_eq!(text(item, "namespace"), namespace, "{name}: {item}");
        if want_type == "function_call" {
            assert_eq!(text(item, "arguments"), args, "{name}");
        }
        if name == "other custom" {
            assert_eq!(text(item, "input"), args, "{name}");
        }
    }
}

#[test]
fn complete_response_apply_patch_input_is_strict() {
    for (arguments, want, invalid) in [
        (
            r#"{"input":"*** Begin Patch\n*** End Patch"}"#,
            "*** Begin Patch\n*** End Patch",
            false,
        ),
        (r#"{"input":12}"#, "", true),
        (r#"{"input":"truncated"#, "", true),
        (r#"{"input":"x","extra":true}"#, "", true),
        (r#"{"input":"\ud800"}"#, "", true),
    ] {
        let (out, error) = patch_complete(
            PATCH_REQUEST,
            &Value::Null,
            &[
                patch_start(0, "c1", "apply_patch"),
                patch_fragment(0, arguments),
                MESSAGE_STOP.to_owned(),
            ],
        );
        if invalid {
            assert_eq!(text(&out, "status"), "failed", "{arguments}: {out}");
            assert_eq!(text(&out, "error.code"), "invalid_tool_arguments");
            assert!(error.is_some(), "{arguments}");
        } else {
            assert_eq!(text(&out, "output.0.input"), want, "{arguments}: {out}");
        }
    }
}

#[test]
fn truncated_apply_patch_call_fails_at_message_stop() {
    let mut stream = patch_stream(PATCH_REQUEST);
    data(&mut stream, &patch_start(0, "c1", "apply_patch"));
    data(&mut stream, &patch_fragment(0, r#"{"input":"unfinished"#));
    let events = data(&mut stream, MESSAGE_STOP);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(kind(&events[0]), "response.failed");
}

#[test]
fn apply_patch_input_survives_any_fragment_split() {
    let arguments = r#"{"input":"line\n\u4f60\u597d \ud83d\ude00 \" \\ \u96ea"}"#;
    for split in 1..arguments.len() {
        let mut stream = patch_stream(PATCH_REQUEST);
        data(&mut stream, &patch_start(0, "c1", "apply_patch"));
        let mut events = data(&mut stream, &patch_fragment(0, &arguments[..split]));
        events.extend(data(&mut stream, &patch_fragment(0, &arguments[split..])));
        events.extend(data_all(&mut stream, &patch_end()));
        let input: String = events
            .iter()
            .filter(|event| kind(event) == "response.custom_tool_call_input.delta")
            .map(|event| text(event, "delta"))
            .collect();
        assert_eq!(input, "line\n你好 😀 \" \\ 雪", "split {split}");
    }
}

#[test]
fn apply_patch_preview_waits_for_both_identity_fields() {
    for (id, name) in [("c1", ""), ("", "apply_patch")] {
        let mut stream = patch_stream(PATCH_REQUEST);
        data(&mut stream, &patch_start(0, id, name));
        let events = data(&mut stream, &patch_fragment(0, r#"{"input":"preview"#));
        assert!(events.is_empty(), "preview without identity: {events:?}");
        let events = data(&mut stream, &patch_start(0, "c1", "apply_patch"));
        let found = events
            .iter()
            .rfind(|event| kind(event) == "response.custom_tool_call_input.delta")
            .is_some_and(|event| {
                text(event, "delta") == "preview" && text(event, "call_id") == "c1"
            });
        assert!(found, "buffer not released after identity: {events:?}");
    }
}

#[test]
fn apply_patch_identity_change_fails_once() {
    for (id, name) in [("other", "apply_patch"), ("c1", "other")] {
        let mut stream = patch_stream(PATCH_REQUEST);
        data(&mut stream, &patch_start(0, "c1", "apply_patch"));
        data(&mut stream, &patch_fragment(0, r#"{"input":"preview"#));
        let mut events = data(&mut stream, &patch_start(0, id, name));
        events.extend(data_all(&mut stream, &patch_end()));
        assert_eq!(events.len(), 1, "{id} {name}: {events:?}");
        assert_eq!(kind(&events[0]), "response.failed");
    }
}

#[test]
fn complete_response_takes_the_client_declaration_over_the_translated_one() {
    let (out, _) = patch_complete(
        r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
        &parse(PATCH_REQUEST),
        &[
            patch_start(0, "c1", "apply_patch"),
            patch_fragment(0, r#"{"not_input":42}"#),
            MESSAGE_STOP.to_owned(),
        ],
    );
    assert_eq!(text(&out, "output.0.type"), "function_call", "{out}");
    assert_eq!(text(&out, "output.0.arguments"), r#"{"not_input":42}"#);
}

#[test]
fn apply_patch_snapshots_are_validated_and_never_previewed() {
    for (name, fragment, snapshot, want, invalid) in [
        ("snapshot only", "", r#"{"input":"whole"}"#, "whole", false),
        (
            "matching suffix",
            r#"{"input":"pre"#,
            r#"{"input":"prefix"}"#,
            "prefix",
            false,
        ),
        (
            "conflict",
            r#"{"input":"wrong"#,
            r#"{"input":"prefix"}"#,
            "",
            true,
        ),
        (
            "completed conflict",
            r#"{"input":"pre"}"#,
            r#"{"input":"prefix"}"#,
            "",
            true,
        ),
        (
            "invalid snapshot",
            r#"{"input":"pre"#,
            r#"{"input":7}"#,
            "",
            true,
        ),
    ] {
        let mut stream = patch_stream(PATCH_REQUEST);
        let start = patch_start(0, "c1", "apply_patch");
        let mut events = data(&mut stream, &start);
        let mut raw = vec![start];
        if !fragment.is_empty() {
            let chunk = patch_fragment(0, fragment);
            events.extend(data(&mut stream, &chunk));
            raw.push(chunk);
        }
        let snapshot = patch_snapshot("c1", "apply_patch", snapshot);
        let snapshot_events = data(&mut stream, &snapshot);
        for event in &snapshot_events {
            assert_ne!(
                kind(event),
                "response.custom_tool_call_input.delta",
                "{name}: snapshot fabricated a preview: {event}"
            );
        }
        events.extend(snapshot_events);
        raw.push(snapshot);
        for chunk in patch_end() {
            events.extend(data(&mut stream, &chunk));
            raw.push(chunk);
        }
        // Upstream ends every line with a newline.
        raw.push(String::new());
        let mut terminal = Value::Null;
        let mut input = String::new();
        for event in &events {
            match &*kind(event) {
                "response.custom_tool_call_input.delta" => input.push_str(&text(event, "delta")),
                "response.failed" | "response.completed" => terminal = event.clone(),
                _ => {}
            }
        }
        let (out, _) = patch_complete(PATCH_REQUEST, &Value::Null, &raw);
        if invalid {
            assert_eq!(kind(&terminal), "response.failed", "{name}: {terminal}");
            assert_eq!(text(&out, "status"), "failed", "{name}: {out}");
        } else {
            assert_eq!(input, want, "{name}");
            assert_eq!(
                text(&terminal, "response.output.0.input"),
                want,
                "{name}: {terminal}"
            );
            assert_eq!(text(&out, "output.0.input"), want, "{name}: {out}");
        }
    }
}

#[test]
fn complete_response_apply_patch_identity_change_fails() {
    let (out, error) = patch_complete(
        PATCH_REQUEST,
        &Value::Null,
        &[
            patch_start(0, "c1", "apply_patch"),
            patch_fragment(0, r#"{"input":"x"}"#),
            patch_start(0, "c2", "apply_patch"),
            MESSAGE_STOP.to_owned(),
        ],
    );
    assert_eq!(text(&out, "status"), "failed", "{out}");
    assert!(error.is_some());
}

#[test]
fn replacing_an_apply_patch_snapshot_fails() {
    let mut stream = patch_stream(PATCH_REQUEST);
    let mut raw = Vec::new();
    let mut events = Vec::new();
    for input in [r#"{"input":"first"}"#, r#"{"input":"replacement"}"#] {
        let chunk = patch_snapshot("c1", "apply_patch", input);
        events.extend(data(&mut stream, &chunk));
        raw.push(chunk);
    }
    for chunk in patch_end() {
        events.extend(data(&mut stream, &chunk));
        raw.push(chunk);
    }
    raw.push(String::new());
    let mut failures = 0;
    for event in &events {
        match &*kind(event) {
            "response.failed" => failures += 1,
            "response.completed" | "response.custom_tool_call_input.done" => {
                panic!("silently replaced snapshot: {event}")
            }
            _ => {}
        }
    }
    let (out, _) = patch_complete(PATCH_REQUEST, &Value::Null, &raw);
    assert_eq!(failures, 1);
    assert_eq!(text(&out, "status"), "failed", "{out}");
}

#[test]
fn a_pending_apply_patch_id_cannot_be_replaced() {
    let mut stream = patch_stream(PATCH_REQUEST);
    data(&mut stream, &patch_start(0, "c1", ""));
    data(&mut stream, &patch_fragment(0, r#"{"input":"x"}"#));
    let events = data(&mut stream, &patch_start(0, "c2", "apply_patch"));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(kind(&events[0]), "response.failed");
}

#[test]
fn apply_patch_call_without_an_id_gets_one_at_the_end() {
    let mut stream = patch_stream(PATCH_REQUEST);
    data(&mut stream, &patch_start(0, "", "apply_patch"));
    data(&mut stream, &patch_fragment(0, r#"{"input":"x"}"#));
    let events = data_all(&mut stream, &patch_end());
    let response = completed_response(&events);
    let item = &response["output"][0];
    assert_eq!(text(item, "type"), "custom_tool_call", "{response}");
    assert_eq!(text(item, "input"), "x");
    assert_ne!(text(item, "call_id"), "");
}

#[test]
fn complete_response_apply_patch_identity_can_arrive_late() {
    let (out, _) = patch_complete(
        PATCH_REQUEST,
        &Value::Null,
        &[
            patch_start(0, "", "apply_patch"),
            patch_fragment(0, r#"{"input":"x"}"#),
            patch_start(0, "c1", ""),
            MESSAGE_STOP.to_owned(),
        ],
    );
    let item = &out["output"][0];
    assert_eq!(text(item, "type"), "custom_tool_call", "{out}");
    assert_eq!(text(item, "input"), "x");
    assert_eq!(text(item, "id"), "ctc_c1");
}

/// `assertApplyPatchClaudeDeferredFailure`.
fn assert_deferred_failure(events: &[Value], stream: &ClaudeToOpenAIResponsesStream, case: &str) {
    let mut failures = 0;
    for event in events {
        match &*kind(event) {
            "response.failed" => {
                failures += 1;
                assert_eq!(
                    text(event, "response.error.code"),
                    "invalid_tool_arguments",
                    "{case}: {event}"
                );
            }
            "response.completed"
            | "response.incomplete"
            | "response.custom_tool_call_input.done"
            | "response.output_item.done" => {
                panic!("{case}: invalid pending evidence succeeded: {event}")
            }
            _ => {}
        }
    }
    assert_eq!(failures, 1, "{case}");
    assert!(stream.tool_input_error().is_some(), "{case}");
}

/// Asserts that a complete response failed on invalid tool arguments.
fn assert_complete_failure(out: &Value, error: Option<&ToolInputError>, case: &str) {
    assert_eq!(text(out, "status"), "failed", "{case}: {out}");
    assert_eq!(
        text(out, "error.code"),
        "invalid_tool_arguments",
        "{case}: {out}"
    );
    assert!(error.is_some(), "{case}");
}

#[test]
fn apply_patch_identity_changes_before_the_name_arrives_are_kept() {
    for (name, request, resolved_id, resolved_name, want_type, invalid) in [
        (
            "keep second ID",
            PATCH_REQUEST,
            "c2",
            "apply_patch",
            "",
            true,
        ),
        (
            "return to first ID",
            PATCH_REQUEST,
            "c1",
            "apply_patch",
            "",
            true,
        ),
        ("omit final ID", PATCH_REQUEST, "", "apply_patch", "", true),
        (
            "ordinary function",
            r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
            "c2",
            "apply_patch",
            "function_call",
            false,
        ),
        (
            "function winner",
            r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}]}"#,
            "c2",
            "apply_patch",
            "function_call",
            false,
        ),
        (
            "other custom",
            r#"{"tools":[{"type":"custom","name":"edit"}]}"#,
            "c2",
            "edit",
            "custom_tool_call",
            false,
        ),
    ] {
        let mut chunks = vec![
            patch_start(0, "c1", ""),
            patch_start(0, "c2", ""),
            patch_start(0, resolved_id, resolved_name),
            patch_fragment(0, r#"{"input":"x"}"#),
        ];
        chunks.extend(patch_end());
        for streaming in [true, false] {
            let case = format!("{name}/{}", if streaming { "stream" } else { "nonstream" });
            let (result, failed) = if streaming {
                let mut stream = patch_stream(request);
                let mut events = Vec::new();
                for (i, chunk) in chunks.iter().enumerate() {
                    let current = data(&mut stream, chunk);
                    assert!(
                        i >= 2 || current.is_empty(),
                        "{case}: unclassified identity emitted events: {current:?}"
                    );
                    events.extend(current);
                }
                if invalid {
                    assert_deferred_failure(&events, &stream, &case);
                    continue;
                }
                (
                    completed_response(&events),
                    stream.tool_input_error().is_some(),
                )
            } else {
                let (result, error) = patch_complete(request, &Value::Null, &chunks);
                if invalid {
                    assert_complete_failure(&result, error.as_ref(), &case);
                    continue;
                }
                (result, error.is_some())
            };
            let item = &result["output"][0];
            assert_eq!(text(&result, "status"), "completed", "{case}: {result}");
            assert_eq!(text(item, "type"), want_type, "{case}: {result}");
            assert_eq!(text(item, "call_id"), "c2", "{case}: {result}");
            assert!(!failed, "{case}");
            if want_type == "function_call" {
                assert_eq!(text(item, "arguments"), r#"{"input":"x"}"#, "{case}");
            }
            if want_type == "custom_tool_call" {
                assert_eq!(text(item, "input"), "x", "{case}");
            }
        }
    }
}

#[derive(Default)]
struct DeferredSnapshotCase {
    name: &'static str,
    first: &'static str,
    second: &'static str,
    fragment: &'static str,
    tool_name: &'static str,
    request: &'static str,
    want: &'static str,
    invalid: bool,
}

#[test]
fn apply_patch_snapshots_before_the_name_arrives_are_checked_later() {
    for case in [
        DeferredSnapshotCase {
            name: "different complete snapshots",
            first: r#"{"input":"first"}"#,
            second: r#"{"input":"replacement"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "complete snapshot cannot extend",
            first: r#"{"input":"pre"}"#,
            second: r#"{"input":"prefix"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "early wrong type",
            first: r#"{"input":42}"#,
            second: r#"{"input":"replacement"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "early extra field",
            first: r#"{"input":"first","extra":1}"#,
            second: r#"{"input":"replacement"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "early duplicate key",
            first: r#"{"input":"first","input":"replacement"}"#,
            second: r#"{"input":"replacement"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "early invalid then placeholder",
            first: r#"{"input":null}"#,
            second: r#"{}"#,
            fragment: r#"{"input":"replacement"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "early invalid then equal decoded input",
            first: r#"{"input":"first","extra":1}"#,
            second: r#"{"input":"first"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "equivalent complete snapshots",
            first: r#"{"input":"\u0078\n"}"#,
            second: r#" { "input" : "x\n" } "#,
            want: "x\n",
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "single snapshot and placeholders",
            first: r#"{"input":"whole"}"#,
            second: r#"{}"#,
            want: "whole",
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "partial source extension",
            first: r#"{}"#,
            second: r#"{"input":"prefix"}"#,
            fragment: r#"{"input":"pre"#,
            want: "prefix",
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "complete source conflict",
            first: r#"{}"#,
            second: r#"{"input":"prefix"}"#,
            fragment: r#"{"input":"pre"}"#,
            invalid: true,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "ordinary placeholders",
            first: r#"{}"#,
            second: r#"{}"#,
            fragment: r#"{"input":"x"}"#,
            want: "x",
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "ordinary function ignores patch evidence",
            first: r#"{"input":42}"#,
            second: r#"{"input":"replacement"}"#,
            fragment: r#"{"other":42}"#,
            request: r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
            want: r#"{"other":42}"#,
            ..Default::default()
        },
        DeferredSnapshotCase {
            name: "other custom ignores patch evidence",
            first: r#"{"input":42}"#,
            second: r#"{"input":"replacement"}"#,
            fragment: r#"{"other":42}"#,
            tool_name: "edit",
            request: r#"{"tools":[{"type":"custom","name":"edit"}]}"#,
            want: r#"{"other":42}"#,
            ..Default::default()
        },
    ] {
        let request = if case.request.is_empty() {
            PATCH_REQUEST
        } else {
            case.request
        };
        let tool_name = if case.tool_name.is_empty() {
            "apply_patch"
        } else {
            case.tool_name
        };
        let mut chunks = vec![patch_snapshot("c1", "", case.first)];
        if !case.fragment.is_empty() {
            chunks.push(patch_fragment(0, case.fragment));
        }
        chunks.push(patch_snapshot("c1", "", case.second));
        chunks.push(patch_start(0, "c1", tool_name));
        chunks.extend(patch_end());
        for streaming in [true, false] {
            let label = format!(
                "{}/{}",
                case.name,
                if streaming { "stream" } else { "nonstream" }
            );
            let (result, failed) = if streaming {
                let mut stream = patch_stream(request);
                let mut events = Vec::new();
                for (i, chunk) in chunks.iter().enumerate() {
                    let current = data(&mut stream, chunk);
                    assert!(
                        i + 3 >= chunks.len() || current.is_empty(),
                        "{label}: unclassified snapshots fabricated output: {current:?}"
                    );
                    events.extend(current);
                }
                if case.invalid {
                    assert_deferred_failure(&events, &stream, &label);
                    continue;
                }
                let deltas: String = events
                    .iter()
                    .filter(|event| kind(event) == "response.custom_tool_call_input.delta")
                    .map(|event| text(event, "delta"))
                    .collect();
                if case.request.is_empty() {
                    assert_eq!(deltas, case.want, "{label}");
                }
                (
                    completed_response(&events),
                    stream.tool_input_error().is_some(),
                )
            } else {
                let (result, error) = patch_complete(request, &Value::Null, &chunks);
                if case.invalid {
                    assert_complete_failure(&result, error.as_ref(), &label);
                    continue;
                }
                (result, error.is_some())
            };
            let field = if !case.request.is_empty() && tool_name == "apply_patch" {
                "output.0.arguments"
            } else {
                "output.0.input"
            };
            assert_eq!(text(&result, "status"), "completed", "{label}: {result}");
            assert_eq!(text(&result, field), case.want, "{label}: {result}");
            assert!(!failed, "{label}");
        }
    }
}

#[test]
fn apply_patch_snapshot_after_the_item_is_done_is_checked_without_new_events() {
    for (name, snapshot, invalid) in [
        ("conflicting complete snapshot", r#"{"input":"y"}"#, true),
        (
            "complete snapshot cannot extend finished input",
            r#"{"input":"xy"}"#,
            true,
        ),
        (
            "equivalent reencoded snapshot",
            r#" { "input" : "\u0078" } "#,
            false,
        ),
        ("placeholder after item completion", r#"{}"#, false),
    ] {
        // The first call is done before its snapshot arrives, while a second
        // call is still open.
        let mut chunks = vec![
            patch_start(0, "c1", "apply_patch"),
            patch_fragment(0, r#"{"input":"x"}"#),
            r#"data: {"type":"content_block_stop","index":0}"#.to_owned(),
            patch_start(1, "c2", "apply_patch"),
            patch_fragment(1, r#"{"input":"ok"}"#),
            patch_snapshot("c1", "apply_patch", snapshot),
        ];
        chunks.extend(patch_end());
        for streaming in [true, false] {
            let label = format!("{name}/{}", if streaming { "stream" } else { "nonstream" });
            let (result, failed) = if streaming {
                let mut stream = patch_stream(PATCH_REQUEST);
                let events = data_all(&mut stream, &chunks[..5]);
                let (mut input_done, mut item_done) = (0, 0);
                let mut preview = String::new();
                for event in &events {
                    match &*kind(event) {
                        "response.custom_tool_call_input.delta" => {
                            if text(event, "call_id") == "c1" {
                                preview.push_str(&text(event, "delta"));
                            }
                        }
                        "response.custom_tool_call_input.done" => {
                            input_done += 1;
                            assert_eq!(text(event, "call_id"), "c1", "{label}: {event}");
                            assert_eq!(text(event, "input"), "x", "{label}: {event}");
                        }
                        "response.output_item.done" => {
                            item_done += 1;
                            assert_eq!(text(event, "item.call_id"), "c1", "{label}: {event}");
                            assert_eq!(text(event, "item.input"), "x", "{label}: {event}");
                        }
                        "response.completed" | "response.incomplete" | "response.failed" => {
                            panic!("{label}: response ended before the late snapshot: {event}")
                        }
                        _ => {}
                    }
                }
                let first = &stream.funcs[&0];
                assert_eq!((input_done, item_done), (1, 1), "{label}");
                assert_eq!(preview, "x", "{label}");
                assert!(first.item_done, "{label}");
                assert!(first.input_snapshot.is_empty(), "{label}");

                let snapshot_events = data(&mut stream, &chunks[5]);
                assert!(
                    invalid || snapshot_events.is_empty(),
                    "{label}: late snapshot fabricated events: {snapshot_events:?}"
                );
                let mut after = snapshot_events.clone();
                after.extend(data_all(&mut stream, &chunks[6..]));
                if invalid {
                    assert_deferred_failure(&after, &stream, &label);
                    assert_eq!(snapshot_events.len(), 1, "{label}: {snapshot_events:?}");
                    assert_eq!(kind(&snapshot_events[0]), "response.failed", "{label}");
                    continue;
                }
                let mut completions = 0;
                for event in &after {
                    match &*kind(event) {
                        "response.custom_tool_call_input.delta"
                        | "response.custom_tool_call_input.done" => {
                            assert_eq!(text(event, "call_id"), "c2", "{label}: {event}");
                        }
                        "response.output_item.done" => {
                            assert_eq!(text(event, "item.call_id"), "c2", "{label}: {event}");
                        }
                        "response.completed" => completions += 1,
                        _ => {}
                    }
                }
                assert_eq!(completions, 1, "{label}");
                (
                    completed_response(&after),
                    stream.tool_input_error().is_some(),
                )
            } else {
                let (result, error) = patch_complete(PATCH_REQUEST, &Value::Null, &chunks);
                if invalid {
                    assert_complete_failure(&result, error.as_ref(), &label);
                    continue;
                }
                (result, error.is_some())
            };
            assert_eq!(text(&result, "status"), "completed", "{label}: {result}");
            assert_eq!(text(&result, "output.0.call_id"), "c1", "{label}: {result}");
            assert_eq!(text(&result, "output.0.input"), "x", "{label}: {result}");
            assert_eq!(text(&result, "output.1.call_id"), "c2", "{label}: {result}");
            assert_eq!(text(&result, "output.1.input"), "ok", "{label}: {result}");
            assert!(!failed, "{label}");
        }
    }
}

#[test]
fn successful_terminal_ignores_anything_after_it() {
    for (name, late) in [
        ("duplicate message_stop", MESSAGE_STOP.to_owned()),
        (
            "post-terminal fragment",
            patch_fragment(0, r#"{"input":"late"}"#),
        ),
        (
            "post-terminal conflicting snapshot",
            patch_snapshot("c1", "apply_patch", r#"{"input":"y"}"#),
        ),
    ] {
        let mut chunks = vec![
            patch_start(0, "c1", "apply_patch"),
            patch_fragment(0, r#"{"input":"x"}"#),
        ];
        chunks.extend(patch_end());

        let mut stream = patch_stream(PATCH_REQUEST);
        let mut completions = 0;
        for event in data_all(&mut stream, &chunks) {
            if kind(&event) == "response.completed" {
                completions += 1;
                assert_eq!(
                    text(&event, "response.output.0.input"),
                    "x",
                    "{name}: {event}"
                );
            }
        }
        assert_eq!(completions, 1, "{name}");
        for chunk in [late.as_str(), MESSAGE_STOP] {
            let events = data(&mut stream, chunk);
            assert!(
                events.is_empty(),
                "{name}: sealed response emitted more events: {events:?}"
            );
        }
        assert!(stream.tool_input_error().is_none(), "{name}");

        chunks.push(late);
        let (out, error) = patch_complete(PATCH_REQUEST, &Value::Null, &chunks);
        assert_eq!(text(&out, "status"), "completed", "{name}: {out}");
        assert_eq!(text(&out, "output.0.input"), "x", "{name}: {out}");
        assert!(error.is_none(), "{name}");
    }
}

#[test]
fn complete_response_restores_a_shortened_custom_tool_name() {
    let tool1 = "custom__long_namespace_path_exceeding_sixty_four_characters_long__operation_query_v1_execute_handler";
    let tool2 = "custom__long_namespace_path_exceeding_sixty_four_characters_long__operation_query_v1_execute_stream";
    let request = json!({"tools": [
        {"type": "custom", "name": tool1},
        {"type": "custom", "name": tool2}
    ]});
    let start = format!(
        r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"call_custom_0","name":{},"input":{{"input":"query_payload"}}}}}}"#,
        quote(&RequestTools::new(&request).claude_name(tool1))
    );
    let out = complete(
        &request,
        &[
            r#"data: {"type":"message_start","message":{"id":"msg_test_custom","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            &start,
            r#"data: {"type":"content_block_stop","index":0}"#,
            MESSAGE_STOP,
        ],
    );
    assert_eq!(text(&out, "output.0.type"), "custom_tool_call", "{out}");
    assert_eq!(text(&out, "output.0.name"), tool1);
}

/// A block 0 `apply_patch` start whose input has an `extra` field nested in
/// `depth` arrays: valid JSON, too deep for serde_json.
fn deep_patch_start(depth: usize) -> String {
    let extra = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
    patch_snapshot(
        "c1",
        "apply_patch",
        &format!(r#"{{"input":"patch","extra":{extra}}}"#),
    )
}

/// Valid JSON lines serde_json can't read: too deep, with an unpaired
/// surrogate escape, and with a byte that isn't UTF-8.
fn unreadable_lines() -> Vec<Vec<u8>> {
    let surrogate = format!("{}ud800", char::from(b'\\'));
    let mut not_utf8 = br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"x"#.to_vec();
    not_utf8.extend_from_slice(b"\xff\"}}");
    vec![
        deep_patch_start(200).into_bytes(),
        patch_fragment(0, "x").replace('x', &surrogate).into_bytes(),
        not_utf8,
    ]
}

#[test]
fn unreadable_event_fails_an_apply_patch_stream() {
    for line in unreadable_lines() {
        let shown = String::from_utf8_lossy(&line).into_owned();
        assert!(parse_data_line(&line).is_none(), "{shown}");
        let mut stream = patch_stream(PATCH_REQUEST);
        let mut events = data(&mut stream, MESSAGE_START);
        events.extend(data(&mut stream, &patch_start(0, "c1", "apply_patch")));
        events.extend(
            sse_events(&stream.translate_line(&line))
                .into_iter()
                .map(|(_, data)| data),
        );
        events.extend(data(
            &mut stream,
            &patch_fragment(0, r#"{"input":"patch"}"#),
        ));
        events.extend(data_all(&mut stream, &patch_end()));
        let ends: Vec<String> = events
            .iter()
            .map(kind)
            .filter(|kind| {
                [
                    "response.failed",
                    "response.completed",
                    "response.incomplete",
                ]
                .contains(&kind.as_str())
            })
            .collect();
        assert_eq!(ends, ["response.failed"], "{shown}");
        let failed = events
            .iter()
            .find(|e| kind(e) == "response.failed")
            .unwrap();
        assert_eq!(text(failed, "response.id"), "msg_123", "{shown}");
        assert_eq!(
            text(failed, "response.error.code"),
            "invalid_tool_arguments"
        );
        assert_eq!(
            stream
                .tool_input_error()
                .map(ToString::to_string)
                .as_deref(),
            Some("unreadable upstream event in apply_patch stream"),
            "{shown}"
        );
        // Nothing follows the failure, not even at the end of the stream.
        assert_eq!(stream.finalize_tool_input(), "");
    }
}

#[test]
fn complete_response_unreadable_event_fails_an_apply_patch_call() {
    for line in unreadable_lines() {
        let shown = String::from_utf8_lossy(&line).into_owned();
        let mut body = [MESSAGE_START, &patch_start(0, "c1", "apply_patch")]
            .join("\n")
            .into_bytes();
        body.push(b'\n');
        body.extend_from_slice(&line);
        body.extend_from_slice(b"\n");
        body.extend_from_slice(patch_fragment(0, r#"{"input":"patch"}"#).as_bytes());
        body.extend_from_slice(b"\n");
        body.extend_from_slice(MESSAGE_STOP.as_bytes());
        let (out, error) = non_stream(&parse(PATCH_REQUEST), &Value::Null, &body);
        assert_eq!(text(&out, "status"), "failed", "{shown}: {out}");
        assert_eq!(text(&out, "id"), "msg_123");
        assert_eq!(text(&out, "error.code"), "invalid_tool_arguments");
        assert!(matches!(error, Some(ToolInputError::Unreadable)), "{shown}");
    }
}

#[test]
fn unreadable_event_is_skipped_without_apply_patch() {
    let request = r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#;
    for line in unreadable_lines() {
        let shown = String::from_utf8_lossy(&line).into_owned();
        let mut stream = patch_stream(request);
        data(&mut stream, MESSAGE_START);
        assert_eq!(stream.translate_line(&line), "", "{shown}");
        let events = data(&mut stream, MESSAGE_STOP);
        assert_eq!(
            kind(events.last().unwrap()),
            "response.completed",
            "{shown}"
        );
        assert!(stream.tool_input_error().is_none());

        let mut body = MESSAGE_START.as_bytes().to_vec();
        body.push(b'\n');
        body.extend_from_slice(&line);
        let (out, error) = non_stream(&parse(request), &Value::Null, &body);
        assert_eq!(text(&out, "status"), "completed", "{shown}: {out}");
        assert!(error.is_none());
    }
}

#[test]
fn invalid_json_is_skipped_in_an_apply_patch_stream() {
    let mut stream = patch_stream(PATCH_REQUEST);
    data(&mut stream, MESSAGE_START);
    for line in [
        "data: ping",
        "data:",
        r#"data: {"type":"content_block_delta""#,
        "event: content_block_delta",
        ": comment",
    ] {
        assert_eq!(stream.translate_line(line.as_bytes()), "", "{line}");
    }
    assert!(stream.tool_input_error().is_none());
    let events = data(&mut stream, MESSAGE_STOP);
    assert_eq!(kind(events.last().unwrap()), "response.completed");
}

#[test]
fn unreadable_snapshot_fails_before_the_name_arrives() {
    // serde_json rejects the lone surrogate escape. Upstream reads the
    // snapshot and fails the call once its name arrives. The request declares
    // `apply_patch`, so here the line fails the response at once.
    let first = patch_snapshot("c1", "", r#"{"input":"\ud800"}"#);
    let mut stream = patch_stream(PATCH_REQUEST);
    let events = data(&mut stream, &first);
    assert_eq!(
        events.iter().map(kind).collect::<Vec<_>>(),
        ["response.failed"]
    );
    let mut rest = vec![
        patch_snapshot("c1", "", r#"{"input":"replacement"}"#),
        patch_start(0, "c1", "apply_patch"),
    ];
    rest.extend(patch_end());
    assert!(data_all(&mut stream, &rest).is_empty());
    assert!(stream.tool_input_error().is_some());

    let mut lines = vec![first];
    lines.extend(rest);
    let (out, error) = patch_complete(PATCH_REQUEST, &Value::Null, &lines);
    assert_complete_failure(&out, error.as_ref(), "unreadable snapshot");
}

// Not upstream's: Go writes a float64 negative zero as `-0`, both where it
// reads a field as a float (`top_p`) and where it reads one as a value
// (`tools`).
#[test]
fn completed_echo_keeps_negative_zero() {
    let original_request = crate::json::exact::from_str(
        r#"{"top_p":-0,"tools":[{"type":"function","name":"f","parameters":{"minimum":-0.0}}]}"#,
    )
    .unwrap();
    let mut stream =
        ClaudeToOpenAIResponsesStream::new("claude-test", &original_request, &Value::Null);
    let out: String = [MESSAGE_START, r#"data: {"type":"message_stop"}"#]
        .iter()
        .map(|line| stream.translate_line(line.as_bytes()))
        .collect();
    let completed = out
        .split("\n\n")
        .find(|frame| frame.starts_with("event: response.completed"))
        .expect("the stream completes");
    for want in [r#""top_p":-0,"#, r#""parameters":{"minimum":-0}"#] {
        assert!(completed.contains(want), "{want} in {completed}");
    }
}
