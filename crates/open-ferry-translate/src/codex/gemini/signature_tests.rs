// Ported from CLIProxyAPI internal/translator/codex/gemini/*_test.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI
use serde_json::{Value, json};
use super::{CodexToGeminiStream, convert_gemini_request_to_codex, convert_codex_response_to_gemini_non_stream, signature};
use base64::{Engine as _, engine::general_purpose::STANDARD};

fn sig() -> String { format!("Q-PaDgE{}", "A".repeat(1419)) }
fn send(stream: &mut CodexToGeminiStream, event: Value) -> Vec<Value> {
    stream.translate_line(format!("data: {event}").as_bytes())
}

#[test]
fn sdk_carrier_roundtrip_is_target_gated_and_deduplicated() {
    let native = sig();
    for summary in [json!([]), json!([{"type":"summary_text","text":"hidden"}])] {
        let event = json!({"type":"response.completed","response":{"output":[{"type":"reasoning","encrypted_content":native,"summary":summary}]}});
        let out = convert_codex_response_to_gemini_non_stream("muse-spark", &Value::Null, &event).unwrap();
        let part = &out["candidates"][0]["content"]["parts"][0];
        let carrier = part["thoughtSignature"].as_str().unwrap();
        let sdk_bytes = STANDARD.decode(carrier).unwrap();
        assert_eq!(STANDARD.encode(sdk_bytes), carrier);
        assert_eq!(signature::decode(carrier), native);
        let request = json!({"contents":[{"role":"model","parts":[part,part]}]});
        for model in ["openai-compatible/muse-spark", "gpt-5", "grok-4", "claude-sonnet"] {
            let request = convert_gemini_request_to_codex(model, &request);
            let reasoning: Vec<_> = request["input"].as_array().unwrap().iter().filter(|p| p["type"] == "reasoning").collect();
            assert_eq!(reasoning.len(), usize::from(model.contains("muse")));
            if let Some(item) = reasoning.first() { assert_eq!(item["encrypted_content"], native); }
        }
    }
}

#[test]
fn stream_preserves_calls_images_and_empty_delta_signature() {
    let mut stream = CodexToGeminiStream::new("muse-spark", &Value::Null);
    let reasoning = json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"r","encrypted_content":sig()}});
    assert!(send(&mut stream, reasoning.clone()).is_empty());
    assert!(send(&mut stream, reasoning).is_empty());
    let empty = send(&mut stream, json!({"type":"response.output_text.delta","delta":""}));
    assert!(empty[0]["candidates"][0]["content"]["parts"][0].get("thoughtSignature").is_none());
    for name in ["first", "second"] {
        assert!(send(&mut stream, json!({"type":"response.output_item.done","item":{"type":"function_call","name":name,"call_id":name,"arguments":"{}"}})).is_empty());
    }
    let chunks = send(&mut stream, json!({"type":"response.image_generation_call.partial_image","item_id":"image","partial_image_b64":"AAAA"}));
    assert_eq!(chunks.len(), 3);
    let parts: Vec<_> = chunks.iter().map(|c| &c["candidates"][0]["content"]["parts"][0]).collect();
    assert_eq!(parts[0]["functionCall"]["name"], "first");
    assert_eq!(parts[1]["functionCall"]["name"], "second");
    assert_eq!(signature::decode(parts[0]["thoughtSignature"].as_str().unwrap()), sig());
    assert!(parts[2].get("inlineData").is_some());
}
