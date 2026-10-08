// Ported from CLIProxyAPI
// internal/translator/gemini/openai/chat-completions/gemini_openai_request_test.go,
// gemini_openai_file_data_test.go, gemini_openai_signature_test.go and the
// request test in noop_optimization_test.go (v8.0.20, MIT), and
// gemini_openai_file_id_test.go, gemini_openai_media_url_test.go and
// gemini_openai_user_turn_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported; table-driven Go subtests become one table per test.
// The tests after them are new; their expected output comes from upstream.

use serde_json::{Value, json};

use super::*;
use crate::signature::{
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, validate_gemini_function_call_pairing,
};

const CAPTURED_GEMINI_TOOL_CALL_THOUGHT_SIGNATURE: &str =
    "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

/// `ConvertOpenAIRequestToGemini`'s body, which must come without a refusal.
fn convert_openai_request_to_gemini(model: &str, request: &Value, stream: bool) -> Value {
    let (body, err) = super::convert_openai_request_to_gemini(model, request, stream);
    assert_eq!(err, None, "refused: {body}");
    body
}

fn translate(model: &str, input: &str) -> Value {
    convert_openai_request_to_gemini(model, &serde_json::from_str(input).unwrap(), false)
}

/// Translates `input` for model `m` and returns the result without its
/// safety settings, as compact JSON, checking the defaults were added.
fn convert(input: &str) -> String {
    let mut output = translate("m", input);
    let settings = output
        .as_object_mut()
        .and_then(|fields| fields.shift_remove("safetySettings"));
    assert_eq!(
        settings.as_ref().and_then(Value::as_array).map(Vec::len),
        Some(5),
        "{input}"
    );
    output.to_string()
}

fn function_calling_config(output: &Value) -> &Value {
    &output["toolConfig"]["functionCallingConfig"]
}

#[test]
fn strips_trailing_assistant_prefill() {
    let output = translate(
        "gemini-3.1-pro-high",
        r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"previous answer"}]}"#,
    );
    let contents = output["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1, "{output}");
    assert_eq!(contents[0]["role"], "user");
}

#[test]
fn preserves_input_audio() {
    let output = translate(
        "gemini-3.1-pro-high",
        r#"{"model":"gpt-5.5","messages":[{"role":"user","content":[
            {"type":"text","text":"Transcribe this audio verbatim."},
            {"type":"input_audio","input_audio":{"data":"SUQzBA==","format":"mp3"}}]}]}"#,
    );
    assert_eq!(
        output["contents"][0]["parts"],
        json!([
            {"text": "Transcribe this audio verbatim."},
            {"inlineData": {"mime_type": "audio/mpeg", "data": "SUQzBA=="}}
        ])
    );
}

#[test]
fn preserves_video_url() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[{"role":"user","content":[
            {"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAAIGZ0eXBtcDQy"}},
            {"type":"text","text":"Describe the video"}]}]}"#,
    );
    assert_eq!(
        output["contents"][0]["parts"],
        json!([
            {"inlineData": {"mime_type": "video/mp4", "data": "AAAAIGZ0eXBtcDQy"}},
            {"text": "Describe the video"}
        ])
    );
}

#[test]
fn skips_empty_text_parts_without_nulls() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
            {"role":"user","content":[{"type":"text","text":""},{"type":"input_audio","input_audio":{"data":"SUQzBA==","format":"mp3"}}]},
            {"role":"assistant","content":[{"type":"text","text":""}],"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"{\"output\":\"ok\"}"},
            {"role":"user","content":"done"}]}"#,
    );
    let user_parts = output["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(user_parts.len(), 1, "{output}");
    assert_eq!(user_parts[0]["inlineData"]["mime_type"], "audio/mpeg");
    let assistant_parts = output["contents"][1]["parts"].as_array().unwrap();
    assert_eq!(assistant_parts.len(), 1, "{output}");
    assert!(assistant_parts[0].get("functionCall").is_some(), "{output}");
}

#[test]
fn preserves_reasoning_content() {
    let output = convert_openai_request_to_gemini(
        "gemini-3-flash",
        &serde_json::from_str(
            r#"{"model":"gemini-3-flash","messages":[{"role":"user","content":"hi"},
            {"role":"assistant","content":"","reasoning_content":"thinking only"},
            {"role":"user","content":"say ok"}]}"#,
        )
        .unwrap(),
        true,
    );
    let contents = output["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3, "{output}");
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0],
        json!({"text": "thinking only", "thought": true})
    );
}

#[test]
fn preserves_reasoning_before_visible_content_and_tool_call() {
    let output = convert_openai_request_to_gemini(
        "gemini-3-flash",
        &serde_json::from_str(
            r#"{"model":"gemini-3-flash","messages":[{"role":"user","content":"hi"},
            {"role":"assistant","content":"visible answer","reasoning_content":"thinking only","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"{\"output\":\"ok\"}"},
            {"role":"user","content":"say ok"}]}"#,
        )
        .unwrap(),
        true,
    );
    let contents = output["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 4, "{output}");
    assert_eq!(
        contents[1]["parts"],
        json!([
            {"text": "thinking only", "thought": true},
            {"text": "visible answer"},
            {
                "functionCall": {"name": "read_file", "args": {}},
                "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
            }
        ])
    );
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
}

#[test]
fn skips_empty_assistant_messages() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[{"role":"user","content":"hi"},
        {"role":"assistant","content":"","tool_calls":[{"type":"function","function":{"name":"","arguments":"{}"}},{"type":"custom"}]},
        {"role":"user","content":"say ok"}]}"#,
    );
    assert_eq!(output["contents"].as_array().unwrap().len(), 2, "{output}");
}

#[test]
fn mid_session_developer_message_does_not_mutate_system_instruction() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"system","content":"You are a helpful assistant"},
        {"role":"user","content":"Turn 1 user"},
        {"role":"assistant","content":"Turn 1 assistant"},
        {"role":"developer","content":"<image_resize_notice>Image 1 was resized to 800x600</image_resize_notice>"},
        {"role":"user","content":"Turn 2 user"}]}"#,
    );
    assert_eq!(
        output["systemInstruction"]["parts"],
        json!([{"text": "You are a helpful assistant"}])
    );
    assert_eq!(
        output["contents"],
        json!([
            {"role": "user", "parts": [{"text": "Turn 1 user"}]},
            {"role": "model", "parts": [{"text": "Turn 1 assistant"}]},
            {"role": "user", "parts": [{"text": "<system-reminder>\n<image_resize_notice>Image 1 was resized to 800x600</image_resize_notice>\n</system-reminder>"}]},
            {"role": "user", "parts": [{"text": "Turn 2 user"}]}
        ])
    );
}

#[test]
fn mid_session_system_reminder_envelope() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"system","content":"You are a helpful assistant"},
        {"role":"user","content":"Hello"},
        {"role":"assistant","content":"Hi there"},
        {"role":"system","content":"Please decide which tool to call next."},
        {"role":"user","content":"Search for news"}]}"#,
    );
    assert_eq!(output["contents"].as_array().unwrap().len(), 4, "{output}");
    assert_eq!(
        output["contents"][2]["parts"][0]["text"],
        "<system-reminder>\nPlease decide which tool to call next.\n</system-reminder>"
    );
}

#[test]
fn mid_session_transient_system_instruction_preserves_turn_boundaries() {
    let with = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"system","content":"System prompt"},
        {"role":"user","content":"Turn 1 user"},
        {"role":"assistant","content":"Turn 1 assistant"},
        {"role":"system","content":"Call tool now"},
        {"role":"user","content":"Turn 2 user"}]}"#,
    );
    let without = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"system","content":"System prompt"},
        {"role":"user","content":"Turn 1 user"},
        {"role":"assistant","content":"Turn 1 assistant"},
        {"role":"user","content":"Turn 2 user"},
        {"role":"assistant","content":"Turn 2 assistant"}]}"#,
    );
    let with = with["contents"].as_array().unwrap();
    let without = without["contents"].as_array().unwrap();
    assert_eq!(with.len(), 4);
    assert_eq!(
        with[2],
        json!({"role": "user", "parts": [{"text": "<system-reminder>\nCall tool now\n</system-reminder>"}]})
    );
    assert_eq!(
        with[3],
        json!({"role": "user", "parts": [{"text": "Turn 2 user"}]})
    );
    assert_eq!(with[0].to_string(), without[0].to_string());
    assert_eq!(with[1].to_string(), without[1].to_string());
    assert_eq!(with[3]["parts"][0]["text"], without[2]["parts"][0]["text"]);
}

#[test]
fn mid_session_system_reminder_object_and_array_content() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"user","content":"Hello"},
        {"role":"assistant","content":"Hi"},
        {"role":"system","content":{"type":"text","text":"Object instruction"}},
        {"role":"developer","content":[{"type":"text","text":"Array instruction"}]}]}"#,
    );
    assert_eq!(output["contents"].as_array().unwrap().len(), 4, "{output}");
    assert_eq!(
        output["contents"][2]["parts"][0]["text"],
        "<system-reminder>\nObject instruction\n</system-reminder>"
    );
    assert_eq!(
        output["contents"][3]["parts"][0]["text"],
        "<system-reminder>\nArray instruction\n</system-reminder>"
    );
}

#[test]
fn maps_max_tokens() {
    for (name, body, want) in [
        (
            "max_tokens",
            r#"{"model":"gemini-2.0-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":30}"#,
            30,
        ),
        (
            "max_completion_tokens",
            r#"{"model":"gemini-2.0-flash","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":40}"#,
            40,
        ),
        (
            "max_tokens preferred over max_completion_tokens",
            r#"{"model":"gemini-2.0-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":30,"max_completion_tokens":40}"#,
            30,
        ),
    ] {
        let output = translate("gemini-2.0-flash", body);
        assert_eq!(
            output["generationConfig"]["maxOutputTokens"],
            json!(want),
            "{name}"
        );
    }
}

#[test]
fn cleans_tool_schema_required_fields() {
    let output = translate(
        "gemini-2.0-flash",
        r#"{"model":"gemini-2.0-flash","messages":[{"role":"user","content":"hi"}],
        "tools":[{"type":"function","function":{"name":"search_company","description":"Search",
        "parameters":{"type":"object","title":"SearchCompany","properties":{"country":{"type":"string"},"industry":{"type":"string"}},
        "required":["country","industry","stale_field","another_stale"]}}}]}"#,
    );
    let schema = &output["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
    assert!(schema.is_object(), "{output}");
    assert!(schema.get("title").is_none(), "{output}");
    assert_eq!(schema["required"], json!(["country", "industry"]));
}

#[test]
fn response_format_json_schema() {
    let output = translate(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","generationConfig":{"temperature":0.2,"responseSchema":{"type":"string"}},
        "messages":[{"role":"user","content":"Return structured JSON."}],
        "response_format":{"type":"json_schema","json_schema":{"name":"response","strict":true,
        "schema":{"type":"object","properties":{"cleanedContent":{"type":"string"}},"required":["cleanedContent"],"additionalProperties":false}}}}"#,
    );
    let config = &output["generationConfig"];
    assert_eq!(config["responseMimeType"], "application/json");
    assert!(config.get("responseSchema").is_none(), "{output}");
    assert_eq!(
        config["responseJsonSchema"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(config["temperature"].as_f64(), Some(0.2));
}

#[test]
fn response_format_json_object() {
    let output = translate(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","generationConfig":{"temperature":0.6},
        "messages":[{"role":"user","content":"Return a JSON object."}],"response_format":{"type":"json_object"}}"#,
    );
    let config = &output["generationConfig"];
    assert_eq!(config["responseMimeType"], "application/json");
    assert!(config.get("responseJsonSchema").is_none(), "{output}");
    assert_eq!(config["temperature"].as_f64(), Some(0.6));
}

#[test]
fn response_format_json_schema_without_schema() {
    let output = translate(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"Return structured JSON."}],
        "response_format":{"type":"json_schema","json_schema":{"name":"response"}}}"#,
    );
    let config = &output["generationConfig"];
    assert_eq!(config["responseMimeType"], "application/json");
    assert!(config.get("responseJsonSchema").is_none(), "{output}");
}

#[test]
fn response_format_no_op() {
    for (name, body) in [
        (
            "absent",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"plain text"}],"temperature":0.5}"#,
        ),
        (
            "unknown type",
            r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"plain text"}],"temperature":0.5,"response_format":{"type":"text"}}"#,
        ),
    ] {
        let output = translate("gemini-3.1-flash-lite", body);
        let config = &output["generationConfig"];
        assert!(config.get("responseMimeType").is_none(), "{name}");
        assert!(config.get("responseJsonSchema").is_none(), "{name}");
        assert_eq!(config["temperature"].as_f64(), Some(0.5), "{name}");
    }
}

#[test]
fn multi_turn_repeated_tool_call_id_issue_5933() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"user","content":"list files"},
        {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"glob","arguments":"{\"pattern\":\"*.go\"}"}}]},
        {"role":"tool","tool_call_id":"call_1","content":"[\"main.go\"]"},
        {"role":"user","content":"read main.go"},
        {"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"main.go\"}"}}]},
        {"role":"tool","tool_call_id":"call_1","content":"package main"}]}"#,
    );
    let contents = &output["contents"];
    assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "glob");
    let response = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!(response["name"], "glob");
    assert_eq!(response["response"]["result"], r#""[\"main.go\"]""#);
    assert_eq!(contents[4]["parts"][0]["functionCall"]["name"], "read");
    let response = &contents[5]["parts"][0]["functionResponse"];
    assert_eq!(response["name"], "read");
    assert_eq!(response["response"]["result"], r#""package main""#);
    validate_gemini_function_call_pairing(&output).unwrap();
}

#[test]
fn parallel_and_out_of_order_tool_responses() {
    let output = translate(
        "gemini-3-flash",
        r#"{"model":"gemini-3-flash","messages":[
        {"role":"user","content":"run parallel tools"},
        {"role":"assistant","tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"tool_a","arguments":"{}"}},
            {"id":"call_2","type":"function","function":{"name":"tool_b","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call_2","content":"res_b"},
        {"role":"tool","tool_call_id":"call_1","content":"res_a"}]}"#,
    );
    assert_eq!(
        output["contents"][2]["parts"],
        json!([
            {"functionResponse": {"name": "tool_a", "response": {"result": "\"res_a\""}}},
            {"functionResponse": {"name": "tool_b", "response": {"result": "\"res_b\""}}}
        ])
    );
    validate_gemini_function_call_pairing(&output).unwrap();
}

/// The declarations shared by most tool choice cases.
const TOOL_A: &str = r#"{"type":"function","function":{"name":"tool_a","parameters":{"type":"object","properties":{}}}}"#;
const TOOL_B: &str = r#"{"type":"function","function":{"name":"tool_b","parameters":{"type":"object","properties":{}}}}"#;
const TOOL_1: &str = r#"{"type":"function","function":{"name":"1tool","parameters":{"type":"object","properties":{}}}}"#;
const TOOL_UNDERSCORE_1: &str = r#"{"type":"function","function":{"name":"_1tool","parameters":{"type":"object","properties":{}}}}"#;

#[test]
fn tool_choice() {
    struct Case {
        name: &'static str,
        /// The `tool_choice` and `parallel_tool_calls` fields, if any.
        extra: &'static str,
        tools: &'static [&'static str],
        /// The expected mode, or `None` for no `toolConfig` at all.
        mode: Option<&'static str>,
        allowed: Option<&'static [&'static str]>,
        declarations: Option<&'static [&'static str]>,
    }
    let cases = [
        Case {
            name: "named function maps to toolConfig mode ANY and allowedFunctionNames",
            extra: r#""tool_choice":{"type":"function","function":{"name":"tool_a"}},"#,
            tools: &[TOOL_A],
            mode: Some("ANY"),
            allowed: Some(&["tool_a"]),
            declarations: None,
        },
        Case {
            name: "none maps to mode NONE",
            extra: r#""tool_choice":"none","#,
            tools: &[TOOL_A],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "auto maps to mode AUTO",
            extra: r#""tool_choice":"auto","#,
            tools: &[TOOL_A],
            mode: Some("AUTO"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "required maps to mode ANY",
            extra: r#""tool_choice":"required","#,
            tools: &[TOOL_A],
            mode: Some("ANY"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "parallel_tool_calls false fails closed to NONE",
            extra: r#""tool_choice":"auto","parallel_tool_calls":false,"#,
            tools: &[TOOL_A],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "required tool_choice with parallel_tool_calls false fails closed to NONE",
            extra: r#""tool_choice":"required","parallel_tool_calls":false,"#,
            tools: &[TOOL_A],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "parallel_tool_calls null does not fail closed to NONE",
            extra: r#""tool_choice":"auto","parallel_tool_calls":null,"#,
            tools: &[TOOL_A],
            mode: Some("AUTO"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "parallel_tool_calls true does not fail closed to NONE",
            extra: r#""tool_choice":"auto","parallel_tool_calls":true,"#,
            tools: &[TOOL_A],
            mode: Some("AUTO"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "allowed_tools filters function declarations and sets AUTO mode without allowedFunctionNames",
            extra: r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"tool_b"}}]}},"#,
            tools: &[TOOL_A, TOOL_B],
            mode: Some("AUTO"),
            allowed: None,
            declarations: Some(&["tool_b"]),
        },
        Case {
            name: "allowed_tools with required mode sets mode ANY and allowedFunctionNames",
            extra: r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"tool_b"}}]}},"#,
            tools: &[TOOL_A, TOOL_B],
            mode: Some("ANY"),
            allowed: Some(&["tool_b"]),
            declarations: None,
        },
        Case {
            name: "empty allowed_tools fails closed to NONE",
            extra: r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"tools":[]}},"#,
            tools: &[TOOL_A],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "function choice with missing name fails closed to NONE",
            extra: r#""tool_choice":{"type":"function","function":{}},"#,
            tools: &[TOOL_A],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "allowed_tools matches exact original name and does not conflate sanitization",
            extra: r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"tools":[{"type":"function","function":{"name":"1tool"}}]}},"#,
            tools: &[TOOL_UNDERSCORE_1],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "sanitized name collision fails closed to NONE",
            extra: r#""tool_choice":"auto","#,
            tools: &[TOOL_1, TOOL_UNDERSCORE_1],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "undeclared function choice fails closed to NONE",
            extra: r#""tool_choice":{"type":"function","function":{"name":"undeclared_tool"}},"#,
            tools: &[TOOL_A],
            mode: Some("NONE"),
            allowed: None,
            declarations: None,
        },
        Case {
            name: "allowed_tools filtering avoids false collision when excluded tool collides",
            extra: r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"_1tool"}}]}},"#,
            tools: &[TOOL_1, TOOL_UNDERSCORE_1],
            mode: Some("AUTO"),
            allowed: None,
            declarations: Some(&["_1tool"]),
        },
        Case {
            name: "tool_choice null does not create toolConfig or fail closed",
            extra: r#""tool_choice":null,"#,
            tools: &[TOOL_A],
            mode: None,
            allowed: None,
            declarations: None,
        },
    ];
    for case in cases {
        let input = format!(
            r#"{{"model":"gemini-3.1-pro-high","messages":[{{"role":"user","content":"test"}}],{}"tools":[{}]}}"#,
            case.extra,
            case.tools.join(",")
        );
        let output = translate("gemini-3.1-pro-high", &input);
        let config = function_calling_config(&output);
        match case.mode {
            Some(mode) => assert_eq!(config["mode"], mode, "{}: {output}", case.name),
            None => assert!(
                output.get("toolConfig").is_none(),
                "{}: {output}",
                case.name
            ),
        }
        match case.allowed {
            Some(names) => assert_eq!(
                config["allowedFunctionNames"],
                json!(names),
                "{}",
                case.name
            ),
            None => assert!(
                config.get("allowedFunctionNames").is_none(),
                "{}: {output}",
                case.name
            ),
        }
        if let Some(names) = case.declarations {
            let declared: Vec<&Value> = output["tools"][0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|declaration| &declaration["name"])
                .collect();
            assert_eq!(json!(declared), json!(names), "{}", case.name);
        }
    }
}

#[test]
fn tool_strict_maps_to_validated_mode() {
    for (name, tool_choice, mode, allowed) in [
        (
            "absent tool_choice maps to VALIDATED",
            "",
            "VALIDATED",
            None,
        ),
        (
            "explicit auto tool_choice maps to VALIDATED",
            r#""tool_choice":"auto","#,
            "VALIDATED",
            None,
        ),
        (
            "null tool_choice maps to VALIDATED",
            r#""tool_choice":null,"#,
            "VALIDATED",
            None,
        ),
        (
            "required tool_choice maps to ANY",
            r#""tool_choice":"required","#,
            "ANY",
            None,
        ),
        (
            "none tool_choice maps to NONE",
            r#""tool_choice":"none","#,
            "NONE",
            None,
        ),
        (
            "specific function tool_choice maps to ANY with allowedFunctionNames",
            r#""tool_choice":{"type":"function","function":{"name":"tool_a"}},"#,
            "ANY",
            Some(json!(["tool_a"])),
        ),
        (
            "allowed_tools with auto mode and strict tool maps to VALIDATED",
            r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"tool_a"}}]}},"#,
            "VALIDATED",
            None,
        ),
        (
            "allowed_tools with required mode and strict tool maps to ANY",
            r#""tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"tool_a"}}]}},"#,
            "ANY",
            Some(json!(["tool_a"])),
        ),
    ] {
        let input = format!(
            r#"{{"model":"gemini-3.1-pro-high","messages":[{{"role":"user","content":"hi"}}],{tool_choice}
            "tools":[{{"type":"function","function":{{"name":"tool_a","description":"Controlled tool.","strict":true,
            "parameters":{{"type":"object","properties":{{}}}}}}}}]}}"#
        );
        let output = translate("gemini-3.1-pro-high", &input);
        assert!(
            output["tools"][0]["functionDeclarations"][0]
                .get("strict")
                .is_none(),
            "{name}: {output}"
        );
        let config = function_calling_config(&output);
        assert_eq!(config["mode"], mode, "{name}: {output}");
        if let Some(allowed) = allowed {
            assert_eq!(config["allowedFunctionNames"], allowed, "{name}");
        }
    }
}

#[test]
fn mixed_tools_where_one_is_strict_map_to_validated() {
    let output = translate(
        "gemini-3.1-pro-high",
        r#"{"model":"gemini-3.1-pro-high","messages":[{"role":"user","content":"hi"}],"tools":[
        {"type":"function","function":{"name":"tool_a","description":"Loose tool.","strict":false,"parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{"name":"tool_b","description":"Strict tool.","strict":true,"parameters":{"type":"object","properties":{}}}}]}"#,
    );
    assert_eq!(function_calling_config(&output)["mode"], "VALIDATED");
}

#[test]
fn non_strict_tools_omit_tool_config() {
    let output = translate(
        "gemini-3.1-pro-high",
        r#"{"model":"gemini-3.1-pro-high","messages":[{"role":"user","content":"hi"}],"tools":[
        {"type":"function","function":{"name":"tool_a","description":"Loose tool.","strict":false,"parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{"name":"tool_b","description":"Unspecified tool.","parameters":{"type":"object","properties":{}}}}]}"#,
    );
    assert!(output.get("toolConfig").is_none(), "{output}");
}

#[test]
fn parameters_json_schema_preserves_additional_properties_and_pattern_issue_5959() {
    let output = translate(
        "gemini-2.5-flash",
        r#"{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"Use the submit tool."}],
        "tools":[{"type":"function","function":{"name":"submit","description":"Submit a bounded schema test value.",
        "parameters":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,
        "properties":{"recipient":{"type":"string","pattern":"^(alice|bob)$"},"amount":{"type":"number"}},
        "required":["recipient","amount"]}}}]}"#,
    );
    let schema = &output["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
    assert!(schema.is_object(), "{output}");
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"]["recipient"]["pattern"],
        "^(alice|bob)$"
    );
    assert_ne!(schema["description"], "No extra properties allowed");
    assert!(
        !str_of(schema["properties"]["recipient"].get("description")).contains("pattern:"),
        "{schema}"
    );
}

#[test]
fn normalizes_file_data_url() {
    let output = translate(
        "gemini-2.5-pro",
        r#"{"model":"gemini-2.5-pro","messages":[{"role":"user","content":[{"type":"file","file":{"filename":"test.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}]}]}"#,
    );
    assert_eq!(
        output["contents"][0]["parts"][0]["inlineData"],
        json!({"mime_type": "application/pdf", "data": "JVBERi0xLjQK"})
    );
}

#[test]
fn tool_call_signature_compatibility() {
    let gemini = format!("gemini#{CAPTURED_GEMINI_TOOL_CALL_THOUGHT_SIGNATURE}");
    for (name, raw, want) in [
        (
            "Gemini signature is preserved",
            gemini.as_str(),
            CAPTURED_GEMINI_TOOL_CALL_THOUGHT_SIGNATURE,
        ),
        (
            "unknown signature uses bypass",
            "not-a-provider-signature",
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        ),
    ] {
        let input = json!({
            "model": "gemini-3.5-flash",
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": raw}}
                }]
            }]
        });
        let output = convert_openai_request_to_gemini("gemini-3.5-flash", &input, false);
        assert_eq!(
            output["contents"][0]["parts"][0]["thoughtSignature"], want,
            "{name}: {output}"
        );
    }
}

#[test]
fn normalizes_tool_name_and_strict() {
    let output = translate(
        "gemini-test",
        r#"{"messages":[],"tools":[{"type":"function","function":{"name":true,"strict":true,"parameters":{"type":"object"}}}]}"#,
    );
    let declaration = &output["tools"][0]["functionDeclarations"][0];
    assert_eq!(declaration["name"], "true");
    assert!(declaration.get("strict").is_none(), "{output}");
}

#[test]
fn unparsable_arguments_leave_args_out() {
    // Upstream copies the text as raw JSON and so sends an invalid request.
    let output = translate(
        "m",
        r#"{"messages":[{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{oops"}}]}]}"#,
    );
    assert_eq!(
        output["contents"][0]["parts"][0]["functionCall"],
        json!({"name": "f"})
    );
}

#[test]
fn messages_and_system_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"messages":[{"role":"system","content":"S1"},{"role":"developer","content":{"type":"text","#,
            r#""text":"S2"}},{"role":"system","content":[{"type":"text","text":"S3"},{"type":"image_url"},"#,
            r#""x"]},{"role":"system","content":5},{"role":"user","content":[{"type":"text","text":"u"},"#,
            r#"{"type":"text","text":""},{"type":"image_url","image_url":{"url":"data:image/png;base64,"#,
            r#"QUJD"}},{"type":"input_audio","input_audio":{"data":"AA","format":"pcm16"}},{"type":"file","#,
            r#""file":{"filename":"x.txt","file_data":"data:text/plain;base64,QQ"}},{"type":"unknown"}]},"#,
            r#"{"role":"developer","content":"  "},{"role":"system","content":[{"type":"text","text":"mid"},"#,
            r#"{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j"}}]},{"role":"tool","#,
            r#""tool_call_id":"t","content":"orphan"},{"role":"assistant","content":[{"type":"text","#,
            r#""text":"a"},{"type":"image_url","image_url":{"url":"data:image/gif;base64,R0"}},"#,
            r#"{"type":"input_audio","input_audio":{"data":"AA"}}],"reasoning_content":"r"},{"role":"assistant","#,
            r#""content":"","reasoning_content":5},{"role":"model","content":"m"},{"role":"user","#,
            r#""content":""},{"role":"assistant","content":"tail"}]}"#
        )),
        concat!(
            r#"{"contents":[{"role":"user","parts":[{"text":"u"},{"inlineData":{"mime_type":"image/png","#,
            r#""data":"QUJD"}},{"inlineData":{"mime_type":"audio/pcm","data":"AA"}},{"inlineData":{"mime_type":"text/plain","#,
            r#""data":"QQ"}}]},{"role":"user","parts":[{"text":"  "}]},{"role":"user","parts":[{"text":"<system-reminder>\nmid\n</system-reminder>"},"#,
            r#"{"inlineData":{"mime_type":"image/jpeg","data":"/9j"}}]},{"role":"model","parts":[{"text":"r","#,
            r#""thought":true},{"text":"a"},{"inlineData":{"mime_type":"image/gif","data":"R0"}}]},"#,
            r#"{"role":"user","parts":[{"text":""}]}],"model":"m","systemInstruction":{"role":"user","#,
            r#""parts":[{"text":"S1"},{"text":"S2"},{"text":"S3"},{"text":""},{"text":""}]}}"#
        )
    );
}

#[test]
fn tool_calls_and_results_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"c","tool_calls":[{"id":"c1","#,
            r#""type":"function","function":{"name":"a b","arguments":"{\"x\":1}"},"extra_content":{"google":{"thought_signature":"gemini#EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"}}},"#,
            r#"{"id":"c2","type":"function","function":{"name":"9g","arguments":"[1]","extra_content":{"google":{"thought_signature":"foreign"}}}},"#,
            r#"{"id":"c3","type":"function","function":{"name":"h","arguments":{"y":2}},"thoughtSignature":null},"#,
            r#"{"id":"c4","type":"custom","function":{"name":"skip"}},{"id":"c5","type":"function","#,
            r#""function":{"name":""}},{"type":"function","function":{"name":"noid","arguments":"5"},"#,
            r#""thought_signature":"x"}]},{"role":"tool","tool_call_id":"c1","content":"first"},"#,
            r#"{"role":"user","content":"between"},{"role":"tool","tool_call_id":"c1","content":{"k":[1,"#,
            r#"2]}},{"role":"tool","tool_call_id":"c2"},{"role":"tool","tool_call_id":"","content":"empty id"},"#,
            r#"{"role":"assistant","tool_calls":[]},{"role":"tool","tool_call_id":"c3","content":"after next assistant"},"#,
            r#"{"role":"assistant","tool_calls":"x","content":"plain"},{"role":"user","content":"end"}]}"#
        )),
        concat!(
            r#"{"contents":[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"c"},"#,
            r#"{"functionCall":{"name":"a_b","args":{"x":1}},"thoughtSignature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"},"#,
            r#"{"functionCall":{"name":"_9g","args":[1]},"thoughtSignature":"skip_thought_signature_validator"},"#,
            r#"{"functionCall":{"name":"h","args":{"y":2}},"thoughtSignature":"skip_thought_signature_validator"},"#,
            r#"{"functionCall":{"name":"noid","args":5},"thoughtSignature":"skip_thought_signature_validator"}]},"#,
            r#"{"role":"user","parts":[{"functionResponse":{"name":"a_b","response":{"result":"{\"k\":[1,"#,
            r#"2]}"}}},{"functionResponse":{"name":"_9g","response":{"result":"{}"}}},{"functionResponse":{"name":"h","#,
            r#""response":{"result":"{}"}}},{"functionResponse":{"name":"noid","response":{"result":"{}"}}}]},"#,
            r#"{"role":"user","parts":[{"text":"between"}]},{"role":"model","parts":[{"text":"plain"}]},"#,
            r#"{"role":"user","parts":[{"text":"end"}]}],"model":"m"}"#
        )
    );
}

#[test]
fn tool_declarations_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a b","description":"d","parameters":{"type":"object","#,
            r#""title":"T","properties":{"p":{"type":"string"}}},"strict":true}},{"type":"function","#,
            r#""function":{"name":7,"parametersJsonSchema":{"type":"string","properties":{"q":1},"#,
            r#""z":1}}},{"type":"function","function":{"description":"no name"}},{"type":"function","#,
            r#""function":{"name":"arr","parametersJsonSchema":[1]},"google_search":{}},{"type":"function","#,
            r#""function":"notobj","google_search":{"a":1}},{"type":"function","function":{"name":"ts","#,
            r#""parameters":null},"strict":true},{"code_execution":{}},{"url_context":{"b":2}},"#,
            r#"{"google_search":null},"str",5],"tool_choice":{"type":"function","function":{"name":" a b "}}}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a_b","description":"d","#,
            r#""parametersJsonSchema":{"type":"object","properties":{"p":{"type":"string"}}}},{"name":"_7","#,
            r#""parametersJsonSchema":{"type":"object","properties":{},"z":1}},{"description":"no name","#,
            r#""parametersJsonSchema":{"type":"object","properties":{}},"name":""},{"name":"ts","#,
            r#""parametersJsonSchema":null}]},{"googleSearch":{"a":1}},{"googleSearch":null},{"codeExecution":{}},"#,
            r#"{"urlContext":{"b":2}}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a_b"]}}}"#
        )
    );
}

#[test]
fn allowed_tools_object_form_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a","parameters":{}}},{"type":"function","#,
            r#""function":{"name":"b","strict":true},"code_execution":{}},{"type":"function","function":{"name":"c"},"#,
            r#""url_context":{}}],"tool_choice":{"type":"allowed_tools","tools":{"name":" b "},"#,
            r#""mode":" ANY "}}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"b","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]},{"codeExecution":{}}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","#,
            r#""allowedFunctionNames":["b"]}}}"#
        )
    );
}

#[test]
fn allowed_tools_with_parallel_tool_calls_false() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"allowed_tools","#,
            r#""allowed_tools":{"tools":[{"function":{"name":"a"}}],"mode":"required"}},"parallel_tool_calls":false}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"NONE"}}}"#
        )
    );
}

#[test]
fn strict_tool_with_auto_choice() {
    assert_eq!(
        convert(
            r#"{"tools":[{"type":"function","function":{"name":"a","strict":true}}],"tool_choice":" AUTO "}"#
        ),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"VALIDATED"}}}"#
        )
    );
}

#[test]
fn unrecognized_tool_choice_fails_closed() {
    assert_eq!(
        convert(r#"{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":5}"#),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"NONE"}}}"#
        )
    );
}

#[test]
fn tool_type_choice_names_a_function() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"tool","#,
            r#""name":"a"}}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a"]}}}"#
        )
    );
}

#[test]
fn parallel_tool_calls_string_is_ignored() {
    assert_eq!(
        convert(
            r#"{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":"any","parallel_tool_calls":"false"}"#
        ),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY"}}}"#
        )
    );
}

#[test]
fn function_choice_without_function_fails_closed() {
    assert_eq!(
        convert(
            r#"{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"function"}}"#
        ),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"NONE"}}}"#
        )
    );
}

#[test]
fn allowed_tools_without_mode_uses_auto() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"allowed_tools","#,
            r#""allowed_tools":{"tools":[{"name":"a"}]}},"parallel_tool_calls":true}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"AUTO"}}}"#
        )
    );
}

#[test]
fn generation_settings_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"generationConfig":{"responseSchema":{"type":"string"},"temperature":1},"reasoning_effort":" HIGH ","#,
            r#""temperature":0.5,"top_p":"0.3","top_k":40,"max_tokens":"9","max_completion_tokens":12,"#,
            r#""n":3,"response_format":{"type":" JSON_SCHEMA ","json_schema":{"schema":{"type":"object"}}},"#,
            r#""modalities":["Text","IMAGE","audio",5],"image_config":{"aspect_ratio":"16:9","image_size":4}}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","generationConfig":{"temperature":0.5,"thinkingConfig":{"thinkingLevel":"high"},"#,
            r#""topK":40,"maxOutputTokens":12,"candidateCount":3,"responseMimeType":"application/json","#,
            r#""responseJsonSchema":{"type":"object"},"responseModalities":["TEXT","IMAGE"],"imageConfig":{"aspectRatio":"16:9"}}}"#
        )
    );
}

#[test]
fn auto_reasoning_and_fractional_tokens() {
    assert_eq!(
        convert(concat!(
            r#"{"reasoning_effort":"auto","n":1,"modalities":["audio"],"response_format":{"type":"json_object"},"#,
            r#""image_config":"x","max_tokens":2.5,"top_p":1e-7}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","generationConfig":{"thinkingConfig":{"thinkingBudget":-1},"#,
            r#""topP":0.0000001,"maxOutputTokens":2.5,"responseMimeType":"application/json"}}"#
        )
    );
}

#[test]
fn blank_reasoning_and_non_array_messages() {
    assert_eq!(
        convert(r#"{"reasoning_effort":"  ","messages":"x","tools":[]}"#),
        r#"{"contents":[],"model":"m"}"#
    );
}

#[test]
fn sanitized_name_collision_with_named_choice() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a b"}},{"type":"function","function":{"name":"a_b"}}],"#,
            r#""tool_choice":{"type":"function","function":{"name":"a b"}}}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a_b","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}},{"name":"a_b","parametersJsonSchema":{"type":"object","properties":{}}}]}],"#,
            r#""toolConfig":{"functionCallingConfig":{"mode":"NONE"}}}"#
        )
    );
}

#[test]
fn strict_flag_sources() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"type":"function","function":{"name":"a","strict":"true"}},{"type":"function","#,
            r#""function":{"name":"b"},"strict":true}]}"#
        )),
        concat!(
            r#"{"contents":[],"model":"m","tools":[{"functionDeclarations":[{"name":"a","parametersJsonSchema":{"type":"object","#,
            r#""properties":{}}},{"name":"b","parametersJsonSchema":{"type":"object","properties":{}}}]}],"#,
            r#""toolConfig":{"functionCallingConfig":{"mode":"VALIDATED"}}}"#
        )
    );
}

#[test]
fn lone_system_message_is_demoted() {
    assert_eq!(
        convert(r#"{"messages":[{"role":"system","content":"only"}]}"#),
        concat!(
            r#"{"contents":[{"role":"user","parts":[{"text":"<system-reminder>\nonly\n</system-reminder>"}]}],"#,
            r#""model":"m"}"#
        )
    );
}

/// Upstream slices an assistant message's data URLs by byte, so a URL whose
/// fixed-width prefix ends inside a character leaves stray bytes, which
/// become U+FFFD as upstream's JSON writer does.
#[test]
fn data_url_slicing_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"messages":[{"role":"user","content":"q"},{"role":"assistant","content":[{"type":"image_url","#,
            r#""image_url":{"url":"dataé;base64é,Q"}},{"type":"image_url","image_url":{"url":"data:a;b;base64,Q"}},"#,
            r#"{"type":"image_url","image_url":{"url":"data:"}}]},{"role":"user","content":"next"}]}"#
        )),
        concat!(
            r#"{"contents":[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"inlineData":{"mime_type":"�","data":"�,Q"}},"#,
            r#"{"inlineData":{"mime_type":"a","data":"4,Q"}}]},{"role":"user","parts":[{"text":"next"}]}],"model":"m"}"#
        )
    );
}

/// A user message's media is read as `NormalizeOpenAIFileData` reads it:
/// only a `data:` URL marked base64 is inlined.
#[test]
fn user_media_needs_a_base64_data_url() {
    let (body, err) = convert_checked(concat!(
        r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"dataé;base64é,"#,
        r#"Q"}},{"type":"video_url","video_url":{"url":"data:a;b;base64,Q"}},{"type":"image_url","#,
        r#""image_url":{"url":"data:"}},{"type":"file","file":{"file_data":"QQ"}},{"type":"input_audio","#,
        r#""input_audio":{"data":"","format":"mp3"}}]},{"role":"assistant","content":"x"},{"role":"user","#,
        r#""content":5}]}"#
    ));
    assert_eq!(err, None, "{body}");
    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"inlineData": {"mime_type": "a", "data": "Q"}}]}])
    );
}

// Not upstream's: upstream copies the parameters' text into
// `parametersJsonSchema` (`SetRaw`), so each number keeps its spelling.
#[test]
fn tool_parameters_keep_number_text() {
    let request = crate::json::exact::from_str(
        r#"{"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"x":{"type":"number","minimum":-0,"maximum":1E20}}}}}]}"#,
    )
    .unwrap();
    let output = convert_openai_request_to_gemini("m", &request, false).to_string();
    let want = r#""x":{"type":"number","minimum":-0,"maximum":1E20}"#;
    assert!(output.contains(want), "{output}");
}

/// `convertOpenAIRequestToGemini`: the body and the refusal.
fn convert_checked(input: &str) -> (Value, Option<UnsupportedPartError>) {
    let request: Value = serde_json::from_str(input).expect("test request is valid JSON");
    super::convert_openai_request_to_gemini("m", &request, false)
}

/// `TranslateRequestEnvelope` from Chat Completions to Gemini: the body, or
/// the refusal.
fn openai_to_gemini_envelope(model: &str, input: &str) -> Result<Value, UnsupportedPartError> {
    let request: Value = serde_json::from_str(input).expect("test request is valid JSON");
    crate::registry::Registry::global().translate_request_checked(
        &"openai".into(),
        &"gemini".into(),
        model,
        request,
        false,
    )
}

fn require_refusal(name: &str, input: &str, want: &str) {
    let (body, err) = convert_checked(input);
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

// TestConvertOpenAIRequestToGemini_FileID
#[test]
fn file_id() {
    const TEXT: &str = r#"{"type":"text","text":"read it"}"#;
    const MISSING: &str = r#"{"type":"file","file":{"file_id":"file-absent"}}"#;
    let payload = |content: &str| {
        format!(
            r#"{{"model":"gemini-2.5-pro","messages":[{{"role":"user","content":[{content}]}}]}}"#
        )
    };
    let err = openai_to_gemini_envelope("gemini-2.5-pro", &payload(MISSING))
        .expect_err("unknown file id alone");
    assert_eq!(err.part_type, "file");
    let body = openai_to_gemini_envelope("gemini-2.5-pro", &payload(&format!("{TEXT},{MISSING}")))
        .expect("text beside an unknown file id");
    assert_eq!(
        body["contents"][0]["parts"].as_array().map(Vec::len),
        Some(1),
        "{body}"
    );
}

const MEDIA_IMAGE_URL: &str = r#"{"type":"image_url","image_url":{"url":"https://x.test/a.png"}}"#;
const MEDIA_IMAGE_DATA: &str =
    r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}"#;
const MEDIA_VIDEO_URL: &str = r#"{"type":"video_url","video_url":{"url":"https://x.test/a.mp4"}}"#;
const MEDIA_AUDIO_NONE: &str = r#"{"type":"input_audio","input_audio":{"data":"","format":"wav"}}"#;

fn media_turn(parts: &str) -> String {
    format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{parts}]}}]}}"#
    )
}

// TestConvertOpenAIRequestToGemini_RefusesAnAttachmentOnlyTurnItCannotSend
#[test]
fn refuses_an_attachment_only_turn_it_cannot_send() {
    let cases = [
        ("http image_url", MEDIA_IMAGE_URL.to_owned(), "image_url"),
        (
            "image_url without a base64 payload",
            r#"{"type":"image_url","image_url":{"url":"data:image/png,raw"}}"#.to_owned(),
            "image_url",
        ),
        (
            "image_url as a bare string",
            r#"{"type":"image_url","image_url":"https://x.test/a.png"}"#.to_owned(),
            "image_url",
        ),
        ("http video_url", MEDIA_VIDEO_URL.to_owned(), "video_url"),
        (
            "input_audio without bytes",
            MEDIA_AUDIO_NONE.to_owned(),
            "input_audio",
        ),
        (
            "whitespace text beside http image_url",
            format!(r#"{{"type":"text","text":"  "}},{MEDIA_IMAGE_URL}"#),
            "image_url",
        ),
        (
            "first dropped type is reported",
            format!("{MEDIA_VIDEO_URL},{MEDIA_IMAGE_URL}"),
            "video_url",
        ),
    ];
    for (name, parts, want) in cases {
        require_refusal(name, &media_turn(&parts), want);
    }
}

const USER_TURN_FILE_ID: &str = r#"{"type":"file","file":{"file_id":"file-absent"}}"#;
const USER_TURN_TEXT: &str = r#"{"type":"text","text":"keep me"}"#;
const USER_TURN_INLINE: &str = r#"{"type":"file","file":{"filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}"#;

// TestConvertOpenAIRequestToGemini_RealTextBesideAnUnsendableAttachmentStillSucceeds
#[test]
fn real_text_beside_an_unsendable_attachment_still_succeeds() {
    for parts in [
        format!("{USER_TURN_TEXT},{MEDIA_IMAGE_URL}"),
        format!("{MEDIA_IMAGE_URL},{USER_TURN_TEXT}"),
        format!(
            r#"{{"type":"text","text":"  "}},{USER_TURN_TEXT},{MEDIA_VIDEO_URL},{MEDIA_AUDIO_NONE}"#
        ),
    ] {
        let (body, err) = convert_checked(&media_turn(&parts));
        assert_eq!(err, None, "{parts}: {body}");
        let found = body["contents"][2]["parts"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(|part| part["text"] == "keep me"));
        assert!(found, "{parts}: text was lost: {body}");
    }
}

// TestConvertOpenAIRequestToGemini_DataURLImageStillConverts
#[test]
fn data_url_image_still_converts() {
    let (body, err) = convert_checked(&media_turn(MEDIA_IMAGE_DATA));
    assert_eq!(err, None, "{body}");
    assert_eq!(
        body["contents"][2]["parts"][0]["inlineData"],
        json!({"mime_type": "image/png", "data": "iVBORw0KGgo="}),
        "{body}"
    );
}

// TestOpenAIToGeminiRegistryCarriesTheImageURLRefusal
#[test]
fn registry_carries_the_image_url_refusal() {
    let err = openai_to_gemini_envelope("m", &media_turn(MEDIA_IMAGE_URL)).expect_err("refused");
    assert_eq!(err.part_type, "image_url");
}

// TestConvertOpenAIRequestToGemini_RefusesAnyEmptiedUserTurn
#[test]
fn refuses_any_emptied_user_turn() {
    let cases = [
        (
            "history then file id only",
            format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{USER_TURN_FILE_ID}]}}]}}"#
            ),
        ),
        (
            "system and developer prompts do not hide the empty turn",
            format!(
                r#"{{"model":"m","messages":[{{"role":"system","content":"sys"}},{{"role":"developer","content":"dev"}},{{"role":"user","content":[{USER_TURN_FILE_ID}]}}]}}"#
            ),
        ),
        (
            "emptied turn before a later text turn",
            format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":[{USER_TURN_FILE_ID}]}},{{"role":"assistant","content":"ok"}},{{"role":"user","content":"next"}}]}}"#
            ),
        ),
    ];
    for (name, input) in cases {
        require_refusal(name, &input, "file");
    }
}

// TestConvertOpenAIRequestToGemini_KeepsTurnWithTextBesideAttachment
#[test]
fn keeps_turn_with_text_beside_attachment() {
    let (body, err) = convert_checked(&media_turn(&format!(
        "{USER_TURN_TEXT},{USER_TURN_FILE_ID}"
    )));
    assert_eq!(err, None, "{body}");
    assert_eq!(body["contents"][2]["parts"][0]["text"], "keep me", "{body}");
}

// TestConvertOpenAIRequestToGemini_InlineFileAfterHistoryStaysInlineData
#[test]
fn inline_file_after_history_stays_inline_data() {
    let (body, err) = convert_checked(&media_turn(USER_TURN_INLINE));
    assert_eq!(err, None, "{body}");
    assert_eq!(
        body["contents"][2]["parts"][0]["inlineData"]["mime_type"], "application/pdf",
        "{body}"
    );
}
