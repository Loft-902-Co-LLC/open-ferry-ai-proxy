// Ported from CLIProxyAPI internal/translator/interactions/claude/interactions_claude_test.go
// (the request tests) and interactions_claude_compat_test.go (v8.0.15, MIT),
// and interactions_claude_user_turn_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. PreservesBusinessObjectsInToolResultArray checked
// the result's JSON text for `"exit_code": 1`, which is the client's own
// spacing copied through; we check the values instead. The tests after them
// are new; their expected output comes from upstream.

use serde_json::{Value, json};

use super::*;

/// [`super::convert_claude_request_to_interactions`], for requests it
/// doesn't refuse.
fn convert_claude_request_to_interactions(model: &str, request: &Value, stream: bool) -> Value {
    let (body, err) = super::convert_claude_request_to_interactions(model, request, stream);
    assert_eq!(err, None, "{body}");
    body
}

/// [`super::convert_claude_request_to_interactions_with_compat`], for
/// requests it doesn't refuse.
fn convert_claude_request_to_interactions_with_compat(
    model: &str,
    request: &Value,
    stream: bool,
) -> Value {
    let (body, err) =
        super::convert_claude_request_to_interactions_with_compat(model, request, stream);
    assert_eq!(err, None, "{body}");
    body
}

fn translate(model: &str, input: &str, stream: bool) -> Value {
    convert_claude_request_to_interactions(model, &serde_json::from_str(input).unwrap(), stream)
}

// TestConvertClaudeRequestToInteractionsMapsMessagesToolsAndStream
#[test]
fn maps_messages_tools_and_stream() {
    let out = translate(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","stream":true,"max_tokens":1024,"tools":[{"name":"get_weather","description":"Weather","input_schema":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}],"messages":[{"role":"user","content":[{"type":"text","text":"今天北京的天气怎么样？"}]}]}"#,
        true,
    );
    assert_eq!(out["model"], "gemini-3.1-flash-lite", "{out}");
    assert_eq!(out["stream"], true, "{out}");
    assert_eq!(out["generation_config"]["max_output_tokens"], 1024, "{out}");
    assert_eq!(out["input"][0]["type"], "user_input", "{out}");
    assert_eq!(
        out["input"][0]["content"][0]["text"], "今天北京的天气怎么样？",
        "{out}"
    );
    assert_eq!(
        out["tools"][0]["parameters"]["properties"]["location"]["type"], "string",
        "{out}"
    );
    assert_eq!(out["tools"][0]["type"], "function", "{out}");
}

// TestConvertClaudeRequestToInteractionsMapsToolUseAndResult
#[test]
fn maps_tool_use_and_result() {
    let out = translate(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"location":"北京"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"晴"}]}]}"#,
        false,
    );
    let input = &out["input"];
    assert_eq!(input[0]["type"], "function_call", "{out}");
    assert_eq!(input[0]["id"], "toolu_1", "{out}");
    assert!(input[0].get("call_id").is_none(), "{out}");
    assert_eq!(input[1]["type"], "function_result", "{out}");
    assert_eq!(input[1]["name"], "get_weather", "{out}");
    assert_eq!(input[1]["call_id"], "toolu_1", "{out}");
    assert!(input[1].get("id").is_none(), "{out}");
    assert_eq!(input[1]["result"], "晴", "{out}");
}

// TestConvertClaudeRequestToInteractionsInfersToolNamesForOutOfOrderResults
#[test]
fn infers_tool_names_for_out_of_order_results() {
    let out = translate(
        "gemini-3.1-flash-lite",
        r#"{
            "model": "gemini-3.1-flash-lite",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}},
                        {"type": "tool_use", "id": "toolu_2", "name": "weather", "input": {"city": "bj"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_2", "content": "sunny"},
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"}
                    ]
                }
            ]
        }"#,
        false,
    );
    // The results are put in the order of the calls.
    let input = &out["input"];
    assert_eq!(input[2]["call_id"], "toolu_1", "{out}");
    assert_eq!(input[2]["name"], "lookup", "{out}");
    assert_eq!(input[3]["call_id"], "toolu_2", "{out}");
    assert_eq!(input[3]["name"], "weather", "{out}");
}

// TestConvertClaudeRequestToInteractionsPropagatesIsError
#[test]
fn propagates_is_error() {
    let out = translate(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_err","content":"command failed","is_error":true}]}]}"#,
        false,
    );
    assert_eq!(out["input"][0]["is_error"], true, "{out}");
    assert_eq!(out["input"][0]["call_id"], "toolu_err", "{out}");
    assert!(out["input"][0].get("id").is_none(), "{out}");
}

// TestConvertClaudeRequestToInteractions_PreservesToolAdjacencyWithInterveningSystemMessage
#[test]
fn preserves_tool_adjacency_with_intervening_system_message() {
    let out = translate(
        "gemini-3.1-flash-lite",
        r#"{
            "model": "gemini-3.1-flash-lite",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Execute tools"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "call_1", "name": "tool_one", "input": {"a": 1}},
                        {"type": "tool_use", "id": "call_2", "name": "tool_two", "input": {"b": 2}}
                    ]
                },
                {"role": "system", "content": "Context reminder between tool_use and tool_result"},
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
        false,
    );
    let inputs = out["input"].as_array().unwrap();
    let types: Vec<&str> = inputs
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "user_input",
            "function_call",
            "function_call",
            "function_result",
            "function_result",
            "user_input",
            "user_input",
        ],
        "{out}"
    );
    assert_eq!(inputs[3]["call_id"], "call_1");
    assert_eq!(inputs[4]["call_id"], "call_2");
    assert_eq!(
        inputs[5]["content"][0]["text"],
        "<system-reminder>\nContext reminder between tool_use and tool_result\n</system-reminder>"
    );
    assert_eq!(inputs[6]["content"][0]["text"], "Now summarize");
}

// TestConvertClaudeRequestToInteractionsPreservesImagesInToolResult
#[test]
fn preserves_images_in_tool_result() {
    let out = translate(
        "devin/swe-2",
        r#"{
            "model": "devin/swe-2",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tool_image_1", "name": "screenshot", "input": {}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool_image_1",
                            "content": [
                                {"type": "text", "text": "Captured desktop"},
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#,
        false,
    );
    let result = out["input"][1]["result"].as_array().unwrap();
    assert!(
        result
            .iter()
            .any(|item| item["type"] == "text" && item["text"] == "Captured desktop"),
        "{out}"
    );
    assert!(
        result.iter().any(|item| item["type"] == "image"
            && item["mime_type"] == "image/png"
            && item["data"].as_str().is_some_and(|data| !data.is_empty())),
        "{out}"
    );
}

// TestConvertClaudeRequestToInteractionsPreservesBusinessObjectsInToolResultArray
#[test]
fn preserves_business_objects_in_tool_result_array() {
    let out = translate(
        "devin/swe-2",
        r#"{
            "model": "devin/swe-2",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool_call_1",
                            "content": [
                                {"text": "failed", "exit_code": 1, "retryable": true},
                                {"type": "text", "text": "failed with code", "exit_code": 2}
                            ]
                        }
                    ]
                }
            ]
        }"#,
        false,
    );
    let result = out["input"][0]["result"].as_array().unwrap();
    assert_eq!(result[0]["exit_code"], 1, "{out}");
    assert_eq!(result[0]["retryable"], true, "{out}");
    assert_eq!(result[1]["exit_code"], 2, "{out}");
}

// TestConvertClaudeRequestToInteractionsWithCompatPreservesEmptyThinking
#[test]
fn with_compat_preserves_empty_thinking() {
    let payload = json!({"messages": [{"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": ""}]}]});

    let without_compat = convert_claude_request_to_interactions("deepseek-v4", &payload, false);
    assert_eq!(without_compat["input"], json!([]), "{without_compat}");

    let with_compat =
        convert_claude_request_to_interactions_with_compat("deepseek-v4", &payload, false);
    assert_eq!(with_compat["input"][0]["type"], "thought", "{with_compat}");
}

// Not upstream's: the whole output for thinking, effort, tool choice and the
// generation settings, with a reminder held back until the tool result.
#[test]
fn settings_and_held_back_reminder() {
    let out = translate(
        " ",
        r#"{"model":"claude-x","max_tokens":10,"temperature":0.5,"top_p":1,"stop_sequences":["x"],
            "thinking":{"type":"enabled","budget_tokens":2048},"output_config":{"effort":" HIGH "},
            "tool_choice":{"type":"tool","name":" lookup "},"system":[{"type":"text","text":"a"},"b",{"text":""}],
            "messages":[
                {"role":"assistant","content":[{"type":"text","text":"t"},{"type":"tool_use","id":"c1","name":"lookup","input":{"q":1}}]},
                {"role":"system","content":"note"},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"r","cache_control":{}}]}]}
            ],
            "tools":[{"name":" lookup ","description":5,"input_schema":{"type":"object"}},{"name":" "}]}"#,
        false,
    );
    assert_eq!(
        out,
        json!({
            "model": "claude-x",
            "input": [
                {"type": "model_output", "content": [{"type": "text", "text": "t"}]},
                {"type": "function_call", "name": "lookup", "arguments": {"q": 1}, "id": "c1"},
                {
                    "type": "function_result",
                    "call_id": "c1",
                    "result": [{"type": "text", "text": "r"}],
                    "name": "lookup",
                },
                {
                    "type": "user_input",
                    "content": [{"type": "text", "text": "<system-reminder>\nnote\n</system-reminder>"}],
                },
            ],
            "system_instruction": "a\nb",
            "generation_config": {
                "max_output_tokens": 10,
                "temperature": 0.5,
                "top_p": 1,
                "stop_sequences": ["x"],
                "thinking_config": {"thinking_budget": 2048},
                "thinking_level": "high",
                "tool_choice": {"type": "function", "name": "lookup"},
            },
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}, "description": "5"}],
        })
    );
}

// Not upstream's: a tool call's input, read with each number as written,
// goes on with that text, as upstream copies it (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let request = format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"a","name":"f","input":{spelled}}}]}}]}}"#
    );
    let request = crate::json::exact::from_str(&request).unwrap();
    let out = convert_claude_request_to_interactions("m", &request, false);
    assert_eq!(out["input"][0]["arguments"].to_string(), spelled);
}

const USER_TURN_UPLOAD: &str = r#"{"type":"container_upload","file_id":"file-1"}"#;

/// `ConvertClaudeRequestToInteractionsWithCompat`, with its refusal.
fn convert_checked(input: &str) -> (Value, Option<UnsupportedPartError>) {
    let request = serde_json::from_str(input).expect("test request is valid JSON");
    super::convert_claude_request_to_interactions_with_compat("m", &request, false)
}

// TestConvertClaudeRequestToInteractions_RefusesAnyEmptiedUserTurn
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
        let (body, err) = convert_checked(&input);
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

// TestConvertClaudeRequestToInteractions_KeepsTurnWithTextBesideAttachment
#[test]
fn keeps_turn_with_text_beside_attachment() {
    let input = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{{"type":"text","text":"keep me"}},{USER_TURN_UPLOAD}]}}]}}"#
    );
    let (body, err) = convert_checked(&input);
    assert_eq!(err, None, "{body}");
    assert_eq!(body["input"][2]["content"][0]["text"], "keep me", "{body}");
}

// TestConvertClaudeRequestToInteractions_Base64DocumentAfterHistoryStaysMedia
#[test]
fn base64_document_after_history_stays_media() {
    let (body, err) = convert_checked(
        r#"{"model":"m","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0xLjQK"}}]}]}"#,
    );
    assert_eq!(err, None, "{body}");
    let part = &body["input"][2]["content"][0];
    assert_eq!(part["type"], "document", "{body}");
    assert_eq!(part["mime_type"], "application/pdf", "{body}");
    assert_eq!(part["data"], "JVBERi0xLjQK", "{body}");
}

// TestConvertClaudeRequestToInteractions_WhitespaceTextDoesNotHideAnUnrepresentableImage
#[test]
fn whitespace_text_does_not_hide_an_unrepresentable_image() {
    let file_image = r#"{"type":"image","source":{"type":"file","file_id":"f1"}}"#;
    let cases = [
        (
            "spaces before image",
            format!(r#"{{"type":"text","text":"  "}},{file_image}"#),
        ),
        (
            "mixed whitespace before image",
            format!(r#"{{"type":"text","text":" \n\t"}},{file_image}"#),
        ),
        (
            "spaces after image",
            format!(r#"{file_image},{{"type":"text","text":"  "}}"#),
        ),
    ];
    for (name, parts) in cases {
        let input =
            format!(r#"{{"model":"m","messages":[{{"role":"user","content":[{parts}]}}]}}"#);
        let (body, err) = convert_checked(&input);
        let err = err.unwrap_or_else(|| panic!("{name}: no refusal; body = {body}"));
        assert_eq!(err.part_type, "image", "{name}");
        assert_eq!(err.status_code(), 400, "{name}");
    }
}

// TestConvertClaudeRequestToInteractions_RealTextBesideWhitespaceAndAnImageStillSucceeds
#[test]
fn real_text_beside_whitespace_and_an_image_still_succeeds() {
    let (body, err) = convert_checked(
        r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"  "},{"type":"text","text":"keep me"},{"type":"image","source":{"type":"file","file_id":"f1"}}]}]}"#,
    );
    assert_eq!(err, None, "{body}");
    let parts = body["input"][0]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        parts.iter().any(|part| part["text"] == "keep me"),
        "text was lost: {body}"
    );
}

// TestClaudeToInteractionsRegistrationCarriesWhitespaceImageRefusal
#[test]
fn registration_carries_whitespace_image_refusal() {
    let input = json!({"model": "m", "messages": [{"role": "user", "content": [{"type": "text", "text": "  "}, {"type": "image", "source": {"type": "file", "file_id": "f1"}}]}]});
    let err = crate::registry::Registry::global()
        .translate_request_checked(&"claude".into(), &"interactions".into(), "m", input, false)
        .expect_err("the registration carries the refusal");
    assert_eq!(err.part_type, "image");
}

// Not upstream's: an assistant's unsendable media isn't refused, and a
// container upload with its bytes becomes a media part of its own type,
// in a message and in a tool result.
#[test]
fn only_user_media_is_refused_and_uploads_with_bytes_are_sent() {
    let (body, err) = convert_checked(&format!(
        r#"{{"messages":[{{"role":"user","content":"q"}},{{"role":"assistant","content":[{USER_TURN_UPLOAD}]}}]}}"#
    ));
    assert_eq!(err, None, "{body}");

    let upload = r#"{"type":"container_upload","source":{"media_type":"text/csv","data":"YQ=="}}"#;
    let (body, err) = convert_checked(&format!(
        r#"{{"messages":[{{"role":"user","content":[{upload}]}},{{"role":"assistant","content":[{{"type":"tool_use","id":"t","name":"f","input":{{}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":[{upload}]}}]}}]}}"#
    ));
    assert_eq!(err, None, "{body}");
    assert_eq!(
        body["input"][0]["content"][0],
        json!({"type": "container_upload", "mime_type": "text/csv", "data": "YQ=="}),
        "{body}"
    );
    assert_eq!(
        body["input"][2]["result"][0],
        json!({"type": "container_upload", "mime_type": "text/csv", "data": "YQ=="}),
        "{body}"
    );
}
