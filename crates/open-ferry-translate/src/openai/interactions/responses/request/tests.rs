// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_request_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

// 24 of the 30 tests are ported whole, and two in part. Dropped, because an
// Antigravity model is handled like any other here:
// TestConvertOpenAIResponsesRequestToInteractions_AntigravitySanitizesGenerationConfigAndSetsAgentConfig,
// TestConvertOpenAIResponsesRequestToInteractionsRenamesConflictingAntigravityToolCallsAndResultsInHistory,
// TestConvertOpenAIResponsesRequestToInteractions_AntigravityCustomToolRenamed and
// TestConvertOpenAIResponsesRequestToInteractions_AntigravityDoesNotFilterDevinTools, and
// the Antigravity halves of
// TestConvertOpenAIResponsesRequestToInteractionsRenamesConflictingAntigravityTools and
// TestConvertOpenAIResponsesRequestToInteractionsRenamesConflictingToolChoice.
// TestConvertOpenAIResponsesRequestToInteractions_DevinToolsFilterAndObfuscate
// checks that tool descriptions are passed on unchanged, where upstream
// checks that two of them are reworded. TestConvertOpenAIResponsesRequestToInteractions_LogFilePayload
// uses its fixture, not a log file.

use serde_json::json;

use super::*;

mod patch_declaration;

fn to_interactions(model: &str, request: Value) -> Value {
    convert_openai_responses_request_to_interactions(model, &request, false)
}

fn to_responses(model: &str, request: Value) -> Value {
    convert_interactions_request_to_openai_responses(model, &request, false)
}

/// Looks up a dotted path such as `input.0.type`, like a plain gjson path.
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
fn items<'v>(value: &'v Value, path: &str) -> &'v [Value] {
    at(value, path)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The names of the output's tools, in order.
fn tool_names(out: &Value) -> Vec<String> {
    items(out, "tools")
        .iter()
        .map(|tool| text_at(tool, "name"))
        .collect()
}

/// TestConvertOpenAIResponsesRequestToInteractions
#[test]
fn converts_a_responses_request() {
    let request = json!({
        "model": "gpt-test",
        "instructions": "be brief",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"},
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
            ]},
            {"type": "function_call", "name": "lookup", "call_id": "call_1", "arguments": "{\"q\":\"x\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": {"ok": true}},
        ],
        "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
        "tool_choice": "auto",
        "reasoning": {"effort": "high", "summary": "auto"},
        "response_format": {"type": "json_object"},
        "stream": true,
    });
    let out = convert_openai_responses_request_to_interactions("gpt-test", &request, true);
    assert_eq!(text_at(&out, "input.0.type"), "user_input", "{out}");
    assert_eq!(text_at(&out, "input.0.content.0.type"), "text", "{out}");
    assert_eq!(text_at(&out, "input.0.content.0.text"), "hi", "{out}");
    assert_eq!(
        text_at(&out, "input.0.content.1.mime_type"),
        "image/png",
        "{out}"
    );
    assert_eq!(text_at(&out, "input.1.call_id"), "call_1", "{out}");
    assert_eq!(text_at(&out, "input.2.type"), "function_result", "{out}");
    assert_eq!(text_at(&out, "input.2.name"), "lookup", "{out}");
    assert_eq!(out["system_instruction"], "be brief", "{out}");
    assert!(at(&out, "system_instruction.parts").is_none(), "{out}");
    assert_eq!(
        text_at(&out, "generation_config.thinking_level"),
        "high",
        "{out}"
    );
    assert_eq!(text_at(&out, "tools.0.name"), "lookup", "{out}");
    assert_eq!(
        text_at(&out, "generation_config.tool_choice"),
        "auto",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "response_format.type"),
        "json_object",
        "{out}"
    );
}

/// TestConvertOpenAIResponsesRequestToInteractionsPreservesRequestStream
#[test]
fn the_request_stream_flag_wins() {
    let out = to_interactions(
        "gpt-test",
        json!({"model": "gpt-test", "input": "hi", "stream": true}),
    );
    assert_eq!(out["stream"], true, "{out}");

    let request = json!({"model": "gpt-test", "input": "hi", "stream": false});
    let out = convert_openai_responses_request_to_interactions("gpt-test", &request, true);
    assert_eq!(out["stream"], false, "{out}");
}

/// TestConvertOpenAIResponsesRequestToInteractionsPreservesPreviousResponseID
#[test]
fn previous_response_id_becomes_previous_interaction_id() {
    let out = to_interactions(
        "gpt-test",
        json!({"model": "gpt-test", "input": "hi", "previous_response_id": "resp_123"}),
    );
    assert_eq!(
        text_at(&out, "previous_interaction_id"),
        "resp_123",
        "{out}"
    );
}

/// TestConvertInteractionsRequestToOpenAIResponsesWithToolMessages
#[test]
fn interactions_tool_steps_become_responses_items() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [
            {"type": "user_input", "content": [{"type": "text", "text": "hi"}]},
            {"type": "function_call", "name": "lookup", "call_id": "call_1", "arguments": {"q": "x"}},
            {"type": "function_result", "name": "lookup", "call_id": "call_1", "result": {"ok": true}},
        ]}),
    );
    let input = items(&out, "input");
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call input not found");
    assert_eq!(text_at(call, "name"), "lookup");
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "function_call_output"),
        "function_call_output input not found: {out}"
    );
}

/// TestConvertInteractionsRequestToOpenAIResponsesPreservesStringSystemAndThinkingConfig
#[test]
fn string_system_instruction_and_thinking_config_are_kept() {
    let request = json!({
        "model": "gpt-test",
        "system_instruction": "You are a helpful assistant.",
        "input": [{"type": "user_input", "content": [{"type": "text", "text": "hi"}]}],
        "tools": [{"name": "lookup", "type": "function", "parameters": {"type": "object"}}],
        "generation_config": {"tool_choice": "auto", "thinking_level": "high", "thinking_summaries": "auto"},
        "stream": true,
    });
    let out = convert_interactions_request_to_openai_responses("gpt-test", &request, true);
    assert_eq!(
        text_at(&out, "instructions"),
        "You are a helpful assistant.",
        "{out}"
    );
    assert_eq!(text_at(&out, "tool_choice"), "auto", "{out}");
    assert_eq!(text_at(&out, "reasoning.effort"), "high", "{out}");
    assert_eq!(text_at(&out, "reasoning.summary"), "auto", "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponsesPreservesInteractionStream
#[test]
fn the_interaction_stream_flag_is_kept() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": "hi", "stream": true}),
    );
    assert_eq!(out["stream"], true, "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponsesPreservesPreviousInteractionID
#[test]
fn previous_interaction_id_becomes_previous_response_id() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": "hi", "previous_interaction_id": "interaction_123"}),
    );
    assert_eq!(
        text_at(&out, "previous_response_id"),
        "interaction_123",
        "{out}"
    );
}

/// TestConvertInteractionsRequestToOpenAIResponsesPreservesToolCallID
#[test]
fn tool_call_ids_are_kept() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [
            {"type": "function_call", "name": "lookup", "call_id": "call_gateway", "arguments": {"q": "x"}},
            {"type": "function_result", "name": "lookup", "call_id": "call_gateway", "result": {"ok": true}},
        ]}),
    );
    let input = items(&out, "input");
    for kind in ["function_call", "function_call_output"] {
        let item = input
            .iter()
            .find(|item| item["type"] == kind)
            .unwrap_or_else(|| panic!("{kind} input not found: {out}"));
        assert_eq!(text_at(item, "call_id"), "call_gateway", "{out}");
    }
}

/// TestConvertInteractionsRequestToOpenAIResponsesConvertsSimpleTools
#[test]
fn simple_tools_become_functions() {
    let out = to_responses(
        "gpt-test",
        json!({
            "model": "gpt-test",
            "tools": [{"name": "lookup", "description": "Find data", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}],
            "input": "hi",
        }),
    );
    assert_eq!(text_at(&out, "tools.0.type"), "function", "{out}");
    assert_eq!(text_at(&out, "tools.0.name"), "lookup", "{out}");
    assert!(at(&out, "tools.0.function").is_none(), "{out}");
    assert_eq!(
        text_at(&out, "tools.0.parameters.properties.q.type"),
        "string",
        "{out}"
    );
}

/// TestConvertInteractionsRequestToOpenAIResponsesConvertsFunctionDeclarationsTools
#[test]
fn function_declarations_become_functions() {
    let out = to_responses(
        "gpt-test",
        json!({
            "model": "gpt-test",
            "tools": [{"function_declarations": [{"name": "lookup", "description": "Find data", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}]}],
            "input": "hi",
        }),
    );
    assert_eq!(text_at(&out, "tools.0.type"), "function", "{out}");
    assert_eq!(text_at(&out, "tools.0.name"), "lookup", "{out}");
    assert!(at(&out, "tools.0.function_declarations").is_none(), "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponsesWithImageContent
#[test]
fn inline_images_become_data_urls() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [{"type": "user_input", "content": [
            {"type": "text", "text": "describe"},
            {"type": "image", "mime_type": "image/png", "data": "aGVsbG8="},
        ]}]}),
    );
    assert_eq!(
        text_at(&out, "input.0.content.1.type"),
        "input_image",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "input.0.content.1.image_url"),
        "data:image/png;base64,aGVsbG8=",
        "{out}"
    );
}

/// TestConvertInteractionsRequestToOpenAIResponsesPreservesNonImageMediaContent
#[test]
fn audio_video_and_documents_are_not_images() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [{"type": "model_output", "content": [
            {"type": "audio", "mime_type": "audio/wav", "data": "UklGRg=="},
            {"type": "video", "mime_type": "video/mp4", "data": "AAAAIGZ0eXA="},
            {"type": "document", "mime_type": "application/pdf", "data": "JVBERi0="},
        ]}]}),
    );
    assert_eq!(
        text_at(&out, "input.0.content.0.type"),
        "output_text",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "input.0.content.1.type"),
        "output_file",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "input.0.content.2.type"),
        "output_file",
        "{out}"
    );
    assert!(
        !items(&out, "input.0.content")
            .iter()
            .any(|part| part["type"] == "output_image"),
        "{out}"
    );
}

/// TestConvertInteractionsRequestToOpenAIResponsesWithAssistantTextContent
#[test]
fn model_text_becomes_output_text() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [{"type": "model_output", "content": [{"type": "text", "text": "hello"}]}]}),
    );
    assert_eq!(
        text_at(&out, "input.0.content.0.type"),
        "output_text",
        "{out}"
    );
    assert_eq!(text_at(&out, "input.0.content.0.text"), "hello", "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponsesWithUserObjectContent
#[test]
fn user_text_becomes_input_text() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [{"type": "user_input", "content": [{"type": "text", "text": "hi"}]}]}),
    );
    assert_eq!(
        text_at(&out, "input.0.content.0.type"),
        "input_text",
        "{out}"
    );
    assert_eq!(text_at(&out, "input.0.content.0.text"), "hi", "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponsesWithStringFunctionArguments
#[test]
fn function_call_arguments_become_a_string() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": [
            {"type": "function_call", "name": "lookup", "call_id": "call_1", "arguments": {"q": "x"}},
            {"type": "function_result", "name": "lookup", "call_id": "call_1", "result": {"ok": true}},
        ]}),
    );
    let call = items(&out, "input")
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call input not found");
    assert_eq!(call["arguments"], r#"{"q":"x"}"#, "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponsesPreservesExpressibleFields
#[test]
fn expressible_fields_are_kept() {
    let out = to_responses(
        "gpt-test",
        json!({
            "model": "gpt-test",
            "tool_choice": {"type": "function", "function": {"name": "lookup"}},
            "response_modalities": ["text", "image"],
            "service_tier": "priority",
            "store": true,
            "background": true,
            "webhook_config": {"url": "https://example.com"},
            "input": "hi",
        }),
    );
    assert_eq!(text_at(&out, "tool_choice.type"), "function", "{out}");
    assert_eq!(
        text_at(&out, "tool_choice.function.name"),
        "lookup",
        "{out}"
    );
    assert_eq!(text_at(&out, "modalities.0"), "text", "{out}");
    assert_eq!(text_at(&out, "modalities.1"), "image", "{out}");
    assert_eq!(text_at(&out, "service_tier"), "priority", "{out}");
    for key in ["store", "background", "webhook_config"] {
        assert!(
            out.get(key).is_none(),
            "{key} should not be forwarded: {out}"
        );
    }
}

/// TestConvertOpenAIResponsesRequestToInteractions_PreservesEnvironmentID
#[test]
fn responses_environment_id_is_kept() {
    let out = to_interactions(
        "gpt-test",
        json!({"model": "gpt-test", "input": "hi", "previous_response_id": "resp_123", "environment_id": "env_abc456"}),
    );
    assert_eq!(
        text_at(&out, "previous_interaction_id"),
        "resp_123",
        "{out}"
    );
    assert_eq!(text_at(&out, "environment_id"), "env_abc456", "{out}");
}

/// TestConvertInteractionsRequestToOpenAIResponses_PreservesEnvironmentID
#[test]
fn interactions_environment_id_is_kept() {
    let out = to_responses(
        "gpt-test",
        json!({"model": "gpt-test", "input": "hi", "previous_interaction_id": "interaction_123", "environment_id": "env_abc456"}),
    );
    assert_eq!(
        text_at(&out, "previous_response_id"),
        "interaction_123",
        "{out}"
    );
    assert_eq!(text_at(&out, "environment_id"), "env_abc456", "{out}");
}

/// TestConvertOpenAIResponsesRequestToInteractionsRenamesConflictingAntigravityTools,
/// the half for a model that isn't Antigravity.
#[test]
fn tools_are_not_renamed() {
    let request = json!({
        "model": "antigravity-preview-05-2026",
        "input": [{"type": "message", "role": "user", "content": "read file"}],
        "tools": [
            {"type": "function", "name": "read_file", "parameters": {"type": "object"}},
            {"type": "function", "name": "write_file", "parameters": {"type": "object"}},
            {"type": "function", "name": "execute_code", "parameters": {"type": "object"}},
            {"type": "function", "name": "web_search", "parameters": {"type": "object"}},
        ],
    });
    let out = to_interactions("gemini-3.1-flash-lite", request);
    assert_eq!(text_at(&out, "tools.0.name"), "read_file", "{out}");
}

/// TestConvertOpenAIResponsesRequestToInteractionsRenamesConflictingToolChoice,
/// the half for a model that isn't Antigravity.
#[test]
fn tool_choice_is_not_renamed() {
    let request = json!({
        "model": "antigravity-preview-05-2026",
        "input": [{"type": "message", "role": "user", "content": "read file"}],
        "tools": [{"type": "function", "name": "read_file", "parameters": {"type": "object"}}],
        "tool_choice": {"type": "function", "name": "read_file"},
    });
    let out = to_interactions("gemini-3.1-flash-lite", request);
    assert_eq!(
        text_at(&out, "generation_config.tool_choice.name"),
        "read_file",
        "{out}"
    );
}

/// TestConvertOpenAIResponsesRequestToInteractions_NonAntigravityGenerationConfig
#[test]
fn sampling_knobs_go_into_generation_config() {
    let out = to_interactions(
        "devin/swe-2",
        json!({
            "model": "devin/swe-2",
            "input": [{"type": "message", "role": "user", "content": "write a poem"}],
            "max_output_tokens": 400,
            "temperature": 0.7,
            "top_p": 0.95,
        }),
    );
    assert_eq!(out["generation_config"]["max_output_tokens"], 400, "{out}");
    assert_eq!(out["generation_config"]["temperature"], 0.7, "{out}");
    assert_eq!(out["generation_config"]["top_p"], 0.95, "{out}");
}

/// TestConvertOpenAIResponsesRequestToInteractions_FlattensNamespaceTools
#[test]
fn namespace_tools_are_flattened() {
    let out = to_interactions(
        "devin/gemini-3-7-flash",
        json!({
            "model": "devin/gemini-3-7-flash",
            "tools": [
                {
                    "type": "function",
                    "name": "exec_command",
                    "description": "Execute a command",
                    "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]},
                },
                {
                    "type": "namespace",
                    "name": "multi_agent_v1",
                    "description": "Multi agent tools",
                    "tools": [
                        {
                            "type": "function",
                            "name": "close_agent",
                            "description": "Close an agent",
                            "parameters": {"type": "object", "properties": {"target": {"type": "string"}}, "required": ["target"]},
                        },
                        {
                            "type": "function",
                            "name": "resume_agent",
                            "description": "Resume an agent",
                            "parameters": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
                        },
                    ],
                },
                {
                    "type": "namespace",
                    "name": "functions",
                    "tools": [{"type": "custom", "name": "exec", "description": "Run custom command"}],
                },
            ],
            "tool_choice": {"type": "function", "name": "close_agent", "namespace": "multi_agent_v1"},
        }),
    );
    let tools = items(&out, "tools");
    assert_eq!(tools.len(), 4, "{out}");
    assert_eq!(text_at(&tools[0], "name"), "exec_command");
    assert_eq!(text_at(&tools[0], "type"), "function");
    assert_eq!(text_at(&tools[1], "name"), "multi_agent_v1__close_agent");
    assert_eq!(text_at(&tools[1], "description"), "Close an agent");
    assert_eq!(
        text_at(&tools[1], "parameters.properties.target.type"),
        "string"
    );
    assert_eq!(text_at(&tools[2], "name"), "multi_agent_v1__resume_agent");
    assert_eq!(text_at(&tools[3], "name"), "functions__exec");
    assert_eq!(
        text_at(&tools[3], "parameters.properties.input.type"),
        "string"
    );
    assert_eq!(
        text_at(&out, "generation_config.tool_choice.name"),
        "multi_agent_v1__close_agent",
        "{out}"
    );
}

/// TestConvertOpenAIResponsesRequestToInteractions_HistoryNamespaceAndCustomTools
#[test]
fn history_names_are_qualified_and_outputs_named_after_their_calls() {
    let out = to_interactions(
        "devin/gemini-3-7-flash",
        json!({
            "model": "devin/gemini-3-7-flash",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "close_agent",
                    "namespace": "multi_agent_v1",
                    "arguments": "{\"target\":\"agent_123\"}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "name": "close_agent",
                    "namespace": "multi_agent_v1",
                    "output": "{\"status\":\"closed\"}",
                },
                {
                    "type": "custom_tool_call",
                    "call_id": "call_2",
                    "name": "exec",
                    "namespace": "functions",
                    "input": "ls -la",
                },
                {"type": "custom_tool_call_output", "call_id": "call_2", "output": "file1.txt"},
            ],
        }),
    );
    assert_eq!(
        text_at(&out, "input.0.name"),
        "multi_agent_v1__close_agent",
        "{out}"
    );
    assert_eq!(text_at(&out, "input.0.call_id"), "call_1", "{out}");
    assert_eq!(
        text_at(&out, "input.1.name"),
        "multi_agent_v1__close_agent",
        "{out}"
    );
    assert_eq!(text_at(&out, "input.2.name"), "functions__exec", "{out}");
    assert_eq!(text_at(&out, "input.2.arguments.input"), "ls -la", "{out}");
    assert_eq!(text_at(&out, "input.3.name"), "functions__exec", "{out}");
    assert_eq!(text_at(&out, "input.3.result"), "file1.txt", "{out}");
}

/// TestConvertOpenAIResponsesRequestToInteractions_FixedFixtureFullNamespaceExpansion
#[test]
fn namespaces_expand_in_declaration_order() {
    let out = to_interactions(
        "devin/gemini-3-7-flash",
        json!({
            "model": "devin/gemini-3-7-flash",
            "tools": [
                {"type": "function", "name": "top_fn_1"},
                {"type": "function", "name": "top_fn_2"},
                {"type": "namespace", "name": "ns_a", "tools": [
                    {"type": "function", "name": "child_1"},
                    {"type": "function", "name": "child_2"},
                ]},
                {"type": "namespace", "name": "ns_b", "tools": [
                    {"type": "custom", "name": "custom_1"},
                    {"type": "function", "name": "child_3"},
                ]},
                {"type": "function", "name": "top_fn_3"},
            ],
        }),
    );
    assert_eq!(
        tool_names(&out),
        [
            "top_fn_1",
            "top_fn_2",
            "ns_a__child_1",
            "ns_a__child_2",
            "ns_b__custom_1",
            "ns_b__child_3",
            "top_fn_3",
        ],
        "{out}"
    );
}

/// TestConvertOpenAIResponsesRequestToInteractions_LogFilePayload, with its
/// fallback fixture.
#[test]
fn every_flattened_tool_has_a_name() {
    let out = to_interactions(
        "devin/gemini-3-7-flash",
        json!({
            "model": "devin/gemini-3-7-flash",
            "tools": [
                {"type": "function", "name": "t0"},
                {"type": "function", "name": "t1"},
                {"type": "function", "name": "t2"},
                {"type": "function", "name": "t3"},
                {"type": "function", "name": "t4"},
                {"type": "function", "name": "t5"},
                {"type": "function", "name": "t6"},
                {"type": "function", "name": "t7"},
                {"type": "namespace", "name": "ns8", "tools": [{"type": "function", "name": "c1"}, {"type": "function", "name": "c2"}]},
                {"type": "namespace", "name": "ns9", "tools": [{"type": "function", "name": "c1"}]},
                {"type": "function", "name": "t10"},
            ],
        }),
    );
    let tools = items(&out, "tools");
    assert!(!tools.is_empty(), "{out}");
    for tool in tools {
        assert!(!text_at(tool, "name").is_empty(), "{tool}");
        assert!(tool.get("function_declarations").is_none(), "{tool}");
    }
}

/// TestConvertOpenAIResponsesRequestToInteractions_DevinToolsFilterAndObfuscate,
/// checking that descriptions are passed on unchanged, where upstream checks
/// that two of them are reworded.
#[test]
fn automation_update_is_dropped_and_descriptions_kept() {
    let exec_description =
        "Runs a command in a bash shell, returning output or a session ID for ongoing interaction.";
    let stdin_description =
        "Writes characters to an existing unified exec session and returns recent output.";
    let out = to_interactions(
        "devin/swe-2",
        json!({
            "model": "devin/swe-2",
            "tools": [
                {
                    "type": "namespace",
                    "name": "mcp__codex_app",
                    "description": "Codex App tools",
                    "tools": [
                        {
                            "type": "function",
                            "name": "automation_update",
                            "description": "Recurring automations",
                            "parameters": {"type": "object", "properties": {"id": {"type": "string"}}},
                        },
                        {
                            "type": "function",
                            "name": "read_resource",
                            "description": "Read a resource",
                            "parameters": {"type": "object", "properties": {"uri": {"type": "string"}}},
                        },
                    ],
                },
                {
                    "type": "function",
                    "name": "exec_command",
                    "description": exec_description,
                    "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]},
                },
                {
                    "type": "function",
                    "name": "write_stdin",
                    "description": stdin_description,
                    "parameters": {"type": "object", "properties": {"session_id": {"type": "string"}}, "required": ["session_id"]},
                },
                {
                    "type": "function",
                    "name": "other_func",
                    "description": "Regular function description",
                    "parameters": {"type": "object"},
                },
            ],
            "tool_choice": {"type": "function", "name": "automation_update", "namespace": "mcp__codex_app"},
            "input": [{"type": "message", "role": "user", "content": "hello"}],
        }),
    );
    let descriptions: HashMap<String, String> = items(&out, "tools")
        .iter()
        .map(|tool| (text_at(tool, "name"), text_at(tool, "description")))
        .collect();
    assert!(
        !descriptions
            .keys()
            .any(|name| name.contains("automation_update")),
        "{out}"
    );
    assert!(
        descriptions.contains_key("mcp__codex_app__read_resource"),
        "{out}"
    );
    assert_eq!(descriptions["exec_command"], exec_description, "{out}");
    assert_eq!(descriptions["write_stdin"], stdin_description, "{out}");
    assert_eq!(
        descriptions["other_func"], "Regular function description",
        "{out}"
    );
    assert!(at(&out, "generation_config.tool_choice").is_none(), "{out}");
}

/// Not upstream's: a model whose name contains `antigravity` is handled like
/// any other. Its tools keep their names, its sampling knobs stay in
/// `generation_config`, and `automation_update` is still dropped.
#[test]
fn antigravity_models_are_handled_like_any_other() {
    let request = json!({
        "model": "antigravity-preview-05-2026",
        "input": [
            {"type": "function_call", "name": "read_file", "call_id": "call_1", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "hello file"},
        ],
        "max_output_tokens": 2048,
        "temperature": 0.7,
        "tools": [
            {"type": "function", "name": "read_file", "parameters": {"type": "object"}},
            {"type": "namespace", "name": "mcp__codex_app", "tools": [
                {"type": "function", "name": "automation_update"},
            ]},
        ],
        "tool_choice": {"type": "function", "name": "read_file"},
    });
    let out = to_interactions("antigravity-preview-05-2026", request);
    assert_eq!(tool_names(&out), ["read_file"], "{out}");
    assert_eq!(text_at(&out, "input.0.name"), "read_file", "{out}");
    assert_eq!(text_at(&out, "input.1.name"), "read_file", "{out}");
    assert_eq!(
        text_at(&out, "generation_config.tool_choice.name"),
        "read_file"
    );
    assert_eq!(out["generation_config"]["max_output_tokens"], 2048, "{out}");
    assert_eq!(out["generation_config"]["temperature"], 0.7, "{out}");
    assert!(out.get("agent_config").is_none(), "{out}");

    let out = to_responses(
        "antigravity-preview-05-2026",
        json!({"input": [{"type": "function_call", "name": "external_read_file", "arguments": {}}]}),
    );
    assert_eq!(text_at(&out, "input.0.name"), "external_read_file", "{out}");
}

/// Not upstream's: the whole of a small request each way, key order
/// included.
#[test]
fn whole_requests_each_way() {
    let out = to_interactions(
        "",
        json!({
            "model": "gemini-3-flash",
            "instructions": {"content": [{"text": "be "}, {"type": "x"}, {"text": "brief"}]},
            "environment": {"id": "env_1"},
            "input": [
                {"type": "message", "role": "model", "content": "hi"},
                {"type": "input_image", "url": "https://example.com/a.png"},
                {"type": "custom_tool_call", "id": "call_9", "name": "exec", "arguments": "not json"},
                {"type": "function_call_output", "id": "call_9", "result": "[1, 2]"},
                {"role": "user", "content": {"text": "loose"}},
                "ignored",
            ],
            "reasoning": {"effort": " HIGH "},
            "text": {"format": {"type": "json_object"}},
            "max_tokens": "12",
            "stop": ["x"],
        }),
    );
    assert_eq!(
        out,
        json!({
            "model": "gemini-3-flash",
            "input": [
                {"type": "model_output", "content": [{"type": "text", "text": "hi"}]},
                {"type": "user_input", "content": [{"type": "image", "image_url": "https://example.com/a.png"}]},
                {"type": "function_call", "name": "exec", "arguments": "not json", "call_id": "call_9"},
                {"type": "function_result", "name": "exec", "result": [1, 2], "call_id": "call_9"},
                {"type": "user_input", "content": [{"type": "text", "text": "loose"}]},
            ],
            "system_instruction": "be brief",
            "environment_id": "env_1",
            "generation_config": {"thinking_level": "high", "max_output_tokens": 12, "stop_sequences": ["x"]},
            "response_format": {"type": "json_object"},
        })
    );
    let keys: Vec<&String> = out
        .as_object()
        .map(|o| o.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        keys,
        [
            "model",
            "input",
            "system_instruction",
            "environment_id",
            "generation_config",
            "response_format"
        ]
    );

    let out = to_responses(
        "gpt-test",
        json!({
            "system_instruction": {"parts": [{"text": "a"}, {"text": "b"}]},
            "input": [
                {"type": "thought", "content": [{"text": "plan"}, {"content": {"text": "more"}}, {"text": ""}]},
                {"type": "function_result", "id": "call_1", "name": "lookup", "output": 5},
                {"type": "model_output", "content": {"first": {"text": "values of an object"}}},
                "plain",
            ],
            "generation_config": {"thinking_level": 7, "thinkingConfig": {"thinkingLevel": " Low "}},
            "tool_choice": "none",
        }),
    );
    assert_eq!(
        out,
        json!({
            "model": "gpt-test",
            "input": [
                {"type": "reasoning", "summary": [
                    {"type": "summary_text", "text": "plan"},
                    {"type": "summary_text", "text": "more"},
                ]},
                {"type": "function_call_output", "call_id": "call_1", "output": "5", "name": "lookup"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "values of an object"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "plain"}]},
            ],
            "instructions": "ab",
            "tool_choice": "none",
            "reasoning": {"effort": "low"},
        })
    );
}

/// Not upstream's: arguments and results that are strings holding JSON are
/// sent as that JSON, other strings as they are, and missing ones as `{}`.
#[test]
fn json_strings_are_embedded() {
    let out = to_interactions(
        "m",
        json!({"input": [
            {"type": "function_call", "name": "a", "arguments": "{\"q\": 1.50}"},
            {"type": "function_call", "name": "b", "arguments": "{oops"},
            {"type": "function_call", "name": "c"},
            {"type": "function_call_output", "call_id": "x", "output": "\"quoted\""},
            {"type": "function_call_output", "call_id": "y", "output": ""},
        ]}),
    );
    // The number keeps its text, as upstream copies it.
    assert_eq!(
        out["input"][0]["arguments"]["q"].to_string(),
        "1.50",
        "{out}"
    );
    assert_eq!(out["input"][1]["arguments"], "{oops", "{out}");
    assert_eq!(out["input"][2]["arguments"], json!({}), "{out}");
    assert_eq!(out["input"][3]["result"], "quoted", "{out}");
    assert_eq!(out["input"][3]["name"], "", "{out}");
    assert_eq!(out["input"][4]["result"], "", "{out}");
}

/// Not upstream's: the custom `apply_patch` tool and other custom tools get
/// schemas, and a request that is an array is read as its tools.
#[test]
fn custom_tools_get_schemas() {
    let out = to_interactions(
        "m",
        json!([
            {"type": "custom", "name": "apply_patch", "description": "Patch files"},
            {"type": "custom", "name": "run"},
            {"type": "function", "name": "mcp__codex_app__automation_update"},
        ]),
    );
    assert_eq!(tool_names(&out), ["apply_patch", "run"], "{out}");
    assert_eq!(out["tools"][0]["parameters"], apply_patch::parameters());
    assert_eq!(
        out["tools"][0]["description"],
        apply_patch::description(&json!({"description": "Patch files"}))
    );
    assert_eq!(
        out["tools"][1],
        json!({"type": "function", "name": "run", "parameters": {
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"],
        }})
    );
}

/// Not upstream's: a `tool_choice` naming `automation_update` is dropped
/// however it is written, and other names are qualified where they were
/// given.
#[test]
fn tool_choice_names() {
    for choice in [
        json!({"type": "function", "name": "MCP__Codex_App__Automation_Update"}),
        json!({"type": "function", "function": {"name": " automation_update ", "namespace": "mcp__codex_app"}}),
        json!({"custom": {"name": "automation_update", "namespace": "MCP__CODEX_APP"}}),
    ] {
        let out = to_interactions("m", json!({"tool_choice": choice}));
        assert!(out.get("generation_config").is_none(), "{out}");
    }
    let out = to_interactions(
        "m",
        json!({"tool_choice": {"custom": {"name": "exec"}, "namespace": "functions"}}),
    );
    assert_eq!(
        out["generation_config"]["tool_choice"],
        json!({"custom": {"name": "functions__exec"}, "namespace": "functions"})
    );
    let out = to_interactions("m", json!({"tool_choice": {"type": "auto"}}));
    assert_eq!(
        out["generation_config"]["tool_choice"],
        json!({"type": "auto"})
    );
}

/// Not upstream's: a call to a tool the identities name as custom becomes a
/// `custom_tool_call` with the tool's own name, namespace and raw input.
#[test]
fn identities_make_custom_tool_calls() {
    let identities = HashMap::from([(
        "functions__exec".to_owned(),
        ToolIdentity {
            name: "exec".to_owned(),
            namespace: "functions".to_owned(),
            custom: true,
            apply_patch: false,
        },
    )]);
    let step = json!({"type": "function_call", "id": "call_1", "name": "functions__exec", "arguments": {"input": "ls"}});
    assert_eq!(
        interactions_function_call_to_responses_with_identity(&step, Some(&identities)),
        json!({"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": "ls", "namespace": "functions"})
    );
    assert_eq!(
        interactions_function_call_to_responses_with_identity(&step, None),
        json!({"type": "function_call", "call_id": "call_1", "name": "functions__exec", "arguments": "{\"input\":\"ls\"}"})
    );
}

/// Not upstream's: media parts each way.
#[test]
fn media_parts() {
    let part = |value: Value| responses_content_part_to_interactions(&value);
    assert_eq!(
        part(json!({"type": "input_image", "image_url": "data:;base64,AAA"})),
        Some(json!({"type": "image", "mime_type": "application/octet-stream", "data": "AAA"}))
    );
    assert_eq!(
        part(json!({"type": "output_image", "data": "AAA", "mime_type": "image/gif"})),
        Some(json!({"type": "image", "data": "AAA", "mime_type": "image/gif"}))
    );
    assert_eq!(part(json!({"type": "refusal"})), None);

    let part = |value: Value| interactions_content_part_to_responses(&value, "user");
    assert_eq!(
        part(json!({"type": "audio"})),
        Some(
            json!({"type": "output_text", "text": "Audio content: inline data (Format: unknown)"})
        )
    );
    assert_eq!(
        part(json!({"type": "audio", "mime_type": "wav/"})),
        Some(json!({"type": "output_text", "text": "Audio content: inline data (Format: wav/)"}))
    );
    assert_eq!(
        part(json!({"type": "document", "data": "JVBE", "filename": "a.pdf"})),
        Some(
            json!({"type": "input_file", "file_data": "data:application/octet-stream;base64,JVBE", "filename": "a.pdf"})
        )
    );
    assert_eq!(
        part(json!({"type": "image", "url": "https://example.com/a.png"})),
        Some(json!({"type": "input_image", "image_url": "https://example.com/a.png"}))
    );
    assert_eq!(part(json!({"type": "thought"})), None);
}

// Not upstream's: firstNonEmpty keeps the value as it was.
#[test]
fn first_non_empty_skips_white_space() {
    assert_eq!(first_non_empty([" ", "	", " a ", "b"]), " a ");
    assert_eq!(first_non_empty([" ", ""]), "");
}

// Not upstream's: data URLs split as parseDataURL splits them.
#[test]
fn data_urls_split() {
    assert_eq!(
        parse_data_url("data:image/png;base64,AAA"),
        Some(("image/png", "AAA"))
    );
    assert_eq!(
        parse_data_url("data:;base64,AAA"),
        Some(("application/octet-stream", "AAA"))
    );
    assert_eq!(parse_data_url("data:image/png"), None);
    assert_eq!(parse_data_url("https://x"), None);
}

// Not upstream's: a function call's arguments and its output, sent as JSON
// text, keep each number as written, as upstream copies them (checked with
// Go).
#[test]
fn embedded_json_keeps_numbers_as_written() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let out = to_interactions(
        "m",
        json!({"model":"m","input":[
            {"type":"function_call","call_id":"c1","name":"f","arguments":spelled},
            {"type":"function_call_output","call_id":"c1","output":spelled}
        ]}),
    );
    assert_eq!(out["input"][0]["arguments"].to_string(), spelled, "{out}");
    assert_eq!(out["input"][1]["result"].to_string(), spelled, "{out}");
}
