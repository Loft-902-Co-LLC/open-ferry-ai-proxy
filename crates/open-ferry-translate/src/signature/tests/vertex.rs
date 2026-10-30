//! Final v8.0.47 bare-Tink recognition and endpoint replay matrix.
use super::*;
use serde_json::json;

fn bare(len: usize) -> Vec<u8> {
    let mut bytes = vec![0xa5; len];
    for (to, from) in bytes.iter_mut().zip([1, 0x8f, 0x3d, 0x6b, 0x5f]) { *to = from; }
    bytes
}

#[test]
fn vertex_signature_recognition_and_destination_table() {
    let mut unknown = bare(405);
    unknown[1] = 0x69;
    let mut random = vec![0x44; 256];
    random[0] = 1;
    let opaque = |len| {
        let mut bytes = STANDARD_NO_PAD.decode(synthesize_kimi_signature(len, 47)).unwrap();
        bytes[0] = 1;
        STANDARD_NO_PAD.encode(bytes)
    };
    for (name, sig, recognized, bare_tink) in [
        ("vertex", STANDARD.encode(bare(405)), true, true),
        ("floor", STANDARD_NO_PAD.encode(bare(21)), true, true),
        ("prefixed", format!("gemini#{}", STANDARD.encode(bare(405))), true, true),
        ("unknown key", STANDARD.encode(unknown), false, false),
        ("short", STANDARD.encode(bare(20)), false, false),
        ("prefix only", STANDARD.encode([1]), false, false),
        ("random", STANDARD.encode(random), false, false),
        ("xai", opaque(256), false, false),
        ("kimi", opaque(3255), false, false),
        ("wrapped", test_gemini_thought_signature_envelope(), true, false),
    ] {
        assert_eq!(detect_signature_provider(&sig) == Provider::Gemini, recognized, "{name}");
        assert_eq!(is_gemini_vertex_bare_tink_signature(&sig), bare_tink, "{name}");
        if name == "xai" { assert!(is_valid_grok_encrypted_content(&sig)); }
        if name == "kimi" { assert_eq!(detect_signature_provider(&sig), Provider::Kimi); }
        for target in ["vertex", "gemini", "interactions", "claude", "openai"] {
            if target == "claude" || target == "openai" {
                let provider = if target == "claude" { Provider::Claude } else { Provider::Gpt };
                let decision = decide_signature_compatibility(provider, &sig, BlockKind::ClaudeThinking);
                assert_eq!(decision.action, Action::DropBlock, "{name}/{target}");
                continue;
            }
            let mut body = json!({"contents":[{"role":"model","parts":[
                {"thought":true,"text":"reason","thoughtSignature":sig},
                {"functionCall":{"name":"a","args":{}},"thoughtSignature":sig},
                {"thought":true,"text":"parallel thought"},
                {"functionCall":{"name":"b","args":{}},"thoughtSignature":sig},
                {"text":"boundary"},
                {"functionCall":{"name":"c","args":{}},"thoughtSignature":sig},
                {"functionResponse":{"name":"c","response":{}},"thoughtSignature":sig}
            ]}]});
            if target == "vertex" { sanitize_gemini_request_thought_signatures(&mut body, "contents"); }
            else { sanitize_non_vertex_gemini_request_thought_signatures(&mut body, "contents"); }
            let parts = body["contents"][0]["parts"].as_array().unwrap();
            let keep = recognized && (target == "vertex" || !bare_tink);
            let normalized = signature_payload_without_provider_prefix(&sig);
            for index in [0, 3] {
                assert_eq!(parts[index].get("thoughtSignature").and_then(|s| s.as_str()), keep.then_some(normalized), "{name}/{target}/{index}");
            }
            for index in [1, 5] {
                assert_eq!(parts[index]["thoughtSignature"], if keep { normalized } else { GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR }, "{name}/{target}/{index}");
            }
            assert!(parts[6].get("thoughtSignature").is_none());
            assert!(validate_gemini_thought_signatures(&body, GeminiValidationOptions { allow_bypass_sentinel:true, require_known_envelope:true, ..Default::default() }).is_ok());
            let changed = if target == "vertex" { sanitize_gemini_request_thought_signatures(&mut body, "contents") } else { sanitize_non_vertex_gemini_request_thought_signatures(&mut body, "contents") };
            assert!(!changed, "{name}/{target}: must be idempotent");
        }
    }
    for len in 0..21 { assert!(!is_gemini_vertex_bare_tink_signature(&STANDARD.encode(bare(len)))); }
}
