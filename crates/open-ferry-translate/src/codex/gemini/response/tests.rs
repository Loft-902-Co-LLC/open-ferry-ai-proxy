// Ported from CLIProxyAPI internal/translator/codex/gemini/codex_gemini_response_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("test JSON is valid")
}

fn stream(original_request: &str) -> CodexToGeminiStream {
    CodexToGeminiStream::new("gemini-2.5-pro", &parse(original_request))
}

fn non_stream(original_request: &str, event: &str) -> Value {
    convert_codex_response_to_gemini_non_stream(
        "gemini-2.5-pro",
        &parse(original_request),
        &parse(event),
    )
    .expect("a terminal event gives a response")
}

#[test]
fn incomplete_terminal() {
    let terminal = r#"{"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-5.5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#;
    let out = stream("null").translate_line(format!("data: {terminal}").as_bytes());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["candidates"][0]["finishReason"], "MAX_TOKENS");
    assert_eq!(out[0]["usageMetadata"]["totalTokenCount"], 3);

    let out = non_stream("null", terminal);
    assert_eq!(out["candidates"][0]["finishReason"], "MAX_TOKENS");
}

#[test]
fn stream_empty_output_uses_output_item_done_message_fallback() {
    let mut translator = stream(r#"{"tools":[]}"#);
    let mut outputs = Vec::new();
    for line in [
        r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":0}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    ] {
        outputs.extend(translator.translate_line(line.as_bytes()));
    }
    assert!(
        outputs
            .iter()
            .any(|out| out["candidates"][0]["content"]["parts"][0]["text"] == "ok"),
        "{outputs:?}"
    );
}

#[test]
fn stream_partial_image_emits_inline_data() {
    let mut translator = stream(r#"{"tools":[]}"#);
    let line = br#"data: {"type":"response.image_generation_call.partial_image","item_id":"ig_123","output_format":"png","partial_image_b64":"aGVsbG8=","partial_image_index":0}"#;
    let out = translator.translate_line(line);
    assert_eq!(out.len(), 1);
    let part = &out[0]["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["inlineData"]["data"], "aGVsbG8=");
    assert_eq!(part["inlineData"]["mimeType"], "image/png");

    assert!(
        translator.translate_line(line).is_empty(),
        "a repeated image is suppressed"
    );
}

#[test]
fn stream_image_generation_call_done_emits_inline_data() {
    let mut translator = stream(r#"{"tools":[]}"#);
    let out = translator.translate_line(br#"data: {"type":"response.image_generation_call.partial_image","item_id":"ig_123","output_format":"png","partial_image_b64":"aGVsbG8=","partial_image_index":0}"#);
    assert_eq!(out.len(), 1);

    let out = translator.translate_line(br#"data: {"type":"response.output_item.done","item":{"id":"ig_123","type":"image_generation_call","output_format":"png","result":"aGVsbG8="}}"#);
    assert!(
        out.is_empty(),
        "the same image as the last partial one is suppressed"
    );

    let out = translator.translate_line(br#"data: {"type":"response.output_item.done","item":{"id":"ig_123","type":"image_generation_call","output_format":"jpeg","result":"Ymll"}}"#);
    assert_eq!(out.len(), 1);
    let part = &out[0]["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["inlineData"]["data"], "Ymll");
    assert_eq!(part["inlineData"]["mimeType"], "image/jpeg");
}

#[test]
fn non_stream_image_generation_call_adds_inline_data_part() {
    let out = non_stream(
        r#"{"tools":[]}"#,
        r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]},{"type":"image_generation_call","output_format":"png","result":"aGVsbG8="}]}}"#,
    );
    let part = &out["candidates"][0]["content"]["parts"][1];
    assert_eq!(part["inlineData"]["data"], "aGVsbG8=");
    assert_eq!(part["inlineData"]["mimeType"], "image/png");
    assert_eq!(out["createTime"], "2023-11-14T22:13:20Z");
    assert_eq!(out["responseId"], "resp_123");
}

#[test]
fn stream_preserves_function_call_id() {
    let mut translator = stream(r#"{"tools":[]}"#);
    let out = translator.translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_gateway","name":"lookup","arguments":"{\"query\":\"status\"}"}}"#);
    assert!(out.is_empty(), "the function call is held back");

    let out = translator.translate_line(br#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#);
    assert_eq!(out.len(), 2);
    let call = &out[0]["candidates"][0]["content"]["parts"][0]["functionCall"];
    assert_eq!(call["id"], "call_gateway");
    assert_eq!(call["args"], json!({"query": "status"}));
    assert_eq!(out[0]["candidates"][0]["finishReason"], "STOP");
}

#[test]
fn non_stream_preserves_function_call_id() {
    let out = non_stream(
        r#"{"tools":[]}"#,
        r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_gateway","name":"lookup","arguments":"{\"query\":\"status\"}"}]}}"#,
    );
    assert_eq!(
        out["candidates"][0]["content"]["parts"][0]["functionCall"]["id"],
        "call_gateway"
    );
}

// Not upstream's: upstream sets the arguments' text as the call's `args`
// (`SetRaw`), so each number keeps its spelling.
#[test]
fn function_call_args_keep_number_text() {
    let want = r#"{"n":1e400,"z":-0,"e":1E20}"#;
    let mut translator = stream(r#"{"tools":[]}"#);
    translator.translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"f","arguments":"{\"n\":1e400,\"z\":-0,\"e\":1E20}"}}"#);
    let out = translator.translate_line(br#"data: {"type":"response.completed","response":{}}"#);
    let call = &out[0]["candidates"][0]["content"]["parts"][0]["functionCall"];
    assert_eq!(call["args"].to_string(), want);

    let out = non_stream(
        r#"{"tools":[]}"#,
        r#"{"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"c","name":"f","arguments":"{\"n\":1e400,\"z\":-0,\"e\":1E20}"}]}}"#,
    );
    let call = &out["candidates"][0]["content"]["parts"][0]["functionCall"];
    assert_eq!(call["args"].to_string(), want);
}

#[test]
fn function_call_names_map_back_to_declared_names() {
    let long = format!("mcp__server__{}", "a".repeat(70));
    let short = format!("mcp__{}", "a".repeat(59));
    let request = format!(r#"{{"tools":[{{"functionDeclarations":[{{"name":"{long}"}}]}}]}}"#);
    let out = non_stream(
        &request,
        &format!(
            r#"{{"type":"response.completed","response":{{"output":[{{"type":"function_call","name":"{short}","arguments":"[1]"}}]}}}}"#
        ),
    );
    assert_eq!(
        out["candidates"][0]["content"]["parts"][0],
        json!({"functionCall": {"args": {}, "name": long}})
    );
}

#[test]
fn stream_template_and_response_id() {
    let mut translator = stream("{}");
    let out = translator.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_9","model":"gpt-5","created_at":0}}"#);
    assert_eq!(
        out,
        [json!({
            "candidates": [{"content": {"role": "model", "parts": []}}],
            "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
            "modelVersion": "gpt-5",
            "createTime": "1970-01-01T00:00:00Z",
            "responseId": "resp_9"
        })]
    );
    let out = translator
        .translate_line(br#"data: {"type":"response.reasoning_summary_text.delta","delta":"hm"}"#);
    assert_eq!(out[0]["modelVersion"], "gemini-2.5-pro");
    assert_eq!(out[0]["createTime"], TEMPLATE_CREATE_TIME);
    assert_eq!(out[0]["responseId"], "resp_9");
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"],
        json!([{"thought": true, "text": "hm"}])
    );
    assert!(
        translator
            .translate_line(b"event: response.created")
            .is_empty()
    );
    assert!(
        translator
            .translate_line(br#"data: {"type":"response.in_progress"}"#)
            .is_empty()
    );
}

#[test]
fn non_stream_ignores_other_events() {
    assert!(
        convert_codex_response_to_gemini_non_stream(
            "m",
            &Value::Null,
            &json!({"type":"response.created"})
        )
        .is_none()
    );
}

#[test]
fn token_count() {
    assert_eq!(
        gemini_token_count(7),
        json!({"totalTokens": 7, "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 7}]})
    );
}
