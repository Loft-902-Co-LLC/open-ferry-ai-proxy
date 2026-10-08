// Ported from CLIProxyAPI internal/translator/gemini/interactions/interactions_gemini_common_test.go
// and interactions_gemini_file_data_test.go (v8.0.15, MIT), and
// interactions_gemini_user_turn_test.go, interactions_gemini_uri_test.go and
// interactions_gemini_instruction_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The request translators' tests: upstream's request tests are here, and
//! its response tests in `response/tests.rs`. All of them are ported.
//! Upstream's tests through the registry call the translator here and check
//! that the registry gives the same refusal. The tests marked "Not
//! upstream's" are new.

use serde_json::{Value, json};

use super::*;
use crate::gemini::interactions::response::convert_gemini_response_to_interactions_non_stream;
use crate::signature::validate_gemini_function_call_pairing;

const MODEL: &str = "gemini-3.5-flash";

/// The translator's body, which must not be refused.
#[track_caller]
fn convert_interactions_request_to_gemini(model: &str, body: &Value, stream: bool) -> Value {
    let (out, err) = super::convert_interactions_request_to_gemini(model, body, stream);
    assert_eq!(err, None, "{out}");
    out
}

/// The translator's body, which must not be refused.
#[track_caller]
fn convert_gemini_request_to_interactions(model: &str, body: &Value, stream: bool) -> Value {
    let (out, err) = super::convert_gemini_request_to_interactions(model, body, stream);
    assert_eq!(err, None, "{out}");
    out
}

fn to_gemini(body: &str) -> Value {
    let body: Value = serde_json::from_str(body).expect("test request is JSON");
    convert_interactions_request_to_gemini(MODEL, &body, false)
}

fn to_interactions(body: &str) -> Value {
    let body: Value = serde_json::from_str(body).expect("test request is JSON");
    convert_gemini_request_to_interactions(MODEL, &body, false)
}

/// The value at a gjson-like dotted path, where a number indexes an array.
fn get<'v>(value: &'v Value, at: &str) -> Option<&'v Value> {
    at.split('.').try_fold(value, |value, key| match value {
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        Value::Object(fields) => fields.get(key),
        _ => None,
    })
}

/// gjson's `String()` of the value at `at`.
fn text(value: &Value, at: &str) -> String {
    str_of(get(value, at)).into_owned()
}

/// The array at `at`, or nothing.
fn array<'v>(value: &'v Value, at: &str) -> &'v [Value] {
    get(value, at)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

// TestConvertInteractionsRequestToGeminiStringInput.
#[test]
fn string_input() {
    let out = to_gemini(r#"{"model":"gemini-3.5-flash","input":"hello"}"#);
    assert_eq!(text(&out, "contents.0.role"), "user");
    assert_eq!(text(&out, "contents.0.parts.0.text"), "hello");
}

// TestConvertInteractionsRequestToGeminiSystemAndGenerationConfig.
#[test]
fn system_and_generation_config() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","system_instruction":{"text":"be brief"},"generation_config":{"max_output_tokens":32,"top_p":0.8},"input":"hi"}"#,
    );
    assert_eq!(text(&out, "systemInstruction.parts.0.text"), "be brief");
    assert_eq!(
        get(&out, "generationConfig.maxOutputTokens"),
        Some(&json!(32))
    );
    assert_eq!(get(&out, "generationConfig.topP"), Some(&json!(0.8)));
}

// TestConvertInteractionsRequestToGeminiStringSystemInstruction.
#[test]
fn string_system_instruction() {
    let out =
        to_gemini(r#"{"model":"gemini-3.5-flash","system_instruction":"be brief","input":"hi"}"#);
    assert_eq!(text(&out, "systemInstruction.parts.0.text"), "be brief");
}

// TestConvertGeminiRequestToInteractionsStringSystemInstruction.
#[test]
fn gemini_system_instruction_becomes_a_string() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","systemInstruction":{"parts":[{"text":"be brief"},{"text":"answer directly"}]},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    );
    assert_eq!(
        out.get("system_instruction"),
        Some(&json!("be brief\nanswer directly"))
    );
}

// TestConvertInteractionsRequestToGeminiTurnInput.
#[test]
fn turn_input() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":{"role":"user","steps":[{"type":"user_input","content":[{"text":"hi"}]}]}}"#,
    );
    assert_eq!(text(&out, "contents.0.parts.0.text"), "hi");
}

// TestConvertInteractionsRequestToGeminiTurnArrayInput.
#[test]
fn turn_array_input() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"role":"user","steps":[{"type":"user_input","content":[{"text":"hi"}]}]},{"role":"assistant","steps":[{"type":"model_output","content":[{"text":"ok"}]}]}]}"#,
    );
    assert_eq!(text(&out, "contents.0.role"), "user");
    assert_eq!(text(&out, "contents.0.parts.0.text"), "hi");
    assert_eq!(text(&out, "contents.1.role"), "model");
    assert_eq!(text(&out, "contents.1.parts.0.text"), "ok");
}

// TestConvertInteractionsRequestToGeminiPreservesExpressibleTopLevelFields.
#[test]
fn preserves_expressible_top_level_fields() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","tool_choice":{"type":"function","function":{"name":"lookup"}},"response_modalities":["text","image"],"service_tier":"priority","input":"hi"}"#,
    );
    assert_eq!(text(&out, "toolConfig.functionCallingConfig.mode"), "ANY");
    assert_eq!(
        text(
            &out,
            "toolConfig.functionCallingConfig.allowedFunctionNames.0"
        ),
        "lookup"
    );
    assert_eq!(text(&out, "generationConfig.responseModalities.0"), "TEXT");
    assert_eq!(text(&out, "generationConfig.responseModalities.1"), "IMAGE");
    assert_eq!(text(&out, "service_tier"), "priority");
}

// TestConvertInteractionsRequestToGeminiContentInput.
#[test]
fn content_input() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":{"role":"user","parts":[{"text":"hi"}]}}"#,
    );
    assert_eq!(text(&out, "contents.0.role"), "user");
    assert_eq!(text(&out, "contents.0.parts.0.text"), "hi");
}

// TestConvertInteractionsRequestToGeminiContentArrayInput.
#[test]
fn content_array_input() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"role":"user","parts":[{"text":"hi"}]},{"role":"assistant","parts":[{"text":"ok"}]}]}"#,
    );
    assert_eq!(text(&out, "contents.0.role"), "user");
    assert_eq!(text(&out, "contents.0.parts.0.text"), "hi");
    assert_eq!(text(&out, "contents.1.role"), "model");
    assert_eq!(text(&out, "contents.1.parts.0.text"), "ok");
}

// TestConvertInteractionsRequestToGeminiImageContent.
#[test]
fn image_content() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
    );
    assert_eq!(
        text(&out, "contents.0.parts.0.inlineData.mimeType"),
        "image/png"
    );
    assert_eq!(text(&out, "contents.0.parts.0.inlineData.data"), "aGVsbG8=");
}

// TestConvertInteractionsRequestToGeminiModelOutputTypedContent.
#[test]
fn model_output_typed_content() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"type":"model_output","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="},{"type":"document","mime_type":"application/pdf","file_uri":"gs://bucket/doc.pdf"}]}]}"#,
    );
    assert_eq!(text(&out, "contents.0.role"), "model");
    assert_eq!(
        text(&out, "contents.0.parts.0.inlineData.mimeType"),
        "image/png"
    );
    assert_eq!(text(&out, "contents.0.parts.0.inlineData.data"), "aGVsbG8=");
    assert_eq!(
        text(&out, "contents.0.parts.1.fileData.mimeType"),
        "application/pdf"
    );
    assert_eq!(
        text(&out, "contents.0.parts.1.fileData.fileUri"),
        "gs://bucket/doc.pdf"
    );
}

// TestConvertInteractionsRequestToGeminiThoughtTypedContent.
#[test]
fn thought_typed_content() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"type":"thought","content":[{"type":"text","text":"thinking"},{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="}]}]}"#,
    );
    assert_eq!(text(&out, "contents.0.parts.0.text"), "thinking");
    assert_eq!(get(&out, "contents.0.parts.0.thought"), Some(&json!(true)));
    assert_eq!(
        text(&out, "contents.0.parts.1.inlineData.mimeType"),
        "audio/wav"
    );
}

// TestConvertInteractionsRequestToGeminiGenerationConfigAllFields.
#[test]
fn generation_config_all_fields() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","generation_config":{"max_output_tokens":32,"response_schema":{"type":"object"},"seed":42,"thinking_config":{"thinking_budget":1024,"include_thoughts":true},"context_window_compression":{"trigger_tokens":1000}},"input":"hi"}"#,
    );
    assert_eq!(
        get(&out, "generationConfig.maxOutputTokens"),
        Some(&json!(32))
    );
    assert_eq!(text(&out, "generationConfig.responseSchema.type"), "object");
    assert_eq!(get(&out, "generationConfig.seed"), Some(&json!(42)));
    assert_eq!(
        get(&out, "generationConfig.thinkingConfig.thinkingBudget"),
        Some(&json!(1024))
    );
    assert_eq!(
        get(&out, "generationConfig.thinkingConfig.includeThoughts"),
        Some(&json!(true))
    );
    assert_eq!(
        get(
            &out,
            "generationConfig.contextWindowCompression.triggerTokens"
        ),
        Some(&json!(1000))
    );
}

// TestConvertInteractionsRequestToGeminiGenerationConfigProtocolFields.
#[test]
fn generation_config_protocol_fields() {
    let body: Value = serde_json::from_str(
        r#"{"model":"gemini-3.5-flash","generation_config":{"tool_choice":"auto","thinking_level":"high","thinking_summaries":"auto"},"stream":true,"input":"hi"}"#,
    )
    .expect("test request is JSON");
    let out = convert_interactions_request_to_gemini(MODEL, &body, true);
    for at in [
        "stream",
        "generationConfig.toolChoice",
        "generationConfig.thinkingLevel",
        "generationConfig.thinkingSummaries",
    ] {
        assert_eq!(get(&out, at), None, "{at} in {out}");
    }
    assert_eq!(text(&out, "toolConfig.functionCallingConfig.mode"), "AUTO");
    assert_eq!(
        text(&out, "generationConfig.thinkingConfig.thinkingLevel"),
        "high"
    );
    assert_eq!(
        get(&out, "generationConfig.thinkingConfig.includeThoughts"),
        Some(&json!(true))
    );
}

// TestConvertGeminiRequestToInteractionsFunctionCall.
#[test]
fn gemini_function_call() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"ok":true}}}]}]}"#,
    );
    assert_eq!(text(&out, "input.0.type"), "function_call");
    assert_eq!(text(&out, "input.0.name"), "lookup");
    assert_eq!(text(&out, "input.1.type"), "function_result");
}

// TestConvertGeminiRequestToInteractionsTextContentType.
#[test]
fn gemini_text_content_type() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    );
    assert_eq!(text(&out, "input.0.content.0.type"), "text");
    assert_eq!(text(&out, "input.0.content.0.text"), "hi");
}

// TestConvertGeminiRequestToInteractionsMultimodal.
#[test]
fn gemini_multimodal() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"aGVsbG8="}}]}]}"#,
    );
    assert_eq!(text(&out, "input.0.type"), "user_input");
    assert_eq!(text(&out, "input.0.content.0.type"), "audio");
    assert_eq!(text(&out, "input.0.content.0.mime_type"), "audio/wav");
}

// TestConvertGeminiRequestToInteractionsThought.
#[test]
fn gemini_thought() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"text":"thinking","thought":true}]}]}"#,
    );
    assert_eq!(text(&out, "input.0.type"), "thought");
}

// TestConvertInteractionsRequestToGeminiTurnWithModelRole.
#[test]
fn turn_with_model_role() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":{"role":"model","steps":[{"type":"user_input","content":[{"text":"hi"}]},{"type":"model_output","content":[{"text":"ok"}]}]}}"#,
    );
    assert_eq!(text(&out, "contents.0.role"), "model");
    assert_eq!(text(&out, "contents.1.role"), "model");
}

// TestConvertInteractionsRequestToGeminiGenerationConfigPreservesLargeIntegers.
#[test]
fn generation_config_preserves_large_integers() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","generation_config":{"max_output_tokens":32,"large_identity":9223372036854775807},"input":"hi"}"#,
    );
    assert_eq!(
        get(&out, "generationConfig")
            .map(Value::to_string)
            .as_deref(),
        Some(r#"{"maxOutputTokens":32,"largeIdentity":9223372036854775807}"#)
    );
}

// TestConvertInteractionsRequestToGeminiFunctionCallPreservesCallID.
#[test]
fn function_call_preserves_call_id() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"type":"function_call","name":"lookup","call_id":"call_1","arguments":{"q":"x"}}]}"#,
    );
    assert_eq!(
        get(&out, "contents.0.parts.0"),
        Some(
            &json!({ "functionCall": { "name": "lookup", "args": { "q": "x" }, "id": "call_1" } })
        )
    );
}

// TestConvertInteractionsRequestToGeminiFunctionResultPreservesCallID.
#[test]
fn function_result_preserves_call_id() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"type":"function_result","name":"lookup","call_id":"call_1","result":{"ok":true}}]}"#,
    );
    assert_eq!(
        text(&out, "contents.0.parts.0.functionResponse.id"),
        "call_1"
    );
    assert_eq!(
        text(&out, "contents.0.parts.0.functionResponse.name"),
        "lookup"
    );
}

// TestConvertGeminiRequestToInteractionsFunctionCallPreservesID.
#[test]
fn gemini_function_call_preserves_id() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","id":"call_1","response":{"ok":true}}}]}]}"#,
    );
    assert_eq!(text(&out, "input.0.call_id"), "call_1");
    assert_eq!(text(&out, "input.1.call_id"), "call_1");
}

// TestConvertGeminiRequestToInteractionsFunctionCallPreservesCallID.
#[test]
fn gemini_function_call_preserves_call_id() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","call_id":"call_request_1","args":{"q":"x"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","call_id":"call_request_1","response":{"ok":true}}}]}]}"#,
    );
    assert_eq!(text(&out, "input.0.call_id"), "call_request_1");
    assert_eq!(text(&out, "input.1.call_id"), "call_request_1");
}

// TestConvertGeminiRequestToInteractionsGenerationConfig.
#[test]
fn gemini_generation_config() {
    let out = to_interactions(
        r#"{"model":"gemini-3.5-flash","generationConfig":{"maxOutputTokens":32,"topP":0.8,"thinkingConfig":{"thinkingBudget":1024}},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
    );
    assert_eq!(
        get(&out, "generation_config.max_output_tokens"),
        Some(&json!(32))
    );
    assert_eq!(get(&out, "generation_config.top_p"), Some(&json!(0.8)));
    assert_eq!(
        get(&out, "generation_config.thinking_config.thinking_budget"),
        Some(&json!(1024))
    );
}

// TestConvertInteractionsRequestToGeminiBuiltinTools: url_context only,
// code_execution only and google_search only.
#[test]
fn builtin_tools() {
    for (kind, input, gemini) in [
        ("url_context", "read url", "urlContext"),
        ("code_execution", "execute code", "codeExecution"),
        ("google_search", "search web", "googleSearch"),
    ] {
        let body = json!({ "model": MODEL, "input": input, "tools": [{ "type": kind }] });
        let out = convert_interactions_request_to_gemini(MODEL, &body, false);
        let tools = array(&out, "tools");
        assert_eq!(tools.len(), 1, "{out}");
        assert!(tools[0].get(gemini).is_some(), "{out}");
        assert_eq!(tools[0].get("type"), None, "{out}");
    }
}

// TestConvertInteractionsRequestToGeminiBuiltinTools: reverse gemini to
// interactions builtin tools.
#[test]
fn builtin_tools_reverse() {
    let out = to_interactions(
        r#"{
            "model":"gemini-3.5-flash",
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"urlContext":{}},{"codeExecution":{}},{"googleSearch":{}}]
        }"#,
    );
    let tools = array(&out, "tools");
    assert_eq!(tools.len(), 3, "{out}");
    assert_eq!(text(&tools[0], "type"), "url_context");
    assert_eq!(text(&tools[1], "type"), "code_execution");
    assert_eq!(text(&tools[2], "type"), "google_search");
}

// TestConvertInteractionsRequestToGeminiBuiltinTools: reverse gemini to
// interactions with options preserved.
#[test]
fn builtin_tools_reverse_options() {
    let out = to_interactions(
        r#"{
            "model":"gemini-3.5-flash",
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"googleSearch":{"mode":"search"}},{"codeExecution":{"sandbox":true}}]
        }"#,
    );
    let tools = array(&out, "tools");
    assert_eq!(tools.len(), 2, "{out}");
    assert_eq!(text(&tools[0], "google_search.mode"), "search");
    assert_eq!(get(&tools[1], "code_execution.sandbox"), Some(&json!(true)));
}

// TestConvertInteractionsRequestToGeminiBuiltinTools: nested parameters and
// aliases preserved.
#[test]
fn builtin_tools_options_and_aliases() {
    let out = to_gemini(
        r#"{
            "model":"gemini-3.5-flash",
            "input":"test options",
            "tools":[
                {"type":"url_context","url_context":{"max_urls":3}},
                {"code_execution":{"environment":"sandbox"}},
                {"type":"web_search","google_search":{"mode":"search"}}
            ]
        }"#,
    );
    let tools = array(&out, "tools");
    assert_eq!(tools.len(), 3, "{out}");
    assert_eq!(get(&tools[0], "urlContext.max_urls"), Some(&json!(3)));
    assert_eq!(text(&tools[1], "codeExecution.environment"), "sandbox");
    assert_eq!(text(&tools[2], "googleSearch.mode"), "search");
}

// TestConvertInteractionsRequestToGeminiBuiltinTools: native composite tools
// preserved without truncation.
#[test]
fn builtin_tools_native_composite() {
    let out = to_gemini(
        r#"{
            "model":"gemini-3.5-flash",
            "input":"native composite tools",
            "tools":[{"googleSearch":{},"urlContext":{}}]
        }"#,
    );
    let tools = array(&out, "tools");
    assert_eq!(tools.len(), 1, "{out}");
    assert!(tools[0].get("googleSearch").is_some(), "{out}");
    assert!(tools[0].get("urlContext").is_some(), "{out}");

    let out = to_interactions(
        r#"{
            "model":"gemini-3.5-flash",
            "contents":[{"role":"user","parts":[{"text":"hello"}]}],
            "tools":[{"googleSearch":{},"urlContext":{}}]
        }"#,
    );
    let types: Vec<String> = array(&out, "tools")
        .iter()
        .map(|tool| text(tool, "type"))
        .collect();
    assert_eq!(types.len(), 2, "{out}");
    assert!(types.iter().any(|kind| kind == "url_context"), "{out}");
    assert!(types.iter().any(|kind| kind == "google_search"), "{out}");
}

// TestConvertInteractionsRequestToGeminiBuiltinTools: unrecognized tool
// retained and not silently dropped.
#[test]
fn builtin_tools_unrecognized_retained() {
    let out = to_gemini(
        r#"{
            "model":"gemini-3.5-flash",
            "input":"unrecognized tool",
            "tools":[{"type":"file_search","file_search":{"max_results":5}}]
        }"#,
    );
    let tools = array(&out, "tools");
    assert_eq!(tools.len(), 1, "{out}");
    assert_eq!(text(&tools[0], "type"), "file_search");
}

// TestConvertInteractionsRequestToGemini_FunctionResponseJSONRef.
#[test]
fn function_response_json_ref() {
    let out = to_gemini(
        r##"{
            "model": "gemini-3.5-flash",
            "input": [
                {
                    "type": "function_result",
                    "name": "lookup",
                    "call_id": "call_1",
                    "result": {
                        "schema": {
                            "$ref": "#/components/schemas/ErrorModel"
                        }
                    }
                }
            ]
        }"##,
    );
    let result = get(&out, "contents.0.parts.0.functionResponse.response.result");
    let Some(Value::String(result)) = result else {
        panic!("result is not a string: {out}");
    };
    assert!(result.contains("#/components/schemas/ErrorModel"), "{out}");
}

// TestConvertInteractionsRequestToGemini_ParallelToolCallsHistory.
#[test]
fn parallel_tool_calls_history() {
    let out = to_gemini(
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "user_input", "content": [{"type": "text", "text": "run tools"}]},
                {"type": "thought", "signature": "sig_turn1"},
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {"a": 1}},
                {"type": "function_call", "name": "f2", "call_id": "c2", "arguments": {"b": 2}},
                {"type": "function_call", "name": "f3", "call_id": "c3", "arguments": {"c": 3}},
                {"type": "function_result", "name": "f1", "call_id": "c1", "result": {"r": 1}},
                {"type": "function_result", "name": "f2", "call_id": "c2", "result": {"r": 2}},
                {"type": "function_result", "name": "f3", "call_id": "c3", "result": {"r": 3}}
            ]
        }"#,
    );
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 3, "{out}");
    assert!(validate_gemini_function_call_pairing(&out).is_ok(), "{out}");
    let model_parts = array(&contents[1], "parts");
    assert_eq!(model_parts.len(), 3, "{out}");
    assert_eq!(text(&model_parts[0], "thoughtSignature"), "sig_turn1");
    assert_eq!(array(&contents[2], "parts").len(), 3, "{out}");
}

// TestConvertInteractionsRequestToGemini_ThoughtSummaryAndSignature.
#[test]
fn thought_summary_and_signature() {
    let out = to_gemini(
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "thought", "summary": [{"type": "text", "text": "my thinking"}], "signature": "sig_thought"},
                {"type": "model_output", "content": [{"type": "text", "text": "my answer"}]}
            ]
        }"#,
    );
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 1, "{out}");
    let parts = array(&contents[0], "parts");
    assert!(
        parts
            .iter()
            .any(|part| part.get("thought") == Some(&json!(true))
                && text(part, "text").contains("my thinking")),
        "{out}"
    );
    assert!(
        parts
            .iter()
            .any(|part| text(part, "thoughtSignature") == "sig_thought"),
        "{out}"
    );
}

// TestConvertGeminiRequestToInteractions_SignedFunctionCallPreserved.
#[test]
fn gemini_signed_function_call_preserved() {
    let out = to_interactions(
        r#"{
            "model": "gemini-3.5-flash",
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "lookup", "id": "call_1", "args": {"q": "x"}}, "thoughtSignature": "sig_fc_pres"}
                    ]
                }
            ]
        }"#,
    );
    let steps = array(&out, "input");
    assert_eq!(steps.len(), 2, "{out}");
    assert_eq!(text(&steps[0], "type"), "thought");
    assert_eq!(text(&steps[0], "signature"), "sig_fc_pres");
    assert_eq!(text(&steps[1], "type"), "function_call");
    assert_eq!(text(&steps[1], "name"), "lookup");
}

// TestConvertInteractionsRequestToGemini_InterleavedModelTurnSteps.
#[test]
fn interleaved_model_turn_steps() {
    let out = to_gemini(
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {"a": 1}, "signature": "sig1"},
                {"type": "model_output", "content": [{"type": "text", "text": "explanation"}]},
                {"type": "function_call", "name": "f2", "call_id": "c2", "arguments": {"b": 2}},
                {"type": "function_result", "name": "f1", "call_id": "c1", "result": {"r": 1}},
                {"type": "function_result", "name": "f2", "call_id": "c2", "result": {"r": 2}}
            ]
        }"#,
    );
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 2, "{out}");
    assert!(validate_gemini_function_call_pairing(&out).is_ok(), "{out}");
    let model_parts = array(&contents[0], "parts");
    assert_eq!(model_parts.len(), 3, "{out}");
    assert_eq!(text(&model_parts[0], "thoughtSignature"), "sig1");
    assert_eq!(array(&contents[1], "parts").len(), 2, "{out}");
}

// TestConvertInteractionsRequestToGemini_ResponseToRequestRoundTrip.
#[test]
fn response_to_request_round_trip() {
    let response = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "lookup", "id": "call_1", "args": {"q": "x"}}, "thoughtSignature": "sig_rt_fc"},
                        {"functionCall": {"name": "search", "id": "call_2", "args": {"q": "y"}}}
                    ]
                }
            }]
        }"#,
    );
    let mut input =
        vec![json!({ "type": "user_input", "content": [{ "type": "text", "text": "hello" }] })];
    input.extend(array(&response, "steps").iter().cloned());
    input.push(json!({ "type": "function_result", "name": "lookup", "call_id": "call_1", "result": { "ok": true } }));
    input.push(json!({ "type": "function_result", "name": "search", "call_id": "call_2", "result": { "found": true } }));
    let body = json!({ "model": MODEL, "input": input });
    let out = convert_interactions_request_to_gemini(MODEL, &body, false);
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 3, "{out}");
    assert!(validate_gemini_function_call_pairing(&out).is_ok(), "{out}");
    let model_parts = array(&contents[1], "parts");
    assert_eq!(model_parts.len(), 2, "{out}");
    assert_eq!(text(&model_parts[0], "thoughtSignature"), "sig_rt_fc");
}

// TestConvertInteractionsRequestToGemini_MultipleSignaturesPreserved.
#[test]
fn multiple_signatures_preserved() {
    let out = to_gemini(
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "thought", "summary": [{"type": "text", "text": "thought 1"}], "signature": "sig_thought_1"},
                {"type": "thought", "signature": "sig_fc_1"},
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {}}
            ]
        }"#,
    );
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 1, "{out}");
    let parts = array(&contents[0], "parts");
    assert!(
        parts
            .iter()
            .any(|part| text(part, "thoughtSignature") == "sig_thought_1"),
        "{out}"
    );
    assert!(
        parts.iter().any(|part| part.get("functionCall").is_some()
            && text(part, "thoughtSignature") == "sig_fc_1"),
        "{out}"
    );
}

// TestConvertInteractionsRequestToGemini_TrailingSignatureRoundTrip.
#[test]
fn trailing_signature_round_trip() {
    let response = convert_gemini_response_to_interactions_non_stream(
        MODEL,
        br#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "answer"},
                        {"text": "", "thoughtSignature": "sig_trailing_rt"}
                    ]
                }
            }]
        }"#,
    );
    let steps = get(&response, "steps").cloned().unwrap_or(Value::Null);
    let body = json!({ "model": MODEL, "input": steps });
    let out = convert_interactions_request_to_gemini(MODEL, &body, false);
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 1, "{out}");
    let parts = array(&contents[0], "parts");
    assert!(
        parts.iter().any(|part| text(part, "text") == "answer"),
        "{out}"
    );
    assert!(
        parts
            .iter()
            .any(|part| text(part, "thoughtSignature") == "sig_trailing_rt"),
        "{out}"
    );
}

// TestConvertInteractionsRequestToGemini_ExplicitSignatureClearsPending.
#[test]
fn explicit_signature_clears_pending() {
    let out = to_gemini(
        r#"{
            "model": "gemini-3.5-flash",
            "input": [
                {"type": "thought", "signature": "sig_shared"},
                {"type": "function_call", "name": "f1", "call_id": "c1", "arguments": {}, "signature": "sig_shared"},
                {"type": "function_call", "name": "f2", "call_id": "c2", "arguments": {}}
            ]
        }"#,
    );
    let contents = array(&out, "contents");
    assert_eq!(contents.len(), 1, "{out}");
    let parts = array(&contents[0], "parts");
    assert_eq!(parts.len(), 2, "{out}");
    assert_eq!(text(&parts[0], "thoughtSignature"), "sig_shared");
    assert_eq!(text(&parts[1], "thoughtSignature"), "");
}

// TestConvertInteractionsRequestToGeminiNormalizesOpenAIFileDataURL.
#[test]
fn normalizes_openai_file_data_url() {
    let out = to_gemini(
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"file","file":{"filename":"test.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}]}]}"#,
    );
    assert_eq!(
        text(&out, "contents.0.parts.0.inlineData.mimeType"),
        "application/pdf"
    );
    assert_eq!(
        text(&out, "contents.0.parts.0.inlineData.data"),
        "JVBERi0xLjQK"
    );
}

// Not upstream's: a whole request, checked against upstream's output.
#[test]
fn whole_request() {
    let body: Value = serde_json::from_str(
        r#"{"model":"m","system_instruction":"be brief","generation_config":{"temperature":0.2,"max_output_tokens":64,"thinking_level":"low","thinking_summaries":"auto","stop_sequences":["x"]},"response_modalities":["text"],"tools":[{"type":"function","name":"lookup","description":"d","parameters":{"type":"object"}},{"type":"google_search"}],"tool_choice":"any","service_tier":"flex","input":[{"type":"user_input","content":[{"type":"text","text":"hi"},{"type":"image","uri":"gs://b/i.png","mime_type":"image/png"}]},{"type":"thought","summary":[{"type":"text","text":"hmm"}],"signature":"sig_a"},{"type":"function_call","name":"lookup","call_id":"c1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"c1","result":"plain text"},{"type":"model_output","content":[{"type":"text","text":"done"}]}],"stream":true}"#,
    )
    .expect("test request is JSON");
    let out = convert_interactions_request_to_gemini(MODEL, &body, true);
    assert_eq!(
        out.to_string(),
        r#"{"model":"gemini-3.5-flash","contents":[{"role":"user","parts":[{"text":"hi"}]},{"role":"user","parts":[{"fileData":{"mimeType":"image/png","fileUri":"gs://b/i.png"}}]},{"role":"model","parts":[{"text":"hmm","thought":true},{"functionCall":{"name":"lookup","args":{"q":"x"},"id":"c1"},"thoughtSignature":"sig_a"}]},{"role":"user","parts":[{"functionResponse":{"name":"lookup","response":"plain text","id":"c1"}}]},{"role":"model","parts":[{"text":"done"}]}],"systemInstruction":{"parts":[{"text":"be brief"}]},"generationConfig":{"temperature":0.2,"maxOutputTokens":64,"stopSequences":["x"],"thinkingConfig":{"thinkingLevel":"low","includeThoughts":true},"responseModalities":["TEXT"]},"tools":[{"functionDeclarations":[{"description":"d","name":"lookup","parameters":{"type":"object"}}]},{"googleSearch":{}}],"toolConfig":{"functionCallingConfig":{"mode":"ANY"}},"service_tier":"flex"}"#
    );
}

// Not upstream's: a whole Gemini request, checked against upstream's output.
#[test]
fn whole_gemini_request() {
    let body: Value = serde_json::from_str(
        r#"{"model":"m","systemInstruction":{"parts":[{"text":"a"},{"text":"b"}]},"generationConfig":{"temperature":0.5,"maxOutputTokens":10,"thinkingConfig":{"thinkingLevel":"HIGH","includeThoughts":true},"responseMimeType":"application/json"},"tools":[{"functionDeclarations":[{"name":"f","description":"d","parameters":{"type":"object"}}]},{"googleSearch":{}}],"contents":[{"role":"user","parts":[{"text":"hi"},{"inlineData":{"mimeType":"image/png","data":"AAA="}},{"fileData":{"mimeType":"application/pdf","fileUri":"gs://x"}}]},{"role":"model","parts":[{"text":"think","thought":true},{"functionCall":{"name":"f","args":{"a":1},"id":"c1"},"thoughtSignature":"s1"}]},{"role":"user","parts":[{"functionResponse":{"name":"f","id":"c1","response":{"ok":true}}}]},{"role":"model","parts":[{"text":"done"},{"text":"","thoughtSignature":"s2"}]}]}"#,
    )
    .expect("test request is JSON");
    let out = convert_gemini_request_to_interactions(MODEL, &body, true);
    assert_eq!(
        out.to_string(),
        r#"{"model":"gemini-3.5-flash","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"AAA="}]},{"type":"user_input","content":[{"type":"document","uri":"gs://x","mime_type":"application/pdf"}]},{"type":"thought","content":[{"type":"text","text":"think"}]},{"type":"thought","signature":"s1"},{"type":"function_call","name":"f","arguments":{"a":1},"call_id":"c1"},{"type":"function_result","name":"f","result":{"ok":true},"call_id":"c1"},{"type":"model_output","content":[{"type":"text","text":"done"}]},{"type":"thought","signature":"s2"}],"system_instruction":"a\nb","generation_config":{"temperature":0.5,"max_output_tokens":10,"thinking_config":{"thinking_level":"HIGH","include_thoughts":true},"response_mime_type":"application/json","thinking_level":"high","thinking_summaries":"auto"},"tools":[{"description":"d","name":"f","parameters":{"type":"object"},"type":"function"},{"type":"google_search"}],"stream":true}"#
    );
}

// Not upstream's: upstream writes inline data with Go's `%q` and reads it
// back with gjson, which stops at the escapes JSON doesn't have.
#[test]
fn quoted_data_is_cut_at_escapes_gjson_does_not_read() {
    for (data, want) in [
        ("ab\u{7}cd", "ab"),
        ("ab\u{7f}cd", "ab"),
        ("ab\u{e0001}cd", "ab"),
        ("ab\u{1f600}cd", "ab\u{1f600}cd"),
        ("ab\u{85}cd", "ab\u{85}cd"),
        ("ab\tcd", "ab\tcd"),
    ] {
        let body = json!({
            "model": "m",
            "input": [{ "type": "user_input", "content": [{ "type": "image", "mime_type": "image/png", "data": data }] }],
        });
        let out = convert_interactions_request_to_gemini(MODEL, &body, false);
        assert_eq!(text(&out, "contents.0.parts.0.inlineData.data"), want);
    }
}

// The v8.0.20 tests: interactions_gemini_user_turn_test.go,
// interactions_gemini_uri_test.go and interactions_gemini_instruction_test.go.

/// `interactionsGeminiHistory`.
const INTERACTIONS_GEMINI_HISTORY: &str = r#"{"type":"user_input","content":[{"type":"text","text":"a"}]},{"type":"model_output","content":[{"type":"text","text":"b"}]}"#;

/// `geminiInstructionSteps`: developer and system steps in the shapes an
/// Interactions client can send. Each one carries the text "note".
const GEMINI_INSTRUCTION_STEPS: [(&str, &str); 7] = [
    (
        "developer user_input",
        r#"{"type":"user_input","role":"developer","content":[{"type":"text","text":"note"}]}"#,
    ),
    (
        "system user_input",
        r#"{"type":"user_input","role":"system","content":[{"type":"text","text":"note"}]}"#,
    ),
    (
        "developer string content",
        r#"{"type":"user_input","role":"developer","content":"note"}"#,
    ),
    (
        "bare system object",
        r#"{"role":"system","content":"note"}"#,
    ),
    ("bare system text", r#"{"type":"system","text":"note"}"#),
    (
        "developer native parts",
        r#"{"role":"developer","parts":[{"text":"note"}]}"#,
    ),
    (
        "developer wrapper",
        r#"{"role":"developer","steps":[{"type":"user_input","content":[{"type":"text","text":"note"}]}]}"#,
    ),
];

/// `geminiEmptiedStep`: a user step whose only part Gemini can't carry.
const GEMINI_EMPTIED_STEP: &str =
    r#"{"type":"user_input","content":[{"type":"document","uri":"gs://b/a.pdf"}]}"#;

/// The translator's body and refusal for `input`, after checking that the
/// registration gives the same refusal (`TranslateRequestEnvelope`).
fn checked_envelope(to_gemini: bool, input: &str) -> (Value, Option<UnsupportedPartError>) {
    let request: Value = serde_json::from_str(input).expect("test request is JSON");
    let (from, to) = if to_gemini {
        ("interactions", "gemini")
    } else {
        ("gemini", "interactions")
    };
    let registered = crate::registry::Registry::global().translate_request_checked(
        &from.into(),
        &to.into(),
        "m",
        request.clone(),
        false,
    );
    let (body, err) = if to_gemini {
        super::convert_interactions_request_to_gemini("m", &request, false)
    } else {
        super::convert_gemini_request_to_interactions("m", &request, false)
    };
    assert_eq!(registered.err(), err, "the registration's refusal");
    (body, err)
}

/// `interactionsToGeminiEnvelope`.
fn interactions_to_gemini_envelope(input: &str) -> (Value, Option<UnsupportedPartError>) {
    checked_envelope(true, input)
}

/// `interactionsFinalTurnToGemini`: two history turns followed by a final
/// user step holding `final_content`.
fn interactions_final_turn_to_gemini(final_content: &str) -> (Value, Option<UnsupportedPartError>) {
    interactions_to_gemini_envelope(&format!(
        r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{{"type":"user_input","content":[{final_content}]}}]}}"#
    ))
}

/// `geminiTurnToInteractions`: two history turns followed by a final user
/// turn holding `final_parts`. `extra` is spliced into the request root.
fn gemini_turn_to_interactions(
    extra: &str,
    final_parts: &str,
) -> (Value, Option<UnsupportedPartError>) {
    checked_envelope(
        false,
        &format!(
            r#"{{"model":"m",{extra}"contents":[{{"role":"user","parts":[{{"text":"a"}}]}},{{"role":"model","parts":[{{"text":"b"}}]}},{{"role":"user","parts":[{final_parts}]}}]}}"#
        ),
    )
}

/// `requireInteractionsToGeminiRefusal` and `requireFileDataRefusal`.
#[track_caller]
fn require_refusal((body, err): (Value, Option<UnsupportedPartError>), want: &str) {
    let err = err.unwrap_or_else(|| panic!("no refusal, want {want}; body = {body}"));
    assert_eq!(err.part_type, want, "{body}");
    assert_eq!(err.status_code(), 400);
    assert!(err.is_request_scoped());
    assert_eq!(err.to_string(), format!("unsupported content part: {want}"));
    assert!(body.is_object(), "{body}");
}

#[track_caller]
fn require_sent((body, err): (Value, Option<UnsupportedPartError>)) -> Value {
    assert_eq!(err, None, "{body}");
    body
}

// TestGeminiToInteractionsFileDataOnlyTurnKeepsTheFile
#[test]
fn gemini_file_data_only_turn_keeps_the_file() {
    for (name, part, want_type, want_uri, want_mime) in [
        (
            "camel case pdf",
            r#"{"fileData":{"mimeType":"application/pdf","fileUri":"gs://bucket/doc.pdf"}}"#,
            "document",
            "gs://bucket/doc.pdf",
            Some("application/pdf"),
        ),
        (
            "snake case video",
            r#"{"file_data":{"mime_type":"video/mp4","file_uri":"https://example.test/a.mp4"}}"#,
            "video",
            "https://example.test/a.mp4",
            Some("video/mp4"),
        ),
        (
            "image",
            r#"{"fileData":{"mimeType":"image/png","fileUri":"https://example.test/a.png"}}"#,
            "image",
            "https://example.test/a.png",
            Some("image/png"),
        ),
        (
            "without a mime type",
            r#"{"fileData":{"fileUri":"https://example.test/a"}}"#,
            "document",
            "https://example.test/a",
            None,
        ),
    ] {
        let out = require_sent(gemini_turn_to_interactions("", part));
        assert_eq!(array(&out, "input").len(), 3, "{name}: {out}");
        assert_eq!(
            text(&out, "input.2.content.0.type"),
            want_type,
            "{name}: {out}"
        );
        assert_eq!(
            text(&out, "input.2.content.0.uri"),
            want_uri,
            "{name}: {out}"
        );
        assert_eq!(
            get(&out, "input.2.content.0.mime_type").and_then(Value::as_str),
            want_mime,
            "{name}: {out}"
        );
    }
}

// TestGeminiToInteractionsFileDataWithoutURIIsRefused
#[test]
fn gemini_file_data_without_uri_is_refused() {
    for (name, extra, parts) in [
        (
            "history then file only",
            "",
            r#"{"fileData":{"mimeType":"application/pdf"}}"#,
        ),
        (
            "snake case spelling",
            "",
            r#"{"file_data":{"mime_type":"application/pdf","file_uri":""}}"#,
        ),
        (
            "system instruction does not hide it",
            r#""systemInstruction":{"parts":[{"text":"be brief"}]},"#,
            r#"{"fileData":{"mimeType":"application/pdf"}}"#,
        ),
        (
            "empty text does not hide it",
            "",
            r#"{"text":""},{"fileData":{"mimeType":"application/pdf"}}"#,
        ),
    ] {
        eprintln!("{name}");
        require_refusal(gemini_turn_to_interactions(extra, parts), "fileData");
    }
}

// TestGeminiToInteractionsRefusesAnEmptiedTurnBeforeALaterTextTurn
#[test]
fn gemini_refuses_an_emptied_turn_before_a_later_text_turn() {
    require_refusal(
        checked_envelope(
            false,
            r#"{"model":"m","contents":[{"role":"user","parts":[{"fileData":{"mimeType":"application/pdf"}}]},{"role":"model","parts":[{"text":"ok"}]},{"role":"user","parts":[{"text":"next"}]}]}"#,
        ),
        "fileData",
    );
}

// TestGeminiToInteractionsFileDataWithoutURIBesideTextStillSucceeds
#[test]
fn gemini_file_data_without_uri_beside_text_still_succeeds() {
    let out = require_sent(gemini_turn_to_interactions(
        "",
        r#"{"text":"keep me"},{"fileData":{"mimeType":"application/pdf"}}"#,
    ));
    assert_eq!(text(&out, "input.2.content.0.text"), "keep me", "{out}");
    assert_eq!(array(&out, "input").len(), 3, "{out}");
}

// TestGeminiToInteractionsInlineDataOnlyTurnStillConverts
#[test]
fn gemini_inline_data_only_turn_still_converts() {
    let out = require_sent(gemini_turn_to_interactions(
        "",
        r#"{"inlineData":{"mimeType":"application/pdf","data":"JVBERi0xLjQK"}}"#,
    ));
    assert_eq!(
        get(&out, "input.2.content.0"),
        Some(&json!({"type": "document", "mime_type": "application/pdf", "data": "JVBERi0xLjQK"})),
        "{out}"
    );
}

// TestGeminiToInteractionsModelFileDataWithoutURIIsNotARefusal
#[test]
fn gemini_model_file_data_without_uri_is_not_a_refusal() {
    require_sent(checked_envelope(
        false,
        r#"{"model":"m","contents":[{"role":"user","parts":[{"text":"a"}]},{"role":"model","parts":[{"fileData":{"mimeType":"application/pdf"}}]},{"role":"user","parts":[{"text":"b"}]}]}"#,
    ));
}

// TestGeminiToInteractionsExportedWrapperKeepsAJSONBody
#[test]
fn gemini_refusal_keeps_a_json_body() {
    let (body, err) = super::convert_gemini_request_to_interactions(
        "m",
        &json!({"model": "m", "contents": [{"role": "user", "parts": [{"fileData": {"mimeType": "application/pdf"}}]}]}),
        false,
    );
    assert!(err.is_some() && body.is_object(), "{body}");
}

// TestGeminiFileDataSurvivesTheRoundTripThroughInteractions
#[test]
fn gemini_file_data_survives_the_round_trip_through_interactions() {
    let interactions = require_sent(gemini_turn_to_interactions(
        "",
        r#"{"fileData":{"mimeType":"image/png","fileUri":"gs://b/a.png"}}"#,
    ));
    assert_eq!(
        text(&interactions, "input.2.content.0.uri"),
        "gs://b/a.png",
        "{interactions}"
    );
    let out = require_sent(interactions_to_gemini_envelope(&interactions.to_string()));
    assert_eq!(array(&out, "contents").len(), 3, "{out}");
    assert_eq!(
        get(&out, "contents.2.parts.0.fileData"),
        Some(&json!({"mimeType": "image/png", "fileUri": "gs://b/a.png"})),
        "{out}"
    );
}

// TestInteractionsToGeminiReadsURIAsFileURI
#[test]
fn interactions_reads_uri_as_file_uri() {
    for field in ["uri", "file_uri", "fileUri"] {
        let out = require_sent(interactions_final_turn_to_gemini(&format!(
            r#"{{"type":"document","mime_type":"application/pdf","{field}":"gs://b/a.pdf"}}"#
        )));
        assert_eq!(
            get(&out, "contents.2.parts.0.fileData"),
            Some(&json!({"mimeType": "application/pdf", "fileUri": "gs://b/a.pdf"})),
            "{field}: {out}"
        );
    }
}

// TestInteractionsToGeminiInlineDataStillConverts
#[test]
fn interactions_inline_data_still_converts() {
    let out = require_sent(interactions_final_turn_to_gemini(
        r#"{"type":"image","mime_type":"image/png","data":"aGVsbG8="}"#,
    ));
    assert_eq!(
        get(&out, "contents.2.parts.0.inlineData"),
        Some(&json!({"mimeType": "image/png", "data": "aGVsbG8="})),
        "{out}"
    );
}

// TestInteractionsToGeminiUnrepresentableAttachmentOnlyTurnIsRefused
#[test]
fn interactions_unrepresentable_attachment_only_turn_is_refused() {
    for (name, content, want) in [
        (
            "audio without bytes or uri",
            r#"{"type":"audio","mime_type":"audio/wav"}"#,
            "audio",
        ),
        ("image without anything", r#"{"type":"image"}"#, "image"),
        (
            "uri without a mime type",
            r#"{"type":"document","uri":"gs://b/a.pdf"}"#,
            "document",
        ),
        (
            "image_url that is not a data url",
            r#"{"type":"image_url","image_url":{"url":"https://example.test/a.png"}}"#,
            "image_url",
        ),
        (
            "empty text does not hide it",
            r#"{"type":"text","text":""},{"type":"video","mime_type":"video/mp4"}"#,
            "video",
        ),
        (
            "whitespace text does not hide it",
            r#"{"type":"text","text":" \n"},{"type":"audio","mime_type":"audio/wav"}"#,
            "audio",
        ),
    ] {
        eprintln!("{name}");
        require_refusal(interactions_final_turn_to_gemini(content), want);
    }
}

// TestInteractionsToGeminiUnrepresentableNativePartIsRefused
#[test]
fn interactions_unrepresentable_native_part_is_refused() {
    require_refusal(
        interactions_to_gemini_envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{{"role":"user","parts":[{{"fileData":{{"mimeType":"image/png"}}}}]}}]}}"#
        )),
        "fileData",
    );
}

// TestInteractionsToGeminiRefusesAnEmptiedTurnBeforeALaterTextTurn
#[test]
fn interactions_refuses_an_emptied_turn_before_a_later_text_turn() {
    require_refusal(
        interactions_to_gemini_envelope(
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]},{"type":"model_output","content":[{"type":"text","text":"ok"}]},{"type":"user_input","content":[{"type":"text","text":"next"}]}]}"#,
        ),
        "audio",
    );
}

// TestInteractionsToGeminiUnrepresentableAttachmentBesideTextStillSucceeds
#[test]
fn interactions_unrepresentable_attachment_beside_text_still_succeeds() {
    let out = require_sent(interactions_final_turn_to_gemini(
        r#"{"type":"text","text":"keep me"},{"type":"audio","mime_type":"audio/wav"}"#,
    ));
    assert_eq!(text(&out, "contents.2.parts.0.text"), "keep me", "{out}");
    assert_eq!(array(&out, "contents.2.parts").len(), 1, "{out}");
}

// TestInteractionsToGeminiNeighbouringStepKeepsTheTurnAlive
#[test]
fn interactions_neighbouring_step_keeps_the_turn_alive() {
    for (name, steps) in [
        (
            "text step",
            r#"{"type":"user_input","content":[{"type":"text","text":"keep me"}]},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "tool result",
            r#"{"type":"function_call","name":"f","call_id":"c1","arguments":{}},{"type":"function_result","name":"f","call_id":"c1","result":{"ok":true}},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "string step",
            r#""keep me",{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "native step",
            r#"{"role":"user","parts":[{"text":"keep me"}]},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}"#,
        ),
        (
            "turn of steps",
            r#"{"role":"user","steps":[{"type":"user_input","content":[{"type":"text","text":"keep me"}]},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]}]}"#,
        ),
    ] {
        eprintln!("{name}");
        require_sent(interactions_to_gemini_envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{steps}]}}"#
        )));
    }
}

// TestInteractionsToGeminiExportedWrapperKeepsAJSONBody
#[test]
fn interactions_refusal_keeps_a_json_body() {
    let input: Value = serde_json::from_str(&format!(
        r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{{"type":"user_input","content":[{{"type":"audio","mime_type":"audio/wav"}}]}}]}}"#
    ))
    .expect("test request is JSON");
    let (body, err) = super::convert_interactions_request_to_gemini("m", &input, false);
    assert!(err.is_some() && body.is_object(), "{body}");
}

// TestInteractionsToGeminiDeveloperOrSystemStepDoesNotHideAnEmptiedUserTurn
#[test]
fn interactions_developer_or_system_step_does_not_hide_an_emptied_user_turn() {
    for (name, note) in GEMINI_INSTRUCTION_STEPS {
        eprintln!("after {name}");
        require_refusal(
            interactions_to_gemini_envelope(&format!(
                r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{GEMINI_EMPTIED_STEP},{note}]}}"#
            )),
            "document",
        );
        eprintln!("before {name}");
        require_refusal(
            interactions_to_gemini_envelope(&format!(
                r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{note},{GEMINI_EMPTIED_STEP}]}}"#
            )),
            "document",
        );
    }
}

// TestInteractionsToGeminiDeveloperOrSystemStepStillSendsItsText
#[test]
fn interactions_developer_or_system_step_still_sends_its_text() {
    for (name, note) in GEMINI_INSTRUCTION_STEPS {
        let out = require_sent(interactions_to_gemini_envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{{"type":"user_input","content":[{{"type":"text","text":"keep me"}}]}},{note}]}}"#
        )));
        let out = out.to_string();
        assert!(
            out.contains("note") && out.contains("keep me"),
            "{name}: {out}"
        );
    }
}

// TestInteractionsToGeminiRealUserTextBesideAnUnrepresentableAttachmentSurvivesAnInstructionStep
#[test]
fn interactions_real_user_text_beside_an_attachment_survives_an_instruction_step() {
    for (name, note) in GEMINI_INSTRUCTION_STEPS {
        let out = require_sent(interactions_to_gemini_envelope(&format!(
            r#"{{"model":"m","input":[{INTERACTIONS_GEMINI_HISTORY},{note},{{"type":"user_input","content":[{{"type":"text","text":"keep me"}},{{"type":"document","uri":"gs://b/a.pdf"}}]}}]}}"#
        )));
        assert!(out.to_string().contains("keep me"), "{name}: {out}");
    }
}

// Not upstream's: a fileData part's URI is trimmed, its MIME type kept as
// written, and either spelling read; a model turn's fileData without a URI
// is dropped without a refusal. The expected output comes from upstream.
#[test]
fn gemini_file_data_in_detail() {
    let (out, err) = checked_envelope(
        false,
        r#"{"model":"m","contents":[{"role":"user","parts":[{"text":"a"}]},{"role":"model","parts":[{"fileData":{"mimeType":"application/pdf"}},{"text":"b"}]},{"role":"user","parts":[{"fileData":{"mimeType":"IMAGE/PNG","fileUri":"  gs://b/a.png "}},{"file_data":{"mime_type":"text/plain","file_uri":"","fileUri":"x"}},{"file_data":{"file_uri":" u "}}]}]}"#,
    );
    assert_eq!(err, None, "{out}");
    assert_eq!(
        out.to_string(),
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":"a"}]},{"type":"model_output","content":[{"type":"text","text":"b"}]},{"type":"user_input","content":[{"type":"image","uri":"gs://b/a.png","mime_type":"IMAGE/PNG"}]},{"type":"user_input","content":[{"type":"document","uri":"x","mime_type":"text/plain"}]},{"type":"user_input","content":[{"type":"document","uri":"u"}]}],"stream":false}"#
    );
    // A null fileData is dropped; the refusal is the first emptied turn's,
    // and the body still goes with the rest.
    let (out, err) = checked_envelope(
        false,
        r#"{"model":"m","contents":[{"role":"user","parts":[{"text":""},{"fileData":null}]},{"role":"user","parts":[{"fileData":{"fileUri":"gs://x"}}]}]}"#,
    );
    require_refusal((out.clone(), err), "fileData");
    assert_eq!(
        out.to_string(),
        r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"document","uri":"gs://x"}]}],"stream":false}"#
    );
    // Whitespace text that isn't empty is sent as a step of its own, which
    // keeps the turn, as upstream's does; so does a tool result.
    let (out, err) = checked_envelope(
        false,
        r#"{"model":"m","contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{}}},{"fileData":{}}]},{"parts":[{"text":"  "},{"file_data":{"mime_type":"a/b"}}]}]}"#,
    );
    assert_eq!(err, None, "{out}");
    assert_eq!(
        out.to_string(),
        r#"{"model":"m","input":[{"type":"function_result","name":"f","result":{}},{"type":"user_input","content":[{"type":"text","text":"  "}]}],"stream":false}"#
    );
}

// Not upstream's: Interactions user turns in detail. The expected output
// comes from upstream.
#[test]
fn interactions_user_turns_in_detail() {
    for (input, want_body, want_err) in [
        // A blank file URI falls through to the part's url; a URI is trimmed.
        (
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"image","file_uri":" ","url":"data:image/png;base64,AAAA"}]},{"type":"user_input","content":[{"type":"document","mime_type":"application/pdf","uri":"  gs://b/a.pdf  "}]}]}"#,
            r#"{"model":"m","contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]},{"role":"user","parts":[{"fileData":{"mimeType":"application/pdf","fileUri":"gs://b/a.pdf"}}]}]}"#,
            None,
        ),
        // Model text doesn't keep a user turn, and a developer wrapper's
        // steps are instruction content, string or not.
        (
            r#"{"model":"m","input":[{"role":"model","steps":["hi"]},{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav"}]},{"role":"developer","steps":["x",{"type":"user_input","content":[{"type":"text","text":"y"}]}]}]}"#,
            r#"{"model":"m","contents":[{"role":"model","parts":[{"text":"hi"}]},{"role":"user","parts":[{"text":"x"}]},{"role":"user","parts":[{"text":"y"}]}]}"#,
            Some("audio"),
        ),
        // A step naming the user role inside a system turn is the user's.
        (
            r#"{"model":"m","input":{"role":"system","steps":[{"type":"user_input","content":[{"type":"text","text":"note"}]},{"type":"user_input","role":"user","content":[{"type":"image"}]}]}}"#,
            r#"{"model":"m","contents":[{"role":"user","parts":[{"text":"note"}]}]}"#,
            Some("image"),
        ),
        // Unicode space is blank too.
        (
            "{\"model\":\"m\",\"input\":{\"type\":\"user_input\",\"content\":[{\"type\":\"image\"},{\"type\":\"text\",\"text\":\"\u{a0}\"}]}}",
            "{\"model\":\"m\",\"contents\":[{\"role\":\"user\",\"parts\":[{\"text\":\"\u{a0}\"}]}]}",
            Some("image"),
        ),
        // Native parts: blank and empty thought text don't keep the turn.
        (
            r#"{"model":"m","input":[{"role":"user","parts":[{"text":" "},{"inline_data":{"mime_type":"x/y"}},{"thought":true,"text":""}]}]}"#,
            r#"{"model":"m","contents":[{"role":"user","parts":[{"text":" "},{"thought":true,"text":""}]}]}"#,
            Some("inlineData"),
        ),
        // Blank string content doesn't either.
        (
            r#"{"model":"m","input":[{"type":"thought","signature":"s"},{"type":"user_input","content":"  "},{"type":"user_input","content":[{"type":"input_audio"}]}]}"#,
            r#"{"model":"m","contents":[{"role":"model","parts":[{"text":"","thoughtSignature":"s"}]},{"role":"user","parts":[{"text":"  "}]}]}"#,
            Some("input_audio"),
        ),
    ] {
        let (out, err) = interactions_to_gemini_envelope(input);
        assert_eq!(out.to_string(), want_body, "{input}");
        assert_eq!(
            err.map(|err| err.part_type),
            want_err.map(str::to_owned),
            "{input}"
        );
    }
}
