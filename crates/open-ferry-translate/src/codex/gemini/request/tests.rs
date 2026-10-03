// Ported from CLIProxyAPI internal/translator/codex/gemini/codex_gemini_request_test.go
// and noop_optimization_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// Changed: TestSetCodexToolChoiceFromGeminiToolConfigReusesAutoChoice checks
// that the payload isn't copied; here it checks that the tool choice and its
// position are unchanged, since a `Map` is edited in place anyway.

use serde_json::{Value, json};

use super::*;

fn convert(request: &str) -> Value {
    let request: Value = serde_json::from_str(request).expect("test request is valid JSON");
    convert_gemini_request_to_codex("gpt-5.1-codex", &request)
}

fn input(out: &Value) -> &[Value] {
    out["input"].as_array().expect("input is an array")
}

#[test]
fn preserves_custom_call_ids() {
    for (call_field, want) in [
        (r#""id":"call_gateway_id""#, "call_gateway_id"),
        (
            r#""call_id":"call_gateway_call_id""#,
            "call_gateway_call_id",
        ),
    ] {
        let out = convert(&format!(
            r#"{{
                "contents": [
                    {{"role": "model", "parts": [
                        {{"functionCall": {{"name": "lookup", {call_field}, "args": {{"query": "status"}}}}}}
                    ]}},
                    {{"role": "user", "parts": [
                        {{"functionResponse": {{"name": "lookup", {call_field}, "response": {{"result": "ok"}}}}}}
                    ]}}
                ]
            }}"#
        ));
        assert_eq!(out["input"][0]["call_id"], want, "{out}");
        assert_eq!(out["input"][1]["call_id"], want, "{out}");
    }
}

#[test]
fn accepts_inline_data() {
    let out = convert(
        r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}]}"#,
    );
    assert_eq!(out["input"][0]["content"][0]["type"], "input_image");
    assert_eq!(
        out["input"][0]["content"][0]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
}

#[test]
fn splits_non_image_inline_data_by_mime() {
    let out = convert(
        r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}},{"inlineData":{"mimeType":"video/mp4","data":"AAAAIGZ0eXA="}},{"inlineData":{"mimeType":"application/pdf","data":"JVBERi0="}}]}]}"#,
    );
    assert_eq!(out["input"][0]["content"][0]["type"], "input_audio");
    assert_eq!(
        out["input"][0]["content"][0]["input_audio"]["format"],
        "wav"
    );
    assert_eq!(out["input"][1]["content"][0]["type"], "input_file");
    assert_eq!(out["input"][1]["content"][0]["filename"], "video");
    assert_eq!(out["input"][2]["content"][0]["type"], "input_file");
    assert_eq!(out["input"][2]["content"][0]["filename"], "document.pdf");
}

#[test]
fn drops_hidden_thought_parts() {
    // A turn of only a thought.
    let out = convert(
        r#"{"contents":[
            {"role":"model","parts":[{"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"}]},
            {"role":"user","parts":[{"text":"continue"}]}
        ]}"#,
    );
    let items = input(&out);
    assert_eq!(items.len(), 1, "{out}");
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["content"][0]["text"], "continue");

    // A thought beside visible text.
    let out = convert(
        r#"{"contents":[{"role":"model","parts":[
            {"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"},
            {"text":"visible answer"}
        ]}]}"#,
    );
    let items = input(&out);
    assert_eq!(items.len(), 1, "{out}");
    assert_eq!(items[0]["content"][0]["type"], "output_text");
    assert_eq!(items[0]["content"][0]["text"], "visible answer");
}

#[test]
fn deterministic_call_ids() {
    let raw = r#"{"contents": [
        {"role": "model", "parts": [{"functionCall": {"name": "first_tool", "args": {"q": "one"}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "first_tool", "response": {"result": "ok1"}}}]},
        {"role": "model", "parts": [{"functionCall": {"name": "second_tool", "args": {"q": "two"}}}]},
        {"role": "user", "parts": [{"functionResponse": {"name": "second_tool", "response": {"result": "ok2"}}}]}
    ]}"#;
    let first = convert(raw);
    assert_eq!(first, convert(raw));
    let ids: Vec<&Value> = input(&first).iter().map(|item| &item["call_id"]).collect();
    assert_eq!(
        ids,
        [
            "call_gemini_0000000000000001",
            "call_gemini_0000000000000001",
            "call_gemini_0000000000000002",
            "call_gemini_0000000000000002"
        ]
    );
}

#[test]
fn clean_parameters_preserves_canonical_schema() {
    let input = json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false});
    let output = clean_parameters(&input);
    assert_eq!(output.to_string(), input.to_string());
}

#[test]
fn set_tool_choice_reuses_auto_choice() {
    let mut out = Map::new();
    out.insert("tool_choice".into(), "auto".into());
    out.insert("input".into(), json!([]));
    set_tool_choice(&mut out, &json!({"mode":"AUTO"}));
    assert_eq!(
        Value::Object(out).to_string(),
        r#"{"tool_choice":"auto","input":[]}"#
    );
}

#[test]
fn clean_parameters_normalizes_schema() {
    let output =
        clean_parameters(&json!({"type":"object","$schema":"draft","additionalProperties":true}));
    assert!(output.get("$schema").is_none());
    assert_eq!(output["additionalProperties"], false);
}

#[test]
fn clean_parameters_follows_sjson_for_other_values() {
    assert_eq!(
        clean_parameters(&json!("x")),
        json!({"additionalProperties": false})
    );
    assert_eq!(
        clean_parameters(&json!(null)),
        json!({"additionalProperties": false})
    );
    assert_eq!(clean_parameters(&json!([1])), json!([1]));
}

#[test]
fn builds_the_whole_request_in_upstream_order() {
    let out = convert(
        r#"{
            "service_tier": " Fast ",
            "system_instruction": {"parts": [{"text": "be brief"}, {"thought": true, "text": "x"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}, {"fileData": {"fileUri": "gs://a/b", "mimeType": "audio/mp3"}}]},
                {"role": "model", "parts": [{"functionCall": {"name": "Lookup", "args": {"q": 1}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "Lookup", "response": {"items": [1, 2]}}}]}
            ],
            "tools": [{"functionDeclarations": [
                {"name": "Lookup", "description": "Find", "parameters": {"$schema": "x", "type": "OBJECT", "properties": {"q": {"type": "INTEGER"}}}}
            ]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["Lookup"]}},
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 1024}}
        }"#,
    );
    assert_eq!(
        out,
        json!({
            "model": "gpt-5.1-codex",
            "instructions": "",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "be brief"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "File: gs://a/b (Type: audio/mp3)"}]},
                {"type": "function_call", "name": "Lookup", "arguments": "{\"q\":1}", "call_id": "call_gemini_0000000000000001"},
                {"type": "function_call_output", "output": "{\"items\":[1,2]}", "call_id": "call_gemini_0000000000000001"}
            ],
            "service_tier": "priority",
            "tool_choice": {"type": "function", "name": "Lookup"},
            "tools": [{
                "type": "function",
                "name": "Lookup",
                "description": "Find",
                "parameters": {"type": "object", "properties": {"q": {"type": "integer"}}, "additionalProperties": false},
                "strict": false
            }],
            "parallel_tool_calls": true,
            "reasoning": {"effort": "low"},
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"]
        })
    );
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "model",
            "instructions",
            "input",
            "service_tier",
            "tool_choice",
            "tools",
            "parallel_tool_calls",
            "reasoning",
            "stream",
            "store",
            "include"
        ]
    );
}

#[test]
fn reasoning_effort_sources() {
    let effort = |config: &str| {
        convert(&format!(r#"{{"generationConfig":{config}}}"#))["reasoning"]["effort"].clone()
    };
    assert_eq!(effort(r#"{"thinkingLevel":" HIGH "}"#), "high");
    assert_eq!(effort(r#"{"thinking_level":"low"}"#), "low");
    // A blank level doesn't fall back to the thinking config.
    assert_eq!(
        effort(r#"{"thinkingLevel":" ","thinkingConfig":{"thinkingLevel":"high"}}"#),
        "medium"
    );
    assert_eq!(
        effort(r#"{"thinkingConfig":{"thinking_level":"Minimal"}}"#),
        "minimal"
    );
    assert_eq!(effort(r#"{"thinkingConfig":{"thinkingBudget":0}}"#), "none");
    assert_eq!(effort(r#"{"thinkingConfig":"x"}"#), "medium");
    assert_eq!(convert("{}")["reasoning"]["effort"], "medium");
}

#[test]
fn long_tool_names_are_shortened_uniquely() {
    let long = format!("mcp__server__{}", "a".repeat(70));
    let other = "b".repeat(70);
    let out = convert(&format!(
        r#"{{"tools":[{{"functionDeclarations":[{{"name":"{long}"}},{{"name":"{other}"}},{{"name":"{}"}}]}}],
            "contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"{long}"}}}}]}}]}}"#,
        &other[..64]
    ));
    let names: Vec<&Value> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| &tool["name"])
        .collect();
    let mcp = format!("mcp__{}", "a".repeat(59));
    let b64 = "b".repeat(64);
    let b62 = format!("{}_1", "b".repeat(62));
    assert_eq!(names, [mcp.as_str(), b64.as_str(), b62.as_str()]);
    assert_eq!(out["input"][0]["name"], mcp);
    assert!(out["input"][0].get("arguments").is_none());
}

#[test]
fn tool_choice_modes() {
    let choice = |config: &str| {
        convert(&format!(
            r#"{{"toolConfig":{{"functionCallingConfig":{config}}}}}"#
        ))
        .get("tool_choice")
        .cloned()
    };
    assert_eq!(choice(r#"{"mode":"NONE"}"#), Some(json!("none")));
    assert_eq!(choice(r#"{"mode":"AUTO"}"#), Some(json!("auto")));
    assert_eq!(choice(r#"{"mode":"ANY"}"#), Some(json!("required")));
    assert_eq!(
        choice(r#"{"mode":"ANY","allowedFunctionNames":["a","b"]}"#),
        Some(json!("required"))
    );
    assert_eq!(choice(r#"{"mode":"any"}"#), None);
}
