// Ported from CLIProxyAPI internal/translator/gemini/claude/gemini_claude_request_test.go,
// gemini_claude_compat_test.go and internal/util/claude_tool_result_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests after them are new; their expected output
// comes from upstream.

use serde_json::{Value, json};

use super::*;
use crate::signature::validate_gemini_function_call_pairing;

const CAPTURED_GEMINI_THINKING_SIGNATURE: &str =
    "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

fn models() -> &'static ModelCatalog {
    ModelCatalog::embedded()
}

fn translate(model: &str, input: Value) -> Value {
    convert_claude_request_to_gemini(model, &input, false, models())
}

fn translate_compat(model: &str, input: Value) -> Value {
    convert_claude_request_to_gemini_with_compat(model, &input, false, models())
}

/// Translates `input` for `gemini-2.5-pro` and returns the result without
/// its safety settings, as compact JSON, checking the defaults were added.
fn convert(input: &str) -> String {
    let mut output = translate("gemini-2.5-pro", serde_json::from_str(input).unwrap());
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

#[test]
fn tool_choice_specific_tool() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{
                "name": "json",
                "description": "A JSON tool",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": {"type": "tool", "name": "json"}
        }),
    );
    let config = &output["toolConfig"]["functionCallingConfig"];
    assert_eq!(config["mode"], "ANY");
    assert_eq!(config["allowedFunctionNames"], json!(["json"]));
}

#[test]
fn string_system_instruction() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "system": "Be concise",
            "messages": [{"role": "user", "content": "Hello"}]
        }),
    );
    assert_eq!(
        output["systemInstruction"]["parts"][0]["text"],
        "Be concise"
    );
    assert!(output["systemInstruction"].get("role").is_none());
    assert!(output.get("system_instruction").is_none());
}

#[test]
fn image_content() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this image"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "aGVsbG8="
                }}
            ]}]
        }),
    );
    let parts = output["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "describe this image");
    assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
    assert_eq!(parts[1]["inline_data"]["data"], "aGVsbG8=");
}

#[test]
fn strips_claude_code_attribution() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "claude-sonnet-4-5",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.63.abc; cc_entrypoint=cli; cch=12345;"},
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"type": "text", "text": "User system prompt"}
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }),
    );
    assert_eq!(
        output["systemInstruction"]["parts"],
        json!([
            {"text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
            {"text": "User system prompt"}
        ])
    );
}

#[test]
fn converts_message_system_role_to_user_content() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "system": [{"type": "text", "text": "Top-level rules"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "system", "content": "String mid-conversation rule"},
                {"role": "system", "content": [{"type": "text", "text": "Array mid-conversation rule"}]}
            ]
        }),
    );
    assert_eq!(
        output["contents"],
        json!([{"role": "user", "parts": [
            {"text": "Hello"},
            {"text": "<system-reminder>\nString mid-conversation rule\n</system-reminder>"},
            {"text": "<system-reminder>\nArray mid-conversation rule\n</system-reminder>"}
        ]}])
    );
    assert_eq!(
        output["systemInstruction"]["parts"],
        json!([{"text": "Top-level rules"}])
    );
}

#[test]
fn message_level_developer_instructions_become_merged_user_reminder() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "system": "Top-level rules",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {"role": "developer", "content": "String mid-conversation developer rule"},
                {"role": "developer", "content": [{"type": "text", "text": "Array mid-conversation developer rule"}]}
            ]
        }),
    );
    assert_eq!(
        output["contents"],
        json!([{"role": "user", "parts": [
            {"text": "Hello"},
            {"text": "<system-reminder>\nString mid-conversation developer rule\n</system-reminder>"},
            {"text": "<system-reminder>\nArray mid-conversation developer rule\n</system-reminder>"}
        ]}])
    );
    assert_eq!(
        output["systemInstruction"]["parts"],
        json!([{"text": "Top-level rules"}])
    );
}

#[test]
fn preserves_tool_pairing_with_intervening_system_message() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Run two tools"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "tool_one", "input": {"a": 1}},
                    {"type": "tool_use", "id": "toolu_2", "name": "tool_two", "input": {"b": 2}}
                ]},
                {"role": "system", "content": "Context reminder between tool_use and tool_result"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "result 2"},
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "result 1"}
                ]}
            ]
        }),
    );
    validate_gemini_function_call_pairing(&output).unwrap();
    let contents = output["contents"].as_array().unwrap();
    let roles: Vec<&Value> = contents.iter().map(|content| &content["role"]).collect();
    assert_eq!(roles, ["user", "model", "user"]);
    let ids: Vec<&Value> = contents[2]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|part| part["functionResponse"].get("id"))
        .collect();
    assert_eq!(ids, ["toolu_1", "toolu_2"]);
}

#[test]
fn skips_empty_text_parts() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": ""},
                {"type": "text", "text": "hello"},
                {"type": "text", "text": ""}
            ]}]
        }),
    );
    assert_eq!(output["contents"][0]["parts"], json!([{"text": "hello"}]));
}

#[test]
fn structured_tool_result() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "json-call-1", "name": "json", "input": {"ok": true}}
                ]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "json-call-1",
                    "content": [
                        {"type": "text", "text": "alpha"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                    ]
                }]}
            ]
        }),
    );
    let parts = &output["contents"][1]["parts"];
    assert_eq!(
        parts[0]["functionResponse"]["response"]["result"]["text"],
        "alpha"
    );
    assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
    assert_eq!(parts[1]["inline_data"]["data"], "aGVsbG8=");
}

#[test]
fn aligns_permuted_parallel_tool_results_with_mixed_text() {
    let output = translate(
        "gemini-3.7-flash-high",
        json!({
            "model": "gemini-3.7-flash-high",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "Read", "input": {"file_path": "/tmp/1"}},
                    {"type": "tool_use", "id": "call_2", "name": "Read", "input": {"file_path": "/tmp/2"}},
                    {"type": "tool_use", "id": "call_3", "name": "Read", "input": {"file_path": "/tmp/3"}}
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "Results arrived."},
                    {"type": "tool_result", "tool_use_id": "call_3", "content": "three"},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "two"},
                    {"type": "text", "text": "Continue."}
                ]}
            ]
        }),
    );
    let calls = output["contents"][0]["parts"].as_array().unwrap();
    let responses = output["contents"][1]["parts"].as_array().unwrap();
    assert_eq!((calls.len(), responses.len()), (3, 5));
    for (index, id) in ["call_1", "call_2", "call_3"].into_iter().enumerate() {
        assert_eq!(calls[index]["functionCall"]["id"], id);
        assert_eq!(responses[index + 2]["functionResponse"]["id"], id);
        assert_eq!(responses[index + 2]["functionResponse"]["name"], "Read");
    }
    assert_eq!(responses[0]["text"], "Results arrived.");
    assert_eq!(responses[1]["text"], "Continue.");
    validate_gemini_function_call_pairing(&output).unwrap();
}

#[test]
fn string_tool_result() {
    let output = translate(
        "gemini-3-flash-preview",
        json!({
            "model": "gemini-3-flash-preview",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "json-call-1", "name": "json", "input": {"ok": true}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "json-call-1", "content": "alpha"}
                ]}
            ]
        }),
    );
    assert_eq!(
        output["contents"][1]["parts"][0]["functionResponse"]["response"]["result"],
        "alpha"
    );
}

#[test]
fn tool_result_with_trailing_system_reminder_reorders_parts() {
    let reminder = "<system-reminder>\n<total_tokens>1234</total_tokens>\n</system-reminder>";
    let output = translate(
        "gemini-3.8-flash",
        json!({
            "model": "gemini-3.8-flash",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read the file"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_01_read", "name": "Read", "input": {"path": "main.go"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01_read", "content": "package main"},
                    {"type": "text", "text": reminder}
                ]}
            ]
        }),
    );
    let contents = output["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    let parts = contents[2]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], reminder);
    assert_eq!(parts[1]["functionResponse"]["id"], "toolu_01_read");
}

#[test]
fn tool_strict_maps_to_validated_mode() {
    let cases: [(Option<Value>, &str, Option<Value>); 6] = [
        (None, "VALIDATED", None),
        (Some(json!({"type": "auto"})), "VALIDATED", None),
        (Some(Value::Null), "VALIDATED", None),
        (Some(json!({"type": "none"})), "NONE", None),
        (Some(json!({"type": "any"})), "ANY", None),
        (
            Some(json!({"type": "tool", "name": "tool_a"})),
            "ANY",
            Some(json!(["tool_a"])),
        ),
    ];
    for (tool_choice, mode, allowed) in cases {
        let mut input = json!({
            "model": "gemini-3.8-flash",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "tool_a",
                "description": "Controlled tool.",
                "strict": true,
                "input_schema": {"type": "object", "properties": {}}
            }]
        });
        if let Some(tool_choice) = tool_choice {
            input["tool_choice"] = tool_choice;
        }
        let output = translate("gemini-3.8-flash", input);
        assert!(
            output["tools"][0]["functionDeclarations"][0]
                .get("strict")
                .is_none()
        );
        let config = &output["toolConfig"]["functionCallingConfig"];
        assert_eq!(config["mode"], mode);
        if let Some(allowed) = allowed {
            assert_eq!(config["allowedFunctionNames"], allowed);
        }
    }

    // Mixed tools where one is strict map to VALIDATED.
    let output = translate(
        "gemini-3.8-flash",
        json!({
            "model": "gemini-3.8-flash",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "tool_a", "description": "Loose tool.", "strict": false,
                 "input_schema": {"type": "object", "properties": {}}},
                {"name": "tool_b", "description": "Strict tool.", "strict": true,
                 "input_schema": {"type": "object", "properties": {}}}
            ]
        }),
    );
    assert_eq!(
        output["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );

    // Non-strict tools leave out the tool config.
    let output = translate(
        "gemini-3.8-flash",
        json!({
            "model": "gemini-3.8-flash",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "tool_a", "description": "Loose tool.", "strict": false,
                 "input_schema": {"type": "object", "properties": {}}},
                {"name": "tool_b", "description": "Unspecified tool.",
                 "input_schema": {"type": "object", "properties": {}}}
            ]
        }),
    );
    assert!(output.get("toolConfig").is_none());
}

#[test]
fn parameters_json_schema_preserves_additional_properties_and_pattern_issue_5959() {
    let output = translate(
        "gemini-2.5-flash",
        json!({
            "model": "gemini-2.5-flash",
            "messages": [{"role": "user", "content": "Use the submit tool."}],
            "tools": [{
                "name": "submit",
                "description": "Submit a bounded schema test value.",
                "input_schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "recipient": {"type": "string", "pattern": "^(alice|bob)$"},
                        "amount": {"type": "number"}
                    },
                    "required": ["recipient", "amount"]
                }
            }]
        }),
    );
    let schema = &output["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["recipient"]["pattern"],
        "^(alice|bob)$"
    );
    assert_ne!(schema["description"], "No extra properties allowed");
    assert!(!str_of(schema["properties"]["recipient"].get("description")).contains("pattern:"));
}

#[test]
fn function_response_json_ref() {
    let output = translate(
        "gemini-3.8-flash",
        json!({
            "model": "gemini-3.8-flash",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_schema_1", "name": "get_schema", "input": {}}
                ]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_schema_1",
                    "content": {"schema": {"$ref": "#/components/schemas/ErrorModel"}}
                }]}
            ]
        }),
    );
    let result = &output["contents"][1]["parts"][0]["functionResponse"]["response"]["result"];
    assert!(
        result
            .as_str()
            .unwrap()
            .contains("#/components/schemas/ErrorModel")
    );
}

#[test]
fn compat_signature_compatibility() {
    for (signature, want) in [
        (
            format!("gemini#{CAPTURED_GEMINI_THINKING_SIGNATURE}"),
            CAPTURED_GEMINI_THINKING_SIGNATURE,
        ),
        (
            "claude#opaque-signature-12345".to_owned(),
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        ),
        (String::new(), GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR),
    ] {
        let output = translate_compat(
            "deepseek-v4",
            json!({"messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reason", "signature": signature}
            ]}]}),
        );
        let part = &output["contents"][0]["parts"][0];
        assert_eq!(part["thought"], true);
        assert_eq!(part["text"], "reason");
        assert_eq!(part["thoughtSignature"], want, "{signature}");
    }
}

#[test]
fn compat_preserves_empty_thinking() {
    let input = json!({"messages": [{"role": "assistant", "content": [
        {"type": "thinking", "thinking": "reason", "signature": ""}
    ]}]});

    let output = translate("deepseek-v4", input.clone());
    assert_eq!(output["contents"][0]["parts"], json!([]));

    let output = translate_compat("deepseek-v4", input);
    let part = &output["contents"][0]["parts"][0];
    assert_eq!(part["thought"], true);
    assert_eq!(part["text"], "reason");
    assert_eq!(
        part["thoughtSignature"],
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
    );
}

#[test]
fn convert_claude_tool_result_content() {
    let image = json!({"type": "image", "source": {
        "type": "base64", "media_type": "image/png", "data": "aGVsbG8="
    }});
    let image_without_data = json!({"type": "image", "source": {
        "type": "base64", "media_type": "image/png", "data": ""
    }});
    let alpha = json!({"type": "text", "text": "alpha"});
    let beta = json!({"type": "text", "text": "beta"});
    let cases = [
        (Some(json!("alpha")), json!("alpha"), 0),
        (Some(json!([alpha])), alpha.clone(), 0),
        (Some(json!([alpha, beta])), json!([alpha, beta]), 0),
        (Some(json!([alpha, image])), alpha.clone(), 1),
        (Some(json!([image])), json!(""), 1),
        (Some(json!([image_without_data])), json!(""), 0),
        (Some(json!({"foo": "bar"})), json!({"foo": "bar"}), 0),
        (Some(image.clone()), json!(""), 1),
        (None, json!(""), 0),
    ];
    for (content, result, images) in cases {
        let converted = convert_tool_result_content(content.as_ref());
        assert_eq!(converted.result, result, "{content:?}");
        assert_eq!(converted.images.len(), images, "{content:?}");
    }
}

#[test]
fn convert_claude_tool_result_content_image_fields() {
    let converted = convert_tool_result_content(Some(&json!([{"type": "image", "source": {
        "type": "base64", "media_type": "image/png", "data": "aGVsbG8="
    }}])));
    assert_eq!(
        converted.images,
        [Image {
            mime_type: "image/png".to_owned(),
            data: "aGVsbG8=".to_owned(),
        }]
    );
}

#[test]
fn tool_calls_and_results_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"","name":"a b","input":"{\"a\":1}"},"#,
            r#"{"type":"tool_use","id":"x-1","name":"f","input":3},"#,
            r#"{"type":"tool_use","id":"y","name":"9g","input":" {\"b\":[1]} "}]},"#,
            r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"Read-tool-7","content":[{"type":"text","text":"a"},"#,
            r#"{"type":"image","source":{"type":"base64","data":""}},"s"]},"#,
            r#"{"type":"tool_result","tool_use_id":"nodash","content":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"QQ=="}}},"#,
            r#"{"type":"tool_result","tool_use_id":"y","content":5},{"type":"tool_result","tool_use_id":"x-1","content":null},"#,
            r##"{"type":"tool_result","tool_use_id":"-z","content":{"$ref":"#/a"}},{"type":"tool_result","tool_use_id":"q"}]}]}"##
        )),
        concat!(
            r#"{"contents":[{"role":"model","parts":[{"thoughtSignature":"skip_thought_signature_validator","functionCall":{"name":"a_b","args":{"a":1}}},"#,
            r#"{"thoughtSignature":"skip_thought_signature_validator","functionCall":{"name":"_9g","args":{"b":[1]},"id":"y"}}]},"#,
            r#"{"role":"user","parts":[{"functionResponse":{"name":"Read-tool","response":{"result":[{"type":"text","text":"a"},"s"]},"id":"Read-tool-7"}},"#,
            r#"{"functionResponse":{"name":"nodash","response":{"result":""},"id":"nodash"}},{"inline_data":{"mime_type":"image/png","data":"QQ=="}},"#,
            r#"{"functionResponse":{"name":"_9g","response":{"result":5},"id":"y"}},{"functionResponse":{"name":"f","response":{"result":null},"id":"x-1"}},"#,
            r##"{"functionResponse":{"name":"_-z","response":{"result":"{\"$ref\":\"#/a\"}"},"id":"-z"}},"##,
            r#"{"functionResponse":{"name":"q","response":{"result":""},"id":"q"}}]}],"model":"gemini-2.5-pro"}"#
        )
    );
}

#[test]
fn tool_declarations_and_choice_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"tools":[{"input_schema":{"type":"object"},"strict":true,"type":"custom","cache_control":{},"x":1},"#,
            r#"{"name":12,"input_schema":{"type":"object"}},"#,
            r#"{"name":"a b","input_schema":{"type":"object"},"parametersJsonSchema":1},{"name":"ok","strict":true}],"tool_choice":"auto"}"#
        )),
        concat!(
            r#"{"contents":[],"model":"gemini-2.5-pro","tools":[{"functionDeclarations":["#,
            r#"{"x":1,"parametersJsonSchema":{"type":"object"},"name":""},{"name":"_12","parametersJsonSchema":{"type":"object"}},"#,
            r#"{"name":"a_b","parametersJsonSchema":{"type":"object"}}]}],"toolConfig":{"functionCallingConfig":{"mode":"VALIDATED"}}}"#
        )
    );
    // A strict tool without a schema doesn't make the mode validated.
    assert_eq!(
        convert(r#"{"tools":[{"name":"ok","strict":true}]}"#),
        r#"{"contents":[],"model":"gemini-2.5-pro"}"#
    );
    assert_eq!(
        convert(r#"{"tools":[{"name":"ok","strict":true,"input_schema":{}}],"tool_choice":5}"#),
        concat!(
            r#"{"contents":[],"model":"gemini-2.5-pro","#,
            r#""tools":[{"functionDeclarations":[{"name":"ok","parametersJsonSchema":{}}]}]}"#
        )
    );
    assert_eq!(
        convert(
            r#"{"tools":[{"name":"ok","input_schema":{}}],"tool_choice":{"type":"tool","name":"a b"}}"#
        ),
        concat!(
            r#"{"contents":[],"model":"gemini-2.5-pro","#,
            r#""tools":[{"functionDeclarations":[{"name":"ok","parametersJsonSchema":{}}]}],"#,
            r#""toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a_b"]}}}"#
        )
    );
}

#[test]
fn thinking_and_sampling() {
    assert_eq!(
        convert(r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":" HIGH "}}"#),
        concat!(
            r#"{"contents":[],"model":"gemini-2.5-pro","#,
            r#""generationConfig":{"thinkingConfig":{"thinkingLevel":"high"}}}"#
        )
    );
    assert_eq!(
        convert(r#"{"thinking":{"type":"auto"}}"#),
        concat!(
            r#"{"contents":[],"model":"gemini-2.5-pro","#,
            r#""generationConfig":{"thinkingConfig":{"thinkingBudget":32768}}}"#
        )
    );
    assert_eq!(
        convert(
            r#"{"thinking":{"type":"enabled","budget_tokens":1.9},"temperature":0.5,"top_p":"1","top_k":40}"#
        ),
        concat!(
            r#"{"contents":[],"model":"gemini-2.5-pro","#,
            r#""generationConfig":{"thinkingConfig":{"thinkingBudget":1},"temperature":0.5,"topK":40}}"#
        )
    );
    // An unknown model has no largest budget.
    let output = translate_compat("nope", json!({"thinking": {"type": "auto"}}));
    assert_eq!(
        output["generationConfig"],
        json!({"thinkingConfig": {"thinkingLevel": "high"}})
    );
    // A sampling setting that isn't finite is left out.
    let output = translate(
        "m",
        serde_json::from_str(r#"{"temperature":1e400}"#).unwrap(),
    );
    assert!(output.get("generationConfig").is_none());
}

#[test]
fn messages_and_system_in_detail() {
    assert_eq!(
        convert(concat!(
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: a"},{"type":"text","text":1},"s"],"#,
            r#""messages":[{"role":3,"content":"x"},{"role":"foo","content":[{"type":"text","text":{"a":1}},"#,
            r#"{"type":"tool_result","tool_use_id":"t","content":"r"},{"type":"text","text":"after"}]},{"role":"user"},"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"n","input":{}}]}]}"#
        )),
        concat!(
            r#"{"contents":[{"role":"foo","parts":[{"text":"{\"a\":1}"},"#,
            r#"{"functionResponse":{"name":"t","response":{"result":"r"},"id":"t"}},{"text":"after"}]}],"#,
            r#""model":"gemini-2.5-pro"}"#
        )
    );
    assert_eq!(
        convert(concat!(
            r#"{"system":"x-anthropic-billing-header: a","messages":[{"role":"user","content":[{"type":"thinking","thinking":"t","signature":"s"}]},"#,
            r#"{"role":"model","content":[]},{"role":"user","content":[]}]}"#
        )),
        r#"{"contents":[],"model":"gemini-2.5-pro"}"#
    );
    let output = translate_compat(
        "nope",
        json!({"messages": [{"role": "user", "content": [
            {"type": "thinking", "thinking": 7, "signature": "s"}
        ]}]}),
    );
    assert_eq!(
        output["contents"],
        json!([{"role": "user", "parts": [{
            "text": "7", "thought": true, "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        }]}])
    );
}
