// Ported from CLIProxyAPI internal/translator/openai/claude/openai_claude_request_test.go
// and openai_claude_compat_test.go (v8.0.15, MIT), and openai_claude_user_turn_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

// All 28 request tests and 5 compat tests, the 4 new request tests and the 8
// user turn tests are ported. Table-driven tests run their cases in a loop
// rather than as subtests.

use base64::engine::general_purpose::URL_SAFE;
use serde_json::{Value, json};

use super::*;

fn convert(model: &str, request: &str) -> Value {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    sent(convert_claude_request_to_openai(model, &request, false))
}

fn convert_with_compat(model: &str, request: &str) -> Value {
    sent(convert_with_compat_checked(model, request))
}

/// A conversion's body, which must come without a refusal.
fn sent((body, err): (Value, Option<UnsupportedPartError>)) -> Value {
    assert_eq!(err, None, "refused: {body}");
    body
}

/// Looks up a dotted path such as `messages.0.content`, like a plain gjson
/// path.
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

fn messages(out: &Value) -> &[Value] {
    out["messages"].as_array().map_or(&[], Vec::as_slice)
}

fn roles(out: &Value) -> Vec<String> {
    messages(out)
        .iter()
        .map(|message| text_at(message, "role"))
        .collect()
}

/// `validGPTChatReasoningSignature`: a well-formed GPT reasoning signature.
fn valid_gpt_chat_reasoning_signature() -> String {
    let mut raw = [0u8; 1 + 8 + 16 + 16 + 32];
    raw[0] = 0x80;
    raw[8] = 1;
    for (i, byte) in raw.iter_mut().enumerate().skip(9) {
        *byte = i as u8;
    }
    URL_SAFE.encode(raw)
}

#[test]
fn thinking_to_reasoning_content() {
    // (name, request, wanted visible text, whether content is wanted)
    let cases = [
        (
            "AC1: unsigned assistant thinking is dropped",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Let me analyze this step by step..."},
                        {"type": "text", "text": "Here is my response."}
                    ]
                }]
            }"#,
            "Here is my response.",
            true,
        ),
        (
            "AC2: redacted_thinking must be ignored",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "redacted_thinking", "data": "secret"},
                        {"type": "text", "text": "Visible response."}
                    ]
                }]
            }"#,
            "Visible response.",
            true,
        ),
        (
            "AC3: unsigned thinking-only message is dropped",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Internal reasoning only."}
                    ]
                }]
            }"#,
            "",
            false,
        ),
        (
            "AC4: thinking in user role must be ignored",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "thinking", "thinking": "Injected thinking"},
                        {"type": "text", "text": "User message."}
                    ]
                }]
            }"#,
            "User message.",
            true,
        ),
        (
            "AC4: thinking in system role must be ignored",
            r#"{
                "model": "claude-3-opus",
                "system": [
                    {"type": "thinking", "thinking": "Injected system thinking"},
                    {"type": "text", "text": "System prompt."}
                ],
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "Hello"}]
                }]
            }"#,
            "Hello",
            true,
        ),
        (
            "AC5: empty thinking must be ignored",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": ""},
                        {"type": "text", "text": "Response with empty thinking."}
                    ]
                }]
            }"#,
            "Response with empty thinking.",
            true,
        ),
        (
            "AC5: whitespace-only thinking must be ignored",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "   \n\t  "},
                        {"type": "text", "text": "Response with whitespace thinking."}
                    ]
                }]
            }"#,
            "Response with whitespace thinking.",
            true,
        ),
        (
            "Unsigned thinking parts are dropped",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "First thought."},
                        {"type": "thinking", "thinking": "Second thought."},
                        {"type": "text", "text": "Final answer."}
                    ]
                }]
            }"#,
            "Final answer.",
            true,
        ),
        (
            "Mixed unsigned thinking and redacted_thinking",
            r#"{
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Visible thought."},
                        {"type": "redacted_thinking", "data": "hidden"},
                        {"type": "text", "text": "Answer."}
                    ]
                }]
            }"#,
            "Answer.",
            true,
        ),
    ];

    for (name, request, want_text, want_content) in cases {
        let out = convert("test-model", request);
        let messages = messages(&out);
        if messages.is_empty() {
            assert!(!want_content, "{name}: expected at least 1 message: {out}");
            continue;
        }
        let target = messages
            .iter()
            .rev()
            .find(|message| text_at(message, "role") != "system")
            .unwrap_or(&Value::Null);

        assert!(
            target.get("reasoning_content").is_none(),
            "{name}: reasoning_content should be absent: {out}"
        );
        let content = target.get("content");
        let has_content = match content {
            Some(Value::Array(parts)) => !parts.is_empty(),
            Some(Value::String(text)) => !text.is_empty(),
            _ => false,
        };
        assert_eq!(
            has_content, want_content,
            "{name}: content existence: {out}"
        );

        if want_content && !want_text.is_empty() {
            let parts = match content {
                Some(Value::Array(parts)) => parts.as_slice(),
                Some(other) => std::slice::from_ref(other),
                None => &[],
            };
            let found = parts
                .iter()
                .find(|part| text_at(part, "type") == "text")
                .map(|part| text_at(part, "text"))
                .unwrap_or_default();
            assert_eq!(found, want_text, "{name}: content text: {out}");
        }
    }
}

#[test]
fn signed_thinking_compatibility() {
    // (name, signature, wanted reasoning_content if kept)
    let cases = [
        (
            "GPT-compatible signature keeps reasoning_content",
            valid_gpt_chat_reasoning_signature(),
            Some("provider state"),
        ),
        (
            "Claude signature drops reasoning_content",
            "claude#EjQ=".to_owned(),
            None,
        ),
        (
            "Gemini signature drops reasoning_content",
            "gemini#EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"
                .to_owned(),
            None,
        ),
        (
            "Unknown signature drops reasoning_content",
            "not-a-provider-signature".to_owned(),
            None,
        ),
    ];

    for (name, signature, want) in cases {
        let request = json!({
            "model": "claude-3-opus",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "provider state", "signature": signature},
                    {"type": "text", "text": "visible answer"}
                ]
            }]
        });
        let out = sent(convert_claude_request_to_openai("gpt-5", &request, false));
        let assistant = at(&out, "messages.0").unwrap_or(&Value::Null);
        assert_eq!(
            assistant.get("reasoning_content").and_then(Value::as_str),
            want,
            "{name}: {out}"
        );
        assert_eq!(
            text_at(assistant, "content.0.text"),
            "visible answer",
            "{name}: {out}"
        );
    }
}

#[test]
fn unsigned_thinking_only_message_dropped() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "What is 2+2?"}]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "thinking", "thinking": "Let me calculate: 2+2=4"}]
                },
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "Thanks"}]
                }
            ]
        }"#,
    );

    let messages = messages(&out);
    assert_eq!(
        messages.len(),
        2,
        "unsigned thinking-only assistant message should be dropped: {out}"
    );
    for message in messages {
        assert!(
            message.get("reasoning_content").is_none(),
            "unsigned thinking should not produce reasoning_content: {out}"
        );
    }
}

#[test]
fn message_system_role_wraps_as_user_reminder() {
    let out = convert(
        "gpt-5",
        r#"{
            "model": "claude-sonnet-4-5",
            "system": [{"type": "text", "text": "Top-level rules"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "system", "content": "String mid-conversation rule"},
                {"role": "assistant", "content": [{"type": "text", "text": "Hi there"}]},
                {"role": "system", "content": [{"type": "text", "text": "Array mid-conversation rule"}]},
                {"role": "user", "content": [{"type": "text", "text": "Follow up"}]}
            ]
        }"#,
    );

    let messages = messages(&out);
    assert_eq!(messages.len(), 6, "{out}");
    assert_eq!(
        roles(&out),
        ["system", "user", "user", "assistant", "user", "user"]
    );
    let system_content = messages[0]["content"].as_array().expect("system content");
    assert_eq!(
        system_content.len(),
        1,
        "only top-level system content: {out}"
    );
    assert_eq!(text_at(&system_content[0], "text"), "Top-level rules");
    assert_eq!(
        text_at(&messages[2], "content.0.text"),
        "<system-reminder>\nString mid-conversation rule\n</system-reminder>"
    );
    assert_eq!(
        text_at(&messages[4], "content.0.text"),
        "<system-reminder>\nArray mid-conversation rule\n</system-reminder>"
    );
}

#[test]
fn preserves_tool_adjacency_with_intervening_system_message() {
    let out = convert(
        "gpt-5",
        r#"{
            "model": "gpt-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Execute tools"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "tool_one", "input": {"a": 1}},
                        {"type": "tool_use", "id": "call_2", "name": "tool_two", "input": {"b": 2}}
                    ]
                },
                {"role": "system", "content": "Context update between tool call and tool result"},
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "call_2", "content": "result 2"},
                        {"type": "tool_result", "tool_use_id": "call_1", "content": "result 1"},
                        {"type": "text", "text": "Now summarize"}
                    ]
                }
            ]
        }"#,
    );

    assert_eq!(
        roles(&out),
        ["user", "assistant", "tool", "tool", "user", "user"]
    );
    let messages = messages(&out);
    assert_eq!(text_at(&messages[2], "tool_call_id"), "call_1");
    assert_eq!(text_at(&messages[3], "tool_call_id"), "call_2");
    assert_eq!(
        text_at(&messages[4], "content.0.text"),
        "<system-reminder>\nContext update between tool call and tool result\n</system-reminder>"
    );
    assert_eq!(text_at(&messages[5], "content.0.text"), "Now summarize");
}

#[test]
fn system_message_scenarios() {
    // (name, request, wanted system text, or None for no system message)
    let cases = [
        (
            "No system field",
            r#"{"model": "claude-3-opus", "messages": [{"role": "user", "content": "hello"}]}"#,
            None,
        ),
        (
            "Empty string system field",
            r#"{"model": "claude-3-opus", "system": "", "messages": [{"role": "user", "content": "hello"}]}"#,
            None,
        ),
        (
            "String system field",
            r#"{"model": "claude-3-opus", "system": "Be helpful", "messages": [{"role": "user", "content": "hello"}]}"#,
            Some("Be helpful"),
        ),
        (
            "Array system field with text",
            r#"{"model": "claude-3-opus", "system": [{"type": "text", "text": "Array system"}], "messages": [{"role": "user", "content": "hello"}]}"#,
            Some("Array system"),
        ),
        (
            "Array system field with multiple text blocks",
            r#"{
                "model": "claude-3-opus",
                "system": [
                    {"type": "text", "text": "Block 1"},
                    {"type": "text", "text": "Block 2"}
                ],
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            Some("Block 2"),
        ),
    ];

    for (name, request, want) in cases {
        let out = convert("test-model", request);
        let system = messages(&out)
            .first()
            .filter(|message| text_at(message, "role") == "system");
        assert_eq!(system.is_some(), want.is_some(), "{name}: {out}");
        if let (Some(system), Some(want)) = (system, want) {
            let got = match &system["content"] {
                Value::Array(parts) => parts
                    .last()
                    .map(|part| text_at(part, "text"))
                    .unwrap_or_default(),
                other => str_of(Some(other)).into_owned(),
            };
            assert_eq!(got, want, "{name}: {out}");
        }
    }
}

#[test]
fn tool_schema_adds_missing_object_properties() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "tools": [
                {
                    "name": "empty_params",
                    "description": "No args",
                    "input_schema": {"type": "object"}
                },
                {
                    "name": "nested_params",
                    "description": "Nested args",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "nested": {"type": "object"},
                            "items": {
                                "type": "array",
                                "items": {"type": "object"}
                            }
                        }
                    }
                }
            ],
            "messages": [{"role": "user", "content": "hello"}]
        }"#,
    );

    for path in [
        "tools.0.function.parameters.properties",
        "tools.1.function.parameters.properties.nested.properties",
        "tools.1.function.parameters.properties.items.items.properties",
    ] {
        assert!(
            at(&out, path).is_some_and(Value::is_object),
            "{path} missing or not an object: {out}"
        );
    }
}

#[test]
fn tool_result_order_and_content() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "before"},
                        {"type": "tool_result", "tool_use_id": "call_1", "content": [{"type":"text","text":"tool ok"}]},
                        {"type": "text", "text": "after"}
                    ]
                }
            ]
        }"#,
    );

    // Tool messages must immediately follow the assistant's tool calls.
    let messages = messages(&out);
    assert_eq!(messages.len(), 3, "{out}");
    assert_eq!(text_at(&messages[0], "role"), "assistant");
    assert!(messages[0].get("tool_calls").is_some(), "{out}");
    assert_eq!(text_at(&messages[1], "role"), "tool");
    assert_eq!(text_at(&messages[1], "tool_call_id"), "call_1");
    assert_eq!(text_at(&messages[1], "content"), "tool ok");
    assert_eq!(text_at(&messages[2], "role"), "user");
    assert_eq!(text_at(&messages[2], "content.0.text"), "before");
    assert_eq!(text_at(&messages[2], "content.1.text"), "after");
}

#[test]
fn tool_result_object_content() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "call_1", "content": {"foo": "bar"}}
                    ]
                }
            ]
        }"#,
    );

    let messages = messages(&out);
    assert_eq!(messages.len(), 2, "{out}");
    assert_eq!(text_at(&messages[1], "role"), "tool");
    let content: Value =
        serde_json::from_str(&text_at(&messages[1], "content")).expect("tool content is JSON");
    assert_eq!(text_at(&content, "foo"), "bar", "{out}");
}

#[test]
fn tool_result_text_and_image_content() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_1",
                            "content": [
                                {"type": "text", "text": "tool ok"},
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "iVBORw0KGgoAAAANSUhEUg=="
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#,
    );

    let messages = messages(&out);
    assert_eq!(messages.len(), 3, "{out}");
    // The tool message keeps only the text; the image is relayed in a user
    // message right after it.
    assert_eq!(text_at(&messages[1], "role"), "tool");
    assert!(messages[1]["content"].is_string(), "{out}");
    assert_eq!(text_at(&messages[1], "content"), "tool ok");
    let relay = &messages[2];
    assert_eq!(text_at(relay, "role"), "user");
    assert!(relay["content"].is_array(), "{out}");
    assert_eq!(
        text_at(relay, "content.0.text"),
        TOOL_RESULT_IMAGE_RELAY_NOTICE
    );
    assert_eq!(text_at(relay, "content.1.type"), "image_url");
    assert_eq!(
        text_at(relay, "content.1.image_url.url"),
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
    );
}

#[test]
fn tool_result_url_image_only() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_1",
                            "content": {
                                "type": "image",
                                "source": {
                                    "type": "url",
                                    "url": "https://example.com/tool.png"
                                }
                            }
                        }
                    ]
                }
            ]
        }"#,
    );

    let messages = messages(&out);
    assert_eq!(messages.len(), 3, "{out}");
    // An image-only result still needs text in the tool message.
    assert_eq!(
        text_at(&messages[1], "content"),
        TOOL_RESULT_IMAGE_PLACEHOLDER
    );
    assert_eq!(text_at(&messages[2], "role"), "user");
    assert_eq!(text_at(&messages[2], "content.1.type"), "image_url");
    assert_eq!(
        text_at(&messages[2], "content.1.image_url.url"),
        "https://example.com/tool.png"
    );
}

#[test]
fn tool_result_image_merges_into_user_text() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "screenshot", "input": {}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_1",
                            "content": [
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "iVBORw0KGgoAAAANSUhEUg=="
                                    }
                                }
                            ]
                        },
                        {"type": "text", "text": "What color?"}
                    ]
                }
            ]
        }"#,
    );

    // The relayed image joins the user text instead of adding a turn.
    let messages = messages(&out);
    assert_eq!(messages.len(), 3, "{out}");
    assert_eq!(text_at(&messages[2], "role"), "user");
    let content = messages[2]["content"].as_array().expect("user content");
    assert_eq!(content.len(), 3, "{out}");
    assert_eq!(text_at(&content[0], "text"), TOOL_RESULT_IMAGE_RELAY_NOTICE);
    assert_eq!(text_at(&content[1], "type"), "image_url");
    assert_eq!(text_at(&content[2], "text"), "What color?");
}

#[test]
fn multiple_tool_results_with_images() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "shot1", "input": {}},
                        {"type": "tool_use", "id": "call_2", "name": "shot2", "input": {}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_1",
                            "content": [
                                {"type": "text", "text": "result 1"},
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "img1"
                                    }
                                }
                            ]
                        },
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_2",
                            "content": {
                                "type": "image",
                                "source": {
                                    "type": "url",
                                    "url": "https://example.com/2.png"
                                }
                            }
                        }
                    ]
                }
            ]
        }"#,
    );

    // assistant (two calls), tool (call_1), tool (call_2), user (both images)
    let messages = messages(&out);
    assert_eq!(messages.len(), 4, "{out}");
    assert_eq!(text_at(&messages[1], "role"), "tool");
    assert_eq!(text_at(&messages[1], "tool_call_id"), "call_1");
    assert_eq!(text_at(&messages[1], "content"), "result 1");
    assert_eq!(text_at(&messages[2], "role"), "tool");
    assert_eq!(text_at(&messages[2], "tool_call_id"), "call_2");
    assert_eq!(
        text_at(&messages[2], "content"),
        TOOL_RESULT_IMAGE_PLACEHOLDER
    );
    assert_eq!(text_at(&messages[3], "role"), "user");
    let relay = messages[3]["content"].as_array().expect("relay content");
    assert_eq!(relay.len(), 3, "notice and two images: {out}");
    assert_eq!(text_at(&relay[0], "text"), TOOL_RESULT_IMAGE_RELAY_NOTICE);
    assert_eq!(
        text_at(&relay[1], "image_url.url"),
        "data:image/png;base64,img1"
    );
    assert_eq!(
        text_at(&relay[2], "image_url.url"),
        "https://example.com/2.png"
    );
}

#[test]
fn assistant_text_tool_use_text_order() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "pre"},
                        {"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}},
                        {"type": "text", "text": "post"}
                    ]
                }
            ]
        }"#,
    );

    // Text and tool calls share one assistant message.
    let messages = messages(&out);
    assert_eq!(messages.len(), 1, "{out}");
    let assistant = &messages[0];
    assert_eq!(text_at(assistant, "role"), "assistant");
    assert!(assistant.get("tool_calls").is_some(), "{out}");
    assert_eq!(text_at(assistant, "tool_calls.0.id"), "call_1");
    assert_eq!(text_at(assistant, "tool_calls.0.function.name"), "do_work");
    assert_eq!(text_at(assistant, "content.0.text"), "pre");
    assert_eq!(text_at(assistant, "content.1.text"), "post");
}

#[test]
fn assistant_thinking_tool_use_thinking_split() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "t1"},
                        {"type": "text", "text": "pre"},
                        {"type": "tool_use", "id": "call_1", "name": "do_work", "input": {"a": 1}},
                        {"type": "thinking", "thinking": "t2"},
                        {"type": "text", "text": "post"}
                    ]
                }
            ]
        }"#,
    );

    // Unsigned thinking is dropped; text and tool calls stay together.
    let messages = messages(&out);
    assert_eq!(messages.len(), 1, "{out}");
    let assistant = &messages[0];
    assert_eq!(text_at(assistant, "role"), "assistant");
    assert_eq!(text_at(assistant, "content.0.text"), "pre");
    assert_eq!(text_at(assistant, "content.1.text"), "post");
    assert!(assistant.get("tool_calls").is_some(), "{out}");
    assert!(
        assistant.get("reasoning_content").is_none(),
        "unsigned thinking should not produce reasoning_content: {out}"
    );
}

#[test]
fn strips_claude_code_attribution() {
    let out = convert(
        "gpt-5",
        r#"{
            "model": "claude-sonnet-4-5",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.63.abc; cc_entrypoint=cli; cch=12345;"},
                {"type": "text", "text": "User system prompt"}
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }"#,
    );

    let messages = messages(&out);
    assert_eq!(text_at(&messages[0], "role"), "system", "{out}");
    let content = messages[0]["content"].as_array().expect("system content");
    assert_eq!(content.len(), 1, "attribution should be stripped: {out}");
    assert_eq!(text_at(&content[0], "text"), "User system prompt");
}

#[test]
fn stop_sequences() {
    let cases: [(&str, &str, &[&str]); 2] = [
        (
            "single stop sequence is emitted as array",
            r#"{"model": "claude-3-opus", "stop_sequences": ["</block>"], "messages": [{"role": "user", "content": "hi"}]}"#,
            &["</block>"],
        ),
        (
            "multiple stop sequences are emitted as array",
            r#"{"model": "claude-3-opus", "stop_sequences": ["stop1", "stop2"], "messages": [{"role": "user", "content": "hi"}]}"#,
            &["stop1", "stop2"],
        ),
    ];

    for (name, request, want) in cases {
        let out = convert("gpt-4o", request);
        let stop = out
            .get("stop")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{name}: stop should be an array: {out}"));
        let got: Vec<String> = stop
            .iter()
            .map(|item| str_of(Some(item)).into_owned())
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn tool_without_input_schema_defaults_parameters() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-opus-5",
            "tools": [
                {
                    "type": "web_search_20250305",
                    "name": "web_search",
                    "max_uses": 8
                },
                {
                    "name": "no_schema_custom"
                },
                {
                    "name": "null_schema_custom",
                    "input_schema": null
                }
            ],
            "messages": [{"role": "user", "content": "hello"}]
        }"#,
    );

    for (i, name) in ["web_search", "no_schema_custom", "null_schema_custom"]
        .into_iter()
        .enumerate()
    {
        let function = at(&out, &format!("tools.{i}.function")).expect("tool function");
        assert_eq!(text_at(function, "name"), name);
        let parameters = function
            .get("parameters")
            .unwrap_or_else(|| panic!("tool {i} parameters missing: {function}"));
        assert_eq!(text_at(parameters, "type"), "object", "{parameters}");
        assert!(
            parameters.get("properties").is_some_and(Value::is_object),
            "tool {i} properties missing or not an object: {parameters}"
        );
    }
}

#[test]
fn strips_unsupported_unicode_property_escape_patterns() {
    let out = convert(
        "gpt-5.6",
        r#"{
            "model": "gpt-5.6",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "Artifact",
                "description": "Render an HTML file to an Artifact",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "field": {
                            "type": "string",
                            "description": "field to replace",
                            "pattern": "^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$"
                        },
                        "asset_id": {
                            "type": "string",
                            "pattern": "^[0-9a-f]{32}$"
                        },
                        "lookahead_safe": {
                            "type": "string",
                            "pattern": "^(?!__.*__$).{1,200}$"
                        }
                    }
                }
            }]
        }"#,
    );

    let parameters = at(&out, "tools.0.function.parameters").expect("parameters");
    // The \p{...} pattern goes; the others stay.
    assert!(
        at(parameters, "properties.field.pattern").is_none(),
        "{parameters}"
    );
    assert_eq!(text_at(parameters, "properties.field.type"), "string");
    assert_eq!(
        text_at(parameters, "properties.asset_id.pattern"),
        "^[0-9a-f]{32}$"
    );
    assert_eq!(
        text_at(parameters, "properties.lookahead_safe.pattern"),
        "^(?!__.*__$).{1,200}$"
    );
}

#[test]
fn preserves_non_schema_pattern_keys() {
    let out = convert(
        "gpt-5.6",
        r#"{
            "model": "gpt-5.6",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "config_tool",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "regex_config": {
                            "type": "object",
                            "default": {
                                "pattern": "\\p{L}+"
                            },
                            "enum": [
                                {"pattern": "\\p{N}+"}
                            ]
                        },
                        "real_schema": {
                            "type": "string",
                            "pattern": "\\p{L}+"
                        }
                    }
                }
            }]
        }"#,
    );

    let parameters = at(&out, "tools.0.function.parameters").expect("parameters");
    assert!(
        at(parameters, "properties.real_schema.pattern").is_none(),
        "{parameters}"
    );
    // Data under default and enum is not a schema and stays.
    assert_eq!(
        text_at(parameters, "properties.regex_config.default.pattern"),
        r"\p{L}+"
    );
    assert_eq!(
        text_at(parameters, "properties.regex_config.enum.0.pattern"),
        r"\p{N}+"
    );
}

#[test]
fn strips_pattern_properties_incompatible_keys() {
    let out = convert(
        "gpt-5.6",
        r#"{
            "model": "gpt-5.6",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "pattern_tool",
                "input_schema": {
                    "type": "object",
                    "patternProperties": {
                        "^\\\\p{L}+$": {
                            "type": "string"
                        },
                        "^[a-z]+$": {
                            "type": "number"
                        }
                    }
                }
            }]
        }"#,
    );

    let pattern_properties = at(&out, "tools.0.function.parameters.patternProperties")
        .and_then(Value::as_object)
        .expect("patternProperties");
    assert!(
        !pattern_properties.contains_key(r"^\p{L}+$"),
        "{pattern_properties:?}"
    );
    assert!(
        pattern_properties.contains_key("^[a-z]+$"),
        "{pattern_properties:?}"
    );
}

#[test]
fn tool_result_preserves_function_name() {
    let out = convert(
        "gemini-3.8-flash",
        r#"{
            "model": "gemini-3.8-flash",
            "max_tokens": 64,
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Get weather",
                    "input_schema": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"]
                    }
                },
                {
                    "name": "get_time",
                    "description": "Get time",
                    "input_schema": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"]
                    }
                }
            ],
            "messages": [
                {"role": "user", "content": "What's the weather and time in Jakarta?"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_01ABC", "name": "get_weather", "input": {"city": "Jakarta"}},
                        {"type": "tool_use", "id": "toolu_02DEF", "name": "get_time", "input": {"city": "Jakarta"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_01ABC", "content": "32C, humid"},
                        {"type": "tool_result", "tool_use_id": "toolu_02DEF", "content": "12:00 PM"}
                    ]
                }
            ]
        }"#,
    );

    let tool_messages: Vec<&Value> = messages(&out)
        .iter()
        .filter(|message| text_at(message, "role") == "tool")
        .collect();
    assert_eq!(tool_messages.len(), 2, "{out}");
    for (message, (id, name, content)) in tool_messages.iter().zip([
        ("toolu_01ABC", "get_weather", "32C, humid"),
        ("toolu_02DEF", "get_time", "12:00 PM"),
    ]) {
        assert_eq!(text_at(message, "tool_call_id"), id);
        assert_eq!(text_at(message, "name"), name);
        assert_eq!(text_at(message, "content"), content);
    }
}

#[test]
fn tool_result_unknown_id_no_name() {
    let out = convert(
        "gemini-3.8-flash",
        r#"{
            "model": "gemini-3.8-flash",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "orphan_call_1", "content": "result"}
                    ]
                }
            ]
        }"#,
    );

    let tool = at(&out, "messages.0").expect("tool message");
    assert_eq!(text_at(tool, "role"), "tool");
    assert_eq!(text_at(tool, "tool_call_id"), "orphan_call_1");
    assert!(
        tool.get("name").is_none(),
        "no name for an unknown ID: {out}"
    );
}

#[test]
fn tool_call_pairing_by_id() {
    let out = convert(
        "deepseek-v4.1-flash",
        r#"{
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Run analysis"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "fetch_data", "input": {"id": 123}},
                        {"type": "tool_use", "id": "call_2", "name": "calc_metric", "input": {"scale": 1.5}}
                    ]
                },
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Waiting for results to continue"},
                        {"type": "thinking", "thinking": "Thinking about next steps", "signature": "sig_abc"}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Reminder: keep timeout short"}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "call_2", "content": "metric_ok"},
                        {"type": "tool_result", "tool_use_id": "call_1", "content": "data_ok"}
                    ]
                }
            ]
        }"#,
    );

    // The results move up to follow the assistant's calls, in their own order.
    let messages = messages(&out);
    assert_eq!(messages.len(), 6, "{out}");
    let roles = roles(&out);
    assert_eq!(roles[1], "assistant");
    assert!(messages[1].get("tool_calls").is_some(), "{out}");
    assert_eq!(roles[2..4], ["tool", "tool"], "{roles:?}");
    assert_eq!(
        [
            text_at(&messages[2], "tool_call_id"),
            text_at(&messages[3], "tool_call_id")
        ],
        ["call_2", "call_1"]
    );
}

#[test]
fn tool_call_pairing_orphan_and_incomplete_preserved() {
    let out = convert(
        "deepseek-v4.1-flash",
        r#"{
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Start"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_alpha", "name": "do_a", "input": {}},
                        {"type": "tool_use", "id": "call_beta", "name": "do_b", "input": {}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Waiting on results"}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "call_unmatched", "content": "orphan_result"}
                    ]
                }
            ]
        }"#,
    );

    // Unanswered calls and an orphan result stay where they are.
    assert_eq!(roles(&out), ["user", "assistant", "user", "tool"], "{out}");
}

#[test]
fn tool_choice() {
    let tools = r#"[{"name": "tool_a", "description": "test", "input_schema": {"type": "object", "properties": {}}}]"#;
    let request = |tool_choice: &str| {
        format!(
            r#"{{"model": "gpt-5.4", "max_tokens": 64, "messages": [{{"role": "user", "content": "test"}}], "tool_choice": {tool_choice}, "tools": {tools}}}"#
        )
    };

    // none does not become auto.
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Answer without calling tools."}],
            "tool_choice": {"type": "none"},
            "tools": [
                {"name": "tool_a", "description": "test", "input_schema": {"type": "object", "properties": {}}},
                {"name": "tool_b", "description": "test", "input_schema": {"type": "object", "properties": {}}}
            ]
        }"#,
    );
    assert_eq!(text_at(&out, "tool_choice"), "none", "{out}");

    // disable_parallel_tool_use maps to parallel_tool_calls false.
    let out = convert(
        "gpt-5.4",
        &request(r#"{"type": "auto", "disable_parallel_tool_use": true}"#),
    );
    assert_eq!(
        out.get("parallel_tool_calls"),
        Some(&Value::Bool(false)),
        "{out}"
    );

    // An unknown type, or a tool with no name, fails closed to none.
    for tool_choice in [
        r#"{"type": "unknown_future_restriction"}"#,
        r#"{"type": "tool", "name": ""}"#,
    ] {
        let out = convert("gpt-5.4", &request(tool_choice));
        assert_eq!(text_at(&out, "tool_choice"), "none", "{out}");
    }

    // A null tool_choice sets nothing.
    let out = convert("gpt-5.4", &request("null"));
    assert!(out.get("tool_choice").is_none(), "{out}");
}

#[test]
fn enabled_thinking_effort() {
    let cases = [
        (
            "explicit output_config effort is preserved without budget",
            r#"{"thinking":{"type":"enabled"},"output_config":{"effort":"high"}}"#,
            "high",
        ),
        (
            "legacy budget remains authoritative when both are present",
            r#"{"thinking":{"type":"enabled","budget_tokens":8192},"output_config":{"effort":"high"}}"#,
            "medium",
        ),
        (
            "enabled without budget or effort keeps auto default",
            r#"{"thinking":{"type":"enabled"}}"#,
            "auto",
        ),
        (
            "enabled with empty effort string falls back to auto",
            r#"{"thinking":{"type":"enabled"},"output_config":{"effort":""}}"#,
            "auto",
        ),
        (
            "enabled with whitespace-only effort falls back to auto",
            r#"{"thinking":{"type":"enabled"},"output_config":{"effort":"   "}}"#,
            "auto",
        ),
        (
            "enabled with non-string effort falls back to auto",
            r#"{"thinking":{"type":"enabled"},"output_config":{"effort":123}}"#,
            "auto",
        ),
    ];

    for (name, request, want) in cases {
        let out = convert("test-model", request);
        assert_eq!(text_at(&out, "reasoning_effort"), want, "{name}: {out}");
    }
}

#[test]
fn normalizes_boolean_subschemas() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "tools": [
                {
                    "name": "patch_tool",
                    "description": "Applies a JSON patch",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "patch": {"type": "array", "items": true},
                            "anything": true,
                            "disabled": false,
                            "enabled_flag": {"type": "boolean", "default": true, "enum": [true, false]},
                            "either": {"anyOf": [true, {"type": "string"}]},
                            "nested_obj": {
                                "type": "object",
                                "properties": {"foo": {"type": "string"}},
                                "additionalProperties": true
                            }
                        },
                        "additionalProperties": false,
                        "$defs": {
                            "wildcard": true
                        }
                    }
                }
            ],
            "messages": [{"role": "user", "content": "hello"}]
        }"#,
    );

    let parameters = at(&out, "tools.0.function.parameters").expect("parameters");
    // A `true` subschema becomes {}.
    for path in [
        "properties.patch.items",
        "properties.anything",
        "properties.either.anyOf.0",
        "$defs.wildcard",
    ] {
        assert_eq!(
            at(parameters, path),
            Some(&json!({})),
            "{path}: {parameters}"
        );
    }
    // additionalProperties, a `false` property and data values stay booleans.
    for (path, want) in [
        ("additionalProperties", false),
        ("properties.nested_obj.additionalProperties", true),
        ("properties.disabled", false),
        ("properties.enabled_flag.default", true),
        ("properties.enabled_flag.enum.0", true),
        ("properties.enabled_flag.enum.1", false),
    ] {
        assert_eq!(
            at(parameters, path),
            Some(&Value::Bool(want)),
            "{path}: {parameters}"
        );
    }
}

// openai_claude_compat_test.go

#[test]
fn with_compat_preserves_empty_signature_thinking() {
    let payload = r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":""}]}]}"#;

    let without_compat = convert("deepseek-v4", payload);
    assert!(
        at(&without_compat, "messages.0.reasoning_content").is_none(),
        "{without_compat}"
    );
    let with_compat = convert_with_compat("deepseek-v4", payload);
    assert_eq!(
        text_at(&with_compat, "messages.0.reasoning_content"),
        "reason",
        "{with_compat}"
    );
}

#[test]
fn with_compat_preserves_thinking_with_tool_calls() {
    let out = convert_with_compat(
        "deepseek-v4",
        r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":""},{"type":"text","text":"Reading files."},{"type":"tool_use","id":"call_1","name":"Read","input":{"path":"main.go"}}]}]}"#,
    );
    assert_eq!(
        text_at(&out, "messages.0.reasoning_content"),
        "reason",
        "{out}"
    );
    assert!(at(&out, "messages.0.tool_calls").is_some(), "{out}");
}

#[test]
fn with_compat_does_not_add_reasoning_without_thinking() {
    let out = convert_with_compat(
        "deepseek-v4",
        r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"Read","input":{}}]}]}"#,
    );
    assert!(at(&out, "messages.0.reasoning_content").is_none(), "{out}");
    assert!(at(&out, "messages.0.tool_calls").is_some(), "{out}");
}

#[test]
fn with_compat_preserves_incompatible_thinking() {
    let out = convert_with_compat(
        "deepseek-v4",
        r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"claude#opaque"},{"type":"tool_use","id":"call_1","name":"Read","input":{}}]}]}"#,
    );
    assert_eq!(
        text_at(&out, "messages.0.reasoning_content"),
        "reason",
        "{out}"
    );
    assert!(at(&out, "messages.0.tool_calls").is_some(), "{out}");
}

#[test]
fn without_compat_does_not_add_reasoning_for_tool_calls() {
    let out = convert(
        "deepseek-v4",
        r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"Read","input":{}}]}]}"#,
    );
    assert!(at(&out, "messages.0.reasoning_content").is_none(), "{out}");
}

/// `ConvertClaudeRequestToOpenAIWithCompat`, with its refusal.
fn convert_with_compat_checked(
    model: &str,
    request: &str,
) -> (Value, Option<UnsupportedPartError>) {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    convert_claude_request_to_openai_with_compat(model, &request, false)
}

/// `TranslateRequestEnvelope` from Claude to Chat Completions: the body, or
/// the refusal.
fn claude_to_openai_envelope(model: &str, input: &str) -> Result<Value, UnsupportedPartError> {
    let request: Value = serde_json::from_str(input).expect("test request is valid JSON");
    crate::registry::Registry::global().translate_request_checked(
        &"claude".into(),
        &"openai".into(),
        model,
        request,
        false,
    )
}

// TestConvertClaudeRequestToOpenAI_UncachedFileKeepsOldDrop
#[test]
fn uncached_file_keeps_old_drop() {
    let input = r#"{"model":"gpt-5","messages":[{"role":"user","content":[{"type":"text","text":"read"},{"type":"container_upload","file_id":"file-absent"}]}]}"#;
    let output = convert("gpt-5", input).to_string();
    assert!(!output.contains("file_data"), "output = {output}");
    assert!(
        output.contains(r#""text":"read""#),
        "text was lost: {output}"
    );
}

// TestClaudeFileOnlyRequestSurfacesUnsupportedPart
#[test]
fn file_only_request_surfaces_unsupported_part() {
    let input = r#"{"model":"gpt-5","messages":[{"role":"user","content":[{"type":"container_upload","file_id":"file-absent"}]}]}"#;
    let err = claude_to_openai_envelope("gpt-5", input).expect_err("refused");
    assert!(err.to_string().contains("container_upload"), "err = {err}");
}

// TestClaudeBase64DocumentBecomesFilePart
#[test]
fn base64_document_becomes_file_part() {
    let input = r#"{"model":"gpt-5","messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0xLjQK"}}]}]}"#;
    let body = claude_to_openai_envelope("gpt-5", input).expect("sent");
    assert_eq!(
        text_at(&body, "messages.0.content.0.type"),
        "file",
        "{body}"
    );
    assert_eq!(
        text_at(&body, "messages.0.content.0.file.file_data"),
        "data:application/pdf;base64,JVBERi0xLjQK",
        "{body}"
    );
}

// TestClaudeTextWithUncachedFileKeepsText
#[test]
fn text_with_uncached_file_keeps_text() {
    let input = r#"{"model":"gpt-5","messages":[{"role":"user","content":[{"type":"text","text":"read"},{"type":"container_upload","file_id":"file-absent"}]}]}"#;
    let body = claude_to_openai_envelope("gpt-5", input).expect("sent");
    assert!(
        body.to_string().contains(r#""text":"read""#),
        "body = {body}"
    );
}

const USER_TURN_UPLOAD: &str = r#"{"type":"container_upload","file_id":"file-1"}"#;

// TestConvertClaudeRequestToOpenAI_RefusesAnyEmptiedUserTurn
#[test]
fn refuses_any_emptied_user_turn() {
    let cases = [
        (
            "history then attachment only",
            format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}},{{"role":"user","content":[{USER_TURN_UPLOAD}]}}]}}"#
            ),
            "container_upload",
        ),
        (
            "system prompt and system reminder",
            format!(
                r#"{{"model":"m","system":"sys","messages":[{{"role":"system","content":"reminder"}},{{"role":"user","content":[{USER_TURN_UPLOAD}]}}]}}"#
            ),
            "container_upload",
        ),
        (
            "emptied turn before a later text turn",
            format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":[{USER_TURN_UPLOAD}]}},{{"role":"assistant","content":[{{"type":"text","text":"ok"}}]}},{{"role":"user","content":"next"}}]}}"#
            ),
            "container_upload",
        ),
        (
            "document without bytes",
            r#"{"model":"m","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":[{"type":"document","source":{"type":"file","file_id":"file-1"}}]}]}"#.to_owned(),
            "document",
        ),
    ];
    for (name, input, want) in cases {
        let (body, err) = convert_with_compat_checked("m", &input);
        let err = err.unwrap_or_else(|| panic!("{name}: no refusal; body = {body}"));
        assert_eq!(err.part_type, want, "{name}");
        assert_eq!(err.status_code(), 400, "{name}");
        assert_eq!(
            err.to_string(),
            format!("unsupported content part: {want}"),
            "{name}"
        );
        assert!(body.is_object(), "{name}: {body}");
    }
}

// TestConvertClaudeRequestToOpenAI_KeepsTurnWithTextBesideAttachment
#[test]
fn keeps_turn_with_text_beside_attachment() {
    let input = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{{"type":"text","text":"keep me"}},{USER_TURN_UPLOAD}]}}]}}"#
    );
    let (body, err) = convert_with_compat_checked("m", &input);
    assert_eq!(err, None);
    assert_eq!(
        text_at(&body, "messages.2.content.0.text"),
        "keep me",
        "{body}"
    );
}

// TestConvertClaudeRequestToOpenAI_Base64DocumentAfterHistoryStaysAFilePart
#[test]
fn base64_document_after_history_stays_a_file_part() {
    let input = r#"{"model":"m","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0xLjQK"}}]}]}"#;
    let (body, err) = convert_with_compat_checked("m", input);
    assert_eq!(err, None);
    assert_eq!(
        text_at(&body, "messages.2.content.0.type"),
        "file",
        "{body}"
    );
    assert_eq!(
        text_at(&body, "messages.2.content.0.file.file_data"),
        "data:application/pdf;base64,JVBERi0xLjQK",
        "{body}"
    );
}

// TestConvertClaudeRequestToOpenAI_ToolResultKeepsTurnBesideUnsendableFile
#[test]
fn tool_result_keeps_turn_beside_unsendable_file() {
    let input = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"run"}},{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"t","input":{{}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}},{USER_TURN_UPLOAD}]}}]}}"#
    );
    let (body, err) = convert_with_compat_checked("m", &input);
    assert_eq!(err, None, "a tool result is a sendable part; body = {body}");
}

/// `claudeImageTurnToOpenAI`: two history turns, then a user turn of
/// `final_content`.
fn claude_image_turn_to_openai(final_content: &str) -> Result<Value, UnsupportedPartError> {
    let input = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"a"}},{{"role":"assistant","content":"b"}},{{"role":"user","content":[{final_content}]}}]}}"#
    );
    claude_to_openai_envelope("m", &input)
}

fn require_image_refusal(envelope: Result<Value, UnsupportedPartError>) {
    let err = envelope.expect_err("refused");
    assert_eq!(err.part_type, "image");
    assert_eq!(err.status_code(), 400);
    assert_eq!(err.to_string(), "unsupported content part: image");
}

// TestClaudeToOpenAIImageOnlyTurnKeepsTheImage
#[test]
fn image_only_turn_keeps_the_image() {
    let cases = [
        (
            "base64",
            r#"{"type":"base64","media_type":"image/png","data":"aGVsbG8="}"#,
            "data:image/png;base64,aGVsbG8=",
        ),
        (
            "http url",
            r#"{"type":"url","url":"https://example.test/a.png"}"#,
            "https://example.test/a.png",
        ),
    ];
    for (name, source, want) in cases {
        let body = claude_image_turn_to_openai(&format!(r#"{{"type":"image","source":{source}}}"#))
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(
            text_at(&body, "messages.2.content.0.type"),
            "image_url",
            "{name}: {body}"
        );
        assert_eq!(
            text_at(&body, "messages.2.content.0.image_url.url"),
            want,
            "{name}: {body}"
        );
    }
}

// TestClaudeToOpenAIFileImageOnlyTurnIsRefused
#[test]
fn file_image_only_turn_is_refused() {
    require_image_refusal(claude_image_turn_to_openai(
        r#"{"type":"image","source":{"type":"file","file_id":"file-1"}}"#,
    ));
}

// TestClaudeToOpenAIFileImageBesideTextStillSucceeds
#[test]
fn file_image_beside_text_still_succeeds() {
    let body = claude_image_turn_to_openai(
        r#"{"type":"text","text":"keep me"},{"type":"image","source":{"type":"file","file_id":"file-1"}}"#,
    )
    .expect("sent");
    let parts = at(&body, "messages.2.content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(parts.len(), 1, "text was lost or an image leaked: {body}");
    assert_eq!(str_of(parts[0].get("text")), "keep me", "{body}");
}

// TestClaudeToOpenAIEmptyTextDoesNotHideAFileImage
#[test]
fn empty_text_does_not_hide_a_file_image() {
    require_image_refusal(claude_image_turn_to_openai(
        r#"{"type":"text","text":""},{"type":"image","source":{"type":"file","file_id":"file-1"}}"#,
    ));
}

// Not upstream's: data that isn't base64 can't be sent, and data with line
// breaks is written again without them.
#[test]
fn document_data_is_read_as_go_reads_base64() {
    let turn = |data: &str| {
        format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":[{{"type":"text","text":"x"}},{{"type":"document","filename":"a.pdf","source":{{"type":"base64","media_type":"","data":"{data}"}}}}]}}]}}"#
        )
    };
    let body = convert("m", &turn("JVBE\\r\\nRi0xLjQK"));
    assert_eq!(
        at(&body, "messages.0.content.1"),
        Some(
            &json!({"type": "file", "file": {"filename": "a.pdf", "file_data": "data:application/octet-stream;base64,JVBERi0xLjQK"}})
        ),
        "{body}"
    );
    let body = convert("m", &turn("not base64!"));
    assert_eq!(array_len(&body, "messages.0.content"), 1, "{body}");
}

fn array_len(value: &Value, path: &str) -> usize {
    at(value, path)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}
