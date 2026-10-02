// Ported from CLIProxyAPI internal/translator/claude/openai/chat-completions/claude_openai_request_test.go
// and claude_openai_compat_test.go (v8.0.10, MIT). https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;

fn convert(model: &str, request: &Value) -> Value {
    convert_openai_chat_completions_request_to_claude(
        model,
        request,
        false,
        ModelCatalog::embedded(),
    )
}

fn convert_with_compat(model: &str, request: &Value) -> Value {
    convert_openai_chat_completions_request_to_claude_with_compat(
        model,
        request,
        false,
        ModelCatalog::embedded(),
    )
}

/// Looks up a dotted path such as `messages.0.content`, like a plain gjson path.
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

/// The value at `path` as gjson's `Array()` would return it: an array's items,
/// nothing for a missing or null value, or else the value alone.
fn array_at<'v>(value: &'v Value, path: &str) -> Vec<&'v Value> {
    match at(value, path) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// The first item of the array at `path` whose `key` is `wanted`, like the
/// gjson query `path.#(key==wanted)`.
fn find<'v>(value: &'v Value, path: &str, key: &str, wanted: &str) -> Option<&'v Value> {
    array_at(value, path)
        .into_iter()
        .find(|item| text_at(item, key) == wanted)
}

fn tool_names(out: &Value) -> Vec<String> {
    array_at(out, "tools")
        .into_iter()
        .map(|tool| text_at(tool, "name"))
        .collect()
}

#[test]
fn thinking_summary_visibility() {
    let cases = [
        (
            "effort only leaves display unspecified",
            json!({"reasoning_effort": "high", "messages": [{"role": "user", "content": "hi"}]}),
            "",
        ),
        (
            "explicit include enables summary",
            json!({"reasoning_effort": "high", "include_reasoning": true, "messages": [{"role": "user", "content": "hi"}]}),
            "summarized",
        ),
        (
            "explicit exclude omits summary",
            json!({"reasoning_effort": "high", "reasoning": {"exclude": true}, "messages": [{"role": "user", "content": "hi"}]}),
            "omitted",
        ),
    ];
    for (name, request, wanted) in cases {
        let out = convert("claude-opus-5-5", &request);
        assert_eq!(
            text_at(&out, "thinking.display"),
            wanted,
            "{name}: thinking.display; body={out}"
        );
    }
}

#[test]
fn with_compat_groups_assistant_thinking_text_and_tools() {
    let out = convert_with_compat(
        "claude-test",
        &json!({
            "messages": [
                {"role": "assistant", "reasoning_content": "reason", "content": "answer"},
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "first", "arguments": "{}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "second", "arguments": "{}"}}
                    ]
                }
            ]
        }),
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 1, "message count: {out}");
    let types: Vec<String> = array_at(messages[0], "content")
        .into_iter()
        .map(|block| text_at(block, "type"))
        .collect();
    assert_eq!(
        types,
        ["thinking", "text", "tool_use", "tool_use"],
        "content types: {out}"
    );
}

#[test]
fn merges_tool_result_with_adjacent_user_content() {
    let out = convert(
        "claude-test",
        &json!({
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "work", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                {"role": "user", "content": "continue"}
            ]
        }),
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 2, "message count: {out}");
    assert_eq!(
        *messages[1],
        json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"},
                {"type": "text", "text": "continue"}
            ]
        }),
        "{out}"
    );
}

#[test]
fn system_does_not_break_user_turn_and_cache_boundary() {
    let out = convert(
        "claude-test",
        &json!({
            "messages": [
                {"role": "user", "content": "first", "cache_control": {"type": "ephemeral"}},
                {"role": "system", "content": "system rule"},
                {"role": "user", "content": "second"}
            ]
        }),
    );
    assert_eq!(
        out["messages"],
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "first", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "second"}
            ]
        }]),
        "{out}"
    );
    assert_eq!(
        out["system"],
        json!([{"type": "text", "text": "system rule"}]),
        "{out}"
    );
}

#[test]
fn sanitizes_tool_call_ids_for_claude() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call.with space:1",
                            "type": "function",
                            "function": {
                                "name": "Read",
                                "arguments": r#"{"path":"README.md"}"#
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call.with space:1",
                    "content": "ok"
                }
            ]
        }),
    );
    assert_eq!(
        text_at(&out, "messages.0.content.0.id"),
        "call_with_space_1",
        "tool_use id: {out}"
    );
    assert_eq!(
        text_at(&out, "messages.1.content.0.tool_use_id"),
        "call_with_space_1",
        "tool_result tool_use_id should be the same sanitized id: {out}"
    );
}

#[test]
fn groups_consecutive_parallel_tool_results() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "user", "content": "Use both tools."},
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "tool_a", "arguments": "{}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "tool_b", "arguments": "{}"}}
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": "one",
                    "cache_control": {"type": "ephemeral"}
                },
                {"role": "tool", "tool_call_id": "call_2", "content": "two"},
                {"role": "assistant", "content": "Done."}
            ]
        }),
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 4, "messages: {}", out["messages"]);
    assert_eq!(
        *messages[2],
        json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": "one",
                    "cache_control": {"type": "ephemeral"}
                },
                {"type": "tool_result", "tool_use_id": "call_2", "content": "two"}
            ]
        }),
        "grouped tool results: {out}"
    );
    assert_eq!(
        text_at(messages[3], "content.0.text"),
        "Done.",
        "following assistant message: {out}"
    );
}

#[test]
fn drops_temperature() {
    let out = convert(
        "claude-sonnet-5",
        &json!({
            "model": "gpt-4.1",
            "temperature": 0.2,
            "top_p": 0.8,
            "messages": [
                {"role": "user", "content": "hi"}
            ]
        }),
    );
    assert!(
        at(&out, "temperature").is_none(),
        "temperature should be removed: {out}"
    );
    assert_eq!(out["top_p"], json!(0.8), "{out}");
}

#[test]
fn tool_result_text_and_base64_image() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "do_work",
                                "arguments": r#"{"a":1}"#
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": [
                        {"type": "text", "text": "tool ok"},
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
                            }
                        }
                    ]
                }
            ]
        }),
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 2, "messages: {}", out["messages"]);
    assert_eq!(
        messages[1]["content"][0],
        json!({
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
        }),
        "{out}"
    );
}

#[test]
fn tool_result_url_image_only() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "do_work",
                                "arguments": r#"{"a":1}"#
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": "https://example.com/tool.png"
                            }
                        }
                    ]
                }
            ]
        }),
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 2, "messages: {}", out["messages"]);
    assert_eq!(
        messages[1]["content"][0]["content"],
        json!([{"type": "image", "source": {"type": "url", "url": "https://example.com/tool.png"}}]),
        "{out}"
    );
}

#[test]
fn system_role_becomes_top_level_system() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello"}
            ]
        }),
    );
    assert_eq!(
        out["system"],
        json!([{"type": "text", "text": "You are a helpful assistant."}]),
        "{out}"
    );
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}]),
        "{out}"
    );
}

#[test]
fn multiple_system_messages_merged_into_top_level_system() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "system", "content": "Rule 1"},
                {"role": "system", "content": [{"type": "text", "text": "Rule 2"}]},
                {"role": "user", "content": "Hello"}
            ]
        }),
    );
    assert_eq!(
        out["system"],
        json!([
            {"type": "text", "text": "Rule 1"},
            {"type": "text", "text": "Rule 2"}
        ]),
        "{out}"
    );
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}]),
        "{out}"
    );
}

#[test]
fn system_only_input_keeps_fallback_user_message() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."}
            ]
        }),
    );
    assert_eq!(
        out["system"],
        json!([{"type": "text", "text": "You are a helpful assistant."}]),
        "{out}"
    );
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": ""}]}]),
        "{out}"
    );
}

#[test]
fn preserves_content_part_cache_control() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "cached prefix", "cache_control": {"type": "ephemeral"}},
                        {"type": "text", "text": "fresh question"}
                    ]
                }
            ]
        }),
    );
    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type": "text", "text": "cached prefix", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "fresh question"}
        ]),
        "{out}"
    );
}

#[test]
fn preserves_message_level_cache_control() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "user",
                    "content": "cache me",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ]
        }),
    );
    assert_eq!(
        out["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"}),
        "{out}"
    );
}

#[test]
fn preserves_tool_cache_control() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "description": "Lookup something",
                        "parameters": {"type": "object", "properties": {}}
                    },
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        }),
    );
    assert_eq!(
        text_at(&out, "tools.0.cache_control.type"),
        "ephemeral",
        "{out}"
    );
    assert_eq!(text_at(&out, "tools.0.name"), "lookup", "{out}");
}

#[test]
fn normalizes_root_tool_schema_unions() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "without_type",
                        "parameters": {
                            "anyOf": [
                                {"type": "object", "properties": {"a": {"type": "string"}}},
                                {"type": "object", "properties": {"b": {"type": "string"}}}
                            ]
                        }
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "constraint_union",
                        "parametersJsonSchema": {
                            "type": "object",
                            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
                            "anyOf": [{"required": ["a"]}, {"required": ["b"]}]
                        }
                    }
                }
            ]
        }),
    );
    for tool_name in ["without_type", "constraint_union"] {
        let schema = find(&out, "tools", "name", tool_name)
            .and_then(|tool| tool.get("input_schema"))
            .unwrap_or(&Value::Null);
        assert_eq!(
            text_at(schema, "type"),
            "object",
            "{tool_name} input_schema.type: {out}"
        );
        assert!(
            at(schema, "anyOf").is_none(),
            "{tool_name} input_schema should not contain root anyOf: {out}"
        );
        assert!(
            at(schema, "properties.a").is_some() && at(schema, "properties.b").is_some(),
            "{tool_name} input_schema should contain properties a and b: {out}"
        );
        assert!(
            at(schema, "required").is_none(),
            "{tool_name} input_schema should not merge alternative required fields: {out}"
        );
    }
}

#[test]
fn part_cache_control_wins_over_message_level() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {
                    "role": "user",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                    "content": [
                        {"type": "text", "text": "part cached", "cache_control": {"type": "ephemeral"}}
                    ]
                }
            ]
        }),
    );
    assert_eq!(
        out["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"}),
        "part-level cache_control should win: {out}"
    );
}

#[test]
fn developer_role_becomes_top_level_system() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "system", "content": "S1"},
                {"role": "developer", "content": [{"type": "text", "text": "D1"}, {"type": "text", "text": "D2"}]},
                {"role": "user", "content": "Hello"}
            ]
        }),
    );
    assert_eq!(
        out["system"],
        json!([
            {"type": "text", "text": "S1"},
            {"type": "text", "text": "D1"},
            {"type": "text", "text": "D2"}
        ]),
        "{out}"
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 1, "messages: {}", out["messages"]);
    assert_eq!(text_at(messages[0], "role"), "user", "{out}");
}

#[test]
fn developer_message_cache_control_applies_to_last_block() {
    let out = convert(
        "claude-sonnet-4-5",
        &json!({
            "model": "gpt-4.1",
            "messages": [
                {"role": "developer", "content": [{"type": "text", "text": "D1"}, {"type": "text", "text": "D2"}], "cache_control": {"type": "ephemeral"}},
                {"role": "user", "content": "Hello"}
            ]
        }),
    );
    assert_eq!(
        out["system"],
        json!([
            {"type": "text", "text": "D1"},
            {"type": "text", "text": "D2", "cache_control": {"type": "ephemeral"}}
        ]),
        "{out}"
    );
}

#[test]
fn deduplicates_tool_results() {
    let out = convert(
        "claude-test",
        &json!({
            "messages": [
                {"role": "user", "content": "Run tools"},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_dup", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_dup", "content": "first output"},
                {"role": "assistant", "content": "Next step", "tool_calls": [
                    {"id": "call_other", "type": "function", "function": {"name": "search", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_dup", "content": "final output"},
                {"role": "tool", "tool_call_id": "call_other", "content": "search output"},
                {"role": "tool", "tool_call_id": "", "content": "empty id output"}
            ]
        }),
    );
    let messages = array_at(&out, "messages");
    assert!(messages.len() >= 5, "expected at least 5 messages: {out}");

    // The assistant's call_dup.
    assert_eq!(text_at(messages[1], "content.0.id"), "call_dup", "{out}");

    // call_dup's last result, in the place of its first.
    assert_eq!(
        messages[2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "call_dup", "content": "final output"}),
        "{out}"
    );

    assert_eq!(text_at(messages[3], "content.0.text"), "Next step", "{out}");
    assert_eq!(text_at(messages[3], "content.1.id"), "call_other", "{out}");

    // call_other's result and the empty id's, without call_dup again.
    let blocks = array_at(messages[4], "content");
    assert_eq!(blocks.len(), 2, "tool_result blocks in message 4: {out}");
    assert_eq!(text_at(blocks[0], "tool_use_id"), "call_other", "{out}");
    assert_eq!(text_at(blocks[0], "content"), "search output", "{out}");
    assert_eq!(text_at(blocks[1], "content"), "empty id output", "{out}");
    // An empty id gets `toolu_<nanos>_<counter>`.
    let generated = text_at(blocks[1], "tool_use_id");
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    assert!(
        generated
            .strip_prefix("toolu_")
            .and_then(|rest| rest.split_once('_'))
            .is_some_and(|(nanos, counter)| digits(nanos) && digits(counter)),
        "generated tool_use_id {generated:?}: {out}"
    );
}

#[test]
fn max_tokens_and_max_completion_tokens() {
    let cases = [
        (
            "only max_completion_tokens",
            json!({"messages": [{"role": "user", "content": "hi"}], "max_completion_tokens": 128000}),
            128000,
        ),
        (
            "only max_tokens",
            json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 4096}),
            4096,
        ),
        (
            "both present prefers max_tokens",
            json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 4096, "max_completion_tokens": 128000}),
            4096,
        ),
        (
            "neither present uses default template limit",
            json!({"messages": [{"role": "user", "content": "hi"}]}),
            32000,
        ),
    ];
    for (name, request, want_limit) in cases {
        let out = convert("claude-3-7-sonnet-20250219", &request);
        assert_eq!(out["max_tokens"], json!(want_limit), "{name}: {out}");
    }
}

#[test]
fn preserves_caller_supplied_metadata_user_id() {
    let cases = [
        (
            "plain string",
            json!({"model": "claude-test", "metadata": {"user_id": "custom-user-123"}, "messages": [{"role": "user", "content": "hello"}]}),
            "custom-user-123",
        ),
        (
            "special characters and json string",
            json!({"model": "claude-test", "metadata": {"user_id": "foo\"bar\nbaz\\qux"}, "messages": [{"role": "user", "content": "hello"}]}),
            "foo\"bar\nbaz\\qux",
        ),
        (
            "claude code json format",
            json!({"model": "claude-test", "metadata": {"user_id": r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#}, "messages": [{"role": "user", "content": "hello"}]}),
            r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#,
        ),
    ];
    for (name, request, expected) in cases {
        let out = convert("claude-test", &request);
        assert_eq!(
            out["metadata"],
            json!({"user_id": expected}),
            "{name}: {out}"
        );
    }
}

#[test]
fn preserves_openai_user_field() {
    let out = convert(
        "claude-test",
        &json!({"model": "claude-test", "user": "openai-user-456", "messages": [{"role": "user", "content": "hello"}]}),
    );
    assert_eq!(
        out["metadata"],
        json!({"user_id": "openai-user-456"}),
        "{out}"
    );
}

// Upstream derives a user ID from the session key. We don't make up user IDs,
// so neither request gets one.
#[test]
fn different_sessions_produce_different_user_ids() {
    for session in ["session-a", "session-b"] {
        let out = convert(
            "claude-test",
            &json!({"model": "claude-test", "prompt_cache_key": session, "messages": [{"role": "user", "content": "hello"}]}),
        );
        assert_eq!(out["metadata"], json!({}), "{session}: {out}");
    }
}

// Upstream derives a user ID from the first message. We don't make up user
// IDs, so neither request gets one.
#[test]
fn deterministic_without_session_key() {
    let first = json!({"model": "claude-test", "messages": [{"role": "user", "content": "stable first message"}]});
    let second = json!({"model": "claude-test", "messages": [
        {"role": "user", "content": "stable first message"},
        {"role": "assistant", "content": "hi"},
        {"role": "user", "content": "second message"}
    ]});
    for request in [first, second] {
        let out = convert("claude-test", &request);
        assert_eq!(out["metadata"], json!({}), "{out}");
    }
}

#[test]
fn response_format_json_schema() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "Extract facts from: Yesterday it rained in Beijing."}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "extracted_facts",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "facts": {
                                "type": "array",
                                "items": {"type": "string"}
                            }
                        },
                        "required": ["facts"]
                    }
                }
            }
        }),
    );
    assert!(
        at(&out, "system").is_some_and(Value::is_array),
        "system blocks missing: {out}"
    );
    let system = array_at(&out, "system");
    assert!(!system.is_empty(), "system blocks empty: {out}");
    assert!(
        system.iter().any(|block| {
            let text = text_at(block, "text");
            text.contains("JSON") && text.contains("facts")
        }),
        "expected structured output instructions containing schema in system prompt: {out}"
    );
}

#[test]
fn response_format_json_object() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "Return a JSON object."}],
            "response_format": {
                "type": "json_object"
            }
        }),
    );
    assert!(
        at(&out, "system").is_some_and(Value::is_array),
        "system blocks missing: {out}"
    );
    let system = array_at(&out, "system");
    assert!(!system.is_empty(), "system blocks empty: {out}");
    assert!(
        system
            .iter()
            .any(|block| text_at(block, "text").contains("JSON object")),
        "expected JSON object instruction in system prompt: {out}"
    );
}

#[test]
fn response_format_preserves_existing_system() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "system", "content": "Custom operator instruction."},
                {"role": "user", "content": "Extract facts."}
            ],
            "response_format": {
                "type": "json_object"
            }
        }),
    );
    assert!(
        at(&out, "system").is_some_and(Value::is_array),
        "system blocks missing: {out}"
    );
    let system = array_at(&out, "system");
    assert!(
        system.len() >= 2,
        "expected at least 2 system blocks (original + response_format): {out}"
    );
    let has = |needle: &str| {
        system
            .iter()
            .any(|block| text_at(block, "text").contains(needle))
    };
    assert!(
        has("Custom operator instruction.") && has("JSON object"),
        "expected both original system and response_format instruction: {out}"
    );
}

#[test]
fn response_format_absent_or_text_no_op() {
    let cases = [
        (
            "absent",
            json!({"model": "claude-sonnet-4-6", "messages": [{"role": "user", "content": "plain text"}]}),
        ),
        (
            "type text",
            json!({"model": "claude-sonnet-4-6", "messages": [{"role": "user", "content": "plain text"}], "response_format": {"type": "text"}}),
        ),
    ];
    for (name, request) in cases {
        let out = convert("claude-sonnet-4-6", &request);
        assert!(
            at(&out, "system").is_none(),
            "{name}: system blocks should not be created when response_format is absent or text: {out}"
        );
    }
}

// Anthropic only accepts cache_control on the tool_result block itself, never
// inside tool_result.content. A part-level marker on an OpenAI tool message
// must be hoisted to the block, while ordinary message parts keep theirs.
#[test]
fn tool_result_part_cache_control_hoisted() {
    let out = convert(
        "claude-test",
        &json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Use calc for 2+2.", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "calc", "arguments": r#"{"expr":"2+2"}"#}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "4", "cache_control": {"type": "ephemeral"}}]}
            ],
            "tools": [{"type": "function", "function": {"name": "calc", "description": "calc", "parameters": {"type": "object", "properties": {"expr": {"type": "string"}}, "required": ["expr"]}}}]
        }),
    );
    let messages = array_at(&out, "messages");
    assert_eq!(messages.len(), 3, "message count: {out}");

    assert_eq!(
        text_at(messages[0], "content.0.cache_control.type"),
        "ephemeral",
        "user text part cache_control must not be stripped: {out}"
    );

    // The marker moves to the tool_result block; the parts inside carry none.
    assert_eq!(
        messages[2]["content"][0],
        json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": [{"type": "text", "text": "4"}],
            "cache_control": {"type": "ephemeral"}
        }),
        "{out}"
    );
}

#[test]
fn tool_choice() {
    struct Case {
        name: &'static str,
        request: Value,
        tool_choice: Option<Value>,
        /// The tools left after `allowed_tools`, where the case checks them.
        tools: Option<&'static [&'static str]>,
    }
    let cases = [
        Case {
            name: "none produces type none",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "Answer without calling tools."}],
                "tool_choice": "none",
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}},
                    {"type": "function", "function": {"name": "tool_b", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "none"})),
            tools: None,
        },
        Case {
            name: "object none produces type none",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "Answer without calling tools."}],
                "tool_choice": {"type": "none"},
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "none"})),
            tools: None,
        },
        Case {
            name: "allowed_tools filters tools and sets auto mode",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "Use tool_b"}],
                "tool_choice": {
                    "type": "allowed_tools",
                    "allowed_tools": {
                        "mode": "auto",
                        "tools": [{"type": "function", "function": {"name": "tool_b"}}]
                    }
                },
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}},
                    {"type": "function", "function": {"name": "tool_b", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "auto"})),
            tools: Some(&["tool_b"]),
        },
        Case {
            name: "allowed_tools multi function filters tools and supports required mode",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "Use tools"}],
                "tool_choice": {
                    "type": "allowed_tools",
                    "allowed_tools": {
                        "mode": "required",
                        "tools": [
                            {"type": "function", "function": {"name": "tool_b"}},
                            {"type": "function", "function": {"name": "tool_c"}}
                        ]
                    }
                },
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}},
                    {"type": "function", "function": {"name": "tool_b", "parameters": {"type": "object", "properties": {}}}},
                    {"type": "function", "function": {"name": "tool_c", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "any"})),
            tools: Some(&["tool_b", "tool_c"]),
        },
        Case {
            name: "parallel_tool_calls false adds disable_parallel_tool_use",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "tool_choice": "required",
                "parallel_tool_calls": false,
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "any", "disable_parallel_tool_use": true})),
            tools: None,
        },
        Case {
            name: "parallel_tool_calls null does not add disable_parallel_tool_use",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "tool_choice": "required",
                "parallel_tool_calls": null,
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "any"})),
            tools: None,
        },
        Case {
            name: "parallel_tool_calls true does not add disable_parallel_tool_use",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "tool_choice": "required",
                "parallel_tool_calls": true,
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "any"})),
            tools: None,
        },
        Case {
            name: "omitted tool_choice with parallel_tool_calls false sets auto with disable_parallel_tool_use",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "parallel_tool_calls": false,
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "auto", "disable_parallel_tool_use": true})),
            tools: None,
        },
        Case {
            name: "empty allowed_tools fails closed to type none",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "tool_choice": {
                    "type": "allowed_tools",
                    "allowed_tools": {
                        "tools": []
                    }
                },
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "none"})),
            tools: None,
        },
        Case {
            name: "function choice with missing name fails closed to type none",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "tool_choice": {"type": "function", "function": {}},
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: Some(json!({"type": "none"})),
            tools: None,
        },
        Case {
            name: "tool_choice null does not set tool_choice",
            request: json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "test"}],
                "tool_choice": null,
                "tools": [
                    {"type": "function", "function": {"name": "tool_a", "parameters": {"type": "object", "properties": {}}}}
                ]
            }),
            tool_choice: None,
            tools: None,
        },
    ];
    for case in cases {
        let name = case.name;
        let out = convert("claude-sonnet-4-6", &case.request);
        assert_eq!(
            at(&out, "tool_choice"),
            case.tool_choice.as_ref(),
            "{name}: {out}"
        );
        if let Some(tools) = case.tools {
            assert_eq!(tool_names(&out), tools, "{name}: {out}");
        }
    }
}

#[test]
fn tool_strict() {
    // (name, tool, expected tools.0.strict)
    let cases = [
        (
            "preserves strict true on function tool",
            json!({
                "type": "function",
                "function": {
                    "name": "tool_a",
                    "description": "Controlled tool.",
                    "strict": true,
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            Some(true),
        ),
        (
            "preserves strict true when on top level tool",
            json!({
                "type": "function",
                "strict": true,
                "function": {
                    "name": "tool_b",
                    "description": "Controlled tool.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            Some(true),
        ),
        (
            "preserves strict false on function tool",
            json!({
                "type": "function",
                "function": {
                    "name": "tool_c",
                    "description": "Controlled tool.",
                    "strict": false,
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            Some(false),
        ),
        (
            "omits strict when not provided",
            json!({
                "type": "function",
                "function": {
                    "name": "tool_d",
                    "description": "Controlled tool.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            None,
        ),
    ];
    for (name, tool, strict) in cases {
        let out = convert(
            "claude-sonnet-4-6",
            &json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [tool]
            }),
        );
        assert_eq!(
            at(&out, "tools.0.strict"),
            strict.map(Value::Bool).as_ref(),
            "{name}: {out}"
        );
    }
}

#[test]
fn sanitizes_tool_names_and_provides_fallback_schema() {
    let out = convert(
        "claude-sonnet-4-6",
        &json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {
                    "role": "assistant",
                    "content": "calling tool",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "mcp.server.special:get_time",
                                "arguments": "{}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": "12:00 PM"
                },
                {
                    "role": "user",
                    "content": "continue"
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "mcp.server.special:get_time",
                        "description": "Get current time"
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "clean_tool",
                        "description": "Parameterless clean tool"
                    }
                }
            ],
            "tool_choice": {
                "type": "function",
                "function": {
                    "name": "mcp.server.special:get_time"
                }
            }
        }),
    );

    // Declared tool names are sanitized.
    assert_eq!(
        text_at(&out, "tools.0.name"),
        "mcp_server_special_get_time",
        "{out}"
    );

    // A tool without parameters gets an object schema.
    for tool in ["tools.0", "tools.1"] {
        assert_eq!(
            text_at(&out, &format!("{tool}.input_schema.type")),
            "object",
            "{tool}.input_schema: {out}"
        );
    }

    // So is the name in an earlier assistant turn's tool_use.
    assert_eq!(
        text_at(&out, "messages.0.content.1.name"),
        "mcp_server_special_get_time",
        "{out}"
    );

    // And the name in tool_choice.
    assert_eq!(
        out["tool_choice"],
        json!({"type": "tool", "name": "mcp_server_special_get_time"}),
        "{out}"
    );
}

#[test]
fn with_compat_preserves_reasoning_content() {
    let request = json!({"messages": [{"role": "assistant", "content": "answer", "reasoning_content": "reason"}]});

    let without_compat = convert("deepseek-v4", &request);
    assert!(
        find(&without_compat, "messages.0.content", "type", "thinking").is_none(),
        "default translation preserved reasoning_content: {without_compat}"
    );

    let with_compat = convert_with_compat("deepseek-v4", &request);
    assert_eq!(
        find(&with_compat, "messages.0.content", "type", "thinking"),
        Some(&json!({"type": "thinking", "thinking": "reason", "signature": ""})),
        "compat translation missing unsigned thinking block: {with_compat}"
    );
}
