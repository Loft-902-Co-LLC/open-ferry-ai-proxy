// Ported from CLIProxyAPI internal/signature/gemini_sanitize_test.go and gemini_validation_test.go (MIT).
// https://github.com/router-for-me/CLIProxyAPI

use open_ferry_translate::signature::{
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, GeminiValidationOptions,
    sanitize_gemini_request_thought_signatures, validate_gemini_thought_signatures,
};
use serde_json::json;

#[test]
fn text_carriers_restart_signature_groups_and_thoughts_do_not() {
    let options = GeminiValidationOptions {
        allow_bypass_sentinel: true,
        ..Default::default()
    };
    for separator in [
        json!({"text":"more"}),
        json!({"text":""}),
        json!({"text":"hidden","thought":true}),
    ] {
        let mut payload = json!({"contents":[{"role":"model","parts":[
            {"functionCall":{"name":"first","args":{}},"thoughtSignature":GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR},
            separator, {"functionCall":{"name":"second","args":{}}},
            {"functionCall":{"name":"parallel","args":{}}}]}]});
        let thought = separator.get("thought") == Some(&json!(true));
        assert_eq!(
            validate_gemini_thought_signatures(&payload, options).is_ok(),
            thought
        );
        assert_eq!(
            sanitize_gemini_request_thought_signatures(&mut payload, "contents"),
            !thought
        );
        let parts = &payload["contents"][0]["parts"];
        assert_eq!(parts[2].get("thoughtSignature").is_some(), !thought);
        assert!(parts[3].get("thoughtSignature").is_none());
        validate_gemini_thought_signatures(&payload, options).unwrap();
        assert!(!sanitize_gemini_request_thought_signatures(
            &mut payload,
            "contents"
        ));
    }
}

#[test]
fn sanitizer_signs_the_call_after_an_empty_text_carrier() {
    let mut payload = json!({"contents":[{"role":"model","parts":[
        {"functionCall":{"name":"first","args":{}},"thoughtSignature":GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR},
        {"text":""}, {"functionCall":{"name":"second","args":{}}},
        {"functionCall":{"name":"parallel","args":{}}}]}]});
    assert!(sanitize_gemini_request_thought_signatures(
        &mut payload,
        "contents"
    ));
    assert_eq!(
        payload["contents"][0]["parts"][2]["thoughtSignature"],
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
    );
    assert!(
        payload["contents"][0]["parts"][3]
            .get("thoughtSignature")
            .is_none()
    );
}
