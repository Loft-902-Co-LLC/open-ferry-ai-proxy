// Ported from CLIProxyAPI internal/translator/claude/gemini/claude_gemini_response_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;

const VALID_GEMINI_SIGNATURE: &str =
    "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

fn signature_cases() -> [(String, &'static str); 2] {
    [
        (
            "foreign_claude_sig_123".to_owned(),
            "skip_thought_signature_validator",
        ),
        (
            format!("gemini#{VALID_GEMINI_SIGNATURE}"),
            VALID_GEMINI_SIGNATURE,
        ),
    ]
}

fn thinking_events(signature: &str) -> Vec<String> {
    vec![
        r#"data: {"type":"message_start","message":{"id":"msg_123","model":"claude-3-7-sonnet-20250219"}}"#.to_owned(),
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#.to_owned(),
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thinking text"}}"#.to_owned(),
        format!(r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"signature_delta","signature":"{signature}"}}}}"#),
        r#"data: {"type":"content_block_stop","index":0}"#.to_owned(),
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#.to_owned(),
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"final answer"}}"#.to_owned(),
        r#"data: {"type":"content_block_stop","index":1}"#.to_owned(),
        r#"data: {"type":"message_stop"}"#.to_owned(),
    ]
}

fn run_stream(lines: &[&str]) -> Vec<Value> {
    let mut translator = ClaudeToGeminiStream::new("gemini-2.5-pro");
    lines
        .iter()
        .flat_map(|line| translator.translate_line(line.as_bytes()))
        .collect()
}

const TOOL_USE_EVENTS: [&str; 3] = [
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_gateway","name":"lookup"}}"#,
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"status\"}"}}"#,
    r#"data: {"type":"content_block_stop","index":0}"#,
];

#[test]
fn stream_preserves_tool_use_id() {
    let mut translator = ClaudeToGeminiStream::new("gemini-2.5-pro");
    assert!(
        translator
            .translate_line(TOOL_USE_EVENTS[0].as_bytes())
            .is_empty()
    );
    assert!(
        translator
            .translate_line(TOOL_USE_EVENTS[1].as_bytes())
            .is_empty()
    );
    let out = translator.translate_line(TOOL_USE_EVENTS[2].as_bytes());
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"][0]["functionCall"],
        json!({"name": "lookup", "args": {"query": "status"}, "id": "toolu_gateway"})
    );
    assert_eq!(out[0]["candidates"][0]["finishReason"], "STOP");
}

#[test]
fn non_stream_preserves_tool_use_id() {
    let out = convert_claude_response_to_gemini_non_stream(
        "gemini-2.5-pro",
        TOOL_USE_EVENTS.join("\n").as_bytes(),
    );
    assert_eq!(
        out["candidates"][0]["content"]["parts"][0]["functionCall"]["id"],
        "toolu_gateway"
    );
}

#[test]
fn stream_thinking_signature() {
    for (signature, want) in signature_cases() {
        let events = thinking_events(&signature);
        let lines: Vec<&str> = events.iter().map(String::as_str).collect();
        let found = run_stream(&lines)
            .iter()
            .flat_map(|out| out["candidates"][0]["content"]["parts"].as_array().cloned())
            .flatten()
            .rfind(|part| part["thought"] == true && part.get("thoughtSignature").is_some())
            .map(|part| part["thoughtSignature"].clone());
        assert_eq!(found, Some(json!(want)), "{signature}");
    }
}

#[test]
fn non_stream_thinking_signature() {
    for (signature, want) in signature_cases() {
        let out = convert_claude_response_to_gemini_non_stream(
            "gemini-2.5-pro",
            thinking_events(&signature).join("\n").as_bytes(),
        );
        assert_eq!(
            out["candidates"][0]["content"]["parts"],
            json!([
                {"thought": true, "text": "thinking text", "thoughtSignature": want},
                {"text": "final answer"}
            ]),
            "{signature}"
        );
        assert_eq!(out["responseId"], "msg_123");
        assert_eq!(out["modelVersion"], "gemini-2.5-pro");
    }
}

#[test]
fn stream_template_follows_message_start() {
    let out = run_stream(&[
        "event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-x"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
        r#"data: {"type":"message_stop"}"#,
    ]);
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0]["modelVersion"], "claude-x");
    assert_eq!(out[0]["responseId"], "msg_1");
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"],
        json!([{"text": "hi"}])
    );
    assert_eq!(out[1]["candidates"][0]["content"]["parts"], json!([]));
    let keys: Vec<&String> = out[0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "candidates",
            "usageMetadata",
            "modelVersion",
            "createTime",
            "responseId"
        ]
    );
}

#[test]
fn stream_message_delta_always_stops_and_reports_usage() {
    let out = run_stream(&[
        r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"input_tokens":3,"output_tokens":4,"cache_creation_input_tokens":2,"cache_read_input_tokens":5,"thinking_tokens":1}}"#,
    ]);
    assert_eq!(out[0]["candidates"][0]["finishReason"], "STOP");
    assert_eq!(
        out[0]["usageMetadata"],
        json!({
            "trafficType": "PROVISIONED_THROUGHPUT",
            "promptTokenCount": 3,
            "candidatesTokenCount": 4,
            "totalTokenCount": 7,
            "cachedContentTokenCount": 7,
            "thoughtsTokenCount": 1
        })
    );
    let keys: Vec<&String> = out[0]["usageMetadata"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(keys[0], "trafficType");
}

#[test]
fn stream_error_event() {
    assert_eq!(
        run_stream(&[r#"data: {"type":"error","error":{"message":"overloaded"}}"#]),
        [json!({"error": {"code": 400, "message": "overloaded", "status": "INVALID_ARGUMENT"}})]
    );
    assert_eq!(
        run_stream(&[r#"data: {"type":"error"}"#])[0]["error"]["message"],
        "Unknown error occurred"
    );
}

#[test]
fn stream_tool_call_without_input_or_name() {
    // A tool call with no name and no input gives nothing, and its ID stays.
    let out = run_stream(&[
        r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":""}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":" [1] "}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
    ]);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"][0],
        json!({"functionCall": {"name": "", "args": [1], "id": "toolu_1"}})
    );
}

#[test]
fn non_stream_usage_and_consolidation() {
    let body = [
        r#"data: {"type":"message_start","message":{"id":"msg_9"}}"#,
        "",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a"}}"#,
        "data:   \r\r",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"b"}}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"f"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"t"}}"#,
        r#"data: {"type":"message_delta","usage":{"input_tokens":1}}"#,
        r#"data: {"type":"message_delta","usage":{"output_tokens":2,"cache_creation_input_tokens":4}}"#,
    ]
    .join("\r\n");
    let out = convert_claude_response_to_gemini_non_stream("gemini-x", body.as_bytes());
    assert_eq!(
        out["candidates"][0]["content"]["parts"],
        json!([
            {"text": "ab"},
            {"functionCall": {"name": "f", "args": {}}},
            {"thought": true, "text": "t"}
        ])
    );
    assert_eq!(
        out["usageMetadata"],
        json!({
            "promptTokenCount": 0,
            "candidatesTokenCount": 2,
            "totalTokenCount": 2,
            "cachedContentTokenCount": 4,
            "trafficType": "PROVISIONED_THROUGHPUT"
        })
    );
    assert_eq!(out["candidates"][0]["finishReason"], "STOP");
    assert_eq!(out["responseId"], "msg_9");
    assert_ne!(out["createTime"], "");
}

#[test]
fn non_stream_without_message_start() {
    let out = convert_claude_response_to_gemini_non_stream("gemini-x", b"");
    assert_eq!(
        out,
        json!({
            "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
            "usageMetadata": {"trafficType": "PROVISIONED_THROUGHPUT"},
            "modelVersion": "gemini-x",
            "createTime": "",
            "responseId": ""
        })
    );
}

#[test]
fn token_count() {
    assert_eq!(
        gemini_token_count(3),
        json!({"totalTokens": 3, "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 3}]})
    );
}
