// Ported from CLIProxyAPI internal/translator/openai/gemini/openai_gemini_request_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// DeterministicToolCallIDs and DeterministicFallbackOrphanResponse convert
// their request a few times rather than a hundred.

use serde_json::{Value, json};

use super::*;

fn convert(request: &str) -> Value {
    let request: Value = serde_json::from_str(request).unwrap();
    convert_gemini_request_to_openai("test-model", &request, false)
}

/// The string at a dotted path, as gjson reads one: missing is `""`.
fn text_at(out: &Value, at: &str) -> String {
    let mut value = out;
    for key in at.split('.') {
        value = match key.parse::<usize>() {
            Ok(index) => &value[index],
            Err(_) => &value[key],
        };
    }
    str_of(Some(value)).into_owned()
}

#[test]
fn function_responses_consume_tool_call_ids_fifo() {
    let out = convert(
        r#"{"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}},
                {"functionCall": {"name": "grep", "args": {"pattern": "needle"}}},
                {"functionCall": {"name": "list_dir", "args": {"path": "."}}}
            ]},
            {"role": "function", "parts": [
                {"functionResponse": {"name": "read_file", "response": {"result": "a"}}},
                {"functionResponse": {"name": "grep", "response": {"result": "b"}}},
                {"functionResponse": {"name": "list_dir", "response": {"result": "c"}}}
            ]}
        ]}"#,
    );
    let ids: Vec<String> = (0..3)
        .map(|i| text_at(&out, &format!("messages.0.tool_calls.{i}.id")))
        .collect();
    assert!(ids.iter().all(|id| !id.is_empty()), "{out}");
    assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            &text_at(&out, &format!("messages.{}.tool_call_id", i + 1)),
            id
        );
    }
}

#[test]
fn function_response_without_prior_call_gets_fallback_id() {
    let out = convert(
        r#"{"contents": [{"role": "function", "parts": [
            {"functionResponse": {"name": "read_file", "response": {"result": "ok"}}}
        ]}]}"#,
    );
    assert!(text_at(&out, "messages.0.tool_call_id").starts_with("call_"));
}

#[test]
fn extra_function_responses_use_fallback_id() {
    let out = convert(
        r#"{"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}
            ]},
            {"role": "function", "parts": [
                {"functionResponse": {"name": "read_file", "response": {"result": "a"}}},
                {"functionResponse": {"name": "read_file", "response": {"result": "extra"}}}
            ]}
        ]}"#,
    );
    let call_id = text_at(&out, "messages.0.tool_calls.0.id");
    assert_eq!(text_at(&out, "messages.1.tool_call_id"), call_id);
    let extra = text_at(&out, "messages.2.tool_call_id");
    assert!(extra.starts_with("call_"));
    assert_ne!(extra, call_id);
}

#[test]
fn preserves_explicit_function_call_ids() {
    for (field, want) in [
        (r#""id":"call_gateway_id""#, "call_gateway_id"),
        (
            r#""call_id":"call_gateway_call_id""#,
            "call_gateway_call_id",
        ),
        (
            r#""callId":"call_gateway_camel_id""#,
            "call_gateway_camel_id",
        ),
    ] {
        let out = convert(&format!(
            r#"{{"contents": [
                {{"role": "model", "parts": [{{"functionCall": {{"name": "lookup", {field}, "args": {{"q": "x"}}}}}}]}},
                {{"role": "function", "parts": [{{"functionResponse": {{"name": "lookup", {field}, "response": {{"result": "ok"}}}}}}]}}
            ]}}"#
        ));
        assert_eq!(text_at(&out, "messages.0.tool_calls.0.id"), want, "{out}");
        assert_eq!(text_at(&out, "messages.1.tool_call_id"), want, "{out}");
    }
}

#[test]
fn accepts_snake_inline_data() {
    let out = convert(
        r#"{"contents":[{"role":"user","parts":[{"inline_data":{"mime_type":"image/png","data":"aGVsbG8="}}]}]}"#,
    );
    assert_eq!(
        text_at(&out, "messages.0.content.0.image_url.url"),
        "data:image/png;base64,aGVsbG8="
    );
}

#[test]
fn splits_non_image_inline_data_by_mime() {
    let out = convert(
        r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}},{"inlineData":{"mimeType":"video/mp4","data":"AAAAIGZ0eXA="}},{"inlineData":{"mimeType":"application/pdf","data":"JVBERi0="}}]}]}"#,
    );
    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}},
            {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAAAIGZ0eXA="}},
            {"type": "file", "file": {"filename": "document.pdf", "file_data": "JVBERi0="}}
        ])
    );
}

#[test]
fn drops_hidden_thought_parts() {
    // A turn of nothing but hidden reasoning goes.
    let out = convert(
        r#"{"contents":[
            {"role":"model","parts":[{"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"}]},
            {"role":"user","parts":[{"text":"continue"}]}
        ]}"#,
    );
    assert_eq!(
        out["messages"],
        json!([{"role": "user", "content": "continue"}])
    );

    // Hidden reasoning goes from a turn with visible text.
    let out = convert(
        r#"{"contents":[{"role":"model","parts":[
            {"thought":true,"text":"internal reasoning","thoughtSignature":"opaque-provider-state"},
            {"text":"visible answer"}
        ]}]}"#,
    );
    assert_eq!(
        out["messages"],
        json!([{"role": "assistant", "content": "visible answer"}])
    );
}

const TWO_CALLS: &str = r#"{"contents": [
    {"role": "model", "parts": [
        {"functionCall": {"name": "read_file", "args": {"path": "main.go"}}},
        {"functionCall": {"name": "grep", "args": {"pattern": "TODO"}}}
    ]},
    {"role": "function", "parts": [
        {"functionResponse": {"name": "read_file", "response": {"result": "code"}}},
        {"functionResponse": {"name": "grep", "response": {"result": "matches"}}}
    ]}
]}"#;

#[test]
fn deterministic_tool_call_ids() {
    let first = convert(TWO_CALLS);
    let call0 = text_at(&first, "messages.0.tool_calls.0.id");
    let call1 = text_at(&first, "messages.0.tool_calls.1.id");
    assert!(call0.starts_with("call_") && call1.starts_with("call_"));
    assert_eq!(text_at(&first, "messages.1.tool_call_id"), call0);
    assert_eq!(text_at(&first, "messages.2.tool_call_id"), call1);
    for _ in 0..3 {
        assert_eq!(convert(TWO_CALLS), first);
    }
}

#[test]
fn same_name_calls_in_same_message_distinct() {
    let out = convert(
        r#"{"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}},
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}
            ]},
            {"role": "function", "parts": [
                {"functionResponse": {"name": "read_file", "response": {"result": "first"}}},
                {"functionResponse": {"name": "read_file", "response": {"result": "second"}}}
            ]}
        ]}"#,
    );
    let id0 = text_at(&out, "messages.0.tool_calls.0.id");
    let id1 = text_at(&out, "messages.0.tool_calls.1.id");
    assert_ne!(id0, id1);
    assert_eq!(text_at(&out, "messages.1.tool_call_id"), id0);
    assert_eq!(text_at(&out, "messages.2.tool_call_id"), id1);
}

#[test]
fn interleaved_per_name_fifo_matching() {
    let out = convert(
        r#"{"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "tool_a", "args": {"step": 1}}},
                {"functionCall": {"name": "tool_b", "args": {"step": 1}}},
                {"functionCall": {"name": "tool_a", "args": {"step": 2}}},
                {"functionCall": {"name": "tool_b", "args": {"step": 2}}}
            ]},
            {"role": "function", "parts": [
                {"functionResponse": {"name": "tool_b", "response": {"step": 1}}},
                {"functionResponse": {"name": "tool_a", "response": {"step": 1}}},
                {"functionResponse": {"name": "tool_b", "response": {"step": 2}}},
                {"functionResponse": {"name": "tool_a", "response": {"step": 2}}}
            ]}
        ]}"#,
    );
    let call = |i: usize| text_at(&out, &format!("messages.0.tool_calls.{i}.id"));
    let response = |i: usize| text_at(&out, &format!("messages.{i}.tool_call_id"));
    assert_eq!(response(1), call(1));
    assert_eq!(response(2), call(0));
    assert_eq!(response(3), call(3));
    assert_eq!(response(4), call(2));
}

#[test]
fn deterministic_fallback_orphan_response() {
    let request = r#"{"contents": [{"role": "function", "parts": [
        {"functionResponse": {"name": "orphan_tool", "response": {"result": "standalone"}}}
    ]}]}"#;
    let first = text_at(&convert(request), "messages.0.tool_call_id");
    assert!(first.starts_with("call_"));
    for _ in 0..3 {
        assert_eq!(text_at(&convert(request), "messages.0.tool_call_id"), first);
    }
}

#[test]
fn explicit_call_inherited_by_implicit_response() {
    let out = convert(
        r#"{"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "lookup", "id": "explicit_call_1", "args": {"q": "foo"}}}
            ]},
            {"role": "function", "parts": [
                {"functionResponse": {"name": "lookup", "response": {"result": "bar"}}}
            ]}
        ]}"#,
    );
    assert_eq!(
        text_at(&out, "messages.0.tool_calls.0.id"),
        "explicit_call_1"
    );
    assert_eq!(text_at(&out, "messages.1.tool_call_id"), "explicit_call_1");
}

#[test]
fn out_of_order_explicit_response_does_not_duplicate_id() {
    let out = convert(
        r#"{"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "foo", "id": "call_1", "args": {"n": 1}}},
                {"functionCall": {"name": "foo", "id": "call_2", "args": {"n": 2}}},
                {"functionCall": {"name": "foo", "id": "call_3", "args": {"n": 3}}}
            ]},
            {"role": "function", "parts": [
                {"functionResponse": {"name": "foo", "id": "call_2", "response": {"r": 2}}},
                {"functionResponse": {"name": "foo", "response": {"r": 1}}},
                {"functionResponse": {"name": "foo", "response": {"r": 3}}}
            ]}
        ]}"#,
    );
    assert_eq!(text_at(&out, "messages.1.tool_call_id"), "call_2");
    assert_eq!(text_at(&out, "messages.2.tool_call_id"), "call_1");
    assert_eq!(text_at(&out, "messages.3.tool_call_id"), "call_3");
}

#[test]
fn builds_the_whole_request_in_upstream_order() {
    // Upstream's output for this request, IDs included.
    let request: Value = serde_json::from_str(
        r#"{"systemInstruction":{"parts":[{"text":"be brief"},{"thought":true,"text":"x"},{"fileData":{"mimeType":"text/plain","fileUri":"gs://b/n.txt"}}]},"contents":[{"role":"user","parts":[{"text":"hi"},{"fileData":{"mimeType":"image/png","fileUri":"https://x/i.png"}}]},{"role":"model","parts":[{"text":"calling"},{"functionCall":{"name":"lookup","args":{"q":"x","n":1}}}]},{"role":"function","parts":[{"functionResponse":{"name":"lookup","response":{"content":"done"}}},{"functionResponse":{"name":"other","response":{"r":[1,2]}}}]}],"generationConfig":{"temperature":0.5,"maxOutputTokens":100,"topP":1,"topK":40,"stopSequences":["END",7],"candidateCount":2,"responseModalities":["TEXT"," Image ","video"],"thinkingConfig":{"thinkingBudget":2048}},"service_tier":"flex","tools":[{"functionDeclarations":[{"name":"lookup","description":"Look up","parameters":{"type":"object","properties":{"q":{"type":"string"}}}},{"name":"bare"}]},{"googleSearch":{}}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup"]}}}"#,
    )
    .unwrap();
    let want: Value = serde_json::from_str(
        r#"{"model":"gpt-x","messages":[{"role":"system","content":[{"type":"text","text":"be brief"},{"type":"file","file":{"filename":"document.txt","file_url":"gs://b/n.txt"}}]},{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"https://x/i.png"}}]},{"role":"assistant","content":"calling","tool_calls":[{"id":"call_ce09959058b5d08d94fab001","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\",\"n\":1}"}}]},{"role":"tool","tool_call_id":"call_ce09959058b5d08d94fab001","content":"\"done\""},{"role":"tool","tool_call_id":"call_1b2a4328edc2486cee2d34a3","content":"{\"r\":[1,2]}"},{"role":"function","content":""}],"temperature":0.5,"max_tokens":100,"top_p":1,"top_k":40,"stop":["END","7"],"n":2,"modalities":["text","image"],"reasoning_effort":"medium","stream":true,"service_tier":"flex","tools":[{"type":"function","function":{"name":"lookup","description":"Look up","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}},{"type":"function","function":{"name":"bare","description":""}}],"tool_choice":{"type":"function","function":{"name":"lookup"}}}"#,
    )
    .unwrap();
    let out = convert_gemini_request_to_openai("gpt-x", &request, true);
    assert_eq!(out.to_string(), want.to_string());
}

#[test]
fn reasoning_effort_from_level_or_budget() {
    let effort = |thinking: &str| {
        let out = convert(&format!(
            r#"{{"generationConfig":{{"thinkingConfig":{thinking}}}}}"#
        ));
        out.get("reasoning_effort").cloned()
    };
    assert_eq!(effort(r#"{"thinkingLevel":" HIGH "}"#), Some(json!("high")));
    assert_eq!(effort(r#"{"thinking_level":"low"}"#), Some(json!("low")));
    // A blank level sets nothing, and the budget isn't read.
    assert_eq!(
        effort(r#"{"thinkingLevel":" ","thinkingBudget":2048}"#),
        None
    );
    assert_eq!(effort(r#"{"thinking_budget":0}"#), Some(json!("none")));
    assert_eq!(effort(r#"{"thinkingBudget":-1}"#), Some(json!("auto")));
    assert_eq!(effort("[]"), None);
}

#[test]
fn tool_choice_modes() {
    let choice = |config: &str| {
        let out = convert(&format!(
            r#"{{"toolConfig":{{"functionCallingConfig":{config}}}}}"#
        ));
        out.get("tool_choice").cloned()
    };
    assert_eq!(choice(r#"{"mode":"NONE"}"#), Some(json!("none")));
    assert_eq!(choice(r#"{"mode":"AUTO"}"#), Some(json!("auto")));
    assert_eq!(choice(r#"{"mode":"ANY"}"#), Some(json!("required")));
    assert_eq!(
        choice(r#"{"mode":"ANY","allowedFunctionNames":["a","b"]}"#),
        Some(json!("required"))
    );
    assert_eq!(choice(r#"{"mode":"VALIDATED"}"#), None);
}

#[test]
fn file_data_and_inline_data_parts() {
    let out = convert(
        r#"{"contents":[{"role":"user","parts":[
            {"fileData":{"mimeType":"video/mp4","fileUri":"gs://v.mp4"}},
            {"fileData":{"mimeType":"application/json","fileUri":"gs://d.json"}},
            {"fileData":{"mimeType":"audio/mpeg","fileUri":"gs://a.mp3"}},
            {"inlineData":{"data":"AAAA"}},
            {"inlineData":{"mimeType":"audio/mpeg","data":"AAAA"}},
            {"inlineData":{"mimeType":"image/png"}}
        ]}]}"#,
    );
    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type": "video_url", "video_url": {"url": "gs://v.mp4"}},
            {"type": "file", "file": {"filename": "document.json", "file_url": "gs://d.json"}},
            {"type": "text", "text": "File: gs://a.mp3 (Type: audio/mpeg)"},
            {"type": "file", "file": {"filename": "document", "file_data": "AAAA"}},
            {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "mp3"}}
        ])
    );
}
