//! Ported from CLIProxyAPI internal/client/codex/tool-schema/
//! tool_schema_test.go, tool_schema_integer_fields_test.go and
//! testdata/history_notes_tools.json (v8.0.20, MIT).
//!
//! `TestNormalizeCodexToolIntegerTypes` is ported case by case. Its "nil or
//! empty headers" case passes an empty user agent, as callers take the
//! value from the headers. Upstream compares bodies byte for byte; these
//! tests compare them as compact JSON, which keeps the key order.
//!
//! Added: Chat Completions tools, namespaces, the key order kept, and type
//! arrays with members that aren't strings.

use serde_json::{Map, Value};

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
    let expected: [(&str, &[&str]); 7] = [
        (
            "exec_command",
            &["yield_time_ms", "max_output_tokens", "timeout_ms"],
        ),
        (
            "write_stdin",
            &["session_id", "yield_time_ms", "max_output_tokens"],
        ),
        ("sleep", &["duration_ms"]),
        ("wait_agent", &["timeout_ms"]),
        ("wait", &["yield_time_ms", "max_tokens"]),
        ("tool_search", &["limit"]),
        (
            "test_sync_tool",
            &[
                "sleep_before_ms",
                "sleep_after_ms",
                "participants",
                "timeout_ms",
            ],
        ),
    ];
    for (name, fields) in expected {
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
    // A namespace's tools are known by their qualified names, so `ns`'s
    // `wait` isn't Codex's `wait`.
    let body = normalized(
        r#"{"tools": [
        {"type": "function", "function": {"name": "sleep", "parameters": {"properties": {"duration_ms": {"type": "number"}}}}},
        {"type": "namespace", "name": "collaboration", "tools": [{"type": "function", "name": "wait_agent", "parameters": {"properties": {"timeout_ms": {"type": "number"}}}}]},
        {"type": "namespace", "name": "ns", "tools": [{"type": "function", "name": "wait", "parameters": {"properties": {"max_tokens": {"type": "number"}}}}]}
    ]}"#,
        "Codex",
    );
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"]["duration_ms"]["type"],
        "integer"
    );
    assert_eq!(
        body["tools"][1]["tools"][0]["parameters"]["properties"]["timeout_ms"]["type"],
        "integer"
    );
    assert_eq!(
        body["tools"][2]["tools"][0]["parameters"]["properties"]["max_tokens"]["type"],
        "number"
    );
}

// Not upstream's: a namespace inside another, or one without a name, names
// no Codex tool.
#[test]
fn nested_and_nameless_namespaces_are_left_alone() {
    let text = r#"{"tools":[{"type":"namespace","name":"outer","tools":[{"type":"namespace","name":"memories","tools":[{"type":"function","name":"read","parameters":{"properties":{"max_lines":{"type":"number"}}}}]}]},{"type":"namespace","tools":[{"type":"function","name":"sleep","parameters":{"properties":{"duration_ms":{"type":"number"}}}}]}]}"#;
    let mut body = json(text);
    assert!(!normalize(&mut body, TUI));
    assert_eq!(body.to_string(), json(text).to_string());
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

/// Sets `value` at the dotted `path` in `body`, as sjson's `SetRawBytes`
/// does: a missing step becomes an array when the step after it is an index
/// and an object otherwise, and an array is padded with nulls up to an index.
fn set_path(body: &mut Value, path: &str, value: Value) {
    let steps: Vec<&str> = path.split('.').collect();
    let mut current = body;
    for (i, step) in steps.iter().enumerate() {
        let fresh = match steps.get(i + 1) {
            Some(next) if next.bytes().all(|b| b.is_ascii_digit()) => Value::Array(Vec::new()),
            Some(_) => Value::Object(Map::new()),
            None => Value::Null,
        };
        let slot = match current {
            Value::Array(items) => {
                let index: usize = step.parse().unwrap();
                if items.len() <= index {
                    items.resize(index + 1, Value::Null);
                }
                &mut items[index]
            }
            Value::Object(map) => map.entry(step.to_string()).or_insert(Value::Null),
            other => panic!("{path} steps through {other}"),
        };
        if slot.is_null() {
            *slot = fresh;
        }
        current = slot;
    }
    *current = value;
}

fn with(body: &Value, path: &str, value: &str) -> Value {
    let mut body = body.clone();
    set_path(&mut body, path, json(value));
    body
}

fn normalized_value(body: &Value, user_agent: &str) -> Value {
    let mut body = body.clone();
    normalize(&mut body, user_agent);
    body
}

// TestNormalizeCodexToolIntegerTypesSourceFields
#[test]
fn source_fields() {
    let cases: [(&str, &[&str]); 28] = [
        (
            "test_sync_tool",
            &[
                "barrier.properties.participants",
                "barrier.properties.timeout_ms",
            ],
        ),
        ("create_goal", &["token_budget"]),
        ("get_channels", &["limit"]),
        ("list_threads", &["limit", "max_chars_per_post"]),
        ("search_posts", &["limit", "max_chars_per_post"]),
        ("read_thread", &["limit", "max_chars_per_post"]),
        ("read_post", &["offset_chars", "limit_chars"]),
        ("memories__list", &["max_results"]),
        ("memories__read", &["line_offset", "max_lines"]),
        ("memories__search", &["context_lines", "max_results"]),
        ("history__list_windows", &["limit"]),
        ("history__list_items", &["limit", "max_chars_per_item"]),
        ("history__read_item", &["offset_chars", "limit_chars"]),
        ("history__search_contents", &["limit"]),
        ("notes__list_files_by_prefix", &["max_results"]),
        (
            "notes__read_file",
            &[
                "start_line",
                "stop_line",
                "start_line.anyOf.0",
                "stop_line.anyOf.0",
            ],
        ),
        (
            "notes__search_contents",
            &["max_matches_per_file", "max_files"],
        ),
        ("image_gen__imagegen", &["num_last_images_to_include"]),
        (
            "web__run",
            &[
                "search_query.items.properties.recency",
                "image_query.items.properties.recency",
                "open.items.properties.lineno",
                "click.items.properties.id",
                "screenshot.items.properties.pageno",
                "weather.items.properties.duration",
                "sports.items.properties.num_games",
            ],
        ),
        ("collaboration__wait_agent", &["timeout_ms"]),
        ("multi_agent_v1__wait_agent", &["timeout_ms"]),
        ("functions__create_goal", &["token_budget"]),
        ("collab__read_post", &["offset_chars", "limit_chars"]),
        ("collaboration__get_channels", &["limit"]),
        (
            "collaboration__list_threads",
            &["limit", "max_chars_per_post"],
        ),
        (
            "collaboration__search_posts",
            &["limit", "max_chars_per_post"],
        ),
        (
            "collaboration__read_thread",
            &["limit", "max_chars_per_post"],
        ),
        ("collaboration__read_post", &["offset_chars", "limit_chars"]),
    ];
    for (name, fields) in cases {
        for kind in [
            r#""number""#,
            r#"["number","integer","null"]"#,
            r#""integer""#,
            r#""string""#,
        ] {
            let mut input = json(
                r#"{"tools":[{"type":"function","name":"","parameters":{"type":"object","properties":{"unrelated":{"type":"number","default":1.5}}},"output_schema":{"properties":{"wall_time_seconds":{"type":"number"}}}}],"input":[{"type":"function_call","arguments":"{\"limit\":1.5}"}]}"#,
            );
            input["tools"][0]["name"] = Value::from(name);
            for field in fields {
                set_path(
                    &mut input,
                    &format!("tools.0.parameters.properties.{field}"),
                    json(&format!(
                        r#"{{"type":{kind},"description":"Keep metadata","minimum":0,"default":1}}"#
                    )),
                );
            }
            for user_agent in [TUI, "curl/8.7.1", ""] {
                let mut want = input.clone();
                if user_agent == TUI {
                    let want_kind = match kind {
                        r#""number""# => r#""integer""#,
                        r#"["number","integer","null"]"# => r#"["integer","null"]"#,
                        other => other,
                    };
                    for field in fields {
                        set_path(
                            &mut want,
                            &format!("tools.0.parameters.properties.{field}.type"),
                            json(want_kind),
                        );
                    }
                }
                let out = normalized_value(&input, user_agent);
                assert_eq!(
                    out.to_string(),
                    want.to_string(),
                    "{name} UA {user_agent:?} type {kind}"
                );
                assert_eq!(
                    normalized_value(&out, user_agent).to_string(),
                    out.to_string(),
                    "{name} is not idempotent"
                );
            }
        }
    }

    // Not upstream's: a namespace named `functions` qualifies its tools as
    // `functions__<tool>`, which are Codex's flat tools.
    let input = json(
        r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"function","name":"exec_command","parameters":{"properties":{"timeout_ms":{"type":"number"}}}}]}]}"#,
    );
    assert_eq!(
        normalized_value(&input, TUI).to_string(),
        with(
            &input,
            "tools.0.tools.0.parameters.properties.timeout_ms.type",
            r#""integer""#
        )
        .to_string()
    );
}

// TestNormalizeCodexToolIntegerTypesNamespaceFormats
#[test]
fn namespace_formats() {
    let schema = r#"{"type":"object","properties":{"line_offset":{"type":"number"},"max_lines":{"type":["number","null"]},"max_tokens":{"type":"number"}}}"#;
    let cases = [
        (
            "responses",
            r#"{"tools":[{"type":"function","name":"memories__read","parameters":SCHEMA}]}"#,
            "tools.0.parameters",
        ),
        (
            "chat",
            r#"{"tools":[{"type":"function","function":{"name":"memories__read","parameters":SCHEMA}}]}"#,
            "tools.0.function.parameters",
        ),
        (
            "claude",
            r#"{"tools":[{"name":"memories__read","input_schema":SCHEMA}]}"#,
            "tools.0.input_schema",
        ),
        (
            "gemini",
            r#"{"tools":[{"function_declarations":[{"name":"memories__read","parameters":SCHEMA}]}]}"#,
            "tools.0.function_declarations.0.parameters",
        ),
        (
            "gemini JSON schema",
            r#"{"tools":[{"functionDeclarations":[{"name":"memories__read","parametersJsonSchema":SCHEMA}]}]}"#,
            "tools.0.functionDeclarations.0.parametersJsonSchema",
        ),
        (
            "namespace",
            r#"{"tools":[{"type":"namespace","name":"memories","tools":[{"type":"function","name":"read","parameters":SCHEMA}]}]}"#,
            "tools.0.tools.0.parameters",
        ),
        (
            "additional namespace",
            r#"{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"memories","tools":[{"type":"function","name":"read","parameters":SCHEMA}]}]}]}"#,
            "input.0.tools.0.tools.0.parameters",
        ),
    ];
    for (name, body, path) in cases {
        let input = json(&body.replace("SCHEMA", schema));
        let want = with(
            &input,
            &format!("{path}.properties.line_offset.type"),
            r#""integer""#,
        );
        let want = with(
            &want,
            &format!("{path}.properties.max_lines.type"),
            r#"["integer","null"]"#,
        );
        assert_eq!(
            normalized_value(&input, "Codex/1.0").to_string(),
            want.to_string(),
            "{name}"
        );
    }
}

// TestNormalizeCodexToolIntegerTypesPreservesUnprovenFields
#[test]
fn preserves_unproven_fields() {
    let cases = [
        ("unknown_tool", ""),
        ("mcp__server__read_post", ""),
        ("read", ""),
        ("run", ""),
        ("imagegen", ""),
        ("read_post", "user_tools"),
        ("wait_agent", "mcp__server"),
        ("read", "skills"),
        ("read", "user_tools"),
        ("read_post", "multi_agent_v1"),
        ("read_post", "arbitrary_collaboration"),
    ];
    for (name, namespace) in cases {
        let mut tool = json(
            r#"{"type":"function","name":"","parameters":{"properties":{"limit":{"type":"number"},"offset_chars":{"type":"number"},"line_offset":{"type":"number"},"timeout_ms":{"type":"number"}}}}"#,
        );
        tool["name"] = Value::from(name);
        if !namespace.is_empty() {
            tool = serde_json::json!({"type": "namespace", "name": namespace, "tools": [tool]});
        }
        let input = serde_json::json!({ "tools": [tool] });
        assert_eq!(
            normalized_value(&input, "codex").to_string(),
            input.to_string(),
            "{namespace}/{name}"
        );
    }

    let input = json(
        r#"{"tools":[{"name":"test_sync_tool","parameters":{"properties":{"barrier.participants":{"type":"number"},"other":{"properties":{"participants":{"type":"number"}}},"barrier":{"properties":{"participants":{"type":"number"},"ratio":{"type":"number"}}}}}}]}"#,
    );
    let want = with(
        &input,
        "tools.0.parameters.properties.barrier.properties.participants.type",
        r#""integer""#,
    );
    assert_eq!(
        normalized_value(&input, "codex").to_string(),
        want.to_string()
    );
}

const HISTORY_NOTES_TOOLS: &str = include_str!("testdata/history_notes_tools.json");

// TestNormalizeCodexToolIntegerTypesHistoryNotesSchemas. Upstream degrades
// the fixture's integers to numbers in its text, and so does this.
#[test]
fn history_notes_schemas() {
    let fixture = json(HISTORY_NOTES_TOOLS);
    let degraded =
        json(&HISTORY_NOTES_TOOLS.replace(r#""type": "integer""#, r#""type": "number""#));
    assert_ne!(fixture, degraded);
    let namespaces = fixture["tools"].as_array().unwrap();
    let degraded_namespaces = degraded["tools"].as_array().unwrap();
    assert_eq!(namespaces.len(), 7);
    for (namespace, degraded_namespace) in namespaces.iter().zip(degraded_namespaces) {
        let tool = &namespace["tools"][0];
        let degraded_tool = &degraded_namespace["tools"][0];
        let tool_name = tool["name"].as_str().unwrap();
        let name = format!("{}__{tool_name}", namespace["name"].as_str().unwrap());
        let formats = |namespace: &Value, tool: &Value| {
            [
                ("namespace", serde_json::json!({ "tools": [namespace] })),
                (
                    "flat",
                    serde_json::json!({"tools": [{"name": name, "parameters": tool["parameters"]}]}),
                ),
                (
                    "additional",
                    serde_json::json!({"input": [{"type": "additional_tools", "tools": [namespace]}]}),
                ),
            ]
        };
        for ((format, body), (_, input)) in formats(namespace, tool)
            .into_iter()
            .zip(formats(degraded_namespace, degraded_tool))
        {
            for user_agent in ["codex", "curl", ""] {
                let want = if user_agent == "codex" { &body } else { &input };
                let out = normalized_value(&input, user_agent);
                assert_eq!(
                    out.to_string(),
                    want.to_string(),
                    "{name} {format} UA {user_agent:?}"
                );
                assert_eq!(
                    normalized_value(&out, user_agent).to_string(),
                    out.to_string(),
                    "{name} {format} is not stable"
                );
            }
        }
        for unknown in [
            tool_name.to_owned(),
            format!("mcp__server__{name}"),
            format!("user__{name}"),
        ] {
            let input = serde_json::json!({"tools": [{"name": unknown, "parameters": degraded_tool["parameters"]}]});
            assert_eq!(
                normalized_value(&input, "codex").to_string(),
                input.to_string(),
                "unknown tool {unknown}"
            );
        }
    }
}

// TestNormalizeCodexToolIntegerTypesNotesExplicitUnionPaths
#[test]
fn notes_explicit_union_paths() {
    let input = json(
        r#"{"tools":[{"name":"notes__read_file","parameters":{"type":"object","properties":{"start_line":{"anyOf":[{"type":"number"},{"type":"null"}],"default":-3},"stop_line":{"anyOf":[{"type":"number"},{"type":"number"}],"default":-1},"ratio":{"anyOf":[{"type":"number"},{"type":"null"}]},"other":{"properties":{"start_line":{"anyOf":[{"type":"number"}]}}}},"required":["path"]}}],"input":[{"type":"function_call","arguments":"{\"start_line\":-3,\"stop_line\":-1}"}]}"#,
    );
    let want = with(
        &input,
        "tools.0.parameters.properties.start_line.anyOf.0.type",
        r#""integer""#,
    );
    let want = with(
        &want,
        "tools.0.parameters.properties.stop_line.anyOf.0.type",
        r#""integer""#,
    );
    assert_eq!(
        normalized_value(&input, "codex").to_string(),
        want.to_string()
    );
}

// Not upstream's: a path step that is an index reaches into an array only,
// and a step that isn't one finds nothing in an array.
#[test]
fn paths_read_indexes_in_arrays_only() {
    let input = json(
        r#"{"tools":[{"name":"notes__read_file","parameters":{"properties":{"start_line":{"anyOf":{"0":{"type":"number"}}},"stop_line":{"anyOf":[]}}}},{"name":"web__run","parameters":{"properties":{"open":{"items":[{"properties":{"lineno":{"type":"number"}}}]}}}}]}"#,
    );
    let want = with(
        &input,
        "tools.0.parameters.properties.start_line.anyOf.0.type",
        r#""integer""#,
    );
    assert_eq!(
        normalized_value(&input, "codex").to_string(),
        want.to_string()
    );
}

#[test]
fn is_codex_user_agent_cases() {
    assert!(is_codex_user_agent("codex-tui/0.154.0"));
    assert!(is_codex_user_agent("MyCODEXTool"));
    assert!(!is_codex_user_agent("curl/8.7.1"));
    assert!(!is_codex_user_agent(""));
}
