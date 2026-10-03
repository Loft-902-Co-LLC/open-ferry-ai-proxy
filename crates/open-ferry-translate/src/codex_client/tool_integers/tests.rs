//! Ported from CLIProxyAPI internal/client/codex/tool-schema/
//! tool_schema_test.go (v8.0.10, MIT).
//!
//! `TestNormalizeCodexToolIntegerTypes` is ported case by case. Its "nil or
//! empty headers" case passes an empty user agent, as callers take the
//! value from the headers.
//!
//! Added: Chat Completions tools, namespaces, the key order kept, and type
//! arrays with members that aren't strings.

use serde_json::Value;

use super::*;

const TUI: &str = "codex-tui/0.154.0";

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn normalized(text: &str, user_agent: &str) -> Value {
    let mut body = json(text);
    normalize(&mut body, user_agent);
    body
}

fn tool<'v>(body: &'v Value, name: &str) -> &'v Value {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap()
}

const INPUT: &str = r#"{
    "tools": [
        {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {
            "cmd": {"type": "string"},
            "yield_time_ms": {"type": "number"},
            "max_output_tokens": {"type": "number"},
            "timeout_ms": {"type": "number"}
        }}},
        {"type": "function", "name": "write_stdin", "parameters": {"type": "object", "properties": {
            "session_id": {"type": "number"},
            "yield_time_ms": {"type": "number"},
            "max_output_tokens": {"type": "number"}
        }}},
        {"type": "function", "name": "sleep", "parameters": {"type": "object", "properties": {
            "duration_ms": {"type": "number"}
        }}},
        {"type": "function", "name": "wait_agent", "parameters": {"type": "object", "properties": {
            "timeout_ms": {"type": "number"}
        }}},
        {"type": "function", "name": "wait", "parameters": {"type": "object", "properties": {
            "yield_time_ms": {"type": "number"},
            "max_tokens": {"type": "number"}
        }}},
        {"type": "function", "name": "tool_search", "parameters": {"type": "object", "properties": {
            "limit": {"type": "number"}
        }}},
        {"type": "function", "name": "test_sync_tool", "parameters": {"type": "object", "properties": {
            "sleep_before_ms": {"type": "number"},
            "sleep_after_ms": {"type": "number"},
            "participants": {"type": "number"},
            "timeout_ms": {"type": "number"}
        }}},
        {"type": "function", "name": "unrelated_tool", "parameters": {"type": "object", "properties": {
            "timeout_ms": {"type": "number"}
        }}}
    ],
    "input": [
        {"type": "additional_tools", "tools": [
            {"type": "function", "name": "functions__exec_command", "parameters": {"type": "object", "properties": {
                "yield_time_ms": {"type": ["number", "null"]}
            }}}
        ]}
    ]
}"#;

#[test]
fn non_codex_user_agent_preserves_numbers() {
    let mut body = json(INPUT);
    assert!(!normalize(&mut body, "curl/8.7.1"));
    assert_eq!(body, json(INPUT));
}

#[test]
fn empty_user_agent_leaves_payload_untouched() {
    let mut body = json(INPUT);
    assert!(!normalize(&mut body, ""));
    assert_eq!(body, json(INPUT));
}

#[test]
fn codex_user_agent_normalizes_specified_fields() {
    let mut body = json(INPUT);
    assert!(normalize(
        &mut body,
        "codex-tui/0.154.0 (Mac OS 26.5.2; arm64)"
    ));
    for (name, fields) in INTEGER_FIELDS {
        let tool = tool(&body, name);
        for field in fields {
            assert_eq!(
                tool["parameters"]["properties"][field]["type"], "integer",
                "{name} {field}"
            );
        }
    }
    assert_eq!(
        tool(&body, "exec_command")["parameters"]["properties"]["cmd"]["type"],
        "string"
    );
    assert_eq!(
        tool(&body, "unrelated_tool")["parameters"]["properties"]["timeout_ms"]["type"],
        "number"
    );
    assert_eq!(
        body["input"][0]["tools"][0]["parameters"]["properties"]["yield_time_ms"]["type"],
        json(r#"["integer","null"]"#)
    );
}

#[test]
fn third_party_mcp_tools_are_not_modified() {
    let body = normalized(
        r#"{"tools": [
        {"type": "function", "name": "mcp__server__sleep", "parameters": {"type": "object", "properties": {"duration_ms": {"type": "number"}}}},
        {"type": "function", "name": "mcp__server__exec_command", "parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}},
        {"type": "function", "name": "functions__sleep", "parameters": {"type": "object", "properties": {"duration_ms": {"type": "number"}}}},
        {"type": "function", "name": "collab__exec_command", "parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}}
    ]}"#,
        TUI,
    );
    let kind = |name: &str, field: &str| {
        tool(&body, name)["parameters"]["properties"][field]["type"].clone()
    };
    assert_eq!(kind("mcp__server__sleep", "duration_ms"), "number");
    assert_eq!(kind("mcp__server__exec_command", "yield_time_ms"), "number");
    assert_eq!(kind("functions__sleep", "duration_ms"), "integer");
    assert_eq!(kind("collab__exec_command", "yield_time_ms"), "integer");
}

#[test]
fn array_type_with_number_and_integer_deduplicates() {
    let body = normalized(
        r#"{"tools": [{"type": "function", "name": "sleep", "parameters": {"type": "object", "properties": {"duration_ms": {"type": ["number", "integer", "null"]}}}}]}"#,
        TUI,
    );
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["duration_ms"]["type"],
        json(r#"["integer","null"]"#)
    );
}

// Not upstream's: a long type array is deduplicated with a set, as
// upstream's is, rather than by scanning what is kept for each member.
#[test]
fn long_type_arrays_are_deduplicated_in_one_pass() {
    let mut kinds: Vec<Value> = (0..50_000)
        .map(|n| Value::from(format!("type_{n:06}")))
        .collect();
    kinds.push(Value::from("number"));
    kinds.push(Value::from("type_000000"));
    let mut body = serde_json::json!({"tools": [{"type": "function", "name": "sleep",
        "parameters": {"properties": {"duration_ms": {"type": kinds}}}}]});
    assert!(normalize(&mut body, TUI));
    let kinds = body["tools"][0]["parameters"]["properties"]["duration_ms"]["type"]
        .as_array()
        .unwrap();
    assert_eq!(kinds.len(), 50_001);
    assert_eq!(kinds[0], "type_000000");
    assert_eq!(kinds[50_000], "integer");
}

#[test]
fn claude_input_schema_format_supported() {
    let body = normalized(
        r#"{"tools": [{"name": "exec_command", "input_schema": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}, "timeout_ms": {"type": "number"}}}}]}"#,
        TUI,
    );
    let properties = &body["tools"][0]["input_schema"]["properties"];
    assert_eq!(properties["yield_time_ms"]["type"], "integer");
    assert_eq!(properties["timeout_ms"]["type"], "integer");
}

#[test]
fn gemini_function_declarations_format_supported() {
    let body = normalized(
        r#"{"tools": [{"function_declarations": [{"name": "sleep", "parameters": {"type": "object", "properties": {"duration_ms": {"type": "number"}}}}]}]}"#,
        "codex-desktop/0.159.0",
    );
    assert_eq!(
        body["tools"][0]["function_declarations"][0]["parameters"]["properties"]["duration_ms"]["type"],
        "integer"
    );
}

#[test]
fn gemini_parameters_json_schema_format_supported_directly() {
    let body = normalized(
        r#"{"tools": [{"functionDeclarations": [{"name": "exec_command", "parametersJsonSchema": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}}]}]}"#,
        "codex-desktop/0.159.0",
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"]["yield_time_ms"]
            ["type"],
        "integer"
    );
}

#[test]
fn chat_completions_tools_and_namespaces_are_normalized() {
    let body = normalized(
        r#"{"tools": [
        {"type": "function", "function": {"name": "sleep", "parameters": {"properties": {"duration_ms": {"type": "number"}}}}},
        {"type": "namespace", "name": "ns", "tools": [{"type": "function", "name": "wait", "parameters": {"properties": {"max_tokens": {"type": "number"}}}}]}
    ]}"#,
        "Codex",
    );
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"]["duration_ms"]["type"],
        "integer"
    );
    assert_eq!(
        body["tools"][1]["tools"][0]["parameters"]["properties"]["max_tokens"]["type"],
        "integer"
    );
}

#[test]
fn parameters_that_are_not_an_object_fall_through() {
    // `parameters` that isn't an object gives way to the next place; one
    // that is, but belongs to another tool, ends the search.
    let body = normalized(
        r#"{"tools": [
        {"name": "sleep", "parameters": "none", "input_schema": {"properties": {"duration_ms": {"type": "number"}}}},
        {"name": "other", "parameters": {}, "function": {"name": "sleep", "parameters": {"properties": {"duration_ms": {"type": "number"}}}}}
    ]}"#,
        TUI,
    );
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["duration_ms"]["type"],
        "integer"
    );
    assert_eq!(
        body["tools"][1]["function"]["parameters"]["properties"]["duration_ms"]["type"],
        "number"
    );
}

#[test]
fn key_order_and_type_array_members() {
    let body = normalized(
        r#"{"tools":[{"type":"function","name":"sleep","parameters":{"properties":{"duration_ms":{"type":"number","minimum":0}}}},{"type":"function","name":"wait","parameters":{"properties":{"max_tokens":{"type":["number",1,null,"number"]},"yield_time_ms":{"type":["integer"]}}}}]}"#,
        TUI,
    );
    assert_eq!(
        body.to_string(),
        r#"{"tools":[{"type":"function","name":"sleep","parameters":{"properties":{"duration_ms":{"type":"integer","minimum":0}}}},{"type":"function","name":"wait","parameters":{"properties":{"max_tokens":{"type":["integer","1",""]},"yield_time_ms":{"type":["integer"]}}}}]}"#
    );
}

#[test]
fn is_codex_user_agent_cases() {
    assert!(is_codex_user_agent("codex-tui/0.154.0"));
    assert!(is_codex_user_agent("MyCODEXTool"));
    assert!(!is_codex_user_agent("curl/8.7.1"));
    assert!(!is_codex_user_agent(""));
}
