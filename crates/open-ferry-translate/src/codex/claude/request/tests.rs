// Ported from CLIProxyAPI internal/translator/codex/claude/codex_claude_request_test.go,
// codex_claude_compat_test.go and noop_optimization_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;
use crate::signature::tests::valid_codex_reasoning_signature;

const GROK_SIGNATURE: &str = "HmlYdr2aCAqCYP/m9mr8PS6KOsdMs72FGDigmydR+Jsmuv8KX97yWPlbOwmXJgWn0CbHaCacdQD3+n5EvpgLfPNmafS3kdICBjRuDf4bzHy7uBiUhNVhqPtp/ee1y9q4imPE4LYgD1VZ4J+bp9mTeqA1+nC9Oue58CiNEMV9SVaGenCD+aBnVuSTzQhD32Y+68i6HLJW0Dx6ifaRfb8hxYtA/sPM+/FTvAMW11nRho5a2BBSkpnzfqqAz/e/vGJ77/bygpXM823QA9wL9i0X";

fn convert(model: &str, request: &str) -> Value {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    convert_claude_request_to_codex(model, &request)
}

/// Looks up a dotted path such as `input.0.content`, like a plain gjson path.
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

fn input(out: &Value) -> &[Value] {
    out["input"].as_array().expect("input is an array")
}

fn item_types(items: &[Value]) -> Vec<String> {
    items.iter().map(|item| text_at(item, "type")).collect()
}

fn count_input_items_by_type(out: &Value, item_type: &str) -> usize {
    input(out)
        .iter()
        .filter(|item| text_at(item, "type") == item_type)
        .count()
}

#[test]
fn system_message_scenarios() {
    let cases: [(&str, &str, &[&str]); 5] = [
        (
            "no system field",
            r#"{
                "model": "claude-3-opus",
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            &[],
        ),
        (
            "empty string system field",
            r#"{
                "model": "claude-3-opus",
                "system": "",
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            &[],
        ),
        (
            "string system field",
            r#"{
                "model": "claude-3-opus",
                "system": "Be helpful",
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            &["Be helpful"],
        ),
        (
            "message system role does not become developer",
            r#"{
                "model": "claude-3-opus",
                "messages": [
                    {"role": "system", "content": "Follow the project instructions"},
                    {"role": "user", "content": "hello"}
                ]
            }"#,
            &[],
        ),
        (
            "array system field with filtered billing header",
            r#"{
                "model": "claude-3-opus",
                "system": [
                    {"type": "text", "text": "x-anthropic-billing-header: tenant-123"},
                    {"type": "text", "text": "Block 1"},
                    {"type": "text", "text": "Block 2"}
                ],
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            &["Block 1", "Block 2"],
        ),
    ];

    for (name, request, want_texts) in cases {
        let out = convert("test-model", request);
        let items = input(&out);
        let has_developer = items
            .first()
            .is_some_and(|item| text_at(item, "role") == "developer");
        assert_eq!(has_developer, !want_texts.is_empty(), "{name}: {out}");
        if !has_developer {
            continue;
        }

        let content = items[0]["content"].as_array().expect("content is an array");
        assert_eq!(content.len(), want_texts.len(), "{name}: {out}");
        for (part, want) in content.iter().zip(want_texts) {
            assert_eq!(text_at(part, "type"), "input_text", "{name}");
            assert_eq!(text_at(part, "text"), *want, "{name}");
        }
    }
}

#[test]
fn message_system_role_wraps_as_user_reminder() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "system": [{"type": "text", "text": "Top-level rules"}],
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "system", "content": "Follow the project instructions"},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "system", "content": [{"type": "text", "text": "Use the current repo"}]}
            ]
        }"#,
    );
    let items = input(&out);
    assert_eq!(items.len(), 5, "{out}");

    assert_eq!(text_at(&items[0], "role"), "developer");
    assert_eq!(text_at(&items[2], "role"), "user");
    assert_eq!(
        text_at(&items[2], "content.0.text"),
        "<system-reminder>\nFollow the project instructions\n</system-reminder>"
    );
    assert_eq!(text_at(&items[4], "role"), "user");
    assert_eq!(
        text_at(&items[4], "content.0.text"),
        "<system-reminder>\nUse the current repo\n</system-reminder>"
    );
}

#[test]
fn preserves_tool_adjacency_with_intervening_system_message() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
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
    let items = input(&out);

    assert_eq!(
        item_types(items),
        [
            "message",
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output",
            "message",
            "message",
        ],
        "{out}"
    );
    assert_eq!(text_at(&items[3], "call_id"), "call_1");
    assert_eq!(text_at(&items[4], "call_id"), "call_2");
    assert_eq!(
        text_at(&items[5], "content.0.text"),
        "<system-reminder>\nContext update between tool call and tool result\n</system-reminder>"
    );
    assert_eq!(text_at(&items[6], "content.0.text"), "Now summarize");
}

#[test]
fn parallel_tool_calls() {
    let cases = [
        (
            "default to true when tool_choice.disable_parallel_tool_use is absent",
            r#"{"model": "claude-3-opus", "messages": [{"role": "user", "content": "hello"}]}"#,
            true,
        ),
        (
            "disable parallel tool calls when client opts out",
            r#"{
                "model": "claude-3-opus",
                "tool_choice": {"disable_parallel_tool_use": true},
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            false,
        ),
        (
            "keep parallel tool calls enabled when client explicitly allows them",
            r#"{
                "model": "claude-3-opus",
                "tool_choice": {"disable_parallel_tool_use": false},
                "messages": [{"role": "user", "content": "hello"}]
            }"#,
            true,
        ),
    ];

    for (name, request, want) in cases {
        let out = convert("test-model", request);
        assert_eq!(out["parallel_tool_calls"], want, "{name}: {out}");
    }
}

#[test]
fn service_tier_mapping() {
    // (name, service_tier JSON, speed JSON, expected service_tier)
    let cases = [
        (
            "priority passes through",
            Some(r#""priority""#),
            None,
            Some("priority"),
        ),
        (
            "fast tier normalizes to priority",
            Some(r#""fast""#),
            None,
            Some("priority"),
        ),
        (
            "unsupported tier is omitted",
            Some(r#""default""#),
            None,
            None,
        ),
        ("non-string tier is omitted", Some("true"), None, None),
        (
            "fast speed maps to priority",
            None,
            Some(r#""fast""#),
            Some("priority"),
        ),
        (
            "standard speed is omitted",
            None,
            Some(r#""standard""#),
            None,
        ),
        ("non-string speed is omitted", None, Some("true"), None),
        (
            "fast speed overrides unsupported Anthropic tier",
            Some(r#""auto""#),
            Some(r#""fast""#),
            Some("priority"),
        ),
    ];

    for (name, tier, speed, want) in cases {
        let mut request = json!({
            "model": "gpt-5.4",
            "messages": [{"role": "user", "content": "Reply with OK"}]
        });
        if let Some(tier) = tier {
            request["service_tier"] = serde_json::from_str(tier).unwrap();
        }
        if let Some(speed) = speed {
            request["speed"] = serde_json::from_str(speed).unwrap();
        }

        let out = convert_claude_request_to_codex("gpt-5.4", &request);
        assert_eq!(
            out.get("service_tier").and_then(Value::as_str),
            want,
            "{name}: {out}"
        );
    }
}

#[test]
fn shorten_long_tool_use_ids() {
    let long_id = format!("toolu_{}", "a".repeat(62));
    assert!(long_id.len() > 64);

    let request = json!({
        "model": "claude-3-opus",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "run pwd"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": long_id, "name": "Bash", "input": {"cmd": "pwd"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": long_id, "content": "ok"}
            ]}
        ]
    });
    let out = convert_claude_request_to_codex("test-model", &request);

    let call_id_of = |item_type: &str| {
        input(&out)
            .iter()
            .find(|item| text_at(item, "type") == item_type)
            .map(|item| text_at(item, "call_id"))
            .unwrap_or_else(|| panic!("missing {item_type} item: {out}"))
    };
    let call_id = call_id_of("function_call");
    assert_eq!(call_id, call_id_of("function_call_output"), "{out}");
    assert!(call_id.len() <= 64, "call_id too long: {call_id:?}");
    assert_ne!(call_id, long_id, "long call_id was not shortened");
}

#[test]
fn tool_choice_mode_mapping() {
    let cases = [
        (
            "any requires at least one tool",
            json!({"type": "any"}),
            "required",
        ),
        ("none disables tools", json!({"type": "none"}), "none"),
        ("auto stays auto", json!({"type": "auto"}), "auto"),
    ];

    for (name, claude_choice, want) in cases {
        let request = json!({
            "model": "claude-3-opus",
            "tools": [
                {"name": "lookup", "description": "Lookup", "input_schema": {"type": "object", "properties": {}}}
            ],
            "tool_choice": claude_choice,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let out = convert_claude_request_to_codex("test-model", &request);
        assert_eq!(text_at(&out, "tool_choice"), want, "{name}: {out}");
    }
}

#[test]
fn tool_choice_specific_function_uses_converted_name() {
    let long_name = "mcp__server_with_a_very_long_name_that_exceeds_sixty_four_characters__search";
    let request = json!({
        "model": "claude-3-opus",
        "tools": [
            {"name": long_name, "description": "Search", "input_schema": {"type": "object", "properties": {}}}
        ],
        "tool_choice": {"type": "tool", "name": long_name},
        "messages": [{"role": "user", "content": "hello"}]
    });
    let out = convert_claude_request_to_codex("test-model", &request);

    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    let choice_name = text_at(&out, "tool_choice.name");
    assert_eq!(choice_name, text_at(&out, "tools.0.name"), "{out}");
    assert_ne!(choice_name, long_name, "{out}");
}

// TestConvertClaudeRequestToCodex_WebSearchSourcesInclude. The translator
// takes no stream flag, so Go's two passes are one.
#[test]
fn web_search_sources_include() {
    let cases = [
        ("no tools", "", false),
        ("empty tools", "[]", false),
        (
            "ordinary function",
            r#"[{"name":"lookup","input_schema":{"type":"object"}}]"#,
            false,
        ),
        (
            "same name function",
            r#"[{"name":"web_search","input_schema":{"type":"object"}}]"#,
            false,
        ),
        (
            "unsupported type",
            r#"[{"type":"web_search_20990101","name":"web_search"}]"#,
            false,
        ),
        (
            "20250305",
            r#"[{"type":"web_search_20250305","name":"web_search"}]"#,
            true,
        ),
        (
            "20260209",
            r#"[{"type":"web_search_20260209","name":"web_search"}]"#,
            true,
        ),
        (
            "custom name",
            r#"[{"type":"web_search_20250305","name":"browser_search"}]"#,
            true,
        ),
        (
            "nameless search",
            r#"[{"type":"web_search_20250305"}]"#,
            true,
        ),
        (
            "multiple searches and function",
            r#"[{"type":"web_search_20250305","name":"search_one"},{"type":"web_search_20260209","name":"search_two"},{"name":"lookup","input_schema":{"type":"object"}}]"#,
            true,
        ),
    ];
    for (name, tools, want_sources) in cases {
        let mut request =
            r#"{"model":"claude-opus-4-7","messages":[{"role":"user","content":"hello"}]"#
                .to_owned();
        if !tools.is_empty() {
            request.push_str(r#","tools":"#);
            request.push_str(tools);
        }
        request.push('}');
        let out = convert("test-model", &request);
        let want = if want_sources {
            r#"["reasoning.encrypted_content","web_search_call.action.sources"]"#
        } else {
            r#"["reasoning.encrypted_content"]"#
        };
        assert_eq!(out["include"].to_string(), want, "{name}");
    }
}

#[test]
fn web_search_tool_mapping() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-3-opus",
            "tools": [
                {
                    "type": "web_search_20260209",
                    "name": "web_search",
                    "allowed_domains": ["example.com"],
                    "blocked_domains": ["blocked.example"],
                    "user_location": {
                        "type": "approximate",
                        "city": "Beijing",
                        "country": "CN",
                        "timezone": "Asia/Shanghai"
                    }
                }
            ],
            "tool_choice": {"type": "tool", "name": "web_search"},
            "messages": [{"role": "user", "content": "hello"}]
        }"#,
    );

    assert_eq!(text_at(&out, "tools.0.type"), "web_search", "{out}");
    assert_eq!(
        text_at(&out, "tools.0.filters.allowed_domains.0"),
        "example.com",
        "{out}"
    );
    assert!(at(&out, "tools.0.blocked_domains").is_none(), "{out}");
    assert_eq!(
        text_at(&out, "tools.0.user_location.city"),
        "Beijing",
        "{out}"
    );
    assert_eq!(text_at(&out, "tool_choice.type"), "web_search", "{out}");
}

#[test]
fn web_search_tool_choice_uses_declared_typed_tool_name() {
    let out = convert(
        "test-model",
        r#"{
            "model": "claude-opus-4-7",
            "tools": [
                {"type": "web_search_20250305", "name": "browser_search"},
                {"name": "web_search", "description": "Local search", "input_schema": {"type": "object", "properties": {}}}
            ],
            "tool_choice": {"type": "tool", "name": "web_search"},
            "messages": [{"role": "user", "content": "hello"}]
        }"#,
    );

    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    assert_eq!(text_at(&out, "tool_choice.name"), "web_search", "{out}");
}

#[test]
fn assistant_thinking_signature_to_reasoning_item() {
    let signature = valid_codex_reasoning_signature();
    let request = json!({
        "model": "claude-3-opus",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": "visible summary must not be replayed",
                        "signature": signature
                    },
                    {"type": "text", "text": "visible answer"}
                ]
            },
            {"role": "user", "content": "continue"}
        ]
    });
    let out = convert_claude_request_to_codex("test-model", &request);
    let items = input(&out);
    assert_eq!(items.len(), 3, "{out}");

    let reasoning = &items[0];
    assert_eq!(text_at(reasoning, "type"), "reasoning", "{out}");
    assert_eq!(text_at(reasoning, "encrypted_content"), signature);
    assert_eq!(reasoning.get("summary"), Some(&json!([])));
    assert_eq!(reasoning.get("content"), Some(&Value::Null));

    let assistant = &items[1];
    assert_eq!(text_at(assistant, "role"), "assistant", "{out}");
    assert_eq!(text_at(assistant, "content.0.type"), "output_text");
    assert_eq!(text_at(assistant, "content.0.text"), "visible answer");
    assert!(
        !out.to_string()
            .contains("visible summary must not be replayed"),
        "thinking text should not be replayed into Codex input: {out}"
    );
}

#[test]
fn preserves_base64_pdf_document_content() {
    let out = convert(
        "gpt-5.6-sol",
        r#"{
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQK"}},
                    {"type": "text", "text": "after"}
                ]
            }]
        }"#,
    );
    let content = at(&out, "input.0.content")
        .and_then(Value::as_array)
        .expect("input.0.content is an array");
    assert_eq!(content.len(), 3, "{out}");

    assert_eq!(
        item_types(content),
        ["input_text", "input_file", "input_text"],
        "{out}"
    );
    assert_eq!(text_at(&content[0], "text"), "before");
    assert_eq!(
        text_at(&content[1], "file_data"),
        "data:application/pdf;base64,JVBERi0xLjQK"
    );
    assert_eq!(text_at(&content[1], "filename"), "document.pdf");
    assert_eq!(text_at(&content[2], "text"), "after");
}

#[test]
fn preserves_content_order_across_tool_and_reasoning_items() {
    let signature = valid_codex_reasoning_signature();
    let request = json!({
        "system": "system rules",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "text", "text": "before reasoning"},
                {"type": "thinking", "signature": signature},
                {"type": "text", "text": "before tool"},
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"query": "test"}},
                {"type": "text", "text": "after tool"}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "text", "text": "tool output"},
                    {"type": "image", "source": {"media_type": "image/png", "data": "aW1hZ2U="}}
                ]},
                {"type": "text", "text": "continue"}
            ]}
        ],
        "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]
    });
    let out = convert_claude_request_to_codex("gpt-5.4", &request);
    let items = input(&out);

    assert_eq!(
        item_types(items),
        [
            "message",
            "message",
            "reasoning",
            "message",
            "function_call",
            "message",
            "function_call_output",
            "message",
        ],
        "{out}"
    );
    assert_eq!(text_at(&items[1], "content.0.text"), "before reasoning");
    assert_eq!(text_at(&items[3], "content.0.text"), "before tool");
    assert_eq!(text_at(&items[5], "content.0.text"), "after tool");
    assert_eq!(text_at(&items[6], "output.0.type"), "input_text");
    assert_eq!(
        text_at(&items[6], "output.1.image_url"),
        "data:image/png;base64,aW1hZ2U="
    );
    assert_eq!(text_at(&items[7], "content.0.text"), "continue");
}

#[test]
fn assistant_grok_signature_to_reasoning_item() {
    let request = json!({
        "model": "grok-4.5",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "summary", "signature": GROK_SIGNATURE},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "user", "content": "next"}
        ]
    });
    // A thinking suffix doesn't hide the base model.
    for model in ["grok-4.5", " GROK-4.5 ", "grok-4.5(high)"] {
        let out = convert_claude_request_to_codex(model, &request);
        assert_eq!(text_at(&out, "input.0.type"), "reasoning", "{model}: {out}");
        assert_eq!(
            text_at(&out, "input.0.encrypted_content"),
            GROK_SIGNATURE,
            "{model}"
        );
    }
}

#[test]
fn ignores_grok_signature_for_non_grok_targets() {
    let request = json!({
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "summary", "signature": GROK_SIGNATURE},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "user", "content": "next"}
        ]
    });

    // Only the base model counts, not a thinking suffix.
    for model in ["gpt-5.4", "claude-sonnet-4-6", "gpt-5.4(grok)"] {
        let out = convert_claude_request_to_codex(model, &request);
        assert_eq!(
            count_input_items_by_type(&out, "reasoning"),
            0,
            "{model}: {out}"
        );
    }
}

#[test]
fn ignores_non_codex_thinking_signatures() {
    let cases = [
        (
            "ignore user thinking even with Codex-shaped signature",
            json!({
                "model": "claude-3-opus",
                "messages": [{
                    "role": "user",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "user supplied thinking",
                            "signature": valid_codex_reasoning_signature()
                        },
                        {"type": "text", "text": "hello"}
                    ]
                }]
            }),
        ),
        (
            "ignore Anthropic native signature",
            json!({
                "model": "claude-3-opus",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "anthropic thinking",
                            "signature": "Eo8Canthropic-state"
                        },
                        {"type": "text", "text": "visible answer"}
                    ]
                }]
            }),
        ),
    ];

    for (name, request) in cases {
        let out = convert_claude_request_to_codex("test-model", &request);
        assert_eq!(
            count_input_items_by_type(&out, "reasoning"),
            0,
            "{name}: {out}"
        );
    }
}

fn convert_with_compat(model: &str, request: &str) -> Value {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    convert_claude_request_to_codex_with_compat(model, &request)
}

/// An assistant message holding one thinking block with `signature`, given as
/// raw JSON.
fn thinking_with_signature(signature: &str) -> String {
    format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"reason","signature":{signature}}}]}}]}}"#
    )
}

#[test]
fn convert_claude_request_to_codex_with_compat_preserves_empty_thinking() {
    let payload = thinking_with_signature(r#""""#);

    let without_compat = convert("deepseek-v4", &payload);
    assert!(input(&without_compat).is_empty(), "{without_compat}");

    let with_compat = convert_with_compat("deepseek-v4", &payload);
    assert_eq!(
        text_at(&with_compat, "input.0.type"),
        "reasoning",
        "{with_compat}"
    );
    assert!(
        at(&with_compat, "input.0.encrypted_content").is_some(),
        "{with_compat}"
    );
}

#[test]
fn convert_claude_request_to_codex_with_compat_unknown_thinking_signatures() {
    const UNKNOWN_SIG: &str = "opaque-encrypted-reasoning-token-xyz";
    let unknown = thinking_with_signature(&format!("\"{UNKNOWN_SIG}\""));
    let with_compat = convert_with_compat("deepseek-v4", &unknown);
    assert_eq!(
        text_at(&with_compat, "input.0.type"),
        "reasoning",
        "{with_compat}"
    );
    assert_eq!(
        text_at(&with_compat, "input.0.encrypted_content"),
        UNKNOWN_SIG
    );
    let without_compat = convert("deepseek-v4", &unknown);
    assert!(input(&without_compat).is_empty(), "{without_compat}");

    // A valid Claude signature is recognized, so it isn't replayed to Codex.
    let claude = thinking_with_signature(&format!(
        "\"{}\"",
        crate::signature::tests::OBSERVED_FABLE5_SAMPLE
    ));
    let claude_compat = convert_with_compat("deepseek-v4", &claude);
    assert!(input(&claude_compat).is_empty(), "{claude_compat}");

    let gpt_raw_sig = valid_codex_reasoning_signature();
    let gpt = thinking_with_signature(&format!("\"gpt#{gpt_raw_sig}\""));
    let gpt_compat = convert_with_compat("deepseek-v4", &gpt);
    assert_eq!(
        text_at(&gpt_compat, "input.0.type"),
        "reasoning",
        "{gpt_compat}"
    );
    assert_eq!(
        text_at(&gpt_compat, "input.0.encrypted_content"),
        gpt_raw_sig
    );

    for signature in ["12345", r#"{"opaque":"data"}"#, "true", r#"["arr"]"#] {
        let out = convert_with_compat("deepseek-v4", &thinking_with_signature(signature));
        assert!(input(&out).is_empty(), "{signature}: {out}");
    }
}

#[test]
fn convert_claude_request_to_codex_with_compat_preserves_message_flushing_and_escaped_unknown_signature()
 {
    let escaped_sig = "enc:\"token\"\u{5c}with\u{5c}unicode-\u{4e00}";
    let payload = r#"{"messages":[{"role":"assistant","content":[{"type":"text","text":"before"},{"type":"thinking","thinking":"reason","signature":"enc:\"token\"\\with\\unicode-ESCAPE"},{"type":"text","text":"after"}]}]}"#
        // A JSON escape, so the decoded signature is what's compared.
        .replace("ESCAPE", "\u{5c}u4e00");

    let with_compat = convert_with_compat("deepseek-v4", &payload);
    let items = input(&with_compat);
    assert_eq!(items.len(), 3, "{with_compat}");
    assert_eq!(text_at(&items[0], "role"), "assistant");
    assert_eq!(text_at(&items[0], "content.0.text"), "before");
    assert_eq!(text_at(&items[1], "type"), "reasoning");
    assert_eq!(text_at(&items[1], "encrypted_content"), escaped_sig);
    assert_eq!(text_at(&items[2], "role"), "assistant");
    assert_eq!(text_at(&items[2], "content.0.text"), "after");
}

#[test]
fn convert_claude_request_to_codex_with_compat_whitespace_and_null_signatures() {
    let whitespace = convert_with_compat("deepseek-v4", &thinking_with_signature(r#""   ""#));
    assert_eq!(
        text_at(&whitespace, "input.0.type"),
        "reasoning",
        "{whitespace}"
    );
    assert_eq!(text_at(&whitespace, "input.0.encrypted_content"), "   ");

    let null = convert_with_compat("deepseek-v4", &thinking_with_signature("null"));
    assert_eq!(text_at(&null, "input.0.type"), "reasoning", "{null}");
    assert_eq!(text_at(&null, "input.0.encrypted_content"), "");
}

#[test]
fn output_config_format_valid_json_schema() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
            "max_tokens": 128,
            "messages": [
                {"role": "user", "content": "Return an object with one string field named answer."}
            ],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "answer": {"type": "string"}
                        },
                        "required": ["answer"],
                        "additionalProperties": false
                    }
                }
            }
        }"#,
    );

    assert!(at(&out, "text.format").is_some(), "{out}");
    assert_eq!(text_at(&out, "text.format.type"), "json_schema");
    assert_eq!(
        text_at(&out, "text.format.name"),
        "cli_proxy_structured_output"
    );
    assert_eq!(at(&out, "text.format.strict"), Some(&Value::Bool(true)));
    assert_eq!(
        text_at(&out, "text.format.schema.properties.answer.type"),
        "string"
    );
}

#[test]
fn output_config_format_with_custom_name_and_strict_false() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "custom_schema",
                    "strict": false,
                    "schema": {"type": "object"}
                }
            }
        }"#,
    );

    assert_eq!(text_at(&out, "text.format.name"), "custom_schema");
    assert_eq!(at(&out, "text.format.strict"), Some(&Value::Bool(false)));
}

#[test]
fn output_config_without_format_sets_no_text_format() {
    let out = convert(
        "gpt-5.4",
        r#"{"model": "gpt-5.4", "messages": [{"role": "user", "content": "hello"}]}"#,
    );

    assert!(at(&out, "text.format").is_none(), "{out}");
}

#[test]
fn output_config_with_effort_only() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "messages": [{"role": "user", "content": "hello"}]
        }"#,
    );

    assert!(at(&out, "text.format").is_none(), "{out}");
    assert_eq!(text_at(&out, "reasoning.effort"), "high");
}

#[test]
fn output_config_json_schema_with_optional_property_downgrades_strict() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "cli_proxy_structured_output",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "answer": {"type": "string"},
                            "impossible": {"type": "string"}
                        },
                        "required": ["answer"],
                        "additionalProperties": false
                    }
                }
            }
        }"#,
    );

    assert_eq!(
        at(&out, "text.format.strict"),
        Some(&Value::Bool(false)),
        "{out}"
    );
    assert_eq!(
        text_at(&out, "text.format.name"),
        "cli_proxy_structured_output"
    );
}

#[test]
fn output_config_json_schema_fully_required_keeps_strict() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "model": "gpt-5.4",
            "messages": [{"role": "user", "content": "hello"}],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "answer": {"type": "string"}
                        },
                        "required": ["answer"],
                        "additionalProperties": false
                    }
                }
            }
        }"#,
    );

    assert_eq!(
        at(&out, "text.format.strict"),
        Some(&Value::Bool(true)),
        "{out}"
    );
}

#[test]
fn normalize_tool_parameters_strips_nested_schema_and_id() {
    let schema: Value = serde_json::from_str(
        r##"{
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "https://example.invalid/root",
            "properties": {
                "q": {
                    "type": "string",
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "$id": "https://example.invalid/q"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string", "$id": "https://example.invalid/tag"}
                },
                "mode": {
                    "anyOf": [
                        {"type": "string", "$schema": "http://json-schema.org/draft-07/schema#"},
                        {"type": "null"}
                    ]
                },
                "refField": {
                    "$ref": "#/$defs/hint"
                }
            },
            "$defs": {
                "hint": {"type": "string", "$id": "https://example.invalid/hint"}
            },
            "required": ["q"]
        }"##,
    )
    .unwrap();
    let got = normalize_tool_parameters(Some(&schema));

    for removed in [
        "$schema",
        "$id",
        "properties.q.$schema",
        "properties.q.$id",
        "properties.tags.items.$id",
        "properties.mode.anyOf.0.$schema",
        "$defs.hint.$id",
    ] {
        assert!(
            at(&got, removed).is_none(),
            "expected {removed} to be removed: {got}"
        );
    }
    assert_eq!(text_at(&got, "properties.refField.$ref"), "#/$defs/hint");
    assert_eq!(text_at(&got, "properties.q.type"), "string");
}

#[test]
fn normalize_tool_parameters_preserves_property_names_and_literal_data() {
    let schema = json!({
        "type": "object",
        "properties": {
            "$schema": {
                "type": "string",
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$id": "https://example.invalid/sub-schema"
            },
            "$id": {
                "type": "string"
            },
            "config": {
                "type": "object",
                "default": {
                    "$id": "default-id-123"
                }
            }
        }
    });
    let got = normalize_tool_parameters(Some(&schema));

    assert!(
        at(&got, "properties.$schema").is_some(),
        "property named $schema was dropped"
    );
    assert!(at(&got, "properties.$schema.$schema").is_none());
    assert!(at(&got, "properties.$schema.$id").is_none());
    assert!(
        at(&got, "properties.$id").is_some(),
        "property named $id was dropped"
    );
    assert_eq!(
        text_at(&got, "properties.config.default.$id"),
        "default-id-123"
    );

    // A missing or null schema falls back to an empty object schema.
    let empty = r#"{"type":"object","properties":{}}"#;
    assert_eq!(normalize_tool_parameters(None).to_string(), empty);
    assert_eq!(
        normalize_tool_parameters(Some(&Value::Null)).to_string(),
        empty
    );

    // A union type that includes "object" is kept and still gets properties.
    let union = normalize_tool_parameters(Some(&json!({"type": ["object", "null"]})));
    assert_eq!(union["type"], json!(["object", "null"]));
    assert!(union.get("properties").is_some(), "{union}");
}

#[test]
fn strips_nested_tool_schema_meta() {
    let out = convert(
        "gpt-5",
        r##"{
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "lookup",
                "description": "Lookup",
                "input_schema": {
                    "type": "object",
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": {
                        "q": {
                            "type": "string",
                            "$schema": "http://json-schema.org/draft-07/schema#",
                            "$id": "https://example.invalid/q"
                        },
                        "tags": {
                            "type": "array",
                            "items": {"type": "string", "$id": "https://example.invalid/tag"}
                        },
                        "mode": {
                            "anyOf": [
                                {"type": "string", "$schema": "http://json-schema.org/draft-07/schema#"},
                                {"type": "null"}
                            ]
                        }
                    },
                    "$defs": {
                        "hint": {"type": "string", "$id": "https://example.invalid/hint"}
                    },
                    "required": ["q"]
                }
            }]
        }"##,
    );
    let params = at(&out, "tools.0.parameters").expect("tools.0.parameters exists");

    for removed in [
        "$schema",
        "properties.q.$schema",
        "properties.q.$id",
        "properties.tags.items.$id",
        "properties.mode.anyOf.0.$schema",
        "$defs.hint.$id",
    ] {
        assert!(
            at(params, removed).is_none(),
            "expected parameters.{removed} to be removed: {params}"
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
                        },
                        "literal_p": {
                            "type": "string",
                            "pattern": "^\\\\p{Cc}$"
                        },
                        "nested": {
                            "type": "object",
                            "properties": {
                                "inner_field": {
                                    "type": "string",
                                    "pattern": "\\P{L}+"
                                }
                            }
                        },
                        "union_field": {
                            "anyOf": [
                                {
                                    "type": "string",
                                    "pattern": "\\p{N}+"
                                },
                                {
                                    "type": "null"
                                }
                            ]
                        }
                    },
                    "required": ["field"]
                }
            }]
        }"#,
    );
    let params = at(&out, "tools.0.parameters").expect("tools.0.parameters exists");

    // \p{...} patterns are dropped; the rest of the property schema stays.
    assert!(at(params, "properties.field.pattern").is_none(), "{params}");
    assert_eq!(text_at(params, "properties.field.type"), "string");
    assert_eq!(
        text_at(params, "properties.field.description"),
        "field to replace"
    );

    // Patterns without an unescaped \p are kept.
    assert_eq!(
        text_at(params, "properties.asset_id.pattern"),
        "^[0-9a-f]{32}$"
    );
    assert_eq!(
        text_at(params, "properties.lookahead_safe.pattern"),
        "^(?!__.*__$).{1,200}$"
    );
    assert_eq!(
        text_at(params, "properties.literal_p.pattern"),
        r"^\\p{Cc}$"
    );

    assert!(
        at(params, "properties.nested.properties.inner_field.pattern").is_none(),
        "{params}"
    );
    assert_eq!(
        text_at(params, "properties.nested.properties.inner_field.type"),
        "string"
    );
    assert!(
        at(params, "properties.union_field.anyOf.0.pattern").is_none(),
        "{params}"
    );
    assert_eq!(text_at(params, "required.0"), "field");
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
    let pattern_props = at(&out, "tools.0.parameters.patternProperties")
        .and_then(Value::as_object)
        .expect("patternProperties is an object");

    assert!(!pattern_props.contains_key(r"^\p{L}+$"), "{out}");
    assert!(pattern_props.contains_key("^[a-z]+$"), "{out}");
}

#[test]
fn normalizes_non_string_tool_name() {
    let out = convert(
        "gpt-test",
        r#"{"messages":[],"tools":[{"name":123,"input_schema":{"type":"object"}}]}"#,
    );

    assert_eq!(out["tools"][0]["name"], json!("123"));
}

// Tests below are not from upstream.

#[test]
fn strips_pattern_properties_keys_with_property_escapes() {
    // Upstream's test above uses an escaped backslash, which is allowed, so it
    // never exercises removal of a key.
    let schema = json!({
        "type": "object",
        "patternProperties": {
            r"^\p{L}+$": {"type": "string"},
            r"^\\p{L}+$": {"type": "string", "$id": "x"},
        }
    });
    let got = normalize_tool_parameters(Some(&schema));
    let pattern_props = got["patternProperties"].as_object().unwrap();

    assert!(!pattern_props.contains_key(r"^\p{L}+$"), "{got}");
    assert_eq!(pattern_props[r"^\\p{L}+$"], json!({"type": "string"}));
}

#[test]
fn long_call_ids_get_a_stable_hash_suffix() {
    let id = format!("toolu_{}", "a".repeat(62));
    let short = shorten_call_id(&id);

    assert_eq!(short.len(), 64);
    assert_eq!(&short[..47], &id[..47]);
    assert_eq!(&short[47..48], "_");
    assert!(short[48..].bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(shorten_call_id(&id), short);
    assert_eq!(shorten_call_id("toolu_short"), "toolu_short");
}

#[test]
fn long_tool_names_are_shortened_and_made_unique() {
    let mcp = format!("mcp__{}__search", "s".repeat(70));
    assert_eq!(shorten_name(&mcp), "mcp__search");

    let long = "x".repeat(70);
    let names = build_tool_name_map(Some(&json!([
        {"name": format!("{long}a")},
        {"name": format!("{long}b")},
    ])));
    assert_eq!(names[&format!("{long}a")], "x".repeat(64));
    assert_eq!(names[&format!("{long}b")], format!("{}_1", "x".repeat(62)));
}

#[test]
fn truncation_keeps_whole_characters() {
    // "é" is two bytes, so a 64-byte cut would land inside the 32nd one.
    let name = format!("a{}", "é".repeat(40));
    let short = shorten_name(&name);

    assert_eq!(short.len(), 63);
    assert!(short.ends_with('é'));
}

#[test]
fn reasoning_effort_follows_thinking_config() {
    let effort = |request: Value| {
        text_at(
            &convert_claude_request_to_codex("gpt-5.4", &request),
            "reasoning.effort",
        )
    };

    assert_eq!(effort(json!({})), "medium");
    assert_eq!(
        effort(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}})),
        "low"
    );
    assert_eq!(
        effort(json!({"thinking": {"type": "enabled", "budget_tokens": 30000}})),
        "xhigh"
    );
    assert_eq!(
        effort(json!({"thinking": {"type": "enabled", "budget_tokens": -5}})),
        "medium"
    );
    assert_eq!(effort(json!({"thinking": {"type": "adaptive"}})), "xhigh");
    assert_eq!(
        effort(json!({"thinking": {"type": "auto"}, "output_config": {"effort": " LOW "}})),
        "low"
    );
    assert_eq!(effort(json!({"thinking": {"type": "disabled"}})), "none");
    // Lowercased like Go's strings.ToLower, not Rust's str::to_lowercase.
    assert_eq!(
        effort(json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "MAXİMUM"}})),
        "maximum"
    );
}

#[test]
fn array_tools_pass_through_unchanged() {
    let out = convert(
        "gpt-test",
        r#"{"messages":[],"tools":[[1,{"name":"x"}],{"name":"y"}]}"#,
    );

    assert_eq!(out["tools"][0], json!([1, {"name": "x"}]));
    assert_eq!(out["tools"][1]["name"], json!("y"));
}

#[test]
fn output_keeps_codex_field_order() {
    let out = convert(
        "gpt-5.4",
        r#"{
            "speed": "fast",
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}]
        }"#,
    );
    let keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();

    assert_eq!(
        keys,
        [
            "model",
            "instructions",
            "input",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "service_tier",
            "stream",
            "store",
            "include",
            "text",
            "tools",
        ]
    );
    // Compared as text because `Value` equality ignores key order.
    assert_eq!(
        out["tools"][0].to_string(),
        r#"{"name":"lookup","type":"function","parameters":{"type":"object","properties":{}},"strict":false}"#
    );
}
