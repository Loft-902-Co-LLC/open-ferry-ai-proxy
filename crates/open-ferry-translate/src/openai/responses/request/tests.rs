// Ported from CLIProxyAPI internal/translator/openai/openai/responses/openai_openai-responses_request_test.go
// (v8.0.15, MIT), and openai_openai-responses_user_turn_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

// All 64 tests are ported, with the request-side tests of
// custom_tool_namespace_recovery_test.go, the two in
// openai_openai-responses_video_test.go, and the one in
// responses_compatibility_digest_test.go, which hashes response streams too.
// Table-driven tests run their cases in a loop rather than as subtests.
// `responsesCustomToolNames`, `responsesSingleCustomToolName` and
// `splitResponsesQualifiedFunctionCallFromRequest` are asked of the request's
// `ToolNames`, which the response translator uses for them.

use sha2::{Digest, Sha256};

use super::super::tool_index::ToolNames;
use super::super::tools::{cap, raw_qualified_name};
use super::*;
use crate::json::int_of;
use crate::openai::responses::OpenAIToOpenAIResponsesStream;

/// `responsesChatToolNameLimit`.
const NAME_LIMIT: usize = 64;

/// The apply_patch instructions a strict apply_patch tool description must
/// carry.
const PATCH_INSTRUCTIONS: [&str; 10] = [
    "*** Begin Patch",
    "*** End Patch",
    "*** Add File:",
    "*** Delete File:",
    "*** Update File:",
    "*** Move to:",
    "*** End of File",
    "@@",
    "start: patch",
    "JSON object",
];

/// `namespaceRecoveryRequest`.
const NAMESPACE_RECOVERY_REQUEST: &str = r#"{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]}"#;

/// The translator's body, which must not be refused.
#[track_caller]
fn convert_openai_responses_request_to_openai_chat_completions(
    model: &str,
    request: &Value,
    stream: bool,
) -> Value {
    let (out, err) =
        super::convert_openai_responses_request_to_openai_chat_completions(model, request, stream);
    assert_eq!(err, None, "{out}");
    out
}

fn convert(request: Value) -> Value {
    convert_openai_responses_request_to_openai_chat_completions("test", &request, false)
}

fn convert_streaming(request: Value) -> Value {
    convert_openai_responses_request_to_openai_chat_completions("test", &request, true)
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Looks up a dotted path such as `messages.0.role`, like a plain gjson path.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` would return it.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// An array's items; nothing for any other value.
fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn messages(out: &Value) -> &[Value] {
    items(&out["messages"])
}

/// The roles of the output's messages, in order.
fn roles(out: &Value) -> Vec<String> {
    messages(out)
        .iter()
        .map(|message| text_at(message, "role"))
        .collect()
}

/// The `function.name` of each tool in the output.
fn tool_names(out: &Value) -> Vec<String> {
    items(&out["tools"])
        .iter()
        .map(|tool| text_at(tool, "function.name"))
        .collect()
}

/// The first tool call's name in each assistant message.
fn assistant_call_names(out: &Value) -> Vec<String> {
    messages(out)
        .iter()
        .filter(|message| message["role"] == "assistant")
        .map(|message| text_at(message, "tool_calls.0.function.name"))
        .collect()
}

/// The first tool call's name in the last assistant message, or `""`.
fn replayed_name(out: &Value) -> String {
    assistant_call_names(out).pop().unwrap_or_default()
}

/// `responsesPerfRequest`: a long conversation with one namespaced tool.
fn responses_perf_request(turns: usize) -> Value {
    let mut input = vec![json!({"role": "user", "content": "start"})];
    for i in 0..turns {
        input.push(json!({
            "type": "function_call",
            "call_id": format!("c{i}"),
            "namespace": "editor",
            "name": "read",
            "arguments": "{}"
        }));
        input.push(json!({
            "type": "function_call_output",
            "call_id": format!("c{i}"),
            "output": "x".repeat(8192)
        }));
    }
    json!({
        "model": "test",
        "instructions": "x".repeat(65536),
        "tools": [{
            "type": "namespace",
            "name": "editor",
            "tools": [{"type": "function", "name": "read", "parameters": {"type": "object"}}]
        }],
        "input": input
    })
}

#[test]
fn merges_consecutive_function_calls() {
    let out = convert_streaming(json!({
        "input": [
            {"type": "function_call", "call_id": "exec_command:0", "name": "exec_command", "arguments": r#"{"cmd":"ls"}"#},
            {"type": "function_call", "call_id": "exec_command:1", "name": "exec_command", "arguments": r#"{"cmd":"pwd"}"#},
            {"type": "function_call_output", "call_id": "exec_command:0", "output": "ok0"},
            {"type": "function_call_output", "call_id": "exec_command:1", "output": "ok1"}
        ]
    }));

    assert!(
        out["messages"].is_array(),
        "messages should be an array: {out}"
    );
    assert_eq!(messages(&out).len(), 3, "messages count: {out}");
    assert_eq!(text_at(&out, "messages.0.role"), "assistant", "{out}");
    assert_eq!(
        items(&out["messages"][0]["tool_calls"]).len(),
        2,
        "messages.0.tool_calls length: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.0.tool_calls.0.id"),
        "exec_command:0",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "messages.0.tool_calls.1.id"),
        "exec_command:1",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.tool_call_id"),
        "exec_command:0",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "messages.2.tool_call_id"),
        "exec_command:1",
        "{out}"
    );
}

#[test]
fn splits_function_calls_when_interrupted() {
    let out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
            {"type": "message", "role": "user", "content": "next"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"}
        ]
    }));

    assert_eq!(messages(&out).len(), 3, "messages count: {out}");
    assert_eq!(
        text_at(&out, "messages.0.tool_calls.0.id"),
        "call_a",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "messages.2.tool_calls.0.id"),
        "call_b",
        "{out}"
    );
}

#[test]
fn defers_message_until_tool_output() {
    let out = convert_streaming(json!({
        "input": [
            {"type": "function_call", "call_id": "call_x", "name": "exec_command", "arguments": r#"{"cmd":"echo hi"}"#},
            {"type": "message", "role": "user", "content": "Approved command prefix saved"},
            {"type": "function_call_output", "call_id": "call_x", "output": "ok"},
            {"type": "message", "role": "user", "content": "next"}
        ]
    }));

    assert_eq!(messages(&out).len(), 4, "messages count: {out}");
    assert_eq!(text_at(&out, "messages.0.role"), "assistant", "{out}");
    assert_eq!(text_at(&out, "messages.1.role"), "tool", "{out}");
    assert_eq!(text_at(&out, "messages.1.tool_call_id"), "call_x", "{out}");
    assert_eq!(text_at(&out, "messages.2.role"), "user", "{out}");
    assert_eq!(
        text_at(&out, "messages.2.content"),
        "Approved command prefix saved",
        "{out}"
    );
    assert_eq!(text_at(&out, "messages.3.content"), "next", "{out}");
}

#[test]
fn unwraps_stringified_tool_output_images() {
    let cases = [
        (
            "Codex input image",
            r#"[{"type":"input_text","text":"Captured screenshot."},{"detail":"original","image_url":"data:image/png;base64,AA==","type":"input_image"}]"#,
            1,
            "data:image/png;base64,AA==",
            "Captured screenshot.",
            "high",
        ),
        (
            "OpenAI image URL",
            r#"[{"type":"image_url","image_url":{"url":"https://example.com/generated.png","detail":"high"}}]"#,
            0,
            "https://example.com/generated.png",
            "",
            "high",
        ),
    ];

    for (name, output, image_index, expected_url, expected_text, detail) in cases {
        let out = convert(json!({
            "input": [
                {"type": "function_call", "call_id": "call_image", "name": "view_image", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_image", "output": output}
            ]
        }));
        let content = &out["messages"][1]["content"];
        assert!(
            content.is_array(),
            "{name}: expected tool content array, got {content}; output={out}"
        );
        let parts = items(content);
        assert!(
            parts.len() > image_index,
            "{name}: expected image part at index {image_index}, got {content}"
        );
        let image_part = &parts[image_index];
        assert_eq!(
            text_at(image_part, "type"),
            "image_url",
            "{name}: {image_part}"
        );
        assert_eq!(
            text_at(image_part, "image_url.url"),
            expected_url,
            "{name}: {image_part}"
        );
        assert_eq!(
            text_at(image_part, "image_url.detail"),
            detail,
            "{name}: {image_part}"
        );
        if !expected_text.is_empty() {
            assert_eq!(text_at(&parts[0], "type"), "text", "{name}: {}", parts[0]);
            assert_eq!(
                text_at(&parts[0], "text"),
                expected_text,
                "{name}: {}",
                parts[0]
            );
        }
    }
}

#[test]
fn unwraps_stringified_custom_tool_output_images() {
    let out = convert(json!({
        "input": [
            {"type": "custom_tool_call", "call_id": "call_image", "name": "view_image", "input": "{}"},
            {
                "type": "custom_tool_call_output",
                "call_id": "call_image",
                "output": r#"[{"type":"input_image","image_url":"data:image/png;base64,AA==","detail":"original"}]"#
            }
        ]
    }));

    let content = &out["messages"][1]["content"];
    assert!(
        content.is_array(),
        "expected custom tool content array, got {content}; output={out}"
    );
    assert_eq!(text_at(content, "0.type"), "image_url", "{out}");
    assert_eq!(
        text_at(content, "0.image_url.url"),
        "data:image/png;base64,AA==",
        "{out}"
    );
    assert_eq!(text_at(content, "0.image_url.detail"), "high", "{out}");
}

#[test]
fn preserves_custom_tool_output_fallbacks() {
    let cases = [
        ("plain text", json!("plain output"), "plain output"),
        (
            "text content array",
            json!([{"type": "input_text", "text": "done"}]),
            "done",
        ),
        (
            "invalid image array",
            json!([{"type": "input_image", "detail": "low"}]),
            "",
        ),
    ];

    for (name, output, expected) in cases {
        let out = convert(json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_output", "name": "inspect", "input": "{}"},
                {"type": "custom_tool_call_output", "call_id": "call_output", "output": output}
            ]
        }));
        let content = &out["messages"][1]["content"];
        assert!(
            content.is_string(),
            "{name}: expected custom tool content string, got {content}; output={out}"
        );
        assert_eq!(content, expected, "{name}: custom tool content: {out}");
    }
}

#[test]
fn converts_structured_tool_output_images() {
    let out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_image", "name": "view_image", "arguments": "{}"},
            {
                "type": "function_call_output",
                "call_id": "call_image",
                "output": [
                    {"type": "input_text", "text": "Captured screenshot."},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA==", "detail": "original"}
                ]
            }
        ]
    }));

    let content = &out["messages"][1]["content"];
    assert!(
        content.is_array(),
        "expected tool content array, got {content}; output={out}"
    );
    assert_eq!(text_at(content, "1.type"), "image_url", "{out}");
    assert_eq!(
        text_at(content, "1.image_url.url"),
        "data:image/png;base64,AA==",
        "{out}"
    );
    assert_eq!(text_at(content, "1.image_url.detail"), "high", "{out}");
}

#[test]
fn keeps_non_image_tool_output_strings() {
    let cases = [
        ("plain text", "plain output"),
        ("JSON object", r#"{"status":"ok"}"#),
        (
            "text-only array",
            r#"[{"type":"input_text","text":"still text"}]"#,
        ),
        (
            "invalid image array",
            r#"[{"type":"input_image","detail":"low"}]"#,
        ),
        (
            "image array with trailing text",
            r#"[{"type":"input_image","image_url":"data:image/png;base64,AA=="}] trailing"#,
        ),
        (
            "truncated image array",
            r#"[{"type":"input_image","image_url":"data:image/png;base64,AA=="}"#,
        ),
        (
            "non-string image URL",
            r#"[{"type":"input_image","image_url":123}]"#,
        ),
        (
            "non-string image detail",
            r#"[{"type":"input_image","image_url":"data:image/png;base64,AA==","detail":123}]"#,
        ),
        (
            "non-string text in image array",
            r#"[{"type":"input_text","text":123},{"type":"input_image","image_url":"data:image/png;base64,AA=="}]"#,
        ),
    ];

    for (name, output) in cases {
        let out = convert(json!({
            "input": [
                {"type": "function_call", "call_id": "call_output", "name": "inspect", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_output", "output": output}
            ]
        }));
        let content = &out["messages"][1]["content"];
        assert!(
            content.is_string(),
            "{name}: expected tool content string, got {content}; output={out}"
        );
        assert_eq!(content, output, "{name}: tool content: {out}");
    }
}

#[test]
fn attaches_reasoning_to_assistant_message() {
    let out = convert(json!({
        "input": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [
                    {"type": "summary_text", "text": "first line\n"},
                    {"type": "summary_text", "text": "second line"}
                ]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "answer"}]
            },
            {"type": "message", "role": "user", "content": "next"}
        ]
    }));

    assert_eq!(messages(&out).len(), 2, "messages count: {out}");
    assert_eq!(text_at(&out, "messages.0.role"), "assistant", "{out}");
    assert_eq!(
        text_at(&out, "messages.0.reasoning_content"),
        "first line\nsecond line",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "messages.0.content.0.text"),
        "answer",
        "{out}"
    );
    assert_eq!(text_at(&out, "messages.1.role"), "user", "{out}");
}

#[test]
fn preserves_assistant_content_with_tool_calls() {
    let out = convert(json!({
        "input": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "inspect the next step"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Step 3 completed; continue to step 4."}]
            },
            {"type": "function_call", "call_id": "call_4", "name": "exec_command", "arguments": r#"{"cmd":"pwd"}"#},
            {"type": "function_call_output", "call_id": "call_4", "output": "ok"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 2, "messages count: {out}");
    let assistant = &messages[0];
    assert_eq!(text_at(assistant, "role"), "assistant", "{out}");
    assert_eq!(
        text_at(assistant, "reasoning_content"),
        "inspect the next step",
        "{out}"
    );
    assert_eq!(
        text_at(assistant, "content.0.text"),
        "Step 3 completed; continue to step 4.",
        "assistant content should be preserved: {out}"
    );
    assert_eq!(text_at(assistant, "tool_calls.0.id"), "call_4", "{out}");
    assert_eq!(text_at(&messages[1], "tool_call_id"), "call_4", "{out}");
}

#[test]
fn does_not_merge_tool_calls_across_user_message() {
    let out = convert(json!({
        "input": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]},
            {"type": "function_call", "call_id": "call_next", "name": "exec_command", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_next", "output": "ok"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 4, "messages count: {out}");
    assert!(
        messages[0].get("tool_calls").is_none(),
        "messages.0 unexpectedly contains tool calls: {out}"
    );
    assert_eq!(text_at(&messages[1], "role"), "user", "{out}");
    assert_eq!(
        text_at(&messages[2], "tool_calls.0.id"),
        "call_next",
        "{out}"
    );
}

#[test]
fn merges_distinct_reasoning_within_assistant_turn() {
    let out = convert(json!({
        "input": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "first"}]},
            {"type": "message", "role": "assistant", "reasoning_content": "first", "content": [{"type": "output_text", "text": "working"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "second"}]},
            {"type": "function_call", "call_id": "call_reasoning", "name": "exec_command", "arguments": "{}"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 1, "messages count: {out}");
    assert_eq!(
        text_at(&messages[0], "reasoning_content"),
        "first\n\nsecond",
        "{out}"
    );
    assert_eq!(
        text_at(&messages[0], "tool_calls.0.id"),
        "call_reasoning",
        "{out}"
    );
}

#[test]
fn replaces_unavailable_reasoning_within_assistant_turn() {
    let out = convert(json!({
        "input": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "role": "assistant", "reasoning_content": "real reasoning", "content": [{"type": "output_text", "text": "working"}]},
            {"type": "function_call", "call_id": "call_real_reasoning", "name": "exec_command", "arguments": "{}"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 1, "messages count: {out}");
    assert_eq!(
        text_at(&messages[0], "reasoning_content"),
        "real reasoning",
        "{out}"
    );
}

#[test]
fn attaches_reasoning_to_tool_call_message() {
    let out = convert_streaming(json!({
        "input": [
            {
                "type": "reasoning",
                "id": "rs_tool",
                "summary": [{"type": "summary_text", "text": "tool reasoning"}]
            },
            {"type": "function_call", "call_id": "call_1", "name": "exec_command", "arguments": r#"{"cmd":"pwd"}"#},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]
    }));

    assert_eq!(messages(&out).len(), 2, "messages count: {out}");
    assert_eq!(text_at(&out, "messages.0.role"), "assistant", "{out}");
    assert_eq!(
        text_at(&out, "messages.0.reasoning_content"),
        "tool reasoning",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "messages.0.tool_calls.0.id"),
        "call_1",
        "{out}"
    );
    assert_eq!(text_at(&out, "messages.1.role"), "tool", "{out}");
}

#[test]
fn keeps_reasoning_before_user_message() {
    let out = convert(json!({
        "input": [
            {"type": "reasoning", "id": "rs_empty", "summary": []},
            {"type": "message", "role": "user", "content": "continue"}
        ]
    }));

    assert_eq!(messages(&out).len(), 2, "messages count: {out}");
    assert_eq!(text_at(&out, "messages.0.role"), "assistant", "{out}");
    assert_eq!(
        text_at(&out, "messages.0.reasoning_content"),
        "[reasoning unavailable]",
        "placeholder: {out}"
    );
    assert_eq!(text_at(&out, "messages.1.role"), "user", "{out}");
}

#[test]
fn preserves_reasoning_on_follow_up_tool_turns() {
    let out = convert_streaming(json!({
        "model": "deepseek-v4.1-flash",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "first plan"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "starting"}]},
            {"type": "function_call", "call_id": "call_1", "name": "exec_command", "arguments": r#"{"cmd":"ls"}"#},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"type": "function_call", "call_id": "call_2", "name": "write_stdin", "arguments": r#"{"data":"x"}"#},
            {"type": "function_call_output", "call_id": "call_2", "output": "ok"},
            {"type": "reasoning", "id": "rs_2", "summary": [{"type": "summary_text", "text": "second plan"}]},
            {"type": "function_call", "call_id": "call_3", "name": "exec_command", "arguments": r#"{"cmd":"pwd"}"#},
            {"type": "function_call_output", "call_id": "call_3", "output": "ok"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "apply_patch is available"}]},
            {"type": "function_call", "call_id": "call_4", "name": "exec_command", "arguments": r#"{"cmd":"cat"}"#},
            {"type": "function_call_output", "call_id": "call_4", "output": "ok"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 8, "messages count: {out}");
    for (i, message) in messages.iter().enumerate() {
        if message["role"] == "assistant" && !items(&message["tool_calls"]).is_empty() {
            assert_ne!(
                text_at(message, "reasoning_content"),
                "",
                "messages[{i}] with tool_calls is missing reasoning_content; message={message}"
            );
        }
    }
    assert_eq!(text_at(&messages[2], "tool_calls.0.id"), "call_2", "{out}");
    assert_eq!(
        text_at(&messages[2], "reasoning_content"),
        "first plan",
        "{out}"
    );
    assert_eq!(text_at(&messages[6], "tool_calls.0.id"), "call_4", "{out}");
    assert_eq!(
        text_at(&messages[6], "reasoning_content"),
        "second plan",
        "{out}"
    );
}

#[test]
fn falls_back_to_placeholder_when_no_prior_reasoning() {
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "exec_command", "arguments": r#"{"cmd":"ls"}"#},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 2, "messages count: {out}");
    assert_eq!(
        text_at(&messages[0], "reasoning_content"),
        "[reasoning unavailable]",
        "{out}"
    );
}

#[test]
fn effort_none_does_not_inject_reasoning() {
    let out = convert(json!({
        "model": "gpt-4o",
        "reasoning": {"effort": "none"},
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "exec_command", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 2, "messages count: {out}");
    assert!(
        messages[0].get("reasoning_content").is_none(),
        "messages.0 should not have reasoning_content when effort is none: {out}"
    );
}

#[test]
fn preserves_reasoning_on_custom_tool_call_turns() {
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "custom plan"}]},
            {"type": "custom_tool_call", "call_id": "cust_1", "name": "do_work", "input": "step1"},
            {"type": "custom_tool_call_output", "call_id": "cust_1", "output": "done"},
            {"type": "custom_tool_call", "call_id": "cust_2", "name": "do_work", "input": "step2"},
            {"type": "custom_tool_call_output", "call_id": "cust_2", "output": "done"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 4, "messages count: {out}");
    assert_eq!(
        text_at(&messages[0], "reasoning_content"),
        "custom plan",
        "{out}"
    );
    assert_eq!(
        text_at(&messages[2], "reasoning_content"),
        "custom plan",
        "{out}"
    );
}

#[test]
fn resets_reasoning_across_user_message_boundary() {
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "reasoning": {"effort": "high"},
        "input": [
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "turn 1 plan"}]},
            {"type": "function_call", "call_id": "call_1", "name": "exec_command", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"type": "message", "role": "user", "content": "now do step 2"},
            {"type": "function_call", "call_id": "call_2", "name": "exec_command", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_2", "output": "ok"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(messages.len(), 5, "messages count: {out}");
    assert_eq!(
        text_at(&messages[0], "reasoning_content"),
        "turn 1 plan",
        "{out}"
    );
    assert_eq!(
        text_at(&messages[3], "reasoning_content"),
        "[reasoning unavailable]",
        "{out}"
    );
}

#[test]
fn flattens_namespace_tools() {
    let out = convert(json!({
        "input": [
            {"role": "user", "content": "Use add_numbers."}
        ],
        "tools": [
            {
                "type": "namespace",
                "name": "mcp__test_mcp__",
                "description": "Tools in the mcp__test_mcp__ namespace.",
                "tools": [
                    {
                        "type": "function",
                        "name": "add_numbers",
                        "description": "Add two numbers",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "a": {"type": "number"},
                                "b": {"type": "number"}
                            },
                            "required": ["a", "b"]
                        }
                    }
                ]
            }
        ],
        "tool_choice": "auto"
    }));

    assert_eq!(items(&out["tools"]).len(), 1, "tools count: {out}");
    assert_eq!(text_at(&out, "tools.0.type"), "function", "{out}");
    assert_eq!(
        text_at(&out, "tools.0.function.name"),
        "mcp__test_mcp__add_numbers",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "tools.0.function.description"),
        "Add two numbers",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "tools.0.function.parameters.required.0"),
        "a",
        "{out}"
    );
}

#[test]
fn qualifies_namespace_function_call_history() {
    let out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_get_me", "name": "get_me", "namespace": "mcp__github", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_get_me", "output": "ok"}
        ],
        "tools": [
            {
                "type": "namespace",
                "name": "mcp__github",
                "tools": [{"type": "function", "name": "get_me", "parameters": {"type": "object"}}]
            }
        ]
    }));

    let history_name = text_at(&out, "messages.0.tool_calls.0.function.name");
    let declared_name = text_at(&out, "tools.0.function.name");
    assert_eq!(history_name, "mcp__github__get_me", "history name: {out}");
    assert_eq!(
        history_name, declared_name,
        "history name should match the declared name: {out}"
    );
}

#[test]
fn flattens_namespace_custom_tools() {
    let cases = [
        (
            "top-level tools",
            json!({
                "tools": [{
                    "type": "namespace",
                    "name": "terminal",
                    "tools": [{"type": "custom", "name": "exec", "description": "Run a command"}]
                }]
            }),
        ),
        (
            "additional tools",
            json!({
                "input": [{
                    "type": "additional_tools",
                    "tools": [{
                        "type": "namespace",
                        "name": "terminal",
                        "tools": [{"type": "custom", "name": "exec", "description": "Run a command"}]
                    }]
                }]
            }),
        ),
    ];

    for (name, request) in cases {
        let out = convert(request);
        assert_eq!(items(&out["tools"]).len(), 1, "{name}: tools count: {out}");
        assert_eq!(
            text_at(&out, "tools.0.function.name"),
            "terminal__exec",
            "{name}: {out}"
        );
        assert_eq!(
            text_at(&out, "tools.0.function.description"),
            "Run a command",
            "{name}: {out}"
        );
        assert_eq!(
            text_at(&out, "tools.0.function.parameters.type"),
            "object",
            "{name}: {out}"
        );
        assert_eq!(
            text_at(&out, "tools.0.function.parameters.properties.input.type"),
            "string",
            "{name}: {out}"
        );
        assert_eq!(
            text_at(&out, "tools.0.function.parameters.required.0"),
            "input",
            "{name}: {out}"
        );
    }
}

#[test]
fn preserves_structured_tool_choice() {
    let out = convert(json!({
        "input": [
            {"role": "user", "content": "Run command."}
        ],
        "tools": [
            {
                "type": "function",
                "name": "run_command",
                "parameters": {"type": "object"}
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": {
                "name": "run_command"
            }
        }
    }));

    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    assert_eq!(
        text_at(&out, "tool_choice.function.name"),
        "run_command",
        "{out}"
    );
}

#[test]
fn converts_canonical_responses_named_tool_choice() {
    let out = convert(json!({
        "model": "gpt-5.4",
        "input": [{"role": "user", "content": "Call gateway_echo with value TOOL_OK."}],
        "tools": [{
            "type": "function",
            "name": "gateway_echo",
            "description": "Returns the given value",
            "parameters": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            }
        }],
        "tool_choice": {"type": "function", "name": "gateway_echo"},
        "max_output_tokens": 512
    }));

    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    assert_eq!(
        text_at(&out, "tool_choice.function.name"),
        "gateway_echo",
        "{out}"
    );
    assert!(
        at(&out, "tool_choice.name").is_none(),
        "tool_choice.name should be absent at top-level: {out}"
    );
}

#[test]
fn converts_namespace_and_custom_tool_choice() {
    let out = convert(json!({
        "model": "gpt-5.4",
        "input": "test",
        "tools": [
            {
                "type": "namespace",
                "name": "service_tools",
                "tools": [
                    {
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type": "object"}
                    }
                ]
            }
        ],
        "tool_choice": {
            "type": "function",
            "name": "lookup"
        }
    }));
    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    assert_eq!(
        text_at(&out, "tool_choice.function.name"),
        "service_tools__lookup",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "tools.0.function.name"),
        text_at(&out, "tool_choice.function.name"),
        "tool_choice.function.name must match the declared tools.0.function.name: {out}"
    );
    assert!(
        at(&out, "tool_choice.name").is_none(),
        "tool_choice.name should be absent at top-level: {out}"
    );

    let out = convert(json!({
        "model": "gpt-5.4",
        "input": "test",
        "tools": [
            {
                "type": "namespace",
                "name": "service_tools",
                "tools": [
                    {
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type": "object"}
                    }
                ]
            }
        ],
        "tool_choice": {
            "type": "function",
            "name": "lookup",
            "namespace": "service_tools"
        }
    }));
    assert_eq!(
        text_at(&out, "tool_choice.function.name"),
        "service_tools__lookup",
        "explicit namespace: {out}"
    );
    assert!(
        at(&out, "tool_choice.namespace").is_none(),
        "tool_choice.namespace should be absent at top-level: {out}"
    );
    assert!(
        at(&out, "tool_choice.name").is_none(),
        "tool_choice.name should be absent at top-level: {out}"
    );

    let out = convert(json!({
        "model": "gpt-5.4",
        "input": "test",
        "tools": [
            {
                "type": "custom",
                "name": "patch_runner",
                "description": "Applies diff"
            }
        ],
        "tool_choice": {
            "type": "custom",
            "name": "patch_runner"
        }
    }));
    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    assert_eq!(
        text_at(&out, "tool_choice.function.name"),
        "patch_runner",
        "{out}"
    );

    for scalar in [r#""auto""#, r#""none""#, r#""required""#] {
        let out = convert(json!({
            "model": "gpt-5.4",
            "input": "test",
            "tools": [
                {
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"}
                }
            ],
            "tool_choice": parse(scalar)
        }));
        assert_eq!(out["tool_choice"].to_string(), scalar, "{out}");
    }
}

#[test]
fn omits_tool_settings_without_tools() {
    let cases = [
        (
            "empty tools",
            json!({
                "input": [{"role": "user", "content": "say ok"}],
                "tools": [],
                "tool_choice": "auto",
                "parallel_tool_calls": false
            }),
        ),
        (
            "unconvertible tools",
            json!({
                "tools": [{"type": "unsupported"}],
                "tool_choice": "auto",
                "parallel_tool_calls": false
            }),
        ),
    ];

    for (name, request) in cases {
        let out = convert(request);
        for field in ["tools", "tool_choice", "parallel_tool_calls"] {
            assert!(
                out.get(field).is_none(),
                "{name}: {field} should be omitted without tools: {out}"
            );
        }
    }
}

#[test]
fn preserves_parallel_tool_calls_with_tools() {
    let out = convert(json!({
        "tools": [
            {
                "type": "function",
                "name": "run_command",
                "parameters": {"type": "object"}
            }
        ],
        "parallel_tool_calls": false
    }));

    assert!(
        out.get("parallel_tool_calls")
            .is_some_and(|value| !bool_of(value)),
        "parallel_tool_calls should be false: {out}"
    );
}

#[test]
fn preserves_json_schema_text_format() {
    let out = convert(json!({
        "text": {
            "format": {
                "type": "json_schema",
                "name": "answer",
                "description": "Structured answer",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "ok": {"type": "boolean"}
                    },
                    "required": ["ok"],
                    "additionalProperties": false
                }
            }
        }
    }));

    assert_eq!(
        text_at(&out, "response_format.type"),
        "json_schema",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "response_format.json_schema.name"),
        "answer",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "response_format.json_schema.description"),
        "Structured answer",
        "{out}"
    );
    assert!(
        at(&out, "response_format.json_schema.strict").is_some_and(bool_of),
        "response_format.json_schema.strict should be true: {out}"
    );
    assert_eq!(
        text_at(
            &out,
            "response_format.json_schema.schema.properties.ok.type"
        ),
        "boolean",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "response_format.json_schema.schema.required.0"),
        "ok",
        "{out}"
    );
    assert!(
        at(
            &out,
            "response_format.json_schema.schema.additionalProperties"
        )
        .is_some_and(|value| !bool_of(value)),
        "response_format.json_schema.schema.additionalProperties should be false: {out}"
    );
}

#[test]
fn preserves_json_object_text_format() {
    let out = convert(json!({"text": {"format": {"type": "json_object"}}}));

    assert_eq!(
        text_at(&out, "response_format.type"),
        "json_object",
        "{out}"
    );
    assert!(
        at(&out, "response_format.json_schema").is_none(),
        "response_format.json_schema should be omitted: {out}"
    );
}

#[test]
fn omits_response_format_without_text_format() {
    let out = convert(json!({"input": "Return plain text."}));

    assert!(
        out.get("response_format").is_none(),
        "response_format should be omitted: {out}"
    );
}

#[test]
fn normalizes_input_image_detail() {
    let cases = [
        ("standard high", json!("high"), "high"),
        ("Codex original", json!("original"), "high"),
        ("unsupported value", json!("medium"), ""),
        ("non-string value", json!(123), ""),
    ];

    for (name, detail, expected_detail) in cases {
        let out = convert(json!({
            "input": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "input_image",
                            "image_url": "https://example.com/image.png",
                            "detail": detail
                        }
                    ]
                }
            ]
        }));
        assert_eq!(
            text_at(&out, "messages.0.content.0.image_url.url"),
            "https://example.com/image.png",
            "{name}: {out}"
        );
        let detail = at(&out, "messages.0.content.0.image_url.detail");
        if expected_detail.is_empty() {
            assert!(
                detail.is_none(),
                "{name}: image detail should be omitted: {out}"
            );
            continue;
        }
        assert_eq!(str_of(detail), expected_detail, "{name}: {out}");
    }
}

#[test]
fn deduplicates_tools_across_additional_tools() {
    let out = convert(json!({
        "input": [
            {"role": "user", "content": "What time is it?"},
            {
                "type": "additional_tools",
                "tools": [
                    {"type": "function", "name": "get_time", "description": "copy from additional_tools", "parameters": {"type": "object", "properties": {"tz": {"type": "string"}}}}
                ]
            }
        ],
        "tools": [
            {"type": "function", "name": "get_time", "description": "authoritative top-level definition", "parameters": {"type": "object", "properties": {"timezone": {"type": "string"}}}}
        ]
    }));

    assert_eq!(items(&out["tools"]).len(), 1, "tools count: {out}");
    assert_eq!(text_at(&out, "tools.0.function.name"), "get_time", "{out}");
    assert_eq!(
        text_at(&out, "tools.0.function.description"),
        "authoritative top-level definition",
        "the top-level definition should win: {out}"
    );
    assert_eq!(
        text_at(&out, "tools.0.function.parameters.properties.timezone.type"),
        "string",
        "parameters should come from the top-level definition: {out}"
    );
}

#[test]
fn deduplicates_namespace_qualified_collision() {
    let out = convert(json!({
        "input": [
            {"role": "user", "content": "Patch the file."}
        ],
        "tools": [
            {"type": "function", "name": "editor__apply_patch", "parameters": {"type": "object"}},
            {
                "type": "namespace",
                "name": "editor",
                "tools": [{"type": "function", "name": "apply_patch", "parameters": {"type": "object"}}]
            }
        ]
    }));

    assert_eq!(items(&out["tools"]).len(), 1, "tools count: {out}");
    assert_eq!(
        text_at(&out, "tools.0.function.name"),
        "editor__apply_patch",
        "{out}"
    );
}

#[test]
fn keeps_distinct_tools_from_both_sources() {
    let out = convert(json!({
        "input": [
            {"role": "user", "content": "Do the thing."},
            {
                "type": "additional_tools",
                "tools": [
                    {"type": "function", "name": "get_date", "parameters": {"type": "object"}},
                    {"type": "function", "name": "get_time", "parameters": {"type": "object"}}
                ]
            }
        ],
        "tools": [
            {"type": "function", "name": "get_time", "parameters": {"type": "object"}},
            {"type": "function", "name": "get_weather", "parameters": {"type": "object"}}
        ]
    }));

    assert_eq!(
        tool_names(&out),
        ["get_time", "get_weather", "get_date"],
        "{out}"
    );
}

#[test]
fn responses_single_custom_tool_name_counts_deduplicated_tools() {
    let request = json!({
        "input": [
            {"role": "user", "content": "Patch the file."},
            {
                "type": "additional_tools",
                "tools": [{"type": "custom", "name": "apply_patch", "description": "copy"}]
            }
        ],
        "tools": [
            {"type": "custom", "name": "apply_patch", "description": "authoritative"}
        ]
    });

    assert_eq!(
        ToolNames::new(&request).single_custom_name(),
        ("apply_patch", true),
        "the only tool, given in both sources"
    );
}

/// `splitResponsesQualifiedFunctionCallFromRequest`.
fn split<'n>(names: &'n ToolNames, qualified: &'n str) -> (&'n str, &'n str) {
    names.identity(qualified)
}

#[test]
fn split_responses_qualified_function_call_first_declaration_wins() {
    let flat_first = json!({
        "tools": [
            {"type": "function", "name": "editor__apply_patch", "parameters": {"type": "object"}},
            {"type": "namespace", "name": "editor", "tools": [{"type": "function", "name": "apply_patch", "parameters": {"type": "object"}}]}
        ]
    });
    let namespace_first = json!({
        "tools": [
            {"type": "namespace", "name": "editor", "tools": [{"type": "function", "name": "apply_patch", "parameters": {"type": "object"}}]},
            {"type": "function", "name": "editor__apply_patch", "parameters": {"type": "object"}}
        ]
    });
    let namespace_only = json!({
        "tools": [
            {"type": "namespace", "name": "mcp__github", "tools": [{"type": "function", "name": "get_me", "parameters": {"type": "object"}}]}
        ]
    });

    let cases = [
        // The flat tool is the one that survives merging, so it must stay
        // flat.
        (
            "flat declared first",
            &flat_first,
            "editor__apply_patch",
            ("editor__apply_patch", ""),
        ),
        // The namespace child survives here, so the call splits back into it.
        (
            "namespace declared first",
            &namespace_first,
            "editor__apply_patch",
            ("apply_patch", "editor"),
        ),
        // No collision: unchanged behaviour.
        (
            "namespace only",
            &namespace_only,
            "mcp__github__get_me",
            ("get_me", "mcp__github"),
        ),
        // An unknown name falls through untouched.
        (
            "unknown name",
            &flat_first,
            "something_else",
            ("something_else", ""),
        ),
    ];
    for (name, request, qualified, want) in cases {
        let names = ToolNames::new(request);
        assert_eq!(
            split(&names, qualified),
            want,
            "{name}: split({qualified:?})"
        );
    }
}

#[test]
fn split_responses_qualified_function_call_matches_merged_tool_identity() {
    // Whatever survives the merge must be what reverse translation reports.
    let request = json!({
        "tools": [
            {"type": "function", "name": "editor__apply_patch", "parameters": {"type": "object"}},
            {"type": "namespace", "name": "editor", "tools": [{"type": "function", "name": "apply_patch", "parameters": {"type": "object"}}]}
        ]
    });

    let index = ToolIndex::new(&request);
    let merged = index.chat_tools();
    assert_eq!(merged.len(), 1, "merged tool count: {merged:?}");
    let emitted = text_at(&merged[0], "function.name");
    assert_eq!(
        split(&index, &emitted),
        (emitted.as_str(), ""),
        "{emitted:?} came from a flat declaration"
    );
}

#[test]
fn responses_custom_tool_names_follows_merged_declaration() {
    // Declarations delivered through the two channels may differ in type: a
    // top-level function and an "additional_tools" custom tool can flatten to
    // the same Chat Completions name. Only the winner may decide whether the
    // tool is freeform, otherwise a plain function call comes back as a
    // custom_tool_call with unwrapped arguments.
    let function_first = json!({
        "input": [
            {"type": "additional_tools", "tools": [{"type": "custom", "name": "exec", "description": "copy"}]}
        ],
        "tools": [
            {"type": "function", "name": "exec", "parameters": {"type": "object"}}
        ]
    });
    let custom_first = json!({
        "input": [
            {"type": "additional_tools", "tools": [{"type": "function", "name": "exec", "parameters": {"type": "object"}}]}
        ],
        "tools": [
            {"type": "custom", "name": "exec", "description": "authoritative"}
        ]
    });

    for (name, request, want_custom) in [
        ("function declaration wins", function_first, false),
        ("custom declaration wins", custom_first, true),
    ] {
        let index = ToolIndex::new(&request);
        let merged = index.chat_tools();
        assert_eq!(merged.len(), 1, "{name}: merged tool count: {merged:?}");
        // Freeform tools are the ones converted to the single-string shape.
        let merged_is_custom = at(&merged[0], "function.parameters.properties.input").is_some();
        assert_eq!(
            merged_is_custom, want_custom,
            "{name}: merged tool custom: {}",
            merged[0]
        );

        assert_eq!(
            index.is_custom("exec"),
            want_custom,
            "{name}: responsesCustomToolNames classified exec as custom"
        );

        let (single, ok) = index.single_custom_name();
        assert_eq!(ok, want_custom, "{name}: responsesSingleCustomToolName ok");
        if ok {
            assert_eq!(single, "exec", "{name}: responsesSingleCustomToolName name");
        }
    }
}

#[test]
fn responses_custom_tool_names_only_reports_merged_tools() {
    // Nested namespaces are not converted, so their children never reach the
    // upstream request and must not be classified as freeform tools either.
    let request = json!({
        "tools": [
            {"type": "namespace", "name": "outer", "tools": [
                {"type": "namespace", "name": "inner", "tools": [{"type": "custom", "name": "buried"}]},
                {"type": "custom", "name": "reachable"}
            ]}
        ]
    });

    let index = ToolIndex::new(&request);
    let merged_names: Vec<String> = index
        .chat_tools()
        .iter()
        .map(|tool| text_at(tool, "function.name"))
        .collect();
    assert!(
        merged_names.iter().any(|name| name == "outer__reachable"),
        "merged tool names = {merged_names:?}, want outer__reachable"
    );

    for name in index.custom_names() {
        assert!(
            merged_names.iter().any(|merged| merged == name),
            "responsesCustomToolNames reported {name:?}, which the merge never emits"
        );
    }
}

#[test]
fn function_call_output_alternate_ids_and_queue_fallback() {
    let cases = [
        ("call_id standard", Some("call_id")),
        ("tool_call_id alternate field", Some("tool_call_id")),
        ("callId alternate field", Some("callId")),
        ("id alternate field", Some("id")),
        ("missing call_id completely fallback to pending queue", None),
    ];

    for (name, output_field) in cases {
        let mut output = json!({"type": "function_call_output", "output": "tool_result_ok"});
        if let Some(field) = output_field {
            output[field] = json!("call_123");
        }
        let out = convert(json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "function_call", "call_id": "call_123", "name": "Bash", "arguments": r#"{"command":"ls"}"#},
                output
            ]
        }));
        let messages = messages(&out);
        assert_eq!(
            messages.len(),
            2,
            "{name}: expected 2 messages (assistant, tool): {out}"
        );
        assert_eq!(
            text_at(&messages[0], "tool_calls.0.id"),
            "call_123",
            "{name}: {out}"
        );
        let tool_message = &messages[1];
        assert_eq!(
            text_at(tool_message, "role"),
            "tool",
            "{name}: expected role tool, got {tool_message}"
        );
        assert_eq!(
            text_at(tool_message, "tool_call_id"),
            "call_123",
            "{name}: {out}"
        );
        assert_eq!(
            text_at(tool_message, "content"),
            "tool_result_ok",
            "{name}: tool message content: {out}"
        );
    }
}

#[test]
fn mixed_missing_and_explicit_parallel_outputs() {
    // Call A, Call B. Output 1 has no ID (result B); output 2 names call_a
    // (result A). Call A must not be taken by output 1; output 1 gets call B.
    let out = convert(json!({
        "model": "deepseek-v4-flash",
        "input": [
            {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"},
            {"type": "function_call_output", "output": "result_b"},
            {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(
        messages.len(),
        3,
        "expected 3 messages (assistant, tool_b, tool_a): {out}"
    );
    let results: HashMap<String, String> = messages[1..]
        .iter()
        .map(|m| (text_at(m, "tool_call_id"), text_at(m, "content")))
        .collect();
    assert_eq!(
        results.get("call_a").map(String::as_str),
        Some("result_a"),
        "{out}"
    );
    assert_eq!(
        results.get("call_b").map(String::as_str),
        Some("result_b"),
        "{out}"
    );
}

#[test]
fn defers_message_until_missing_id_tool_output() {
    // Call A, then a user message, then an output without a call_id: the user
    // message must come after the tool output.
    let out = convert(json!({
        "model": "deepseek-v4-flash",
        "input": [
            {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
            {"type": "message", "role": "user", "content": "User command while running"},
            {"type": "function_call_output", "output": "result_a"}
        ]
    }));

    let messages = messages(&out);
    assert_eq!(
        messages.len(),
        3,
        "expected 3 messages (assistant, tool, user): {out}"
    );
    assert_eq!(text_at(&messages[0], "role"), "assistant", "{out}");
    assert_eq!(
        text_at(&messages[1], "role"),
        "tool",
        "the user message was not deferred: {out}"
    );
    assert_eq!(text_at(&messages[1], "tool_call_id"), "call_a", "{out}");
    assert_eq!(text_at(&messages[2], "role"), "user", "{out}");
    assert_eq!(
        text_at(&messages[2], "content"),
        "User command while running",
        "{out}"
    );
}

#[test]
fn mixed_missing_and_explicit_parallel_outputs_across_user_message() {
    // As mixed_missing_and_explicit_parallel_outputs, with a user message
    // between the outputs.
    let out = convert(json!({
        "model": "deepseek-v4-flash",
        "input": [
            {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"},
            {"type": "function_call_output", "output": "result_b"},
            {"type": "message", "role": "user", "content": "status?"},
            {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
        ]
    }));

    let results: HashMap<String, String> = messages(&out)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| (text_at(m, "tool_call_id"), text_at(m, "content")))
        .collect();
    assert_eq!(
        results.get("call_a").map(String::as_str),
        Some("result_a"),
        "{out}"
    );
    assert_eq!(
        results.get("call_b").map(String::as_str),
        Some("result_b"),
        "{out}"
    );
}

#[test]
fn orphan_function_call_output_becomes_user_message() {
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "Task initialization"}]},
            {
                "type": "function_call_output",
                "id": "fco_01a09fca-8d33-73a1-97fd-4d83ecc02f9d",
                "name": "send_message_to_thread",
                "output": "<codex_delegation>\n  <source_thread_id>01a022d7-d4d0-72b2-8571-4590484ccaee</source_thread_id>\n  <input>Execute sub-task</input>\n</codex_delegation>"
            },
            {"type": "function_call", "call_id": "call_1789387253098037589_85", "name": "Bash", "arguments": r#"{"command":"pwd"}"#},
            {"type": "function_call_output", "call_id": "call_1789387253098037589_85", "id": "fco_01a09fca-a5f0-7b40-9943-21fbc923c537", "output": "/Users/developer"}
        ]
    }));

    let mut delegation_found = false;
    let mut bash_tool_found = false;
    for message in messages(&out) {
        let role = text_at(message, "role");
        assert!(
            !(role == "tool" && text_at(message, "tool_call_id").trim().is_empty()),
            "orphan output emitted as a tool message with an empty tool_call_id: {out}"
        );
        if role == "user" && text_at(message, "content").contains("<codex_delegation>") {
            delegation_found = true;
        }
        if role == "tool" && message["tool_call_id"] == "call_1789387253098037589_85" {
            bash_tool_found = true;
            assert_eq!(
                text_at(message, "content"),
                "/Users/developer",
                "bash tool content: {out}"
            );
        }
    }
    assert!(
        delegation_found,
        "expected the orphan send_message_to_thread output as user content: {out}"
    );
    assert!(
        bash_tool_found,
        "expected a paired Bash tool message: {out}"
    );
}

#[test]
fn unpaired_explicit_call_id_becomes_user_message() {
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "Task initialization"}]},
            {"type": "function_call_output", "call_id": "call_missing", "name": "send_message_to_thread", "output": "<codex_delegation>Execute sub-task</codex_delegation>"},
            {"type": "function_call", "call_id": "call_1789387253098037589_85", "name": "Bash", "arguments": r#"{"command":"pwd"}"#},
            {"type": "function_call_output", "call_id": "call_1789387253098037589_85", "output": "/Users/developer"}
        ]
    }));

    let mut delegation_found = false;
    let mut bash_tool_found = false;
    for message in messages(&out) {
        let role = text_at(message, "role");
        assert!(
            !(role == "tool" && message["tool_call_id"] == "call_missing"),
            "unpaired output emitted as a tool message: {out}"
        );
        if role == "user" && text_at(message, "content").contains("<codex_delegation>") {
            delegation_found = true;
        }
        if role == "tool" && message["tool_call_id"] == "call_1789387253098037589_85" {
            bash_tool_found = true;
        }
    }
    assert!(
        delegation_found,
        "expected the unpaired send_message_to_thread output as user content: {out}"
    );
    assert!(
        bash_tool_found,
        "expected a paired Bash tool message: {out}"
    );
}

#[test]
fn caps_long_namespace_tool_names() {
    let out = convert(json!({
        "input": [
            {"role": "user", "content": "hi"}
        ],
        "tools": [
            {"type": "function", "name": "exec_command", "parameters": {"type": "object"}},
            {
                "type": "namespace",
                "name": "mcp__codex_apps__codex_document_control",
                "tools": [
                    {"type": "function", "name": "_execute_document_command", "parameters": {"type": "object"}},
                    {"type": "function", "name": "_get_document_tool_schemas", "parameters": {"type": "object"}}
                ]
            },
            {
                "type": "namespace",
                "name": "mcp__codex_apps__safety_settings",
                "tools": [
                    {"type": "function", "name": "_prepare_parental_control_update", "parameters": {"type": "object"}}
                ]
            }
        ]
    }));

    let names = tool_names(&out);
    assert_eq!(names.len(), 4, "tools count: {out}");
    let mut seen = HashSet::new();
    for name in &names {
        assert!(
            name.len() <= NAME_LIMIT,
            "function.name {name:?} (len {}) exceeds the 64-character limit: {out}",
            name.len()
        );
        assert!(
            seen.insert(name),
            "duplicate function.name {name:?} after flattening: {out}"
        );
    }
}

#[test]
fn disambiguates_truncation_collisions() {
    // Two distinct namespace tools whose qualified names both truncate to the
    // same 64-character tail must survive as two usable chat tools, not be
    // merged or dropped by the first-wins deduplication.
    let filler = "a".repeat(50);
    let tools = json!([
        {
            "type": "namespace",
            "name": format!("mcp__server_one__{filler}"),
            "tools": [{"type": "function", "name": "_same_tail_tool_name", "parameters": {"type": "object"}}]
        },
        {
            "type": "namespace",
            "name": format!("mcp__server_two__{filler}"),
            "tools": [{"type": "function", "name": "_same_tail_tool_name", "parameters": {"type": "object"}}]
        }
    ]);

    let out = convert(json!({
        "input": [
            {"role": "user", "content": "hi"}
        ],
        "tools": tools.clone()
    }));
    let names = tool_names(&out);
    assert_eq!(names.len(), 2, "tools count: {out}");
    let (first, second) = (&names[0], &names[1]);
    assert_ne!(
        first, second,
        "the truncation collision was not disambiguated: {out}"
    );
    for name in [first, second] {
        assert!(
            name.len() <= NAME_LIMIT,
            "disambiguated name {name:?} (len {}) exceeds 64: {out}",
            name.len()
        );
    }

    // A replayed call to the renamed declaration must resolve to the renamed
    // chat name so the assistant history matches the tools array.
    let replay_out = convert(json!({
        "input": [
            {"type": "custom_tool_call", "namespace": format!("mcp__server_two__{filler}"), "name": "_same_tail_tool_name", "call_id": "call_1", "input": "x"},
            {"type": "custom_tool_call_output", "call_id": "call_1", "output": "y"}
        ],
        "tools": tools
    }));
    assert_eq!(
        &replayed_name(&replay_out),
        second,
        "the replayed call should match the suffixed tool: {replay_out}"
    );
}

#[test]
fn long_declaration_does_not_displace_short_original() {
    // A long namespace declaration whose capped tail equals a later flat
    // declaration's original name must take the suffix itself: the flat
    // tool's original name is what replayed calls and tool_choice carry.
    let long_namespace = format!("mcp__a__{}", "b".repeat(60));
    let long_child = "child_tool";
    let qualified = format!("{long_namespace}__{long_child}");
    let flat_name = cap(&qualified);
    assert!(
        qualified.len() > NAME_LIMIT && flat_name.len() == NAME_LIMIT && flat_name != qualified,
        "fixture drift: qualified {qualified:?} (len {}) must exceed the cap and cap to 64 characters",
        qualified.len()
    );
    let suffixed = cap(&format!("{flat_name}_1"));

    let tools = json!([
        {
            "type": "namespace",
            "name": long_namespace,
            "tools": [{"type": "function", "name": long_child, "parameters": {"type": "object"}}]
        },
        {"type": "function", "name": flat_name, "parameters": {"type": "object"}}
    ]);

    let out = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone()
    }));
    let names = tool_names(&out);
    assert_eq!(names.len(), 2, "tools count: {out}");
    assert_eq!(
        names[1], flat_name,
        "the flat declaration was displaced: {out}"
    );
    assert_eq!(names[0], suffixed, "long declaration name: {out}");
    for name in &names {
        assert!(
            name.len() <= NAME_LIMIT,
            "function.name {name:?} (len {}) exceeds 64: {out}",
            name.len()
        );
    }

    // Replayed calls and tool_choice for the flat tool carry its original
    // name; they must resolve to the flat declaration.
    let replay_out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": flat_name, "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": tools.clone()
    }));
    for name in assistant_call_names(&replay_out) {
        assert_eq!(name, flat_name, "replayed flat call: {replay_out}");
    }

    let forced_out = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone(),
        "tool_choice": {"type": "function", "function": {"name": flat_name}}
    }));
    assert_eq!(
        text_at(&forced_out, "tool_choice.function.name"),
        flat_name,
        "tool_choice for the flat tool: {forced_out}"
    );

    // A replayed call carrying the long declaration's uncut qualified name must
    // resolve to the suffixed chat name, not the capped tail the flat tool has.
    let long_replay_out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_2", "name": qualified, "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_2", "output": "ok"}
        ],
        "tools": tools
    }));
    for name in assistant_call_names(&long_replay_out) {
        assert_eq!(
            name, suffixed,
            "replayed long qualified call: {long_replay_out}"
        );
    }
}

#[test]
fn ambiguous_long_local_name_stays_unresolved() {
    // Two namespaces declaring the same local name over 64 bytes, and a
    // replayed call without a namespace: the call is ambiguous, and its capped
    // fallback must not land on either declaration's alias.
    let long_local = format!("shared_{}", "x".repeat(60));
    let tools = json!([
        {
            "type": "namespace",
            "name": "mcp__alpha",
            "tools": [{"type": "function", "name": long_local, "parameters": {"type": "object"}}]
        },
        {
            "type": "namespace",
            "name": "mcp__beta",
            "tools": [{"type": "function", "name": long_local, "parameters": {"type": "object"}}]
        }
    ]);

    let out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": long_local, "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": tools
    }));

    let declared_aliases: HashSet<String> = tool_names(&out).into_iter().collect();
    assert_eq!(declared_aliases.len(), 2, "tools count: {out}");
    let replayed = replayed_name(&out);
    assert!(
        replayed.len() <= NAME_LIMIT,
        "replayed ambiguous name {replayed:?} (len {}) exceeds 64: {out}",
        replayed.len()
    );
    assert!(
        !declared_aliases.contains(&replayed),
        "the ambiguous replayed call resolved to declared alias {replayed:?}: {out}"
    );
}

#[test]
fn long_alias_does_not_displace_namespaced_local_name() {
    // A long declaration's capped alias must not take the local name (64
    // bytes or less) of a namespaced declaration whose qualified name is over
    // the cap: calls and tool_choice without the namespace carry that local
    // name and must resolve to the namespaced tool.
    let local_name = format!("l{}", "m".repeat(63));
    let long_flat_name = format!("{}{local_name}", "n".repeat(11));
    assert!(
        long_flat_name.len() > NAME_LIMIT && cap(&long_flat_name) == local_name,
        "fixture drift: cap({long_flat_name:?}) = {:?}, want {local_name:?}",
        cap(&long_flat_name)
    );
    // mcp__beta__<local_name> is 75 bytes, so the namespaced declaration is
    // long too, and its alias is local_name itself unless the flat tool is kept
    // off it.
    let tools = json!([
        {"type": "function", "name": long_flat_name, "parameters": {"type": "object"}},
        {
            "type": "namespace",
            "name": "mcp__beta",
            "tools": [{"type": "function", "name": local_name, "parameters": {"type": "object"}}]
        }
    ]);

    let out = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone()
    }));
    let names = tool_names(&out);
    assert_eq!(names.len(), 2, "tools count: {out}");
    assert_ne!(
        names[0], local_name,
        "the long declaration claimed the namespaced local name as its capped alias: {out}"
    );
    let namespaced_alias = &names[1];
    for (i, name) in names.iter().enumerate() {
        assert!(
            name.len() <= NAME_LIMIT,
            "tools[{i}].function.name {name:?} (len {}) exceeds 64: {out}",
            name.len()
        );
    }

    // A replayed call without the namespace carries the bare local name and
    // must resolve to the namespaced declaration's alias.
    let replay_out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": local_name, "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": tools.clone()
    }));
    for name in assistant_call_names(&replay_out) {
        assert_eq!(
            &name, namespaced_alias,
            "replayed bare local name: {replay_out}"
        );
    }

    // tool_choice carrying the bare local name must resolve the same way.
    let forced_out = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools,
        "tool_choice": {"type": "function", "function": {"name": local_name}}
    }));
    assert_eq!(
        &text_at(&forced_out, "tool_choice.function.name"),
        namespaced_alias,
        "tool_choice bare local name: {forced_out}"
    );
}

#[test]
fn shared_local_name_is_never_emitted() {
    // Two namespaces declaring the same 64-byte local name: both qualified
    // names are over the cap and both cut to exactly that local name, which is
    // ambiguous for any call without a namespace. Neither declaration may be
    // given it.
    let shared_local = format!("s{}", "t".repeat(63));
    for namespace in ["mcp__alpha", "mcp__beta"] {
        assert_eq!(
            cap(&raw_qualified_name(namespace, &shared_local)),
            shared_local,
            "fixture drift: {namespace} alias"
        );
    }
    let tools = json!([
        {
            "type": "namespace",
            "name": "mcp__alpha",
            "tools": [{"type": "function", "name": shared_local, "parameters": {"type": "object"}}]
        },
        {
            "type": "namespace",
            "name": "mcp__beta",
            "tools": [{"type": "function", "name": shared_local, "parameters": {"type": "object"}}]
        }
    ]);

    let out = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone()
    }));
    let names = tool_names(&out);
    assert_eq!(names.len(), 2, "tools count: {out}");
    let mut aliases = HashSet::new();
    for (i, name) in names.iter().enumerate() {
        assert!(
            name.len() <= NAME_LIMIT,
            "tools[{i}].function.name {name:?} (len {}) exceeds 64: {out}",
            name.len()
        );
        assert_ne!(
            *name, shared_local,
            "tools[{i}] emits the ambiguous local name: {out}"
        );
        assert!(
            aliases.insert(name.clone()),
            "tools[{i}] duplicates alias {name:?}: {out}"
        );
    }
    let (alpha_alias, beta_alias) = (&names[0], &names[1]);

    // Each namespace still reaches its own declaration.
    for (namespace, want) in [("mcp__alpha", alpha_alias), ("mcp__beta", beta_alias)] {
        let namespaced_out = convert(json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "namespace": namespace, "name": shared_local, "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
            "tools": tools.clone()
        }));
        assert_eq!(
            &replayed_name(&namespaced_out),
            want,
            "namespaced replay for {namespace}: {namespaced_out}"
        );
    }

    // A call or tool_choice without a namespace carrying the ambiguous name
    // must stay unresolved rather than pick a winner.
    let bare_out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": shared_local, "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": tools.clone()
    }));
    for name in assistant_call_names(&bare_out) {
        assert!(
            name.len() <= NAME_LIMIT,
            "ambiguous replayed name {name:?} (len {}) exceeds 64: {bare_out}",
            name.len()
        );
        assert!(
            !aliases.contains(&name),
            "the ambiguous replayed call resolved to declared alias {name:?}: {bare_out}"
        );
    }
    let bare_forced = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone(),
        "tool_choice": {"type": "function", "function": {"name": shared_local}}
    }));
    let forced_name = text_at(&bare_forced, "tool_choice.function.name");
    assert!(
        !aliases.contains(&forced_name),
        "the ambiguous tool_choice resolved to declared alias {forced_name:?}: {bare_forced}"
    );
    let forced_beta = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools,
        "tool_choice": {"type": "function", "namespace": "mcp__beta", "function": {"name": shared_local}}
    }));
    assert_eq!(
        &text_at(&forced_beta, "tool_choice.function.name"),
        beta_alias,
        "namespaced tool_choice: {forced_beta}"
    );
}

#[test]
fn qualified_identity_outranks_foreign_local_name() {
    // A call or tool_choice carrying an uncut qualified name can match the
    // declaration it qualifies (alpha), and another namespace's child that
    // uses that whole string as its own name (beta). Recovering a local name
    // only guesses at the namespace, so it must not outrank the identity.
    let long_child = format!("read_{}", "f".repeat(60));
    let qualified = raw_qualified_name("alpha_ns", &long_child);
    assert!(
        qualified.len() > NAME_LIMIT,
        "fixture drift: qualified identity {qualified:?} (len {}) must exceed the cap",
        qualified.len()
    );
    assert_eq!(
        cap(&raw_qualified_name("beta_ns", &qualified)),
        cap(&qualified),
        "fixture drift: the two declarations must cap onto the same alias"
    );
    let tools = json!([
        {
            "type": "namespace",
            "name": "alpha_ns",
            "tools": [{"type": "function", "name": long_child, "parameters": {"type": "object"}}]
        },
        {
            "type": "namespace",
            "name": "beta_ns",
            "tools": [{"type": "function", "name": qualified, "parameters": {"type": "object"}}]
        }
    ]);

    let out = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone()
    }));
    let names = tool_names(&out);
    assert_eq!(names.len(), 2, "tools count: {out}");
    let (alpha_alias, beta_alias) = (&names[0], &names[1]);
    assert_ne!(
        alpha_alias, beta_alias,
        "both declarations emitted one name: {out}"
    );
    for (i, name) in names.iter().enumerate() {
        assert!(
            name.len() <= NAME_LIMIT,
            "tools[{i}].function.name {name:?} (len {}) exceeds 64: {out}",
            name.len()
        );
    }

    // The bare qualified name names the alpha declaration.
    let bare_out = convert(json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": qualified, "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": tools.clone()
    }));
    for name in assistant_call_names(&bare_out) {
        assert_eq!(
            &name, alpha_alias,
            "the bare qualified name should resolve to the identity owner's alias (beta's is {beta_alias:?}): {bare_out}"
        );
    }
    let bare_forced = convert(json!({
        "input": [{"role": "user", "content": "hi"}],
        "tools": tools.clone(),
        "tool_choice": {"type": "function", "function": {"name": qualified}}
    }));
    assert_eq!(
        &text_at(&bare_forced, "tool_choice.function.name"),
        alpha_alias,
        "tool_choice bare qualified name: {bare_forced}"
    );

    // Both namespaces stay reachable when the namespace is given.
    for (namespace, name, want) in [
        ("alpha_ns", long_child.as_str(), alpha_alias),
        ("beta_ns", qualified.as_str(), beta_alias),
    ] {
        let namespaced_out = convert(json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "namespace": namespace, "name": name, "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
            "tools": tools.clone()
        }));
        assert_eq!(
            &replayed_name(&namespaced_out),
            want,
            "namespaced replay for {namespace}/{name}: {namespaced_out}"
        );
    }
}

#[test]
fn incomplete_tool_calls_do_not_defer_messages() {
    // Calls a and b with only a's output: the history is incomplete, so the
    // user message in between stays where it is.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"},
            {"role": "user", "content": "reminder before results"},
            {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
        ]
    }));

    assert_eq!(
        roles(&out),
        ["assistant", "user", "tool"],
        "expected untouched order: {out}"
    );
}

#[test]
fn complete_tool_calls_do_pair_messages() {
    // Calls a and b with both outputs after a user reminder: the outputs move
    // to right after the assistant's tool calls.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "call_a", "name": "tool_a", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_b", "name": "tool_b", "arguments": "{}"},
            {"role": "user", "content": "reminder during execution"},
            {"type": "function_call_output", "call_id": "call_b", "output": "result_b"},
            {"type": "function_call_output", "call_id": "call_a", "output": "result_a"}
        ]
    }));

    assert_eq!(
        roles(&out),
        ["assistant", "tool", "tool", "user"],
        "expected order: {out}"
    );
    let messages = messages(&out);
    assert_eq!(
        [
            text_at(&messages[1], "tool_call_id"),
            text_at(&messages[2], "tool_call_id")
        ],
        ["call_b", "call_a"],
        "tool messages should keep their relative input order: {out}"
    );
    assert_eq!(
        [
            text_at(&messages[1], "content"),
            text_at(&messages[2], "content")
        ],
        ["result_b", "result_a"],
        "{out}"
    );
}

#[test]
fn mixed_empty_id_does_not_reorder() {
    // An empty call_id next to a valid one: the ambiguous history stays as is.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "", "name": "unknown", "arguments": "{}"},
            {"type": "function_call", "call_id": "a", "name": "known", "arguments": "{}"},
            {"role": "user", "content": "reminder"},
            {"type": "function_call_output", "call_id": "a", "output": "ok"}
        ]
    }));

    assert_eq!(
        roles(&out),
        ["assistant", "user", "tool"],
        "expected untouched order: {out}"
    );
}

#[test]
fn duplicate_call_id_does_not_reorder() {
    // A repeated call_id: the ambiguous history stays as is.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "dup", "name": "tool_1", "arguments": "{}"},
            {"type": "function_call", "call_id": "dup", "name": "tool_2", "arguments": "{}"},
            {"role": "user", "content": "reminder"},
            {"type": "function_call_output", "call_id": "dup", "output": "ok"}
        ]
    }));

    assert_eq!(
        roles(&out),
        ["assistant", "user", "tool"],
        "expected untouched order: {out}"
    );
}

#[test]
fn duplicate_output_call_id_does_not_reorder() {
    // Two function_call_outputs for one call_id: the ambiguous results are not
    // moved.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "call_dup_out", "name": "tool_a", "arguments": "{}"},
            {"role": "user", "content": "reminder before results"},
            {"type": "function_call_output", "call_id": "call_dup_out", "output": "first"},
            {"type": "function_call_output", "call_id": "call_dup_out", "output": "second"}
        ]
    }));

    let roles = roles(&out);
    assert_eq!(roles.len(), 4, "messages count: {out}");
    assert_eq!(
        roles[..2],
        ["assistant", "user"],
        "the user reminder should stay at index 1: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.content"),
        "reminder before results",
        "{out}"
    );
}

#[test]
fn duplicate_custom_output_call_id_does_not_reorder() {
    // Two custom_tool_call_outputs for one call_id: the ambiguous results are
    // not moved.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "custom_tool_call", "call_id": "custom_dup", "name": "custom_a", "input": "{}"},
            {"role": "user", "content": "reminder before custom results"},
            {"type": "custom_tool_call_output", "call_id": "custom_dup", "output": "output 1"},
            {"type": "custom_tool_call_output", "call_id": "custom_dup", "output": "output 2"}
        ]
    }));

    let roles = roles(&out);
    assert_eq!(roles.len(), 4, "messages count: {out}");
    assert_eq!(
        roles[..2],
        ["assistant", "user"],
        "the user reminder should stay at index 1: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.content"),
        "reminder before custom results",
        "{out}"
    );
}

#[test]
fn multiple_outputs_without_id_do_not_guess_or_reorder() {
    // Calls a and b with several outputs without call_ids: pairing them would
    // be a guess, so the history stays as is.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "a", "name": "unknown_a", "arguments": "{}"},
            {"type": "function_call", "call_id": "b", "name": "unknown_b", "arguments": "{}"},
            {"role": "user", "content": "reminder before results"},
            {"type": "function_call_output", "output": "output X"},
            {"type": "function_call_output", "output": "output Y"}
        ]
    }));

    // The outputs must not get a guessed tool_call_id or become tool messages.
    assert_eq!(
        roles(&out),
        ["assistant", "user", "user", "user"],
        "expected the reminder and the unpaired outputs in order: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.content"),
        "reminder before results",
        "{out}"
    );
    assert_eq!(text_at(&out, "messages.2.content"), "output X", "{out}");
    assert_eq!(text_at(&out, "messages.3.content"), "output Y", "{out}");
}

#[test]
fn multiple_outputs_without_id_and_orphan_output_do_not_guess_or_reorder() {
    // Calls a and b, a reminder, two outputs without IDs and one naming an
    // unknown call: the orphan must not make the others be guessed and moved.
    let out = convert(json!({
        "model": "deepseek-v4.1-flash",
        "input": [
            {"type": "function_call", "call_id": "a", "name": "tool_a", "arguments": "{}"},
            {"type": "function_call", "call_id": "b", "name": "tool_b", "arguments": "{}"},
            {"role": "user", "content": "reminder before results"},
            {"type": "function_call_output", "output": "output X"},
            {"type": "function_call_output", "output": "output Y"},
            {"type": "function_call_output", "call_id": "orphan_id", "output": "output Z"}
        ]
    }));

    let roles = roles(&out);
    assert_eq!(roles.len(), 5, "messages count: {out}");
    assert_eq!(
        roles[..2],
        ["assistant", "user"],
        "the user reminder should stay at index 1: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.content"),
        "reminder before results",
        "{out}"
    );
    assert!(
        !roles.iter().any(|role| role == "tool"),
        "unexpected tool message created by guessing: {out}"
    );
}

#[test]
fn maps_max_output_tokens_to_max_tokens() {
    let out = convert(json!({
        "model": "gpt-5.4",
        "input": "hello",
        "max_output_tokens": 1024
    }));
    assert_eq!(int_of(&out["max_tokens"]), 1024, "max_tokens: {out}");
    assert!(
        out.get("max_completion_tokens").is_none(),
        "max_completion_tokens should be absent: {out}"
    );

    let out = convert(json!({
        "model": "gpt-5.4",
        "input": "hello"
    }));
    assert!(
        out.get("max_completion_tokens").is_none(),
        "max_completion_tokens should be absent when omitted: {out}"
    );
    assert!(
        out.get("max_tokens").is_none(),
        "max_tokens should be absent when omitted: {out}"
    );

    let out = convert(json!({
        "model": "gpt-5.4",
        "input": "hello",
        "max_output_tokens": null
    }));
    assert_eq!(
        out.get("max_tokens"),
        Some(&Value::Null),
        "max_tokens should be null: {out}"
    );
    assert!(
        out.get("max_completion_tokens").is_none(),
        "max_completion_tokens should be absent: {out}"
    );
}

#[test]
fn namespace_tool_prefix_collision() {
    let cases = [
        ("fs", "fs_read", "fs__fs_read"),
        ("collab", "collaboration", "collab__collaboration"),
        ("fs", "fs__read", "fs__read"),
        ("fs", "fs", "fs"),
        ("fs__", "read", "fs__read"),
        ("mcp__node_repl", "mcp__node_repl__js", "mcp__node_repl__js"),
    ];
    for (namespace, child, want) in cases {
        assert_eq!(
            raw_qualified_name(namespace, child),
            want,
            "raw_qualified_name({namespace:?}, {child:?})"
        );
    }

    let request = json!({
        "model": "gpt-5.4",
        "tools": [
            {"type": "function", "name": "fs_read", "parameters": {"type": "object"}},
            {"type": "namespace", "name": "fs", "tools": [{"type": "function", "name": "fs_read", "parameters": {"type": "object"}}]}
        ],
        "input": []
    });
    let out = convert(request.clone());
    assert_eq!(
        tool_names(&out),
        ["fs_read", "fs__fs_read"],
        "emitted tool names: {out}"
    );

    let names = ToolNames::new(&request);
    assert_eq!(split(&names, "fs__fs_read"), ("fs_read", "fs"));
    assert_eq!(split(&names, "fs_read"), ("fs_read", ""));
}

#[test]
fn apply_patch_chat_request_contract_and_history() {
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch";
    let out = convert(json!({
        "tools": [{
            "type": "namespace",
            "name": "editor",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"},
                "cache_control": {"type": "ephemeral"}
            }]
        }],
        "input": [
            {"type": "custom_tool_call", "namespace": "editor", "name": "apply_patch", "call_id": "c1", "input": patch},
            {"type": "custom_tool_call_output", "call_id": "c1", "output": "done"}
        ]
    }));

    let tool = &out["tools"][0]["function"];
    let description = text_at(tool, "description");
    let schema = &tool["parameters"];
    for instruction in PATCH_INSTRUCTIONS {
        assert!(
            description.contains(instruction),
            "missing instruction {instruction:?}: {description}"
        );
    }
    assert!(
        !description.contains("do not wrap the patch in JSON"),
        "contradictory freeform instructions: {description}"
    );
    assert!(
        schema.get("additionalProperties").is_some()
            && !bool_of(&schema["additionalProperties"])
            && schema["required"][0] == "input",
        "not strict schema: {schema}"
    );
    let call = &out["messages"][0]["tool_calls"][0];
    let arguments = parse(&text_at(call, "function.arguments"));
    assert_eq!(
        text_at(call, "function.name"),
        "editor__apply_patch",
        "history mismatch: {out}"
    );
    assert_eq!(
        text_at(&arguments, "input"),
        patch,
        "history mismatch: {out}"
    );
    assert_eq!(text_at(call, "id"), "c1", "history mismatch: {out}");
    assert_eq!(
        text_at(&out, "messages.1.tool_call_id"),
        "c1",
        "history mismatch: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.content"),
        "done",
        "history mismatch: {out}"
    );
}

// Ported from custom_tool_namespace_recovery_test.go
// (TestCustomToolReplayPreservesNamespaceAndResultPair).
#[test]
fn custom_tool_replay_preserves_namespace_and_result_pair() {
    for namespace in [Some("functions"), None] {
        let mut request = parse(NAMESPACE_RECOVERY_REQUEST);
        let mut call = json!({"type": "custom_tool_call", "name": "exec"});
        if let Some(namespace) = namespace {
            call["namespace"] = json!(namespace);
        }
        call["call_id"] = json!("call_fixture");
        call["input"] = json!("text(1);");
        let input = request["input"].as_array_mut().unwrap();
        input.push(call);
        input.push(json!({
            "type": "custom_tool_call_output",
            "call_id": "call_fixture",
            "output": [{"type": "input_text", "text": "1"}]
        }));

        let out = convert(request);
        assert_eq!(
            text_at(&out, "messages.0.tool_calls.0.function.name"),
            "functions__exec",
            "replay name ({namespace:?}): {out}"
        );
        assert_eq!(
            text_at(&out, "messages.1.tool_call_id"),
            "call_fixture",
            "result pair lost ({namespace:?}): {out}"
        );
    }
}

// Ported from custom_tool_namespace_recovery_test.go
// (TestNamespaceRecoveryDoesNotGuessAmbiguousOrOverrideExactNames);
// canonicalResponsesToolName is ToolIndex::canonical_name.
#[test]
fn namespace_recovery_does_not_guess_ambiguous_or_override_exact_names() {
    let cases = [
        (NAMESPACE_RECOVERY_REQUEST, "wait", "functions__wait"),
        (NAMESPACE_RECOVERY_REQUEST, "unknown", "unknown"),
        (
            r#"{"tools":[{"type":"function","name":"exec"},{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]}"#,
            "exec",
            "exec",
        ),
        (
            r#"{"tools":[{"type":"namespace","name":"first","tools":[{"type":"custom","name":"exec"}]},{"type":"namespace","name":"second","tools":[{"type":"custom","name":"exec"}]}]}"#,
            "exec",
            "exec",
        ),
    ];
    for (request, name, want) in cases {
        let request = parse(request);
        assert_eq!(
            ToolIndex::new(&request).canonical_name(name),
            want,
            "{name} in {request}"
        );
    }
}

// Ported from openai_openai-responses_video_test.go.
#[test]
fn video_input() {
    let cases = [
        (
            "remote URL",
            r#"{"type":"input_video","video_url":"https://example.com/clip.mp4?part=1&name=a%20b"}"#,
            r#"{"type":"video_url","video_url":{"url":"https://example.com/clip.mp4?part=1&name=a%20b"}}"#,
        ),
        (
            "base64 data URL",
            r#"{"type":"input_video","video_url":"data:video/mp4;base64,AAECAwQ="}"#,
            r#"{"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAECAwQ="}}"#,
        ),
        (
            "processing mode",
            r#"{"type":"input_video","video_url":"https://example.com/clip.webm","processing":"agentic"}"#,
            r#"{"type":"video_url","video_url":{"url":"https://example.com/clip.webm","processing":"agentic"}}"#,
        ),
        (
            "object video URL",
            r#"{"type":"input_video","video_url":{"url":"https://example.com/clip.mp4","processing":"static"}}"#,
            r#"{"type":"video_url","video_url":{"url":"https://example.com/clip.mp4","processing":"static"}}"#,
        ),
        (
            "chat video part in Responses content",
            r#"{"type":"video_url","video_url":{"url":"data:video/webm;base64,AAECAwQ=","processing":"static"}}"#,
            r#"{"type":"video_url","video_url":{"url":"data:video/webm;base64,AAECAwQ=","processing":"static"}}"#,
        ),
        (
            "top-level processing overrides object processing",
            r#"{"type":"input_video","video_url":{"url":"https://example.com/clip.mp4","processing":"static"},"processing":"agentic"}"#,
            r#"{"type":"video_url","video_url":{"url":"https://example.com/clip.mp4","processing":"agentic"}}"#,
        ),
        (
            "missing URL remains a video for upstream validation",
            r#"{"type":"input_video"}"#,
            r#"{"type":"video_url","video_url":{}}"#,
        ),
        (
            "invalid URL is not coerced to a string",
            r#"{"type":"input_video","video_url":123}"#,
            r#"{"type":"video_url","video_url":{"url":123}}"#,
        ),
    ];

    for (name, part, want) in cases {
        for stream in [false, true] {
            let request = json!({"input": [{"role": "user", "content": [parse(part)]}]});
            let out = convert_openai_responses_request_to_openai_chat_completions(
                "video-model",
                &request,
                stream,
            );
            let content = items(&out["messages"][0]["content"]);
            assert_eq!(
                content.len(),
                1,
                "{name}/stream={stream}: video content was lost: {out}"
            );
            assert_eq!(
                content[0],
                parse(want),
                "{name}/stream={stream}: video part"
            );
        }
    }
}

// Ported from openai_openai-responses_video_test.go.
#[test]
fn mixed_video_input_order() {
    let out = convert(
        json!({"input": [{"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "Compare these clips and this image."},
            {"type": "input_video", "video_url": "https://example.com/first.mp4"},
            {"type": "input_image", "image_url": "https://example.com/frame.png", "detail": "low"},
            {"type": "input_video", "video_url": "data:video/mp4;base64,AAECAwQ=", "processing": "static"},
            {"type": "input_text", "text": "Describe the differences."}
        ]}]}),
    );

    let want = json!([
        {"type": "text", "text": "Compare these clips and this image."},
        {"type": "video_url", "video_url": {"url": "https://example.com/first.mp4"}},
        {"type": "image_url", "image_url": {"url": "https://example.com/frame.png", "detail": "low"}},
        {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAECAwQ=", "processing": "static"}},
        {"type": "text", "text": "Describe the differences."}
    ]);
    assert_eq!(
        out["messages"][0]["content"], want,
        "mixed content order or media changed: {out}"
    );
}

// From responses_compatibility_digest_test.go: hashes the requests'
// conversions and the response stream each gives a call to each of a few
// names.
#[test]
fn responses_compatibility_digest() {
    let requests = [
        responses_perf_request(0),
        responses_perf_request(10),
        responses_perf_request(100),
        parse(
            r#"{"model":"test","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"},{"type":"function","name":"read"}]}],"input":[]}"#,
        ),
        parse(
            r#"{"model":"test","tools":[{"type":"function","name":"editor__patch"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"patch"}]}]}]}"#,
        ),
        json!({
            "model": "test",
            "tools": [
                {"type": "namespace", "name": "namespace".repeat(10), "tools": [{"type": "function", "name": "read"}]},
                {"type": "namespace", "name": "other", "tools": [{"type": "function", "name": "read"}]}
            ],
            "input": []
        }),
    ];

    let mut hash = Sha256::new();
    for request in &requests {
        let out =
            convert_openai_responses_request_to_openai_chat_completions("test", request, true);
        hash.update(out.to_string().as_bytes());
        for name in ["editor__read", "read", "editor__patch", "patch", "unknown"] {
            let mut stream = OpenAIToOpenAIResponsesStream::new("test", request, request);
            let chunks = [
                r#"{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"content":"hello"}}]}"#.to_owned(),
                format!(
                    r#"{{"id":"r-test","created":1,"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call-test","function":{{"name":{},"arguments":""}}}}]}}}}]}}"#,
                    Value::from(name)
                ),
                r#"{"id":"r-test","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"hello\"}"}}]}}]}"#.to_owned(),
                r#"{"id":"r-test","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}"#.to_owned(),
                "[DONE]".to_owned(),
            ];
            for chunk in chunks {
                let events = stream.translate_line(format!("data: {chunk}").as_bytes());
                hash.update(events.as_bytes());
            }
        }
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        digest, "be3fc19eade4e6aff373fdfdd0192d586b5aa83e01a6b22884456396443f8411",
        "compatibility digest"
    );
}

// The v8.0.20 tests of openai_openai-responses_user_turn_test.go.

const CHAT_TURN_HELLO: &str =
    r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}"#;
const CHAT_TURN_ASSISTANT: &str =
    r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}"#;
const CHAT_TURN_NEXT: &str =
    r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}"#;
const CHAT_TURN_DEVELOPER: &str =
    r#"{"type":"message","role":"developer","content":[{"type":"input_text","text":"dev"}]}"#;
const CHAT_TURN_FILE_ID: &str = r#"{"type":"input_file","file_id":"file-1"}"#;
const CHAT_TURN_FILE_DATA: &str = r#"{"type":"input_file","filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}"#;
const CHAT_TURN_FILE_URL: &str = r#"{"type":"input_file","file_url":"https://example.test/a.pdf"}"#;
const CHAT_TURN_AUDIO: &str =
    r#"{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}"#;
const CHAT_TURN_NO_AUDIO: &str = r#"{"type":"input_audio","input_audio":{"format":"wav"}}"#;
const CHAT_TURN_TEXT: &str = r#"{"type":"input_text","text":"keep me"}"#;
const CHAT_TURN_EMPTY_TEXT: &str = r#"{"type":"input_text","text":""}"#;

/// `chatUserTurn`: a user message holding `parts`.
fn chat_user_turn(parts: &[&str]) -> String {
    format!(
        r#"{{"type":"message","role":"user","content":[{}]}}"#,
        parts.join(",")
    )
}

/// `chatPayload`: a request with `items` as its input, and `instructions`
/// if given.
fn chat_payload(instructions: &str, items: &[&str]) -> String {
    let mut prefix = r#"{"model":"gpt-5","#.to_owned();
    if !instructions.is_empty() {
        prefix += &format!(r#""instructions":"{instructions}","#);
    }
    format!(r#"{prefix}"input":[{}]}}"#, items.join(","))
}

/// The translator's body and refusal for `input`, after checking that the
/// registration gives the same refusal.
fn checked_request(input: &str) -> (Value, Option<UnsupportedPartError>) {
    let body: Value = serde_json::from_str(input).expect("test request is JSON");
    let registered = crate::registry::Registry::global().translate_request_checked(
        &"openai-response".into(),
        &"openai".into(),
        "gpt-5",
        body.clone(),
        false,
    );
    let (out, err) =
        super::convert_openai_responses_request_to_openai_chat_completions("gpt-5", &body, false);
    assert_eq!(registered.err(), err, "the registration's refusal");
    (out, err)
}

// TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_RefusesAnyEmptiedUserTurn
#[test]
fn refuses_any_emptied_user_turn() {
    for (name, input, want) in [
        (
            "history then file url only",
            chat_payload(
                "",
                &[
                    CHAT_TURN_HELLO,
                    CHAT_TURN_ASSISTANT,
                    &chat_user_turn(&[CHAT_TURN_FILE_URL]),
                ],
            ),
            "input_file",
        ),
        (
            "audio without bytes",
            chat_payload(
                "",
                &[
                    CHAT_TURN_HELLO,
                    CHAT_TURN_ASSISTANT,
                    &chat_user_turn(&[CHAT_TURN_NO_AUDIO]),
                ],
            ),
            "input_audio",
        ),
        (
            "instructions and developer prompt do not hide the empty turn",
            chat_payload(
                "sys",
                &[CHAT_TURN_DEVELOPER, &chat_user_turn(&[CHAT_TURN_FILE_URL])],
            ),
            "input_file",
        ),
        (
            "emptied turn before a later text turn",
            chat_payload(
                "",
                &[
                    &chat_user_turn(&[CHAT_TURN_FILE_URL]),
                    CHAT_TURN_ASSISTANT,
                    CHAT_TURN_NEXT,
                ],
            ),
            "input_file",
        ),
        (
            "empty text beside the unsendable file does not count as sendable",
            chat_payload(
                "",
                &[&chat_user_turn(&[CHAT_TURN_EMPTY_TEXT, CHAT_TURN_FILE_URL])],
            ),
            "input_file",
        ),
    ] {
        let (body, err) = checked_request(&input);
        let err = err.unwrap_or_else(|| panic!("{name}: no refusal; body = {body}"));
        assert_eq!(err.part_type, want, "{name}");
        assert_eq!(err.status_code(), 400);
        assert_eq!(err.to_string(), format!("unsupported content part: {want}"));
        assert!(body.is_object(), "{name}: {body}");
    }
}

// TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MapsFilesAndAudio
#[test]
fn maps_files_and_audio() {
    for (name, part, want) in [
        (
            "file id",
            CHAT_TURN_FILE_ID,
            json!({"type": "file", "file": {"file_id": "file-1"}}),
        ),
        (
            "file data",
            CHAT_TURN_FILE_DATA,
            json!({"type": "file", "file": {"file_data": "data:application/pdf;base64,JVBERi0xLjQK", "filename": "a.pdf"}}),
        ),
        (
            "audio",
            CHAT_TURN_AUDIO,
            json!({"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}),
        ),
    ] {
        let (body, err) = checked_request(&chat_payload(
            "",
            &[
                CHAT_TURN_HELLO,
                CHAT_TURN_ASSISTANT,
                &chat_user_turn(&[part]),
            ],
        ));
        assert_eq!(err, None, "{name}: {body}");
        assert_eq!(body["messages"][2]["content"][0], want, "{name}: {body}");
    }
}

// TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_KeepsTurnWithTextBesideAttachment
#[test]
fn keeps_turn_with_text_beside_attachment() {
    for (name, attachment) in [
        ("file url", CHAT_TURN_FILE_URL),
        ("audio without bytes", CHAT_TURN_NO_AUDIO),
    ] {
        let (body, err) = checked_request(&chat_payload(
            "",
            &[
                CHAT_TURN_HELLO,
                CHAT_TURN_ASSISTANT,
                &chat_user_turn(&[CHAT_TURN_TEXT, attachment]),
            ],
        ));
        assert_eq!(err, None, "{name}: {body}");
        assert_eq!(
            body["messages"][2]["content"][0]["text"], "keep me",
            "{name}: {body}"
        );
    }
}

// TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_MappedAttachmentBesideTextKeepsBoth
#[test]
fn mapped_attachment_beside_text_keeps_both() {
    let (body, err) = checked_request(&chat_payload(
        "",
        &[&chat_user_turn(&[
            CHAT_TURN_TEXT,
            CHAT_TURN_FILE_ID,
            CHAT_TURN_AUDIO,
        ])],
    ));
    assert_eq!(err, None, "{body}");
    assert_eq!(
        items(&body["messages"][0]["content"]).len(),
        3,
        "want text, file and audio: {body}"
    );
}

// TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_ExportedWrapperKeepsAJSONBody
#[test]
fn refusal_keeps_a_json_body() {
    let (body, err) = checked_request(&chat_payload("", &[&chat_user_turn(&[CHAT_TURN_FILE_URL])]));
    assert!(err.is_some() && body.is_object(), "{body}");
}

// Not upstream's: user turns in detail. Only a message whose role is
// exactly `user` is a user turn; a message without a role, a developer's
// and the assistant's aren't. A file id that isn't a string is written as
// text, a file's URL is left out beside its bytes, and audio's bytes and
// format may sit on the part itself; a blank but not empty audio string is
// sent. Unknown parts are dropped without a refusal, and the first emptied
// turn is the one named. The expected output comes from upstream.
#[test]
fn user_turns_in_detail() {
    for (input, want_body, want_err) in [
        (
            r#"{"input":[{"type":"message","content":[{"type":"input_file","file_url":"u"}]},{"type":"","role":"user","content":[{"type":"input_file","file_id":5},{"type":"input_audio","data":"QQ==","format":"mp3"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"","content":[]},{"role":"user","content":[{"type":"file","file":{"file_id":"5"}},{"type":"input_audio","input_audio":{"data":"QQ==","format":"mp3"}}]}],"stream":true}"#,
            None,
        ),
        (
            r#"{"input":[{"role":"developer","content":[{"type":"input_file","file_url":"u"}]},{"role":"user","content":[{"type":"input_audio","input_audio":{"data":" "}}]},{"role":"user","content":[{"type":"input_audio","input_audio":{"data":""},"data":""}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[]},{"role":"user","content":[{"type":"input_audio","input_audio":{"data":" "}}]},{"role":"user","content":[]}],"stream":true}"#,
            Some("input_audio"),
        ),
        (
            r#"{"input":[{"role":"user","content":[{"text":""},{"type":"input_file","file_url":"u"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":""}]}],"stream":true}"#,
            Some("input_file"),
        ),
        (
            r#"{"input":[{"role":"user","content":""},{"role":"user","content":[{"type":"input_audio"}]},{"role":"user","content":[{"type":"input_file"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":""},{"role":"user","content":[]},{"role":"user","content":[]}],"stream":true}"#,
            Some("input_audio"),
        ),
        (
            r#"{"input":[{"role":"assistant","content":[{"type":"input_file","file_url":"u"}]},{"role":"user","content":[{"type":"input_foo"}]},{"role":"user","content":[{"type":"input_video"}]},{"role":"user","content":[{"type":"input_image"}]},{"role":"User","content":[{"type":"input_file"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"assistant","content":[]},{"role":"user","content":[]},{"role":"user","content":[{"type":"video_url","video_url":{}}]},{"role":"user","content":[{"type":"image_url","image_url":{"url":""}}]},{"role":"User","content":[]}],"stream":true}"#,
            None,
        ),
        (
            r#"{"input":[{"role":"user","content":[{"type":"input_file","file_data":"data:text/plain;base64,aGk=","filename":"a.txt","file_url":"u"},{"type":"input_audio","input_audio":{"data":"QQ==","format":""},"format":"wav"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"file","file":{"file_data":"data:text/plain;base64,aGk=","filename":"a.txt"}},{"type":"input_audio","input_audio":{"data":"QQ==","format":"wav"}}]}],"stream":true}"#,
            None,
        ),
        (
            r#"{"input":[{"role":"user","content":[{"type":"output_text","text":""},{"type":"input_audio","input_audio":"x"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":""}]}],"stream":true}"#,
            Some("input_audio"),
        ),
    ] {
        let body: Value = serde_json::from_str(input).expect("test request is JSON");
        let (out, err) =
            super::convert_openai_responses_request_to_openai_chat_completions("m", &body, true);
        assert_eq!(out.to_string(), want_body, "{input}");
        assert_eq!(
            err.map(|err| err.part_type),
            want_err.map(str::to_owned),
            "{input}"
        );
    }
}
