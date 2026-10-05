// Ported from CLIProxyAPI internal/translator/claude/gemini/claude_gemini_request_test.go
// and noop_optimization_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// Changed: TestConvertGeminiRequestToClaude_DifferentSessionsProduceDifferentUserIDs
// and TestConvertGeminiRequestToClaude_DefaultRoleDifferentContentProducesDifferentUserIDs
// check the user ID upstream makes up; here `no_user_id_is_made_up` checks
// that there is none. TestLowercaseClaudeToolSchemaTypesReusesLowercaseSchema
// checks that the payload isn't copied; here it checks that the tool is
// unchanged.

use serde_json::{Value, json};

use super::*;

fn convert(model: &str, request: &str) -> Value {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    convert_gemini_request_to_claude(model, &request, false, ModelCatalog::embedded())
}

fn messages(out: &Value) -> &[Value] {
    out["messages"].as_array().expect("messages is an array")
}

#[test]
fn thinking_summary_visibility() {
    for (include, want) in [("true", "summarized"), ("false", "omitted")] {
        let out = convert(
            "claude-opus-5-5",
            &format!(
                r#"{{"generationConfig":{{"thinkingConfig":{{"thinkingLevel":"high","includeThoughts":{include}}}}},"contents":[{{"role":"user","parts":[{{"text":"hi"}}]}}]}}"#
            ),
        );
        assert_eq!(out["thinking"]["display"], want, "{out}");
    }
}

#[test]
fn preserves_custom_tool_ids() {
    for (field, want) in [
        (r#""id":"call_gateway_id""#, "call_gateway_id"),
        (
            r#""call_id":"call_gateway_call_id""#,
            "call_gateway_call_id",
        ),
    ] {
        let out = convert(
            "claude-sonnet-4",
            &format!(
                r#"{{"contents": [
                    {{"role": "model", "parts": [{{"functionCall": {{"name": "lookup", {field}, "args": {{"query": "status"}}}}}}]}},
                    {{"role": "user", "parts": [{{"functionResponse": {{"name": "lookup", {field}, "response": {{"result": "ok"}}}}}}]}}
                ]}}"#
            ),
        );
        assert_eq!(out["messages"][0]["content"][0]["id"], want, "{out}");
        assert_eq!(
            out["messages"][1]["content"][0]["tool_use_id"], want,
            "{out}"
        );
    }
}

#[test]
fn groups_consecutive_role_turns() {
    let out = convert(
        "claude-test",
        r#"{"contents":[
            {"role":"model","parts":[{"text":"answer"}]},
            {"role":"model","parts":[{"functionCall":{"name":"first","id":"call_1","args":{}}}]},
            {"role":"model","parts":[{"functionCall":{"name":"second","id":"call_2","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"first","id":"call_1","response":{"result":"one"}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"second","id":"call_2","response":{"result":"two"}}}]}
        ]}"#,
    );
    let messages = messages(&out);
    assert_eq!(messages.len(), 2, "{out}");
    let types: Vec<&Value> = messages[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| &block["type"])
        .collect();
    assert_eq!(types, ["text", "tool_use", "tool_use"]);
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "call_1", "content": "one"},
            {"type": "tool_result", "tool_use_id": "call_2", "content": "two"}
        ])
    );
}

#[test]
fn keeps_system_instruction_user_separate() {
    let out = convert(
        "claude-test",
        r#"{"system_instruction":{"parts":[{"text":"system rule"}]},"contents":[{"role":"user","parts":[{"text":"question"}]}]}"#,
    );
    assert_eq!(
        out["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "system rule"}]},
            {"role": "user", "content": [{"type": "text", "text": "question"}]}
        ])
    );
}

#[test]
fn drops_temperature() {
    let out = convert(
        "claude-sonnet-5",
        r#"{"generationConfig":{"temperature":0.2,"topP":0.8},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    );
    assert!(out.get("temperature").is_none());
    assert_eq!(out["top_p"].to_string(), "0.8");
}

#[test]
fn accepts_camel_inline_data() {
    let out = convert(
        "claude-sonnet-4",
        r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}]}"#,
    );
    assert_eq!(
        out["messages"][0]["content"][0],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}})
    );
}

#[test]
fn splits_non_image_inline_data_by_mime() {
    let out = convert(
        "claude-sonnet-4",
        r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}},{"inlineData":{"mimeType":"video/mp4","data":"AAAAIGZ0eXA="}},{"inlineData":{"mimeType":"application/pdf","data":"JVBERi0="}}]}]}"#,
    );
    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type": "text", "text": "Media content: inline data (Type: audio/wav)"},
            {"type": "text", "text": "Media content: inline data (Type: video/mp4)"},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="}}
        ])
    );
}

#[test]
fn drops_hidden_thought_parts() {
    // A turn of only a thought.
    let out = convert(
        "claude-test",
        r#"{"contents":[
            {"role":"model","parts":[{"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"}]},
            {"role":"user","parts":[{"text":"continue"}]}
        ]}"#,
    );
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "continue"}]}])
    );

    // A thought beside visible text.
    let out = convert(
        "claude-test",
        r#"{"contents":[{"role":"model","parts":[
            {"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"},
            {"text":"visible answer"}
        ]}]}"#,
    );
    assert_eq!(
        out["messages"][0]["content"],
        json!([{"type": "text", "text": "visible answer"}])
    );
}

#[test]
fn deterministic_tool_ids() {
    let raw = r#"{"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "first_tool", "args": {"q": "one"}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "first_tool", "response": {"result": "ok1"}}}]},
        {"role": "model", "parts": [{"functionCall": {"name": "second_tool", "args": {"q": "two"}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "second_tool", "response": {"result": "ok2"}}}]}
    ]}"#;
    let out = convert("claude-sonnet-4", raw);
    assert_eq!(out, convert("claude-sonnet-4", raw));
    let first = "toolu_gemini_0000000000000001";
    let second = "toolu_gemini_0000000000000002";
    assert_eq!(out["messages"][0]["content"][0]["id"], first);
    assert_eq!(out["messages"][1]["content"][0]["tool_use_id"], first);
    assert_eq!(out["messages"][2]["content"][0]["id"], second);
    assert_eq!(out["messages"][3]["content"][0]["tool_use_id"], second);
}

#[test]
fn preserves_caller_supplied_metadata_user_id() {
    for (request, want) in [
        (
            r#"{"model":"claude-test","metadata":{"user_id":"custom-gemini-user-123"},"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
            "custom-gemini-user-123",
        ),
        (
            r#"{"model":"claude-test","metadata":{"user_id":"foo\"bar\nbaz\\qux"},"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
            "foo\"bar\nbaz\\qux",
        ),
        (
            r#"{"model":"claude-test","metadata":{"user_id":"{\"device_id\":\"0000\",\"session_id\":\"11111111-2222-4333-8444-555555555555\"}"},"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
            r#"{"device_id":"0000","session_id":"11111111-2222-4333-8444-555555555555"}"#,
        ),
    ] {
        let out = convert("claude-test", request);
        assert_eq!(out["metadata"]["user_id"], want, "{out}");
    }
}

#[test]
fn no_user_id_is_made_up() {
    for request in [
        r#"{"model":"claude-test","prompt_cache_key":"gemini-session-a","contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
        r#"{"contents":[{"parts":[{"text":"first prompt"}]}]}"#,
    ] {
        assert_eq!(convert("claude-test", request)["metadata"], json!({}));
    }
}

#[test]
fn sanitizes_tool_names_and_provides_fallback_schema() {
    let out = convert(
        "claude-test",
        r#"{
            "contents": [
                {"role": "model", "parts": [{"functionCall": {"name": "mcp.server:get_data", "args": {}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "mcp.server:get_data", "response": {"result": "ok"}}}]}
            ],
            "tools": [{"functionDeclarations": [{"name": "mcp.server:get_data", "description": "parameterless mcp tool"}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["mcp.server:get_data"]}}
        }"#,
    );
    assert_eq!(
        out["tools"],
        json!([{
            "description": "parameterless mcp tool",
            "input_schema": {"properties": {}, "type": "object"},
            "name": "mcp_server_get_data"
        }])
    );
    assert_eq!(
        out["messages"][0]["content"][0]["name"],
        "mcp_server_get_data"
    );
    assert_eq!(
        out["tool_choice"],
        json!({"type": "tool", "name": "mcp_server_get_data"})
    );
}

#[test]
fn normalize_schema_preserves_canonical_schema() {
    let input = json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"});
    assert_eq!(normalize_schema(&input).to_string(), input.to_string());
}

#[test]
fn normalize_schema_corrects_wrong_types() {
    let output =
        normalize_schema(&json!({"type":"object","additionalProperties":"false","$schema":123}));
    assert_eq!(
        output,
        json!({"type": "object", "additionalProperties": false, "$schema": DRAFT_07_SCHEMA})
    );
}

#[test]
fn normalize_schema_follows_sjson_for_other_values() {
    let closed = json!({"additionalProperties": false, "$schema": DRAFT_07_SCHEMA});
    assert_eq!(normalize_schema(&json!("x")), closed);
    assert_eq!(normalize_schema(&json!(null)), closed);
    assert_eq!(normalize_schema(&json!([1])), json!([1]));
}

#[test]
fn lowercase_types_keeps_lowercase_schema() {
    let input = json!({"name":"lookup","input_schema":{"type":"object","properties":{"value":{"type":"string"}}}});
    let mut output = input.clone();
    lowercase_types(&mut output);
    assert_eq!(output.to_string(), input.to_string());
}

#[test]
fn lowercase_types_normalizes_non_string_type() {
    let mut tool = json!({"input_schema":{"type":123}});
    lowercase_types(&mut tool);
    assert_eq!(tool["input_schema"]["type"], "123");
}

#[test]
fn lowercase_types_normalizes_uppercase_types() {
    let mut tool =
        json!({"input_schema":{"type":"OBJECT","properties":{"value":{"type":"STRING"}}}});
    lowercase_types(&mut tool);
    assert_eq!(tool["input_schema"]["type"], "object");
    assert_eq!(
        tool["input_schema"]["properties"]["value"]["type"],
        "string"
    );
}

#[test]
fn lowercase_types_follows_sjson_through_replaced_values() {
    // These match what Go gives.
    let lowered = |tool: Value| {
        let mut tool = tool;
        lowercase_types(&mut tool);
        tool
    };
    assert_eq!(
        lowered(json!({"type":{"type":"X"}})),
        json!({"type":{"type":""}})
    );
    assert_eq!(
        lowered(json!({"type":{"a":{"type":"X"}}})),
        json!({"type":{"a":{"type":""}}})
    );
    assert_eq!(
        lowered(json!({"type":{"3":{"type":"X"}}})),
        json!({"type":[null,null,null,{"type":""}]})
    );
    assert_eq!(
        lowered(json!({"type":{"type":"X","b":{"type":"Y"}}})),
        json!({"type":{"b":{"type":""},"type":""}})
    );
    assert_eq!(lowered(json!({"type":["X"]})), json!({"type":"[\"x\"]"}));
    assert_eq!(
        lowered(json!({"a":[{"type":5},{"type":null}]})),
        json!({"a":[{"type":"5"},{"type":""}]})
    );
}

#[test]
fn lowercase_types_takes_path_syntax_in_keys_literally() {
    // Upstream keeps `a|b` and `@this` as they are, sets `ab.type` to "" for
    // `a\b`, and adds a `7` for `:7`.
    let mut tool = json!({"p":{
        "a|b":{"type":"INTEGER"},
        "a\\b":{"type":"INTEGER"},
        ":7":{"type":"INTEGER"},
        "#":{"type":"INTEGER"},
        "@this":{"type":"INTEGER"},
        "x.y*?":{"type":"INTEGER"}
    }});
    lowercase_types(&mut tool);
    assert_eq!(
        tool,
        json!({"p":{
            "a|b":{"type":"integer"},
            "a\\b":{"type":"integer"},
            ":7":{"type":"integer"},
            "#":{"type":"integer"},
            "@this":{"type":"integer"},
            "x.y*?":{"type":"integer"}
        }})
    );
}

#[test]
fn builds_the_whole_request_in_upstream_order() {
    let out = convert(
        "claude-sonnet-4",
        r#"{
            "service_tier": "auto",
            "system_instruction": {"parts": [{"text": ""}, {"text": "a"}, {"thought": true, "text": "x"}, {"text": "b"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}, {"fileData": {"fileUri": "https://x/y.png", "mimeType": "image/png"}}]},
                {"role": "system", "parts": [{"functionResponse": {"name": "dropped", "response": {}}}]},
                {"role": "model", "parts": [{"functionCall": {"name": "Lookup", "args": [1]}}]},
                {"role": "tool", "parts": [{"functionResponse": {"name": "Lookup", "response": {"items": [1, 2]}}}, {"fileData": {"file_uri": "gs://a", "mime_type": "audio/mp3"}}]}
            ],
            "tools": [{"functionDeclarations": [
                {"name": "Lookup", "parametersJsonSchema": {"type": "OBJECT", "properties": {"q": {"type": "INTEGER", "minimum": 1.0}}}}
            ]}],
            "tool_config": {"function_calling_config": {"mode": "NONE"}},
            "generationConfig": {"maxOutputTokens": 512, "stopSequences": ["x", 1], "thinkingConfig": {"thinkingBudget": 2048}}
        }"#,
    );
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "model",
            "max_tokens",
            "messages",
            "metadata",
            "service_tier",
            "stop_sequences",
            "thinking",
            "tools",
            "tool_choice",
            "stream"
        ]
    );
    assert_eq!(out["max_tokens"], 512);
    assert_eq!(out["stop_sequences"], json!(["x", "1"]));
    assert_eq!(
        out["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert_eq!(out["tool_choice"], json!({"type": "none"}));
    assert_eq!(
        out["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "a\nb"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_gemini_0000000000000002", "name": "Lookup", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_gemini_0000000000000002", "content": "{\"items\":[1,2]}"},
                {"type": "text", "text": "File: gs://a (Type: audio/mp3)"}
            ]}
        ])
    );
    assert_eq!(
        out["tools"],
        json!([{
            "description": "",
            "input_schema": {
                "$schema": DRAFT_07_SCHEMA,
                "additionalProperties": false,
                "properties": {"q": {"minimum": 1, "type": "integer"}},
                "type": "object"
            },
            "name": "Lookup"
        }])
    );
}

#[test]
fn thinking_for_adaptive_models() {
    let thinking = |config: &str| {
        let out = convert(
            "claude-opus-5-5",
            &format!(r#"{{"generationConfig":{{"thinkingConfig":{config}}}}}"#),
        );
        (
            out.get("thinking").cloned(),
            out.get("output_config").cloned(),
        )
    };
    assert_eq!(
        thinking(r#"{"thinkingLevel":"HIGH"}"#),
        (
            Some(json!({"type": "adaptive"})),
            Some(json!({"effort": "high"}))
        )
    );
    assert_eq!(
        thinking(r#"{"thinkingBudget":0}"#).0,
        Some(json!({"type": "disabled"}))
    );
    assert_eq!(thinking(r#"{"thinkingLevel":" "}"#), (None, None));
}

#[test]
fn tool_choice_modes() {
    let choice = |config: &str| {
        convert(
            "claude-test",
            &format!(r#"{{"toolConfig":{{"functionCallingConfig":{config}}}}}"#),
        )
        .get("tool_choice")
        .cloned()
    };
    assert_eq!(choice(r#"{"mode":"AUTO"}"#), Some(json!({"type": "auto"})));
    assert_eq!(choice(r#"{"mode":"ANY"}"#), Some(json!({"type": "any"})));
    assert_eq!(
        choice(r#"{"mode":"ANY","allowed_function_names":["a","b"]}"#),
        Some(json!({"type": "any"}))
    );
    assert_eq!(
        choice(r#"{"mode":"ANY","allowed_function_names":["a"]}"#),
        Some(json!({"type": "tool", "name": "a"}))
    );
    assert_eq!(choice(r#"{"mode":"auto"}"#), None);
}
