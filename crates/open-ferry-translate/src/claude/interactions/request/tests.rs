// Ported from CLIProxyAPI internal/translator/claude/interactions/interactions_claude_test.go
// (the request tests) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests after them are new; their expected output
// comes from upstream.

use serde_json::{Value, json};

use super::*;

fn translate(model: &str, input: &str, stream: bool) -> Value {
    convert_interactions_request_to_claude(model, &serde_json::from_str(input).unwrap(), stream)
}

// TestConvertInteractionsRequestToClaudeWithToolMessagesDirect
#[test]
fn with_tool_messages_direct() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","system_instruction":"be brief","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"function_call","name":"lookup","call_id":"toolu_1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"toolu_1","result":{"ok":true}}]}"#,
        false,
    );
    assert_eq!(out["system"], "be brief", "{out}");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hi", "{out}");
    assert_eq!(
        out["messages"][1]["content"][0]["type"], "tool_use",
        "{out}"
    );
    assert_eq!(
        out["messages"][2]["content"][0]["type"], "tool_result",
        "{out}"
    );
    assert_eq!(
        out["messages"][2]["content"][0]["tool_use_id"], "toolu_1",
        "{out}"
    );
}

// TestConvertInteractionsRequestToClaudePropagatesIsError
#[test]
fn propagates_is_error() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":[{"type":"function_result","name":"lookup","call_id":"toolu_err","result":"command failed","is_error":true}]}"#,
        false,
    );
    assert_eq!(
        out["messages"][0]["content"][0]["type"], "tool_result",
        "{out}"
    );
    assert_eq!(out["messages"][0]["content"][0]["is_error"], true, "{out}");
}

// TestConvertInteractionsRequestToClaudeGroupsConsecutiveRoleTurns
#[test]
fn groups_consecutive_role_turns() {
    let out = translate(
        "claude-test",
        r#"{
            "input":[
                {"type":"thought","content":[{"type":"thinking","thinking":"reason"}]},
                {"type":"model_output","content":[{"type":"text","text":"answer"}]},
                {"type":"function_call","name":"first","call_id":"call_1","arguments":{}},
                {"type":"function_call","name":"second","call_id":"call_2","arguments":{}},
                {"type":"function_result","call_id":"call_1","result":{"value":"one"}},
                {"type":"function_result","call_id":"call_2","result":{"value":"two"}}
            ]
        }"#,
        false,
    );
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{out}");
    let types: Vec<&str> = messages[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["thinking", "text", "tool_use", "tool_use"], "{out}");
    let ids: Vec<&str> = messages[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["tool_use_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["call_1", "call_2"], "{out}");
}

// TestConvertInteractionsRequestToClaudeDoesNotMergeAcrossRoleChanges
#[test]
fn does_not_merge_across_role_changes() {
    let out = translate(
        "claude-test",
        r#"{
            "input":[
                {"type":"model_output","content":"first assistant"},
                {"type":"user_input","content":"user reply"},
                {"type":"model_output","content":"second assistant"}
            ]
        }"#,
        false,
    );
    let roles: Vec<&str> = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["assistant", "user", "assistant"], "{out}");
}

// TestConvertInteractionsRequestToClaudeStringInputDirect
#[test]
fn string_input_direct() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":"hello"}"#,
        false,
    );
    assert_eq!(out["messages"][0]["role"], "user", "{out}");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hello", "{out}");
}

// TestConvertInteractionsRequestToClaudeMapsGenerationConfigToolsAndStreamDirect
#[test]
fn maps_generation_config_tools_and_stream_direct() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","stream":true,"input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}],"tools":[{"type":"function","name":"lookup","description":"Lookup data","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],"generation_config":{"max_output_tokens":99,"top_p":0.7,"stop_sequences":["END"],"tool_choice":{"type":"function","name":"lookup"},"thinking_level":"high"}}"#,
        false,
    );
    assert_eq!(out["stream"], true, "{out}");
    assert_eq!(out["max_tokens"], 99, "{out}");
    assert_eq!(
        out["tools"][0]["input_schema"]["properties"]["q"]["type"], "string",
        "{out}"
    );
    assert_eq!(out["tool_choice"]["name"], "lookup", "{out}");
    assert!(
        out["thinking"]["type"]
            .as_str()
            .is_some_and(|kind| !kind.is_empty()),
        "{out}"
    );
}

// TestConvertInteractionsRequestToClaudeAcceptsImageContent
#[test]
fn accepts_image_content() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
        false,
    );
    let block = &out["messages"][0]["content"][0];
    assert_eq!(block["type"], "image", "{out}");
    assert_eq!(block["source"]["media_type"], "image/png", "{out}");
    assert_eq!(block["source"]["data"], "aGVsbG8=", "{out}");
}

// TestConvertInteractionsRequestToClaudePreservesNonImageMediaContent
#[test]
fn preserves_non_image_media_content() {
    let out = translate(
        "claude-test",
        r#"{"model":"claude-test","input":[{"type":"thought","content":[{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="},{"type":"document","mime_type":"application/pdf","data":"JVBERi0="}]}]}"#,
        false,
    );
    let message = &out["messages"][0];
    assert_eq!(message["role"], "assistant", "{out}");
    let content = message["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text", "{out}");
    assert_eq!(content[1]["type"], "text", "{out}");
    assert_eq!(content[2]["type"], "document", "{out}");
    assert!(
        !content.iter().any(|block| block["type"] == "image"),
        "{out}"
    );
}

// Not upstream's: the whole output for camel-case settings overridden by
// `reasoning` and a top-level tool choice, Gemini-style turns, a call whose
// ID is made from its name, and declarations nested in a tool.
#[test]
fn settings_turns_and_declarations() {
    let out = translate(
        "claude-x",
        r#"{"systemInstruction":{"parts":[{"text":"a"},{"text":""},"b"]},
            "generationConfig":{"maxOutputTokens":5,"topP":0.5,"stopSequences":["s"],"thinkingLevel":" NONE ","toolChoice":"required"},
            "reasoning":{"effort":"turbo"},
            "tool_choice":{"type":"tool","function":{"name":"my.tool"}},
            "input":[
                {"role":"model","steps":[{"type":"thought","content":[{"type":"thinking","thinking":"hm"}]},{"role":"user","content":"x"}]},
                {"role":"model","parts":[{"text":"p"}]},
                {"type":"function_call","name":"my.tool","arguments":[1]},
                {"type":"function_result","name":"my.tool","output":{"ok":true}},
                {"type":"user_input","content":[{"type":"image","source":{"media_type":"image/gif","data":"R0"}},{"type":"thinking","thinking":"dropped"},{"type":"audio"}]}
            ],
            "tools":[
                {"functionDeclarations":[{"name":"my.tool","parameters_json_schema":{"type":"object","properties":{"q":{"type":"string"}}}},{"description":"nameless"}]},
                {"function":{"name":"f2","description":"d"}}
            ]}"#,
        false,
    );
    assert_eq!(
        out,
        json!({
            "model": "claude-x",
            "max_tokens": 5,
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm"}]},
                {"role": "user", "content": [{"type": "text", "text": "x"}, {"type": "text", "text": "p"}]},
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_my_tool", "name": "my_tool", "input": {}}],
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_my_tool", "content": "{\"ok\":true}"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": "R0"}},
                    ],
                },
            ],
            "system": "a\nb",
            "top_p": 0.5,
            "stop_sequences": ["s"],
            "thinking": {"type": "adaptive"},
            "tool_choice": {"type": "tool", "name": "my_tool"},
            "output_config": {"effort": "turbo"},
            "tools": [
                {
                    "name": "my_tool",
                    "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
                },
                {"name": "f2", "input_schema": {"type": "object", "properties": {}}, "description": "d"},
            ],
        })
    );
}

// Not upstream's: a call's arguments and a result, read with each number as
// written, go on with that text, as upstream copies them (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let request = format!(
        r#"{{"input":[{{"type":"function_call","id":"a","name":"f","arguments":{spelled}}},{{"type":"function_result","call_id":"a","result":{spelled}}}]}}"#
    );
    let request = crate::json::exact::from_str(&request).unwrap();
    let out = convert_interactions_request_to_claude("m", &request, false);
    assert_eq!(
        out["messages"][0]["content"][0]["input"].to_string(),
        spelled
    );
    assert_eq!(out["messages"][1]["content"][0]["content"], spelled);
}
