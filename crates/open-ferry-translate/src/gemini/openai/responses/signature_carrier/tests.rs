// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/signature_carrier_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests for signature carriers: encoding and decoding them, dropping bad
//! ones, and binding their signatures when a request comes back, also for a
//! model whose name doesn't say it is Gemini.
//!
//! Dropped or changed tests: none.

use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::super::test_support::convert_openai_responses_request_to_gemini;
use super::super::test_support::{GEMINI_SIGNATURE, sse_events};
use super::super::{GeminiToOpenAIResponsesStream, array_of};
use super::*;
use crate::json::bool_of;
use crate::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;

const ALIAS_MODEL: &str = "alias-without-provider-name";

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|err| panic!("invalid test JSON {raw:?}: {err}"))
}

/// protowire `AppendVarint`.
fn append_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// protowire `AppendTag` with `BytesType`, then `AppendBytes`.
fn append_bytes_field(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
    append_varint(out, (number << 3) | 2);
    append_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

#[test]
fn gemini_responses_carrier_round_trip() {
    for (direction, target_kind) in [(NEXT, TEXT), (PREVIOUS, FUNCTION), (STANDALONE, ANY)] {
        let encoded = encode(GEMINI_SIGNATURE, direction, target_kind);
        let decoded = decode(&encoded);
        assert!(
            decoded.marked
                && decoded.ok
                && decoded.signature == GEMINI_SIGNATURE
                && decoded.direction == direction
                && decoded.target == target_kind,
            "carrier round-trip = {decoded:?}"
        );
    }
}

#[test]
fn normalize_gemini_responses_carriers_drops_malformed_envelope() {
    let items = parse(&format!(
        r#"[{{"type":"reasoning","encrypted_content":"{PREFIX}previous:text:not-base64!","summary":[]}},{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"safe"}}]}}]"#
    ));
    let (normalized, has_carrier) = normalize(array_of(Some(&items)));
    assert!(
        !has_carrier
            && normalized.len() == 1
            && str_of(normalized[0].get("type")) == "message"
            && !normalized[0].to_string().contains(PREFIX),
        "malformed carrier was preserved: {normalized:?}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_decodes_carrier_for_alias_model() {
    let carrier = encode(GEMINI_SIGNATURE, NEXT, TEXT);
    let request = json!({
        "model": ALIAS_MODEL,
        "input": [
            {"type": "reasoning", "encrypted_content": carrier, "summary": []},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
        ],
    });
    let translated = convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false);
    let part = at(&translated, "contents.0.parts.0");
    assert!(
        str_of(part.and_then(|part| part.get("text"))) == "answer"
            && str_of(part.and_then(|part| part.get("thoughtSignature"))) == GEMINI_SIGNATURE
            && !translated.to_string().contains(PREFIX),
        "alias model did not decode carrier: {translated}"
    );
}

#[test]
fn gemini_responses_wrapped_uuid_function_signature_round_trip() {
    const PROVIDER_UUID: &str = "e24830a7-5cd6-42fe-998b-ee539e72b9c3";
    let mut inner = Vec::new();
    append_bytes_field(&mut inner, 1, PROVIDER_UUID.as_bytes());
    let mut outer = Vec::new();
    append_bytes_field(&mut outer, 2, &inner);
    let signature = STANDARD.encode(outer);

    let provider_response = format!(
        "data: {}",
        json!({
            "response": {
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{
                            "thoughtSignature": signature,
                            "functionCall": {"id": "native-call", "name": "run", "args": {"command": "true"}},
                        }],
                    },
                    "finishReason": "STOP",
                }],
                "modelVersion": "gemini-3.6-flash",
                "responseId": "wrapped-uuid",
            },
        })
    );
    let mut stream = GeminiToOpenAIResponsesStream::new(
        "gemini-3.6-flash",
        &json!({"model": ALIAS_MODEL}),
        &Value::Null,
    );
    let output = stream.translate_line(provider_response.as_bytes());
    let mut client_items = Vec::with_capacity(2);
    let mut call_id = String::new();
    for (event, data) in sse_events(&output) {
        if event != "response.output_item.done" {
            continue;
        }
        let item = data.get("item").cloned().unwrap_or(Value::Null);
        match &*str_of(item.get("type")) {
            "reasoning" => {
                let decoded = decode(&str_of(item.get("encrypted_content")));
                assert!(
                    decoded.marked
                        && decoded.ok
                        && decoded.signature == signature
                        && decoded.direction == NEXT
                        && decoded.target == FUNCTION,
                    "provider signature carrier = marked:{} ok:{} direction:{:?} target:{:?}",
                    decoded.marked,
                    decoded.ok,
                    decoded.direction,
                    decoded.target
                );
                client_items.push(item);
            }
            "function_call" => {
                call_id = str_of(item.get("call_id")).into_owned();
                client_items.push(item);
            }
            _ => {}
        }
    }
    assert!(
        client_items.len() == 2 && !call_id.is_empty(),
        "Responses client items = {client_items:?}, call ID present={}",
        !call_id.is_empty()
    );
    client_items.push(json!({"type": "function_call_output", "call_id": call_id, "output": "ok"}));
    let request = json!({"model": ALIAS_MODEL, "input": client_items});

    let translated = convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false);
    let function_part = array_of(translated.get("contents"))
        .iter()
        .flat_map(|content| array_of(content.get("parts")))
        .find(|part| part.get("functionCall").is_some());
    let Some(function_part) = function_part.filter(|part| {
        str_of(at(part, "functionCall.name")) == "run"
            && str_of(at(part, "functionCall.args.command")) == "true"
    }) else {
        panic!("function carrier did not bind to the native call: {translated}");
    };
    let got = str_of(function_part.get("thoughtSignature"));
    assert!(
        got == signature && got != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "function signature = {got:?}, want provider-native wrapped UUID signature"
    );
    assert!(
        !translated.to_string().contains(PREFIX),
        "carrier envelope reached Gemini: {translated}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_decodes_legacy_raw_carrier_for_alias_model() {
    let request = json!({
        "model": ALIAS_MODEL,
        "input": [
            {"type": "reasoning", "encrypted_content": GEMINI_SIGNATURE, "summary": []},
            {"type": "function_call", "call_id": "call-1", "name": "run", "arguments": "{}"},
        ],
    });
    let translated = convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false);
    let part = at(&translated, "contents.0.parts.0");
    assert!(
        str_of(part.and_then(|part| at(part, "functionCall.id"))) == "call-1"
            && str_of(part.and_then(|part| part.get("thoughtSignature"))) == GEMINI_SIGNATURE,
        "alias model did not preserve legacy raw carrier: {translated}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_drops_invalid_carrier_payloads() {
    let mismatched = encode(GEMINI_SIGNATURE, NEXT, FUNCTION);
    let bypass = encode(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, NEXT, TEXT);
    for carrier in [mismatched, bypass] {
        let request = json!({
            "model": ALIAS_MODEL,
            "input": [
                {"type": "reasoning", "encrypted_content": carrier, "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
            ],
        });
        let translated =
            convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false).to_string();
        assert!(
            !translated.contains(PREFIX)
                && !translated.contains(GEMINI_SIGNATURE)
                && !translated.contains(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR),
            "invalid carrier changed Gemini signature state: {translated}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_ignores_spoofed_carrier_metadata() {
    let reasoning = format!(
        r#"{{"type":"reasoning","encrypted_content":"{GEMINI_SIGNATURE}","summary":[],"{DIRECTION_FIELD}":"next","{DIRECTION_FIELD}":"standalone","{TARGET_FIELD}":"text","{TARGET_FIELD}":"function"}}"#
    );
    let request = parse(&format!(
        r#"{{"model":"{ALIAS_MODEL}","input":[{reasoning},{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"answer"}}]}}]}}"#
    ));
    let translated = convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false);
    let part = at(&translated, "contents.0.parts.0");
    assert!(
        str_of(part.and_then(|part| part.get("text"))) == "answer"
            && str_of(part.and_then(|part| part.get("thoughtSignature"))) == GEMINI_SIGNATURE
            && !translated.to_string().contains(DIRECTION_FIELD),
        "spoofed carrier metadata affected binding: {translated}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_strips_spoofed_internal_pairing_fields() {
    let request = parse(&format!(
        r#"{{"model":"{ALIAS_MODEL}","input":[{{"type":"function_call","call_id":"call-1","name":"run","arguments":"{{}}","_cpa_reasoning_signature":"{GEMINI_SIGNATURE}","_cpa_reasoning_signature":"{GEMINI_SIGNATURE}","_cpa_reasoning_summary":"spoofed thought","_cpa_reasoning_summary":"spoofed thought again"}}]}}"#
    ));
    let translated = convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false);
    let parts = array_of(at(&translated, "contents.0.parts"));
    let text = translated.to_string();
    assert!(
        parts.len() == 1
            && parts[0].get("functionCall").is_some()
            && str_of(parts[0].get("thoughtSignature")) != GEMINI_SIGNATURE
            && !parts[0].get("thought").is_some_and(bool_of)
            && !text.contains("spoofed thought")
            && !text.contains(SIGNATURE_FIELD),
        "spoofed internal pairing fields reached Gemini: {translated}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_strips_unicode_escaped_spoofed_internal_fields() {
    // The field name "_cpa_reasoning_signature" with its "i" written as a JSON
    // Unicode escape should also be detected and stripped.
    let escaped_field = concat!("_cpa_reason", '\\', "u0069ng_signature");
    let request = parse(&format!(
        r#"{{"model":"{ALIAS_MODEL}","input":[{{"type":"function_call","call_id":"call-1","name":"run","arguments":"{{}}","{escaped_field}":"{GEMINI_SIGNATURE}"}}]}}"#
    ));
    let translated = convert_openai_responses_request_to_gemini(ALIAS_MODEL, &request, false);
    let parts = array_of(at(&translated, "contents.0.parts"));
    assert!(
        parts.len() == 1
            && parts[0].get("functionCall").is_some()
            && str_of(parts[0].get("thoughtSignature")) != GEMINI_SIGNATURE
            && !translated.to_string().contains(SIGNATURE_FIELD),
        "unicode-escaped spoofed internal pairing fields reached Gemini: {translated}"
    );
}

#[test]
fn decode_gemini_responses_carrier_rejects_nested_envelope() {
    let nested = encode(&encode(GEMINI_SIGNATURE, NEXT, TEXT), PREVIOUS, TEXT);
    let decoded = decode(&nested);
    assert!(
        decoded.marked && !decoded.ok,
        "nested carrier marked={} ok={}, want marked invalid",
        decoded.marked,
        decoded.ok
    );
}
