// Ported from CLIProxyAPI internal/translator/openai/gemini/*_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI
use super::*;
use serde_json::{Value, json};

#[test]
fn function_signatures_roundtrip_all_supported_locations() {
    for key in [
        "thoughtSignature",
        "thought_signature",
        "functionCall.thoughtSignature",
        "functionCall.thought_signature",
        "extra_content.google.thought_signature",
    ] {
        let mut part = json!({"functionCall":{"id":"call","name":"lookup","args":{"x":1}}});
        let mut node = &mut part;
        for component in key.split('.') {
            node = &mut node[component];
        }
        *node = "signature".into();
        let request = json!({"contents":[{"role":"model","parts":[part]}]});
        let out = convert_gemini_request_to_openai("model", &request, false);
        let call = &out["messages"][0]["tool_calls"][0];
        assert_eq!(
            call["extra_content"]["google"]["thought_signature"], "signature",
            "{key}"
        );
        let response =
            json!({"choices":[{"message":{"tool_calls":[call]},"finish_reason":"tool_calls"}]})
                .to_string();
        let result = convert_openai_response_to_gemini_non_stream(response.as_bytes());
        assert_eq!(
            result["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
            "signature"
        );
    }
}

#[test]
fn mixed_stream_keeps_signature_only_deltas_and_orders_calls() {
    let mut stream = OpenAIToGeminiStream::new();
    let send = |stream: &mut OpenAIToGeminiStream, event: Value| {
        stream.translate_line(event.to_string().as_bytes())
    };
    let chunks = send(
        &mut stream,
        json!({"choices":[{"delta":{"content":"visible","tool_calls":[{"index":1,"function":{"name":"second","arguments":"{}"}},{"index":0,"function":{"name":"first","arguments":"{}"}}]}}]}),
    );
    assert_eq!(
        chunks[0]["candidates"][0]["content"]["parts"][0]["text"],
        "visible"
    );
    let chunks = send(
        &mut stream,
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"extra_content":{"google":{"thought_signature":"late"}}}]},"finish_reason":"tool_calls"}]}),
    );
    assert_eq!(chunks.len(), 1);
    let parts = &chunks[0]["candidates"][0]["content"]["parts"];
    assert_eq!(parts[0]["functionCall"]["name"], "first");
    assert_eq!(parts[0]["thoughtSignature"], "late");
    assert_eq!(parts[1]["functionCall"]["name"], "second");
}
