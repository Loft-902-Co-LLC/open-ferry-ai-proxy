// Ported from CLIProxyAPI internal/signature/gemini_validation_test.go (v8.0.10,
// MIT). https://github.com/router-for-me/CLIProxyAPI

use super::*;

fn require_known_envelope() -> GeminiValidationOptions {
    GeminiValidationOptions {
        require_known_envelope: true,
        ..Default::default()
    }
}

fn allow_bypass_sentinel() -> GeminiValidationOptions {
    GeminiValidationOptions {
        allow_bypass_sentinel: true,
        ..Default::default()
    }
}

#[test]
fn inspect_gemini_thought_signature_accepts_opaque_base64() {
    let sig = test_gemini_thought_signature(&[0x12, 0x34, 0x56]);

    let info = inspect_gemini_thought_signature(&sig, GeminiValidationOptions::default())
        .unwrap_or_else(|err| panic!("inspect_gemini_thought_signature failed: {err}"));
    assert!(
        !info.is_bypass_sentinel,
        "real signature should not be marked as bypass sentinel"
    );
    assert_eq!(info.decoded_len, 3, "decoded_len");
    assert_eq!(info.first_byte, 0x12, "first_byte");
    assert!(
        info.has_observed_marker,
        "has_observed_marker should be true"
    );
    assert_eq!(info.envelope, Some(GeminiEnvelope::Unknown), "envelope");
    assert!(
        !info.known_envelope,
        "known_envelope should be false for incomplete opaque payload"
    );
}

// Shape observed in CPA-API/signatures/gemini/gemini-3.1-pro.txt.
#[test]
fn inspect_gemini_thought_signature_accepts_gemini31_pro_field2_envelope() {
    let sig = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39, 0xd6, 0xc7, 0x34]);

    let info = inspect_gemini_thought_signature(&sig, require_known_envelope())
        .unwrap_or_else(|err| panic!("Gemini 3.1 Pro field-2 envelope should be known: {err}"));
    assert_eq!(
        info.envelope,
        Some(GeminiEnvelope::ProtobufField2),
        "envelope"
    );
    assert!(
        info.has_observed_marker,
        "Gemini 3.1 Pro envelope should be marked as 0x12"
    );
    assert_eq!(info.record_count, 1, "record_count");
    assert_eq!(info.opaque_payload_len, 6, "opaque_payload_len");
}

// Captured in CPA-API/signatures/gemini/gemini-3.1-flash-lite.txt.
#[test]
fn inspect_gemini_thought_signature_accepts_captured_gemini31_flash_lite_envelope() {
    const SIG: &str = "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

    let info =
        inspect_gemini_thought_signature(SIG, require_known_envelope()).unwrap_or_else(|err| {
            panic!("captured Gemini 3.1 Flash Lite envelope should be known: {err}")
        });
    assert_eq!(
        info.envelope,
        Some(GeminiEnvelope::ProtobufField2),
        "envelope"
    );
    assert_eq!(info.record_count, 1, "record_count");
    assert_eq!(info.opaque_payload_len, 50, "opaque_payload_len");
}

// A live capture from a Gemini 3.8 Flash toolCall (googleSearch).
#[test]
fn inspect_gemini_thought_signature_accepts_server_side_tool_protobuf_envelope() {
    const LIVE_CAPTURED_TOOL_CALL_SIG: &str = "ErUDCrIDCAISrQMBEU0yD9ECvDhSY1DQJNUGafArdfd2mDfO8VQq7XjLx/91zESuo0QPSdkRFWkLeVIocSQmQULonYMOJcs6XDLV2LTRC9myb3MCCP9CUoWbEeqhAvXKTScyS3nwBDDVJYuDDbY3YvR4V86T/DnU3qufpaVZ3wQOiJVyBVZ515dYTN+XGq7SuUc3RpfAqVU06jgxaCM0WKV4Df5mGMJWb25e/aFG2Jc7upSqpf3n6aElj+4c/eWr4GdKd0TUIElXBZ0HEN/vNcWzD3F0S4MeVbk1LDakL6HG6oyaSS2gocxYNYxqm9mdMHaXYa4mIYqWqmqBEnbgcHp8H4fgqBxc3Cx8C3otV8IarO5OALaVDA3NaXB1zjLet1587kEpkCNr9OvrYOES2nCl/i4EgbPK01nlXo+Wwm5jsZU5nEG4/Z0bErzqC5TKwOsqpJ7afL2sPWI0IGrXhXL+QCumWCS5iUtwybSkL7CYSk9GC+iY+ev6FAmC4V5JEc4OaWOc9+m/29LniN/iPTSxtUQSZT94pUa3/irIIdH7ReAS3cpeM6OTvumR1PwNxXx3XM1mEGc=";

    let info =
        inspect_gemini_thought_signature(LIVE_CAPTURED_TOOL_CALL_SIG, require_known_envelope())
            .unwrap_or_else(|err| {
                panic!(
                    "captured Gemini 3.8 Flash server-side toolCall envelope should be known: {err}"
                )
            });
    assert_eq!(
        info.envelope,
        Some(GeminiEnvelope::ProtobufField2),
        "envelope"
    );
    assert!(
        info.known_envelope,
        "known_envelope should be true for server-side toolCall envelope"
    );
    assert_eq!(
        detect_signature_provider_for_block(
            LIVE_CAPTURED_TOOL_CALL_SIG,
            BlockKind::GeminiModelPart
        ),
        Provider::Gemini,
        "detect_signature_provider_for_block"
    );
}

#[test]
fn inspect_gemini_thought_signature_rejects_malformed_tool_invocation_payload() {
    let is_known = |sig: &str| {
        inspect_gemini_thought_signature(sig, require_known_envelope())
            .is_ok_and(|info| info.known_envelope)
    };

    // 1. Truncated protobuf: declares 16 bytes, only gives 1.
    let truncated = test_gemini3_thought_signature(&[0x08, 0x02, 0x12, 0x10, 0x01]);
    assert!(
        !is_known(&truncated),
        "truncated inner protobuf payload should not be a known envelope"
    );

    // 2. Valid protobuf without the Tink 0x01 prefix in any bytes field.
    let no_tink = test_gemini3_thought_signature(&[0x08, 0x02, 0x12, 0x04, 0x99, 0x98, 0x97, 0x96]);
    assert!(
        !is_known(&no_tink),
        "protobuf payload without Tink 0x01 prefix should not be a known envelope"
    );

    // 3. Protobuf with pure varints, no bytes field.
    let pure_varint = test_gemini3_thought_signature(&[0x08, 0x02, 0x10, 0x05]);
    assert!(
        !is_known(&pure_varint),
        "protobuf payload without length-delimited bytes field should not be a known envelope"
    );
}

#[test]
fn validate_gemini_function_call_pairing_accepts_server_side_tool_blocks() {
    let input = json(
        r#"{
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"toolCall": {"id": "search_1", "toolType": "GOOGLE_SEARCH_WEB"}},
                        {"toolResponse": {"id": "search_1", "response": {}}},
                        {"functionCall": {"id": "call_1", "name": "do_something", "args": {}}}
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        {"functionResponse": {"id": "call_1", "name": "do_something", "response": {"result": "ok"}}}
                    ]
                }
            ]
        }"#,
    );
    if let Err(err) = validate_gemini_function_call_pairing(&input) {
        panic!(
            "pairing validator should accept server-side tool blocks alongside functionCall: {err}"
        );
    }
}

#[test]
fn inspect_gemini_thought_signature_accepts_gemini3_wrapped_uuid_envelope() {
    const PROVIDER_UUID: &str = "e24830a7-5cd6-42fe-998b-ee539e72b9c3";
    let sig = test_gemini3_thought_signature(PROVIDER_UUID.as_bytes());

    let info = inspect_gemini_thought_signature(&sig, require_known_envelope())
        .unwrap_or_else(|err| panic!("Gemini 3 wrapped UUID envelope should be known: {err}"));
    assert_eq!(
        info.envelope,
        Some(GeminiEnvelope::ProtobufField2),
        "envelope"
    );
    assert_eq!(info.record_count, 1, "record_count");
    assert_eq!(
        info.opaque_payload_len,
        PROVIDER_UUID.len(),
        "opaque_payload_len"
    );
    assert_eq!(
        detect_signature_provider_for_block(&sig, BlockKind::GeminiFunctionCall),
        Provider::Gemini,
        "provider"
    );
}

// The repeated field-1 envelope was removed. Gemini 2.5 is out of scope, so its
// signatures are no longer a known envelope; on Gemini model parts they degrade
// to the bypass sentinel instead of being replayed verbatim.
#[test]
fn inspect_gemini_thought_signature_rejects_gemini25_field1_envelope() {
    let sig = test_gemini25_thought_signature(&[&[0x01, 0x8f], &[0x01, 0x90, 0x91]]);

    assert!(
        inspect_gemini_thought_signature(&sig, require_known_envelope()).is_err(),
        "Gemini 2.5 field-1 envelope should no longer be a known envelope"
    );

    let info = inspect_gemini_thought_signature(&sig, GeminiValidationOptions::default())
        .unwrap_or_else(|err| {
            panic!("inspection without require_known_envelope should still succeed: {err}")
        });
    assert_eq!(info.envelope, Some(GeminiEnvelope::Unknown), "envelope");
    assert!(
        !info.known_envelope,
        "known_envelope should be false for the retired field-1 envelope"
    );

    // Gemini model parts still recover through the documented sentinel.
    let decision =
        decide_signature_compatibility(Provider::Gemini, &sig, BlockKind::GeminiModelPart);
    assert_eq!(decision.action, Action::ReplaceWithGeminiBypass, "action");
    assert_eq!(
        decision.replacement_signature, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "replacement"
    );
}

// Field 2 holding field 1 is not enough. Observed Gemini 3 payloads wrap an
// opaque blob that starts with internal version byte 0x01.
#[test]
fn inspect_gemini_thought_signature_rejects_malformed_known_envelope() {
    let sig = test_gemini3_thought_signature(&[0x02, 0x0c, 0x39]);

    assert!(
        !is_valid_gemini_thought_signature(&sig, require_known_envelope()),
        "malformed Gemini 3 envelope should fail known-envelope validation"
    );
}

#[test]
fn inspect_gemini_thought_signature_classifies_ascii_uuid_as_opaque() {
    let sig = test_gemini_thought_signature(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");

    let info = inspect_gemini_thought_signature(&sig, GeminiValidationOptions::default())
        .unwrap_or_else(|err| panic!("opaque base64 UUID should pass default validation: {err}"));
    assert_eq!(info.envelope, Some(GeminiEnvelope::AsciiUuid), "envelope");
    assert!(
        !info.known_envelope,
        "base64 UUID should not be a known protobuf envelope"
    );
    assert!(
        !is_valid_gemini_thought_signature(&sig, require_known_envelope()),
        "base64 UUID should fail when known envelope is required"
    );
}

#[test]
fn inspect_gemini_thought_signature_observed_marker_option() {
    let sig = test_gemini_thought_signature(&[0x45, 0x12]);

    if let Err(err) = inspect_gemini_thought_signature(&sig, GeminiValidationOptions::default()) {
        panic!("default validation should accept opaque base64 payload: {err}");
    }
    let err = inspect_gemini_thought_signature(
        &sig,
        GeminiValidationOptions {
            require_observed_marker: true,
            ..Default::default()
        },
    )
    .expect_err("require_observed_marker should reject payloads without 0x12 marker")
    .to_string();
    assert!(
        err.contains("expected observed marker"),
        "unexpected error: {err}"
    );
}

#[test]
fn inspect_gemini_thought_signature_bypass_sentinel_requires_option() {
    assert!(
        !is_valid_gemini_thought_signature(
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
            GeminiValidationOptions::default()
        ),
        "bypass sentinel should not be valid by default"
    );

    let info = inspect_gemini_thought_signature(
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        allow_bypass_sentinel(),
    )
    .unwrap_or_else(|err| {
        panic!("bypass sentinel should be accepted when explicitly allowed: {err}")
    });
    assert!(
        info.is_bypass_sentinel,
        "sentinel should be marked as bypass"
    );
    assert_eq!(
        info.bypass_sentinel, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "bypass_sentinel"
    );
}

#[test]
fn inspect_gemini_thought_signature_rejects_invalid_base64() {
    assert!(
        !is_valid_gemini_thought_signature(
            "not valid base64!!!",
            GeminiValidationOptions::default()
        ),
        "invalid base64 should be rejected"
    );
}

#[test]
fn validate_gemini_thought_signatures_first_function_call_requires_signature() {
    let input = json(
        r#"{
            "contents": [{
                "role": "model",
                "parts": [
                    {"functionCall": {"id": "call-1", "name": "read_file", "args": {}}}
                ]
            }]
        }"#,
    );

    let err = validate_gemini_thought_signatures(&input, GeminiValidationOptions::default())
        .expect_err("missing first functionCall thoughtSignature should fail")
        .to_string();
    assert!(
        err.contains("missing thoughtSignature on first functionCall"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_gemini_thought_signatures_allows_unsigned_parallel_sibling() {
    let input = json(
        r#"{
            "contents": [{
                "role": "model",
                "parts": [
                    {
                        "functionCall": {"id": "call-1", "name": "read_file", "args": {}},
                        "thoughtSignature": "skip_thought_signature_validator"
                    },
                    {"functionCall": {"id": "call-2", "name": "read_file", "args": {}}}
                ]
            }]
        }"#,
    );

    if let Err(err) = validate_gemini_thought_signatures(&input, allow_bypass_sentinel()) {
        panic!("unsigned parallel sibling should be valid: {err}");
    }
}

#[test]
fn validate_gemini_thought_signatures_rejects_sentinel_outside_first_function_call() {
    let cases = [
        (
            "parallel sibling",
            r#"[
                {"functionCall":{"name":"first","args":{}},"thoughtSignature":"skip_thought_signature_validator"},
                {"functionCall":{"name":"second","args":{}},"thoughtSignature":"skip_thought_signature_validator"}
            ]"#,
        ),
        (
            "thought part",
            r#"[{"text":"hidden","thought":true,"thoughtSignature":"skip_thought_signature_validator"}]"#,
        ),
    ];
    for (name, parts) in cases {
        let input = json(&format!(
            r#"{{"contents":[{{"role":"model","parts":{parts}}}]}}"#
        ));
        let result = validate_gemini_thought_signatures(&input, allow_bypass_sentinel());
        assert!(
            matches!(&result, Err(err) if err.to_string().contains("allowed only on the first model functionCall")),
            "{name}: unexpected result: {result:?}"
        );
    }
}

#[test]
fn validate_gemini_thought_signatures_rejects_non_canonical_nested_signature() {
    let signature = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    let input = json(&format!(
        r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}},"thoughtSignature":"{signature}"}}}}]}}]}}"#
    ));

    let result = validate_gemini_thought_signatures(&input, GeminiValidationOptions::default());
    assert!(
        matches!(&result, Err(err) if err.to_string().contains("canonical top-level field")),
        "unexpected result: {result:?}"
    );
}

#[test]
fn validate_gemini_thought_signatures_accepts_wrapped_request_and_sentinel_when_allowed() {
    let input = json(
        r#"{
            "request": {
                "contents": [{
                    "role": "model",
                    "parts": [
                        {
                            "functionCall": {"id": "call-1", "name": "read_file", "args": {}},
                            "thoughtSignature": "skip_thought_signature_validator"
                        }
                    ]
                }]
            }
        }"#,
    );

    if let Err(err) = validate_gemini_thought_signatures(&input, allow_bypass_sentinel()) {
        panic!("sentinel should be valid when explicitly allowed: {err}");
    }
}

#[test]
fn validate_gemini_thought_signatures_rejects_invalid_text_part_signature() {
    let input = json(
        r#"{
            "contents": [{
                "role": "model",
                "parts": [
                    {"text": "previous answer", "thoughtSignature": "bad!!!"}
                ]
            }]
        }"#,
    );

    let err = validate_gemini_thought_signatures(&input, GeminiValidationOptions::default())
        .expect_err("invalid text-part thoughtSignature should fail")
        .to_string();
    assert!(
        err.contains("base64 decode failed"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_gemini_function_call_pairing_valid_parallel_group() {
    let input = json(
        r#"{
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"id": "call-1", "name": "weather", "args": {"city": "Paris"}}},
                        {"functionCall": {"id": "call-2", "name": "weather", "args": {"city": "London"}}}
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        {"functionResponse": {"id": "call-1", "name": "weather", "response": {"temp": "15C"}}},
                        {"functionResponse": {"id": "call-2", "name": "weather", "response": {"temp": "12C"}}}
                    ]
                }
            ]
        }"#,
    );

    if let Err(err) = validate_gemini_function_call_pairing(&input) {
        panic!("valid pairing failed: {err}");
    }
}

#[test]
fn validate_gemini_function_call_pairing_allows_user_boundary_before_response() {
    let payload = json(
        r#"{"contents":[{"role":"model","parts":[{"functionCall":{"id":"call-1","name":"run","args":{}}}]},{"role":"user","parts":[{"text":"boundary"}]},{"role":"user","parts":[{"functionResponse":{"id":"call-1","name":"run","response":{"result":"ok"}}}]}]}"#,
    );
    if let Err(err) = validate_gemini_function_call_pairing(&payload) {
        panic!("user boundary before function response should be accepted: {err}");
    }
}

#[test]
fn validate_gemini_function_call_pairing_rejects_model_boundary_before_response() {
    let payload = json(
        r#"{"contents":[{"role":"model","parts":[{"functionCall":{"id":"call-1","name":"run","args":{}}}]},{"role":"model","parts":[{"text":"boundary"}]},{"role":"user","parts":[{"functionResponse":{"id":"call-1","name":"run","response":{"result":"ok"}}}]}]}"#,
    );
    assert!(
        validate_gemini_function_call_pairing(&payload).is_err(),
        "model boundary before function response should be rejected"
    );
}

#[test]
fn validate_gemini_function_call_pairing_rejects_empty_content_boundary_before_response() {
    for boundary in [
        r#"{"role":"user","parts":[]}"#,
        r#"{"role":"user"}"#,
        r#"{"role":"user","parts":null}"#,
    ] {
        let payload = json(&format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"id":"call-1","name":"run","args":{{}}}}}}]}},{boundary},{{"role":"model","parts":[{{"functionResponse":{{"id":"call-1","name":"run","response":{{"result":"ok"}}}}}}]}}]}}"#
        ));
        assert!(
            validate_gemini_function_call_pairing(&payload).is_err(),
            "content boundary {boundary} before function response was accepted"
        );
    }
}

#[test]
fn validate_gemini_function_call_pairing_rejects_response_count_mismatch() {
    let input = json(
        r#"{
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"id": "call-1", "name": "weather", "args": {}}},
                        {"functionCall": {"id": "call-2", "name": "weather", "args": {}}}
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        {"functionResponse": {"id": "call-1", "name": "weather", "response": {}}}
                    ]
                }
            ]
        }"#,
    );

    let err = validate_gemini_function_call_pairing(&input)
        .expect_err("response count mismatch should fail")
        .to_string();
    assert!(
        err.contains("does not match pending functionCall count"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_gemini_function_call_pairing_rejects_missing_function_call_name() {
    let input = json(
        r#"{
            "contents": [{
                "role": "model",
                "parts": [
                    {"functionCall": {"id": "call-1", "args": {}}}
                ]
            }]
        }"#,
    );

    let err = validate_gemini_function_call_pairing(&input)
        .expect_err("missing functionCall name should fail")
        .to_string();
    assert!(
        err.contains("missing functionCall.name"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_gemini_function_call_pairing_rejects_id_mismatch() {
    let input = json(
        r#"{
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"id": "call-1", "name": "weather", "args": {}}}
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        {"functionResponse": {"id": "call-other", "name": "weather", "response": {}}}
                    ]
                }
            ]
        }"#,
    );

    let err = validate_gemini_function_call_pairing(&input)
        .expect_err("id mismatch should fail")
        .to_string();
    assert!(
        err.contains("does not match functionCall.id"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_gemini_function_call_pairing_rejects_missing_response_name() {
    let input = json(
        r#"{
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"id": "call-1", "name": "weather", "args": {}}}
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        {"functionResponse": {"id": "call-1", "response": {}}}
                    ]
                }
            ]
        }"#,
    );

    let err = validate_gemini_function_call_pairing(&input)
        .expect_err("missing response name should fail")
        .to_string();
    assert!(
        err.contains("missing functionResponse.name"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_gemini_function_call_pairing_rejects_same_content_interleaving() {
    let input = json(
        r#"{
            "contents": [{
                "role": "model",
                "parts": [
                    {"functionCall": {"id": "call-1", "name": "weather", "args": {}}},
                    {"functionResponse": {"id": "call-1", "name": "weather", "response": {}}}
                ]
            }]
        }"#,
    );

    let err = validate_gemini_function_call_pairing(&input)
        .expect_err("same-content interleaving should fail")
        .to_string();
    assert!(
        err.contains("must not be interleaved"),
        "unexpected error: {err}"
    );
}
