// Ported from CLIProxyAPI internal/translator/codex/openai/responses/codex_openai-responses_request_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;
use crate::json::{bool_of, int_of};

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

/// The value at `path` as gjson's `Int()` would return it.
fn int_at(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
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

#[test]
fn convert_system_role_to_developer_basic_conversion() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": "You are a pirate."}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Say hello."}]
                }
            ]
        }),
    );

    assert_eq!(text_at(&out, "input.0.role"), "developer", "{out}");
    assert_eq!(text_at(&out, "input.1.role"), "user", "{out}");
    assert_eq!(
        text_at(&out, "input.0.content.0.text"),
        "You are a pirate.",
        "{out}"
    );
}

#[test]
fn convert_system_role_to_developer_multiple_system_messages() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": "You are helpful."}]
                },
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": "Be concise."}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                }
            ]
        }),
    );

    assert_eq!(text_at(&out, "input.0.role"), "developer", "{out}");
    assert_eq!(text_at(&out, "input.1.role"), "developer", "{out}");
    assert_eq!(text_at(&out, "input.2.role"), "user", "{out}");
}

#[test]
fn convert_system_role_to_developer_no_system_messages() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hi there!"}]
                }
            ]
        }),
    );

    assert_eq!(text_at(&out, "input.0.role"), "user", "{out}");
    assert_eq!(text_at(&out, "input.1.role"), "assistant", "{out}");
}

#[test]
fn convert_system_role_to_developer_empty_input() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": []
        }),
    );

    assert!(
        at(&out, "input").is_some_and(Value::is_array),
        "input should still be an array: {out}"
    );
    assert_eq!(array_at(&out, "input").len(), 0, "{out}");
}

#[test]
fn convert_system_role_to_developer_no_input_field() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "stream": false
        }),
    );

    assert!(
        bool_at(&out, "stream"),
        "stream should be set to true by conversion: {out}"
    );
    assert!(
        !bool_at(&out, "store"),
        "store should be set to false by conversion: {out}"
    );
}

#[test]
fn original_issue() {
    // The exact input that was failing with "System messages are not allowed".
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": "You are a pirate. Always respond in pirate speak."
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": "Say hello."
                }
            ],
            "stream": false
        }),
    );

    assert_eq!(text_at(&out, "input.0.role"), "developer", "{out}");
    assert!(bool_at(&out, "stream"), "stream should be true: {out}");
    assert!(!bool_at(&out, "store"), "store should be false: {out}");
    assert!(
        bool_at(&out, "parallel_tool_calls"),
        "parallel_tool_calls should be true: {out}"
    );

    let include = array_at(&out, "include");
    assert!(
        at(&out, "include").is_some_and(Value::is_array) && include.len() == 1,
        "include should be an array with one element: {out}"
    );
    assert_eq!(
        str_of(Some(include[0])),
        "reasoning.encrypted_content",
        "{out}"
    );
}

// Go also checks the bytes come back uncopied. Rust takes the request by value,
// so this compares the text, which covers key order too.
#[test]
fn reuses_normalized_payload() {
    let raw = r#"{"model":"gpt-5.6","stream":true,"store":false,"parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"service_tier":"priority","input":[{"type":"message","role":"user","content":"hello"}]}"#;

    let out =
        convert_openai_responses_request_to_codex("gpt-5.6", serde_json::from_str(raw).unwrap());

    assert_eq!(out.to_string(), raw, "normalized request changed");
}

// TestConvertOpenAIResponsesRequestToCodex_PreservesWebSearchSourcesInclude.
// The translator takes no stream flag here, so Go's two passes are one. Go
// also checks the caller's bytes are unchanged; Rust takes the request by
// value.
#[test]
fn preserves_web_search_sources_include() {
    let reasoning_only = ["reasoning.encrypted_content"].as_slice();
    let reasoning_and_sources = [
        "reasoning.encrypted_content",
        "web_search_call.action.sources",
    ]
    .as_slice();
    let cases = [
        ("missing include", "", reasoning_only),
        ("null include", "null", reasoning_only),
        (
            "string include",
            r#""web_search_call.action.sources""#,
            reasoning_only,
        ),
        (
            "object include",
            r#"{"web_search_call.action.sources":true}"#,
            reasoning_only,
        ),
        ("non-string entries", "[42,true]", reasoning_only),
        ("empty array", "[]", reasoning_only),
        (
            "sources alone",
            r#"["web_search_call.action.sources"]"#,
            reasoning_and_sources,
        ),
        (
            "reasoning then sources",
            r#"["reasoning.encrypted_content","web_search_call.action.sources"]"#,
            reasoning_and_sources,
        ),
        (
            "sources before reasoning",
            r#"["web_search_call.action.sources","reasoning.encrypted_content"]"#,
            reasoning_and_sources,
        ),
        (
            "duplicate sources",
            r#"["web_search_call.action.sources","web_search_call.action.sources"]"#,
            reasoning_and_sources,
        ),
        (
            "unsupported entries filtered",
            r#"["file_search_call.results","web_search_call.action.sources","code_interpreter_call.outputs"]"#,
            reasoning_and_sources,
        ),
        (
            "non-string entries alongside sources",
            r#"[42,"web_search_call.action.sources",null]"#,
            reasoning_and_sources,
        ),
    ];
    for (name, include, want) in cases {
        let mut input = json!({
            "model": "gpt-5.6",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        });
        if !include.is_empty() {
            input["include"] = serde_json::from_str(include).unwrap();
        }
        let out = convert_openai_responses_request_to_codex("gpt-5.6", input);
        assert_eq!(at(&out, "include"), Some(&json!(want)), "{name}: {out}");
    }
}

// TestConvertOpenAIResponsesRequestToCodex_WebSearchToolDoesNotOptIntoSources
#[test]
fn web_search_tool_does_not_opt_into_sources() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.6",
        json!({
            "model": "gpt-5.6",
            "input": "find python asyncio docs",
            "tools": [{"type": "web_search"}],
            "tool_choice": "required"
        }),
    );
    assert_eq!(
        at(&out, "include"),
        Some(&json!(["reasoning.encrypted_content"])),
        "{out}"
    );
}

// TestConvertOpenAIResponsesRequestToCodexReusesNormalizedPayloadWithSources.
// As in reuses_normalized_payload, this compares the text; the request comes
// parsed, so the spaces the client wrote aren't kept.
#[test]
fn reuses_normalized_payload_with_sources() {
    for include in [
        r#"["reasoning.encrypted_content","web_search_call.action.sources"]"#,
        r#"[ "reasoning.encrypted_content" , "web_search_call.action.sources" ]"#,
        r#"[ "reasoning.encrypted_content" ]"#,
    ] {
        let raw = format!(
            r#"{{"model":"gpt-5.6","stream":true,"store":false,"parallel_tool_calls":true,"include":{include},"service_tier":"priority","input":[{{"type":"message","role":"user","content":"hello"}}]}}"#
        );
        let input: Value = serde_json::from_str(&raw).unwrap();
        let want = input.to_string();
        let out = convert_openai_responses_request_to_codex("gpt-5.6", input);
        assert_eq!(
            out.to_string(),
            want,
            "normalized request changed: {include}"
        );
    }
}

#[test]
fn normalizes_required_fields() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.6",
        json!({
            "model": "gpt-5.6",
            "stream": "true",
            "store": true,
            "parallel_tool_calls": false,
            "include": ["file_search_call.results", "reasoning.encrypted_content"],
            "max_output_tokens": 4096,
            "max_completion_tokens": 4096,
            "temperature": 0.2,
            "top_p": 0.9,
            "service_tier": "standard",
            "truncation": "auto",
            "prompt_cache_options": {"mode": "implicit"},
            "prompt_cache_retention": "24h",
            "user": "request-owner",
            "input": [{"type": "message", "role": "system", "content": "hello"}]
        }),
    );

    assert_eq!(at(&out, "stream"), Some(&Value::Bool(true)), "{out}");
    assert_eq!(at(&out, "store"), Some(&Value::Bool(false)), "{out}");
    assert_eq!(
        at(&out, "parallel_tool_calls"),
        Some(&Value::Bool(true)),
        "{out}"
    );
    let include = array_at(&out, "include");
    assert!(
        include.len() == 1 && include[0].as_str() == Some("reasoning.encrypted_content"),
        "include should be reasoning.encrypted_content only: {out}"
    );
    assert_eq!(text_at(&out, "input.0.role"), "developer", "{out}");
    for path in [
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "service_tier",
        "truncation",
        "prompt_cache_options",
        "prompt_cache_retention",
        "user",
    ] {
        assert!(at(&out, path).is_none(), "{path} should be removed: {out}");
    }
}

#[test]
fn filters_prompt_cache_retention() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.6-terra",
        json!({
            "model": "gpt-5.6-terra",
            "prompt_cache_retention": "24h",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "hello"
                        }
                    ]
                }
            ]
        }),
    );

    assert!(
        at(&out, "prompt_cache_retention").is_none(),
        "prompt_cache_retention should be removed: {out}"
    );
}

#[test]
fn convert_system_role_to_developer_assistant_role() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": "You are helpful."}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hi!"}]
                }
            ]
        }),
    );

    assert_eq!(text_at(&out, "input.0.role"), "developer", "{out}");
    assert_eq!(text_at(&out, "input.1.role"), "user", "{out}");
    assert_eq!(text_at(&out, "input.2.role"), "assistant", "{out}");
}

#[test]
fn normalizes_web_search_preview() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.4-mini",
        json!({
            "model": "gpt-5.4-mini",
            "input": "find latest OpenAI model news",
            "tools": [
                {"type": "web_search_preview_2025_03_11"}
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "tools": [
                    {"type": "web_search_preview"},
                    {"type": "web_search_preview_2025_03_11"}
                ]
            }
        }),
    );

    assert_eq!(text_at(&out, "tools.0.type"), "web_search", "{out}");
    assert_eq!(text_at(&out, "tool_choice.type"), "allowed_tools", "{out}");
    assert_eq!(
        text_at(&out, "tool_choice.tools.0.type"),
        "web_search",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "tool_choice.tools.1.type"),
        "web_search",
        "{out}"
    );
}

#[test]
fn normalizes_top_level_tool_choice_preview_alias() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.4-mini",
        json!({
            "model": "gpt-5.4-mini",
            "input": "find latest OpenAI model news",
            "tool_choice": {"type": "web_search_preview_2025_03_11"}
        }),
    );

    assert_eq!(text_at(&out, "tool_choice.type"), "web_search", "{out}");
}

#[test]
fn user_field_deletion() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "user": "test-user",
            "input": [{"role": "user", "content": "Hello"}]
        }),
    );

    assert!(
        at(&out, "user").is_none(),
        "user field should be deleted: {out}"
    );
}

#[test]
fn context_management_compaction_compatibility() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "context_management": [
                {
                    "type": "compaction",
                    "compact_threshold": 12000
                }
            ],
            "input": [{"role": "user", "content": "hello"}]
        }),
    );

    assert!(
        at(&out, "context_management").is_none(),
        "context_management should be removed for Codex compatibility: {out}"
    );
    assert!(
        at(&out, "truncation").is_none(),
        "truncation should be removed for Codex compatibility: {out}"
    );
}

#[test]
fn truncation_removed_for_codex_compatibility() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "truncation": "disabled",
            "input": [{"role": "user", "content": "hello"}]
        }),
    );

    assert!(
        at(&out, "truncation").is_none(),
        "truncation should be removed for Codex compatibility: {out}"
    );
}

#[test]
fn strip_codex_responses_cache_breakpoints() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "Hello world",
                            "prompt_cache_breakpoint": {"mode": "explicit"}
                        },
                        {
                            "type": "input_text",
                            "text": "Second part"
                        }
                    ]
                }
            ]
        }),
    );

    assert!(
        !out.to_string().contains("prompt_cache_breakpoint"),
        "prompt_cache_breakpoint should not exist in the output JSON: {out}"
    );
    assert_eq!(
        text_at(&out, "input.0.content.0.text"),
        "Hello world",
        "text content should be preserved"
    );
    assert_eq!(
        text_at(&out, "input.0.content.1.text"),
        "Second part",
        "second content part should be preserved"
    );
}

#[test]
fn strip_codex_responses_cache_breakpoints_function_call_output_parts() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "function_call",
                    "name": "shell",
                    "call_id": "call_abc",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": [
                        {
                            "type": "input_text",
                            "text": "tool output",
                            "prompt_cache_breakpoint": {"mode": "explicit"}
                        }
                    ]
                }
            ]
        }),
    );

    assert!(
        !out.to_string().contains("prompt_cache_breakpoint"),
        "prompt_cache_breakpoint should not exist in the output JSON: {out}"
    );
    assert_eq!(
        text_at(&out, "input.1.output.0.text"),
        "tool output",
        "function_call_output text should be preserved"
    );
}

#[test]
fn strip_codex_responses_cache_breakpoints_item_level() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}],
                    "prompt_cache_breakpoint": {"mode": "explicit"}
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": "plain string output",
                    "prompt_cache_breakpoint": {"mode": "explicit"}
                }
            ]
        }),
    );

    assert!(
        !out.to_string().contains("prompt_cache_breakpoint"),
        "prompt_cache_breakpoint should not exist in the output JSON: {out}"
    );
    assert_eq!(
        text_at(&out, "input.0.content.0.text"),
        "hi",
        "message content should be preserved"
    );
    assert_eq!(
        text_at(&out, "input.1.output"),
        "plain string output",
        "string output should be preserved"
    );
}

#[test]
fn strip_codex_responses_cache_breakpoints_combined_item_and_part_level() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_123",
                    "prompt_cache_breakpoint": {"mode": "explicit"},
                    "output": [
                        {
                            "type": "input_text",
                            "text": "result part 1",
                            "prompt_cache_breakpoint": {"mode": "explicit"}
                        },
                        {
                            "type": "input_text",
                            "text": "result part 2"
                        }
                    ]
                }
            ]
        }),
    );

    assert!(
        !out.to_string().contains("prompt_cache_breakpoint"),
        "prompt_cache_breakpoint should not exist in the output JSON: {out}"
    );
    let input_count = at(&out, "input")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(input_count, 1, "expected 1 input item: {out}");
    assert_eq!(
        text_at(&out, "input.0.call_id"),
        "call_123",
        "call_id should be preserved"
    );
    assert_eq!(
        text_at(&out, "input.0.output.0.text"),
        "result part 1",
        "output part 0 text should be preserved"
    );
    assert_eq!(
        text_at(&out, "input.0.output.1.text"),
        "result part 2",
        "output part 1 text should be preserved"
    );
}

#[test]
fn strip_codex_responses_cache_breakpoints_with_system_role() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.2",
        json!({
            "model": "gpt-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "System prompt",
                            "prompt_cache_breakpoint": {"mode": "explicit"}
                        }
                    ]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "User query",
                            "prompt_cache_breakpoint": {"mode": "explicit"}
                        }
                    ]
                }
            ]
        }),
    );

    assert_eq!(text_at(&out, "input.0.role"), "developer", "{out}");
    assert!(
        !out.to_string().contains("prompt_cache_breakpoint"),
        "prompt_cache_breakpoint should not exist in the output JSON: {out}"
    );
    assert_eq!(
        text_at(&out, "input.0.content.0.text"),
        "System prompt",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "input.1.content.0.text"),
        "User query",
        "{out}"
    );
}

#[test]
fn service_tier() {
    // (name, service_tier, expected service_tier or None when it should be removed)
    let cases = [
        ("priority preserved", json!("priority"), Some("priority")),
        (
            "priority case insensitive and trimmed",
            json!(" Priority "),
            Some("priority"),
        ),
        (
            "fast normalized to priority",
            json!("fast"),
            Some("priority"),
        ),
        ("ultrafast preserved", json!("ultrafast"), Some("ultrafast")),
        (
            "ultrafast case insensitive and trimmed",
            json!(" UltraFast "),
            Some("ultrafast"),
        ),
        ("standard stripped", json!("standard"), None),
        ("default stripped", json!("default"), None),
        ("flex stripped", json!("flex"), None),
        ("non-string stripped", json!(123), None),
        ("null stripped", Value::Null, None),
        ("bool stripped", json!(true), None),
        ("empty string stripped", json!(""), None),
        ("whitespace string stripped", json!("   "), None),
    ];

    for (name, tier, want) in cases {
        let out = convert_openai_responses_request_to_codex(
            "gpt-5.6",
            json!({
                "model": "gpt-5.6",
                "service_tier": tier,
                "input": [{"type": "message", "role": "user", "content": "hello"}]
            }),
        );
        assert_eq!(
            at(&out, "service_tier").is_some(),
            want.is_some(),
            "{name}: service_tier exists; output: {out}"
        );
        if let Some(want) = want {
            assert_eq!(text_at(&out, "service_tier"), want, "{name}: output: {out}");
        }
    }
}

#[test]
fn normalizes_empty_function_call_arguments() {
    let out = convert_openai_responses_request_to_codex(
        "gpt-5.6",
        json!({"model": "gpt-5.6", "input": [
            {"type": "function_call", "id": "fc_empty", "call_id": "call_empty", "name": "create_worktree", "arguments": ""},
            {"type": "function_call", "id": "fc_blank", "call_id": "call_blank", "name": "list_artifacts", "arguments": "   \t\n"},
            {"type": "function_call", "id": "fc_kept", "call_id": "call_kept", "name": "exec_command", "arguments": r#"{"cmd":"pwd"}"#},
            {"type": "function_call", "id": "fc_broken", "call_id": "call_broken", "name": "exec_command", "arguments": "not-json"},
            {"type": "function_call", "id": "fc_missing", "call_id": "call_missing", "name": "no_args"},
            {"type": "function_call", "id": "fc_null", "call_id": "call_null", "name": "null_args", "arguments": null},
            {"type": "function_call", "id": "fc_num", "call_id": "call_num", "name": "num_args", "arguments": 123},
            {"type": "function_call_output", "call_id": "call_empty", "output": "worktree created"},
            {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_custom", "name": "apply_patch", "input": "exact patch"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]}),
    );

    for (index, want) in ["{}", "{}", r#"{"cmd":"pwd"}"#, "not-json"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            text_at(&out, &format!("input.{index}.arguments")),
            want,
            "input.{index}.arguments; output: {out}"
        );
    }
    assert!(
        at(&out, "input.4.arguments").is_none(),
        "input.4 (missing arguments) should not gain arguments field; output: {out}"
    );
    // gjson reports a missing value as Null too.
    assert!(
        matches!(at(&out, "input.5.arguments"), None | Some(Value::Null)),
        "input.5.arguments should be null; output: {out}"
    );
    assert_eq!(
        int_at(&out, "input.6.arguments"),
        123,
        "input.6.arguments; output: {out}"
    );
    assert!(
        text_at(&out, "input.7.type") == "function_call_output"
            && text_at(&out, "input.7.output") == "worktree created",
        "input.7 should be function_call_output with unchanged output; output: {out}"
    );
    assert_eq!(
        text_at(&out, "input.8.type"),
        "custom_tool_call",
        "output: {out}"
    );
    assert_eq!(
        text_at(&out, "input.8.input"),
        "exact patch",
        "output: {out}"
    );
    assert!(
        at(&out, "input.8.arguments").is_none(),
        "custom_tool_call must not gain arguments; output: {out}"
    );
    assert_eq!(text_at(&out, "input.9.type"), "message", "output: {out}");
}
