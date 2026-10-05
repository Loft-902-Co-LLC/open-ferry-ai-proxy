// Ported from CLIProxyAPI internal/translator/codex/openai/chat-completions/codex_openai_request_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;
use crate::codex::openai::chat_completions::convert_codex_response_to_openai_chat_completions_non_stream;
use crate::json::bool_of;

/// Looks up a dotted path such as `input.0.role`, like a plain gjson path.
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

/// The value at `path` as gjson's `Bool()` would return it.
fn bool_at(value: &Value, path: &str) -> bool {
    at(value, path).is_some_and(bool_of)
}

/// The value at `path` as gjson's `Array()` would return it: an array's items,
/// nothing for a missing or null value, or else the value alone.
fn array_at<'v>(value: &'v Value, path: &str) -> Vec<&'v Value> {
    match at(value, path) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// Codex tool name → the client's tool name, as upstream's
/// `buildReverseMapFromOriginalOpenAI` builds it for the response translator.
fn build_reverse_map_from_original_openai(request: &Value) -> HashMap<String, String> {
    build_short_name_map(&collect_request_tool_names(request))
        .into_iter()
        .map(|(client, codex)| (codex, client))
        .collect()
}

/// Whether `name` matches `^[a-zA-Z0-9_-]+$`.
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// System, user, a tool-call-only assistant message and its tool result become
// a developer message, a user message, a function_call and its output, with no
// empty assistant message in between.
#[test]
fn tool_call_simple() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "What is the weather in Paris?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": r#"{"city":"Paris"}"#
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": "sunny, 22C"
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get weather for a city",
                        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 4, "expected 4 input items: {out}");

    assert_eq!(text_at(items[0], "type"), "message", "item 0: {out}");
    assert_eq!(text_at(items[0], "role"), "developer", "item 0: {out}");

    assert_eq!(text_at(items[1], "type"), "message", "item 1: {out}");
    assert_eq!(text_at(items[1], "role"), "user", "item 1: {out}");

    assert_eq!(text_at(items[2], "type"), "function_call", "item 2: {out}");
    assert_eq!(text_at(items[2], "call_id"), "call_1", "item 2: {out}");
    assert_eq!(text_at(items[2], "name"), "get_weather", "item 2: {out}");
    assert_eq!(
        text_at(items[2], "arguments"),
        r#"{"city":"Paris"}"#,
        "item 2: {out}"
    );

    assert_eq!(
        text_at(items[3], "type"),
        "function_call_output",
        "item 3: {out}"
    );
    assert_eq!(text_at(items[3], "call_id"), "call_1", "item 3: {out}");
    assert_eq!(text_at(items[3], "output"), "sunny, 22C", "item 3: {out}");
}

// An assistant message with text and tool calls keeps its message, followed by
// the function_call items.
#[test]
fn tool_call_with_content() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "What is the weather?"},
                {
                    "role": "assistant",
                    "content": "Let me check the weather for you.",
                    "tool_calls": [
                        {
                            "id": "call_abc",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc",
                    "content": "rainy, 15C"
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get weather",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    // user + assistant (with content) + function_call + function_call_output
    assert_eq!(items.len(), 4, "expected 4 input items: {out}");

    assert_eq!(text_at(items[0], "role"), "user", "item 0: {out}");

    assert_eq!(text_at(items[1], "type"), "message", "item 1: {out}");
    assert_eq!(text_at(items[1], "role"), "assistant", "item 1: {out}");
    assert!(
        !array_at(items[1], "content").is_empty(),
        "item 1: assistant message should have content parts: {out}"
    );

    assert_eq!(text_at(items[2], "type"), "function_call", "item 2: {out}");
    assert_eq!(text_at(items[2], "call_id"), "call_abc", "item 2: {out}");

    assert_eq!(
        text_at(items[3], "type"),
        "function_call_output",
        "item 3: {out}"
    );
    assert_eq!(text_at(items[3], "call_id"), "call_abc", "item 3: {out}");
}

#[test]
fn tool_call_output_with_multimodal_content() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Show me the generated result."},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_output_1",
                            "type": "function",
                            "function": {"name": "render_output", "arguments": "{}"}
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_output_1",
                    "content": [
                        {"type":"text","text":"Rendered result attached."},
                        {"type":"image_url","image_url":{"url":"https://example.com/generated.png","detail":"high"}},
                        {"type":"image_url","image_url":{"file_id":"file-img-123"}},
                        {"type":"file","file":{"file_id":"file-doc-123","filename":"doc.pdf"}},
                        {"type":"file","file":{"file_data":"SGVsbG8=","filename":"inline.txt"}},
                        {"type":"file","file":{"file_url":"https://example.com/report.pdf","filename":"report.pdf"}}
                    ]
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {"name": "render_output", "description": "Render output", "parameters": {"type": "object", "properties": {}}}
                }
            ]
        }),
        true,
    );

    assert!(
        at(&out, "input.2.output").is_some_and(Value::is_array),
        "expected tool output to be an array: {out}"
    );
    let parts = array_at(&out, "input.2.output");
    assert_eq!(parts.len(), 6, "expected 6 output parts: {out}");
    assert!(
        text_at(parts[0], "type") == "input_text"
            && text_at(parts[0], "text") == "Rendered result attached.",
        "part 0: expected input_text with rendered text, got {}",
        parts[0]
    );
    assert_eq!(text_at(parts[1], "type"), "input_image", "part 1: {out}");
    assert_eq!(
        text_at(parts[1], "image_url"),
        "https://example.com/generated.png",
        "part 1: {out}"
    );
    assert_eq!(text_at(parts[1], "detail"), "high", "part 1: {out}");
    assert!(
        text_at(parts[2], "type") == "input_image"
            && text_at(parts[2], "file_id") == "file-img-123",
        "part 2: expected file_id-backed input_image, got {}",
        parts[2]
    );
    assert!(
        text_at(parts[3], "type") == "input_file" && text_at(parts[3], "file_id") == "file-doc-123",
        "part 3: expected file_id-backed input_file, got {}",
        parts[3]
    );
    assert_eq!(text_at(parts[3], "filename"), "doc.pdf", "part 3: {out}");
    assert!(
        text_at(parts[4], "type") == "input_file" && text_at(parts[4], "file_data") == "SGVsbG8=",
        "part 4: expected file_data-backed input_file, got {}",
        parts[4]
    );
    assert!(
        text_at(parts[5], "type") == "input_file"
            && text_at(parts[5], "file_url") == "https://example.com/report.pdf",
        "part 5: expected file_url-backed input_file, got {}",
        parts[5]
    );
}

#[test]
fn tool_call_output_with_stringified_image_content() {
    struct Case {
        name: &'static str,
        /// The tool message's `content`, as JSON text.
        content: &'static str,
        image_index: usize,
        expected_url: &'static str,
        expected_text: Option<&'static str>,
        detail: &'static str,
    }
    let cases = [
        Case {
            name: "Codex input image",
            content: r#""[{\"type\":\"input_text\",\"text\":\"Captured screenshot.\"},{\"detail\":\"original\",\"image_url\":\"data:image/png;base64,AA==\",\"type\":\"input_image\"}]""#,
            image_index: 1,
            expected_url: "data:image/png;base64,AA==",
            expected_text: Some("Captured screenshot."),
            detail: "original",
        },
        Case {
            name: "OpenAI image URL",
            content: r#""[{\"type\":\"image_url\",\"image_url\":{\"url\":\"https://example.com/generated.png\",\"detail\":\"high\"}}]""#,
            image_index: 0,
            expected_url: "https://example.com/generated.png",
            expected_text: None,
            detail: "high",
        },
    ];

    for case in cases {
        let name = case.name;
        let content: Value = serde_json::from_str(case.content).unwrap();
        let out = convert_openai_chat_completions_request_to_codex(
            "gpt-5.6-sol",
            &json!({
                "model": "gpt-5.6-sol",
                "messages": [
                    {"role": "user", "content": "Inspect the screenshot."},
                    {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {"id": "call_screenshot", "type": "function", "function": {"name": "view_image", "arguments": "{}"}}
                        ]
                    },
                    {
                        "role": "tool",
                        "tool_call_id": "call_screenshot",
                        "content": content
                    }
                ],
                "tools": [
                    {"type": "function", "function": {"name": "view_image", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            true,
        );

        assert!(
            at(&out, "input.2.output").is_some_and(Value::is_array),
            "{name}: expected stringified image output to be an array: {out}"
        );
        let parts = array_at(&out, "input.2.output");
        assert!(
            parts.len() > case.image_index,
            "{name}: expected image part at index {}: {out}",
            case.image_index
        );
        let image = parts[case.image_index];
        assert_eq!(text_at(image, "type"), "input_image", "{name}: {out}");
        assert_eq!(
            text_at(image, "image_url"),
            case.expected_url,
            "{name}: {out}"
        );
        assert_eq!(text_at(image, "detail"), case.detail, "{name}: {out}");
        if let Some(expected_text) = case.expected_text {
            assert!(
                text_at(parts[0], "type") == "input_text"
                    && text_at(parts[0], "text") == expected_text,
                "{name}: expected input_text {expected_text:?}, got {}",
                parts[0]
            );
        }
    }
}

#[test]
fn tool_call_output_keeps_non_image_strings() {
    // (name, the tool message's content as JSON text, expected output)
    let cases = [
        ("plain text", r#""plain output""#, "plain output"),
        (
            "JSON object",
            r#""{\"status\":\"ok\"}""#,
            r#"{"status":"ok"}"#,
        ),
        (
            "text-only array",
            r#""[{\"type\":\"input_text\",\"text\":\"still text\"}]""#,
            r#"[{"type":"input_text","text":"still text"}]"#,
        ),
        (
            "invalid image array",
            r#""[{\"type\":\"input_image\",\"detail\":\"low\"}]""#,
            r#"[{"type":"input_image","detail":"low"}]"#,
        ),
    ];

    for (name, content, expected_output) in cases {
        let content: Value = serde_json::from_str(content).unwrap();
        let out = convert_openai_chat_completions_request_to_codex(
            "gpt-5.6-sol",
            &json!({
                "model": "gpt-5.6-sol",
                "messages": [
                    {"role": "user", "content": "Check tool output."},
                    {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {"id": "call_output", "type": "function", "function": {"name": "inspect", "arguments": "{}"}}
                        ]
                    },
                    {"role": "tool", "tool_call_id": "call_output", "content": content}
                ],
                "tools": [
                    {"type": "function", "function": {"name": "inspect", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            true,
        );

        assert!(
            matches!(at(&out, "input.2.output"), Some(Value::String(_))),
            "{name}: expected output to remain a string: {out}"
        );
        assert_eq!(
            text_at(&out, "input.2.output"),
            expected_output,
            "{name}: {out}"
        );
    }
}

#[test]
fn tool_call_output_falls_back_for_invalid_structured_parts() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Check tool output."},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id": "call_invalid_parts", "type": "function", "function": {"name": "inspect", "arguments": "{}"}}
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_invalid_parts",
                    "content": [
                        {"type":"image_url","image_url":{"detail":"low"}},
                        {"type":"file","file":{"filename":"orphan.txt"}},
                        {"type":"unknown_type","foo":"bar","nested":{"a":1}}
                    ]
                }
            ],
            "tools": [
                {"type": "function", "function": {"name": "inspect", "description": "Inspect", "parameters": {"type": "object", "properties": {}}}}
            ]
        }),
        true,
    );

    let parts = array_at(&out, "input.2.output");
    assert_eq!(parts.len(), 3, "expected 3 output parts: {out}");

    let expected_fallbacks = [
        r#"{"type":"image_url","image_url":{"detail":"low"}}"#,
        r#"{"type":"file","file":{"filename":"orphan.txt"}}"#,
        r#"{"type":"unknown_type","foo":"bar","nested":{"a":1}}"#,
    ];
    for (i, expected_fallback) in expected_fallbacks.into_iter().enumerate() {
        assert_eq!(
            text_at(parts[i], "type"),
            "input_text",
            "part {i}: expected input_text fallback: {out}"
        );
        assert_eq!(
            text_at(parts[i], "text"),
            expected_fallback,
            "part {i}: {out}"
        );
    }
}

#[test]
fn tool_call_output_with_non_string_json_content() {
    // (name, the tool message's content as JSON text, expected output)
    let cases = [
        ("null", "null", "null"),
        (
            "object",
            r#"{"status":"ok","count":2}"#,
            r#"{"status":"ok","count":2}"#,
        ),
    ];

    for (name, content, expected_output) in cases {
        let content: Value = serde_json::from_str(content).unwrap();
        let out = convert_openai_chat_completions_request_to_codex(
            "gpt-4o",
            &json!({
                "model": "gpt-4o",
                "messages": [
                    {"role": "user", "content": "Check tool output."},
                    {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {"id": "call_json", "type": "function", "function": {"name": "inspect", "arguments": "{}"}}
                        ]
                    },
                    {
                        "role": "tool",
                        "tool_call_id": "call_json",
                        "content": content
                    }
                ],
                "tools": [
                    {"type": "function", "function": {"name": "inspect", "description": "Inspect", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            true,
        );

        assert!(
            at(&out, "input.2.output").is_some(),
            "{name}: expected output field to exist: {out}"
        );
        assert_eq!(
            text_at(&out, "input.2.output"),
            expected_output,
            "{name}: {out}"
        );
    }
}

#[test]
fn convert_openai_request_to_codex_preserves_input_audio() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.5",
        &json!({
            "model": "gpt-5.5",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Transcribe this audio verbatim."},
                        {"type": "input_audio", "input_audio": {"data": "SUQzBA==", "format": "mp3"}}
                    ]
                }
            ]
        }),
        true,
    );

    let parts = array_at(&out, "input.0.content");
    assert_eq!(parts.len(), 2, "expected 2 content parts: {out}");
    assert!(
        text_at(parts[0], "type") == "input_text"
            && text_at(parts[0], "text") == "Transcribe this audio verbatim.",
        "part 0: expected input_text with prompt text, got {}",
        parts[0]
    );
    assert_eq!(text_at(parts[1], "type"), "input_audio", "part 1: {out}");
    assert_eq!(text_at(parts[1], "data"), "SUQzBA==", "part 1: {out}");
    assert_eq!(text_at(parts[1], "format"), "mp3", "part 1: {out}");
}

// Three parallel tool calls keep their call IDs, and each output pairs with its
// call.
#[test]
fn multiple_tool_calls() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Compare weather in Paris, London and Tokyo"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_paris",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": r#"{"city":"Paris"}"#
                            }
                        },
                        {
                            "id": "call_london",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": r#"{"city":"London"}"#
                            }
                        },
                        {
                            "id": "call_tokyo",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": r#"{"city":"Tokyo"}"#
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_paris", "content": "sunny, 22C"},
                {"role": "tool", "tool_call_id": "call_london", "content": "cloudy, 14C"},
                {"role": "tool", "tool_call_id": "call_tokyo", "content": "humid, 28C"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get weather",
                        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    // user + 3 function_call + 3 function_call_output
    assert_eq!(items.len(), 7, "expected 7 input items: {out}");

    assert_eq!(text_at(items[0], "role"), "user", "item 0: {out}");

    let expected_call_ids = ["call_paris", "call_london", "call_tokyo"];
    for (i, expected_id) in expected_call_ids.into_iter().enumerate() {
        let idx = i + 1;
        assert_eq!(
            text_at(items[idx], "type"),
            "function_call",
            "item {idx}: {out}"
        );
        assert_eq!(
            text_at(items[idx], "call_id"),
            expected_id,
            "item {idx}: {out}"
        );
    }

    let expected_outputs = ["sunny, 22C", "cloudy, 14C", "humid, 28C"];
    for (i, expected_output) in expected_outputs.into_iter().enumerate() {
        let idx = i + 4;
        assert_eq!(
            text_at(items[idx], "type"),
            "function_call_output",
            "item {idx}: {out}"
        );
        assert_eq!(
            text_at(items[idx], "call_id"),
            expected_call_ids[i],
            "item {idx}: {out}"
        );
        assert_eq!(
            text_at(items[idx], "output"),
            expected_output,
            "item {idx}: {out}"
        );
    }
}

// Regression test for #2132: a tool-call-only assistant message (content null)
// must not produce an empty message item.
#[test]
fn no_spurious_empty_assistant_message() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Call a tool"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_x",
                            "type": "function",
                            "function": {"name": "do_thing", "arguments": "{}"}
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_x", "content": "done"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "do_thing",
                        "description": "Do a thing",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    for (i, item) in items.iter().enumerate() {
        if text_at(item, "type") == "message" && text_at(item, "role") == "assistant" {
            assert!(
                !array_at(item, "content").is_empty(),
                "item {i}: empty assistant message breaks call_id matching. item: {item}"
            );
        }
    }

    // user + function_call + function_call_output
    assert_eq!(
        items.len(),
        3,
        "expected 3 input items (user + function_call + function_call_output): {out}"
    );
    assert!(
        text_at(items[0], "type") == "message" && text_at(items[0], "role") == "user",
        "item 0: expected user message: {out}"
    );
    assert_eq!(text_at(items[1], "type"), "function_call", "item 1: {out}");
    assert_eq!(
        text_at(items[2], "type"),
        "function_call_output",
        "item 2: {out}"
    );
}

// Two rounds of tool calling with a text reply in between.
#[test]
fn multi_turn_tool_calling() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Weather in Paris?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{"id": "call_r1", "type": "function", "function": {"name": "get_weather", "arguments": r#"{"city":"Paris"}"#}}]
                },
                {"role": "tool", "tool_call_id": "call_r1", "content": "sunny"},
                {"role": "assistant", "content": "It is sunny in Paris."},
                {"role": "user", "content": "And London?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{"id": "call_r2", "type": "function", "function": {"name": "get_weather", "arguments": r#"{"city":"London"}"#}}]
                },
                {"role": "tool", "tool_call_id": "call_r2", "content": "rainy"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Get weather",
                        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    // user, call r1, output r1, assistant text, user, call r2, output r2
    assert_eq!(items.len(), 7, "expected 7 input items: {out}");

    for (i, item) in items.iter().enumerate() {
        if text_at(item, "type") == "message" && text_at(item, "role") == "assistant" {
            assert!(
                !array_at(item, "content").is_empty(),
                "item {i}: unexpected empty assistant message: {out}"
            );
        }
    }

    // round 1
    assert_eq!(text_at(items[1], "type"), "function_call", "item 1: {out}");
    assert_eq!(text_at(items[1], "call_id"), "call_r1", "item 1: {out}");
    assert_eq!(
        text_at(items[2], "type"),
        "function_call_output",
        "item 2: {out}"
    );

    // text reply between rounds
    assert!(
        text_at(items[3], "type") == "message" && text_at(items[3], "role") == "assistant",
        "item 3: expected assistant message: {out}"
    );

    // round 2
    assert_eq!(text_at(items[5], "type"), "function_call", "item 5: {out}");
    assert_eq!(text_at(items[5], "call_id"), "call_r2", "item 5: {out}");
    assert_eq!(
        text_at(items[6], "type"),
        "function_call_output",
        "item 6: {out}"
    );
}

// Tool names over 64 characters are shortened; the call ID stays the same.
#[test]
fn tool_name_shortening() {
    let long_name = "a_very_long_tool_name_that_exceeds_sixty_four_characters_limit_here_test";
    assert!(
        long_name.len() > 64,
        "test setup error: name must be > 64 chars, got {}",
        long_name.len()
    );

    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Do it"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_long",
                            "type": "function",
                            "function": {
                                "name": long_name,
                                "arguments": "{}"
                            }
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_long", "content": "ok"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": long_name,
                        "description": "A tool with a very long name",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    let call = items
        .iter()
        .find(|item| text_at(item, "type") == "function_call")
        .unwrap_or_else(|| panic!("no function_call item found in output: {out}"));

    assert_eq!(
        text_at(call, "call_id"),
        "call_long",
        "call_id changed: {out}"
    );

    let translated_name = text_at(call, "name");
    assert_ne!(
        translated_name, long_name,
        "tool name was NOT shortened: {out}"
    );
    assert!(
        translated_name.len() <= 64,
        "shortened name still > 64 chars: len={} name={translated_name:?}",
        translated_name.len()
    );
}

#[test]
fn custom_tool_name_shortening() {
    let long_name = "a_very_long_custom_tool_name_that_exceeds_sixty_four_characters_limit_test";
    assert!(
        long_name.len() > 64,
        "test setup error: name must be > 64 chars, got {}",
        long_name.len()
    );

    let input = json!({
        "messages": [
            {"role":"user","content":"Apply the patch."},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_custom_long","type":"function","function":{"name":long_name,"arguments":"patch"}}
            ]},
            {"role":"tool","tool_call_id":"call_custom_long","content":"patched"}
        ],
        "tools": [
            {"type":"custom","name":long_name,"description":"Apply a patch."}
        ],
        "tool_choice":{"type":"custom","name":long_name}
    });
    let out = convert_openai_chat_completions_request_to_codex("gpt-5.6-sol", &input, true);

    let items = array_at(&out, "input");
    assert_eq!(
        items.len(),
        3,
        "expected user, custom call, and custom output: {out}"
    );
    assert_eq!(
        text_at(items[1], "type"),
        "custom_tool_call",
        "item 1: {out}"
    );
    let short_name = text_at(items[1], "name");
    assert!(
        short_name != long_name && short_name.len() <= 64,
        "expected shortened custom tool name, got {short_name:?}"
    );
    assert_eq!(
        text_at(&out, "tools.0.name"),
        short_name,
        "custom declaration name: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.type"),
        "custom",
        "expected custom tool choice: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.name"),
        short_name,
        "expected shortened custom tool choice name: {out}"
    );
    assert_eq!(
        text_at(items[2], "type"),
        "custom_tool_call_output",
        "item 2: {out}"
    );
    assert_eq!(
        build_reverse_map_from_original_openai(&input)
            .get(&short_name)
            .map(String::as_str),
        Some(long_name),
        "expected reverse name mapping"
    );
}

#[test]
fn custom_tool_short_name_collision_preserves_function_family() {
    let custom_name = "a_very_long_custom_tool_name_that_exceeds_sixty_four_characters_limit_test";
    let function_name = shorten_name_if_needed(custom_name);
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_function","type":"function","function":{"name":function_name,"arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_function","content":"done"}
            ],
            "tools": [
                {"type":"custom","name":custom_name,"description":"Custom tool."},
                {"type":"function","function":{"name":function_name,"parameters":{"type":"object"}}}
            ],
            "tool_choice":{"type":"function","function":{"name":function_name}}
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 2, "expected function call and output: {out}");
    assert_eq!(
        text_at(items[0], "type"),
        "function_call",
        "expected colliding original function name to remain function_call: {out}"
    );
    assert_eq!(
        text_at(items[1], "type"),
        "function_call_output",
        "expected colliding function output to remain function_call_output: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.type"),
        "function",
        "expected colliding function choice to remain function: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.name"),
        text_at(&out, "tools.1.name"),
        "expected function choice name to match translated declaration: {out}"
    );
}

#[test]
fn same_name_custom_and_function_defaults_to_function_family() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_shared","type":"function","function":{"name":"shared_tool","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_shared","content":"done"}
            ],
            "tools": [
                {"type":"custom","name":"shared_tool","description":"Custom tool."},
                {"type":"function","function":{"name":"shared_tool","parameters":{"type":"object"}}}
            ],
            "tool_choice":{"type":"function","function":{"name":"shared_tool"}}
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 2, "expected function call and output: {out}");
    assert_eq!(
        text_at(items[0], "type"),
        "function_call",
        "expected ambiguous normalized call to preserve function family: {out}"
    );
    assert_eq!(
        text_at(items[1], "type"),
        "function_call_output",
        "expected ambiguous output to preserve function family: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.type"),
        "function",
        "expected ambiguous function choice to preserve function family: {out}"
    );
    assert_eq!(
        text_at(&out, "tools.0.name"),
        text_at(&out, "tools.1.name"),
        "expected same-name declarations to use a consistent translated name: {out}"
    );
}

// An empty string content is treated like null.
#[test]
fn empty_string_content() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Do something"},
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_empty",
                            "type": "function",
                            "function": {"name": "action", "arguments": "{}"}
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_empty", "content": "result"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "action",
                        "description": "An action",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    for (i, item) in items.iter().enumerate() {
        if text_at(item, "type") == "message" && text_at(item, "role") == "assistant" {
            assert!(
                !array_at(item, "content").is_empty(),
                "item {i}: empty assistant message from content \"\": {out}"
            );
        }
    }

    // user + function_call + function_call_output
    assert_eq!(items.len(), 3, "expected 3 input items: {out}");
}

// Every function_call_output has a function_call with its call ID.
#[test]
fn call_ids_match_between_call_and_output() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Multi-tool"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id": "id_a", "type": "function", "function": {"name": "tool_a", "arguments": "{}"}},
                        {"id": "id_b", "type": "function", "function": {"name": "tool_b", "arguments": "{}"}}
                    ]
                },
                {"role": "tool", "tool_call_id": "id_a", "content": "res_a"},
                {"role": "tool", "tool_call_id": "id_b", "content": "res_b"}
            ],
            "tools": [
                {"type": "function", "function": {"name": "tool_a", "description": "A", "parameters": {"type": "object", "properties": {}}}},
                {"type": "function", "function": {"name": "tool_b", "description": "B", "parameters": {"type": "object", "properties": {}}}}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    let call_ids: HashSet<String> = items
        .iter()
        .filter(|item| text_at(item, "type") == "function_call")
        .map(|item| text_at(item, "call_id"))
        .collect();

    for (i, item) in items.iter().enumerate() {
        if text_at(item, "type") == "function_call_output" {
            let out_id = text_at(item, "call_id");
            assert!(
                call_ids.contains(&out_id),
                "item {i}: function_call_output has call_id {out_id:?} with no matching function_call: {out}"
            );
        }
    }

    let count = |kind: &str| {
        items
            .iter()
            .filter(|item| text_at(item, "type") == kind)
            .count()
    };
    assert_eq!(count("function_call"), 2, "function_calls: {out}");
    assert_eq!(
        count("function_call_output"),
        2,
        "function_call_outputs: {out}"
    );
}

#[test]
fn custom_tool_call_history() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "user", "content": "Update the specification."},
                {
                    "role": "assistant",
                    "content": "I will update the file.",
                    "tool_calls": [
                        {
                            "id": "call_apply_patch",
                            "type": "function",
                            "function": {
                                "name": "apply_patch",
                                "arguments": "*** Begin Patch\n*** Add File: spec.md\n+done\n*** End Patch"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_apply_patch",
                    "content": "Added spec.md"
                },
                {"role": "assistant", "content": "The specification is updated."}
            ],
            "tools": [
                {
                    "type": "custom",
                    "name": "apply_patch",
                    "description": "Apply a freeform patch."
                }
            ],
            "tool_choice": {"type":"function","function":{"name":"apply_patch"}}
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 5, "expected 5 input items: {out}");

    let custom_call = items[2];
    assert_eq!(
        text_at(custom_call, "type"),
        "custom_tool_call",
        "{custom_call}"
    );
    assert_eq!(
        text_at(custom_call, "call_id"),
        "call_apply_patch",
        "expected custom call_id to be preserved: {custom_call}"
    );
    assert_eq!(text_at(custom_call, "name"), "apply_patch", "{custom_call}");
    assert_eq!(
        text_at(custom_call, "input"),
        "*** Begin Patch\n*** Add File: spec.md\n+done\n*** End Patch",
        "expected custom tool input to be preserved: {custom_call}"
    );

    let custom_output = items[3];
    assert_eq!(
        text_at(custom_output, "type"),
        "custom_tool_call_output",
        "{custom_output}"
    );
    assert_eq!(
        text_at(custom_output, "call_id"),
        "call_apply_patch",
        "expected custom output call_id to be preserved: {custom_output}"
    );
    assert_eq!(
        text_at(custom_output, "output"),
        "Added spec.md",
        "expected custom tool output to be preserved: {custom_output}"
    );
    assert_eq!(
        text_at(items[4], "content.0.text"),
        "The specification is updated.",
        "expected final assistant continuation: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.type"),
        "custom",
        "expected normalized custom tool choice: {out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.name"),
        "apply_patch",
        "expected custom tool choice name apply_patch: {out}"
    );
}

#[test]
fn custom_tool_call_response_follow_up_round_trip() {
    let original_request = json!({
        "messages":[{"role":"user","content":"Apply the patch."}],
        "tools":[{"type":"custom","name":"apply_patch","description":"Apply a patch."}]
    });
    let upstream_response = json!({
        "type":"response.completed",
        "response":{
            "status":"completed",
            "output":[
                {"type":"custom_tool_call","call_id":"call_patch","name":"apply_patch","input":"patch"}
            ]
        }
    });

    let chat_response = convert_codex_response_to_openai_chat_completions_non_stream(
        &original_request,
        &upstream_response,
    )
    .expect("a completed response gives a chat completion");
    let assistant_message = at(&chat_response, "choices.0.message")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        text_at(&assistant_message, "tool_calls.0.type"),
        "function",
        "expected response to normalize custom call as function: {assistant_message}"
    );
    assert_eq!(
        text_at(&assistant_message, "tool_calls.0.function.arguments"),
        r#"{"input":"patch"}"#,
        "expected normalized custom input: {assistant_message}"
    );

    let follow_up_request = json!({
        "messages":[
            {"role":"user","content":"Apply the patch."},
            assistant_message,
            {"role":"tool","tool_call_id":"call_patch","content":"patched"}
        ],
        "tools":[{"type":"custom","name":"apply_patch","description":"Apply a patch."}]
    });
    let out =
        convert_openai_chat_completions_request_to_codex("gpt-5.6-sol", &follow_up_request, true);
    let items = array_at(&out, "input");
    assert_eq!(
        items.len(),
        3,
        "expected user, custom call, and custom output: {out}"
    );
    assert_eq!(
        text_at(items[1], "type"),
        "custom_tool_call",
        "expected custom_tool_call after response round trip: {out}"
    );
    assert_eq!(
        text_at(items[2], "type"),
        "custom_tool_call_output",
        "expected custom_tool_call_output after response round trip: {out}"
    );
    assert_eq!(
        text_at(items[1], "input"),
        "patch",
        "raw follow-up input: {out}"
    );
}

#[test]
fn mixed_tool_call_history_preserves_call_families() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"user","content":"Run both tools."},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_function","type":"function","function":{"name":"lookup","arguments":"{}"}},
                    {"id":"call_custom","type":"function","function":{"name":"apply_patch","arguments":"patch"}}
                ]},
                {"role":"tool","tool_call_id":"call_custom","content":"patched"},
                {"role":"tool","tool_call_id":"call_function","content":"found"}
            ],
            "tools": [
                {"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}},
                {"type":"custom","name":"apply_patch","description":"Apply a patch."}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 5, "expected 5 input items: {out}");

    let expected_types = [
        "message",
        "function_call",
        "custom_tool_call",
        "custom_tool_call_output",
        "function_call_output",
    ];
    for (i, expected_type) in expected_types.into_iter().enumerate() {
        assert_eq!(
            text_at(items[i], "type"),
            expected_type,
            "item {i}: {}",
            items[i]
        );
    }
    assert_eq!(
        text_at(items[3], "call_id"),
        "call_custom",
        "expected custom output call_id call_custom: {}",
        items[3]
    );
    assert_eq!(
        text_at(items[4], "call_id"),
        "call_function",
        "expected function output call_id call_function: {}",
        items[4]
    );
}

#[test]
fn tool_call_history_allows_reused_call_id_across_rounds() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"user","content":"Run the first tool."},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_reused","type":"function","function":{"name":"lookup","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_reused","content":"found"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_reused","type":"custom","custom":{"name":"apply_patch","input":"patch"}}
                ]},
                {"role":"tool","tool_call_id":"call_reused","content":"patched"}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 5, "expected 5 input items: {out}");
    assert_eq!(
        text_at(items[2], "type"),
        "function_call_output",
        "expected first reused call output to remain function_call_output: {}",
        items[2]
    );
    assert_eq!(
        text_at(items[4], "type"),
        "custom_tool_call_output",
        "expected second reused call output to be custom_tool_call_output: {}",
        items[4]
    );
}

#[test]
fn custom_tool_call_history_synthesizes_missing_call_id() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"tool","content":"orphan"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"type":"custom","custom":{"name":"apply_patch","input":"patch"}}
                ]},
                {"role":"tool","content":"patched"}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(
        items.len(),
        2,
        "expected orphan output to be dropped and missing ID pair preserved: {out}"
    );
    assert_eq!(
        text_at(items[0], "type"),
        "custom_tool_call",
        "{}",
        items[0]
    );
    assert_eq!(
        text_at(items[1], "type"),
        "custom_tool_call_output",
        "{}",
        items[1]
    );
    let call_id = text_at(items[0], "call_id");
    assert!(
        !call_id.is_empty(),
        "expected synthesized call_id: {}",
        items[0]
    );
    assert_eq!(
        text_at(items[1], "call_id"),
        call_id,
        "expected synthesized call_id on output: {}",
        items[1]
    );
}

#[test]
fn tool_call_history_clears_unmatched_call_at_new_batch() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_reused","type":"custom","custom":{"name":"apply_patch","input":"old patch"}}
                ]},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_reused","type":"function","function":{"name":"lookup","arguments":"{}"}}
                ]},
                {"role":"tool","tool_call_id":"call_reused","content":"found"}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 3, "expected two calls and one output: {out}");
    assert_eq!(
        text_at(items[2], "type"),
        "function_call_output",
        "expected new batch output to match function call: {}",
        items[2]
    );
}

#[test]
fn tool_call_output_without_id_uses_pending_call() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_explicit","type":"function","function":{"name":"lookup","arguments":"{}"}},
                    {"type":"custom","custom":{"name":"apply_patch","input":"patch"}}
                ]},
                {"role":"tool","content":"found"},
                {"role":"tool","content":"patched"}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(items.len(), 4, "expected two calls and two outputs: {out}");
    assert_eq!(
        text_at(items[2], "type"),
        "function_call_output",
        "expected first empty-ID output to match function call: {}",
        items[2]
    );
    assert_eq!(
        text_at(items[2], "call_id"),
        "call_explicit",
        "expected explicit pending call_id: {}",
        items[2]
    );
    assert_eq!(
        text_at(items[3], "type"),
        "custom_tool_call_output",
        "expected second empty-ID output to match custom call: {}",
        items[3]
    );
    assert!(
        !text_at(items[3], "call_id").is_empty(),
        "expected synthesized custom output call_id: {}",
        items[3]
    );
}

#[test]
fn ambiguous_duplicate_tool_call_ids_are_dropped() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"user","content":"Run both tools."},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_duplicate","type":"function","function":{"name":"lookup","arguments":"{}"}},
                    {"id":"call_duplicate","type":"custom","custom":{"name":"apply_patch","input":"patch"}}
                ]},
                {"role":"tool","tool_call_id":"call_duplicate","content":"first"},
                {"role":"tool","tool_call_id":"call_duplicate","content":"second"}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert!(
        items.len() == 1 && text_at(items[0], "role") == "user",
        "expected ambiguous calls and outputs to be dropped: {out}"
    );
}

#[test]
fn orphan_and_duplicate_tool_call_outputs_are_dropped() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "messages": [
                {"role":"tool","tool_call_id":"call_orphan","content":"orphan"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_custom","type":"function","function":{"name":"apply_patch","arguments":"patch"}}
                ]},
                {"role":"tool","tool_call_id":"call_custom","content":"patched"},
                {"role":"tool","tool_call_id":"call_custom","content":"duplicate"}
            ],
            "tools": [
                {"type":"custom","name":"apply_patch","description":"Apply a patch."}
            ]
        }),
        true,
    );

    let items = array_at(&out, "input");
    assert_eq!(
        items.len(),
        2,
        "expected only the matched call and first output: {out}"
    );
    assert_eq!(
        text_at(items[0], "type"),
        "custom_tool_call",
        "{}",
        items[0]
    );
    assert_eq!(
        text_at(items[1], "type"),
        "custom_tool_call_output",
        "{}",
        items[1]
    );
    assert_eq!(
        text_at(items[1], "output"),
        "patched",
        "expected first matched output to be preserved: {}",
        items[1]
    );
}

// The tools carry over to the Responses request.
#[test]
fn tools_definition_translated() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-4o",
        &json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "Hi"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "search",
                        "description": "Search the web",
                        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}
                    }
                }
            ]
        }),
        true,
    );

    let tools = array_at(&out, "tools");
    assert!(!tools.is_empty(), "no tools found in output: {out}");
    assert!(
        tools.iter().any(|tool| text_at(tool, "name") == "search"),
        "tool 'search' not found in output tools: {out}"
    );
}

#[test]
fn function_tool_strict_defaults_to_false() {
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "user", "content": "Hi"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "omitted",
                        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "explicit_true",
                        "strict": true,
                        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"], "additionalProperties": false}
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "explicit_false",
                        "strict": false,
                        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
                    }
                }
            ]
        }),
        true,
    );

    let tools = array_at(&out, "tools");
    assert_eq!(tools.len(), 3, "expected 3 tools: {out}");

    let expected = HashMap::from([
        ("omitted", false),
        ("explicit_true", true),
        ("explicit_false", false),
    ]);
    for tool in tools {
        let name = text_at(tool, "name");
        assert!(
            at(tool, "strict").is_some(),
            "tool {name:?}: strict missing in output: {tool}"
        );
        let want = expected.get(name.as_str()).copied().unwrap_or_default();
        assert_eq!(bool_at(tool, "strict"), want, "tool {name:?}: strict");
    }
}

#[test]
fn normalize_invalid_tool_names() {
    let name_with_invalid_chars = "mcp.server:search tool";
    let input = json!({
        "model": "gpt-5.6-sol",
        "messages": [
            {"role": "user", "content": "Search for info"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": name_with_invalid_chars,
                            "arguments": r#"{"query":"test"}"#
                        }
                    }
                ]
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "result"}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": name_with_invalid_chars,
                    "description": "Search tool",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": {"name": name_with_invalid_chars}
        }
    });
    let out = convert_openai_chat_completions_request_to_codex("gpt-5.6-sol", &input, true);

    let tool_name_in_tools = text_at(&out, "tools.0.name");
    assert!(
        is_valid_tool_name(&tool_name_in_tools),
        "expected tool name in tools to match ^[a-zA-Z0-9_-]+$, got {tool_name_in_tools:?}"
    );

    let func_call_name = array_at(&out, "input")
        .into_iter()
        .find(|item| text_at(item, "type") == "function_call")
        .map(|item| text_at(item, "name"))
        .unwrap_or_default();
    assert_eq!(
        func_call_name, tool_name_in_tools,
        "expected function_call name to match tools declaration"
    );

    assert_eq!(
        text_at(&out, "tool_choice.name"),
        tool_name_in_tools,
        "expected tool_choice name to match tools declaration"
    );

    let rev = build_reverse_map_from_original_openai(&input);
    assert_eq!(
        rev.get(&tool_name_in_tools).map(String::as_str),
        Some(name_with_invalid_chars),
        "reverse map for {tool_name_in_tools:?}"
    );
}

#[test]
fn normalize_invalid_tool_names_collision_and_non_ascii() {
    let name1 = "tool.search";
    let name2 = "tool:search";
    let name_unicode = "工具_run";
    let out = convert_openai_chat_completions_request_to_codex(
        "gpt-5.6-sol",
        &json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {
                    "type": "function",
                    "function": {"name": name1}
                },
                {
                    "type": "function",
                    "function": {"name": name2}
                },
                {
                    "type": "function",
                    "function": {"name": name_unicode}
                }
            ]
        }),
        true,
    );

    let tools = array_at(&out, "tools");
    assert_eq!(tools.len(), 3, "expected 3 tools: {out}");
    let mut seen = HashSet::new();
    for (i, tool) in tools.into_iter().enumerate() {
        let name = text_at(tool, "name");
        assert!(
            is_valid_tool_name(&name),
            "tool {i} name {name:?} does not match ^[a-zA-Z0-9_-]+$"
        );
        assert!(
            seen.insert(name.clone()),
            "collision detected for tool {i}: {name:?}"
        );
    }
}

#[test]
fn historical_tool_call_collision_with_declared_tool() {
    let declared_name = "tool:search";
    let historical_name = "tool.search";
    let input = json!({
        "model": "gpt-5.6-sol",
        "messages": [
            {"role": "user", "content": "previous call"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_hist_1",
                        "type": "function",
                        "function": {
                            "name": historical_name,
                            "arguments": "{}"
                        }
                    }
                ]
            },
            {"role": "tool", "tool_call_id": "call_hist_1", "content": "hist result"},
            {"role": "user", "content": "new query"}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": declared_name,
                    "description": "current search tool"
                }
            }
        ]
    });
    let out = convert_openai_chat_completions_request_to_codex("gpt-5.6-sol", &input, true);

    let tool_declared = text_at(&out, "tools.0.name");
    let hist_call_name = array_at(&out, "input")
        .into_iter()
        .find(|item| text_at(item, "type") == "function_call")
        .map(|item| text_at(item, "name"))
        .unwrap_or_default();

    assert!(
        !tool_declared.is_empty() && !hist_call_name.is_empty(),
        "expected both names non-empty, got declared={tool_declared:?} hist={hist_call_name:?}"
    );
    assert_ne!(
        tool_declared, hist_call_name,
        "expected historical tool name and declared tool name not to collide"
    );

    // The response's reverse mapping restores both.
    let rev = build_reverse_map_from_original_openai(&input);
    assert_eq!(
        rev.get(&tool_declared).map(String::as_str),
        Some(declared_name),
        "reverse map for declared {tool_declared:?}"
    );
    assert_eq!(
        rev.get(&hist_call_name).map(String::as_str),
        Some(historical_name),
        "reverse map for historical {hist_call_name:?}"
    );
}

#[test]
fn convert_openai_request_to_codex_service_tier() {
    // (name, body, expected service_tier or None when omitted, expected effort)
    let cases = [
        (
            "priority service tier preserved",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high","service_tier":"priority"}"#,
            Some("priority"),
            "high",
        ),
        (
            "priority case-insensitive and trimmed",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"low","service_tier":"  PRIORITY  "}"#,
            Some("priority"),
            "low",
        ),
        (
            "fast service tier normalized to priority",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"fast"}"#,
            Some("priority"),
            "medium",
        ),
        (
            "fast case-insensitive and trimmed",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"  Fast  "}"#,
            Some("priority"),
            "medium",
        ),
        (
            "ultrafast service tier preserved",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"ultrafast"}"#,
            Some("ultrafast"),
            "medium",
        ),
        (
            "default service tier omitted",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"default"}"#,
            None,
            "medium",
        ),
        (
            "auto service tier omitted",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"auto"}"#,
            None,
            "medium",
        ),
        (
            "non-string service tier omitted",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}],"service_tier":1}"#,
            None,
            "medium",
        ),
        (
            "absent service tier omitted",
            r#"{"model":"gpt-6-sol","messages":[{"role":"user","content":"hi"}]}"#,
            None,
            "medium",
        ),
    ];

    for (name, body, want_tier, want_effort) in cases {
        let request: Value = serde_json::from_str(body).unwrap();
        let out = convert_openai_chat_completions_request_to_codex("gpt-6-sol", &request, true);
        assert_eq!(
            at(&out, "service_tier").is_some(),
            want_tier.is_some(),
            "{name}: service_tier exists; payload={out}"
        );
        if let Some(want_tier) = want_tier {
            assert_eq!(
                text_at(&out, "service_tier"),
                want_tier,
                "{name}: payload={out}"
            );
        }
        assert_eq!(
            text_at(&out, "reasoning.effort"),
            want_effort,
            "{name}: payload={out}"
        );
    }
}

#[test]
fn apply_patch_chat_history_boundary() {
    // (name, tools, call, expected item type, expected input or arguments)
    let cases = [
        (
            "normalized",
            r#"[{"type":"custom","name":"apply_patch"}]"#,
            r#"{"type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#,
            "custom_tool_call",
            "p",
        ),
        (
            "legacy",
            r#"[{"type":"custom","name":"apply_patch"}]"#,
            r#"{"type":"function","function":{"name":"apply_patch","arguments":"raw patch"}}"#,
            "custom_tool_call",
            "raw patch",
        ),
        (
            "explicit",
            r#"[{"type":"custom","name":"apply_patch"}]"#,
            r#"{"type":"custom","custom":{"name":"apply_patch","input":"{\"input\":\"p\"}"}}"#,
            "custom_tool_call",
            r#"{"input":"p"}"#,
        ),
        (
            "invalid-wrapper",
            r#"[{"type":"custom","name":"apply_patch"}]"#,
            r#"{"type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"p\",\"extra\":1}"}}"#,
            "custom_tool_call",
            r#"{"input":"p","extra":1}"#,
        ),
        (
            "function-preference",
            r#"[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch","parameters":{}}}]"#,
            r#"{"type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#,
            "function_call",
            r#"{"input":"p"}"#,
        ),
    ];

    for (name, tools, call, want_type, want) in cases {
        let raw = format!(
            r#"{{"tools":{tools},"messages":[{{"role":"assistant","tool_calls":[{call}]}}]}}"#
        );
        let request: Value = serde_json::from_str(&raw).unwrap();
        let out = convert_openai_chat_completions_request_to_codex("m", &request, true);
        let items = array_at(&out, "input");
        let item = items
            .last()
            .unwrap_or_else(|| panic!("{name}: no input items: {out}"));
        let field = if want_type == "custom_tool_call" {
            "input"
        } else {
            "arguments"
        };
        assert!(
            text_at(item, "type") == want_type && text_at(item, field) == want,
            "{name}: history boundary: {out}"
        );
    }
}
