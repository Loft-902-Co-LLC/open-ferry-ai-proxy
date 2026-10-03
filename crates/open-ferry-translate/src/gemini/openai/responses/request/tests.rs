// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_request_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests for the OpenAI Responses to Gemini request translator: thought
//! signatures and their carriers, tool call and output pairing, system and
//! developer messages, tool outputs with media, media parts, tool schemas and
//! structured output settings.
//!
//! Dropped or changed tests: none.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use serde_json::json;

use super::super::at;
use super::super::test_support::{GEMINI_SIGNATURE, different_gemini_signature};
use super::*;
use crate::signature::validate_gemini_function_call_pairing;

/// `ConvertOpenAIResponsesRequestToGemini` without streaming.
fn convert(model: &str, request: &Value) -> Value {
    convert_openai_responses_request_to_gemini(model, request, false)
}

/// gjson `Get(path).String()`, where a number in the path indexes an array.
fn string(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// gjson `Get(path).Array()`.
fn array<'v>(value: &'v Value, path: &str) -> &'v [Value] {
    array_of(at(value, path))
}

/// gjson `Get(path).Bool()`.
fn boolean(value: &Value, path: &str) -> bool {
    at(value, path).is_some_and(bool_of)
}

/// gjson `Get(path).Exists()`.
fn exists(value: &Value, path: &str) -> bool {
    at(value, path).is_some()
}

/// Every part of every content, in order.
fn all_parts(out: &Value) -> impl Iterator<Item = &Value> {
    array(out, "contents")
        .iter()
        .flat_map(|content| array(content, "parts"))
}

/// `internalsignature.ValidateGeminiFunctionCallPairing`, failing the test
/// with `context` if the history is invalid.
fn assert_pairing(output: &Value, context: &str) {
    if let Err(err) = validate_gemini_function_call_pairing(output) {
        panic!("{context}: {err}; output={output}");
    }
}

/// `validResponsesGPTReasoningSignature`: a well-formed GPT reasoning
/// signature.
fn valid_responses_gpt_reasoning_signature() -> String {
    let mut raw = vec![0u8; 1 + 8 + 16 + 16 + 32];
    raw[0] = 0x80;
    raw[8] = 1;
    for (i, byte) in raw.iter_mut().enumerate().skip(9) {
        *byte = i as u8;
    }
    URL_SAFE.encode(raw)
}

#[test]
fn reorder_openai_responses_detached_reasoning_does_not_cross_user_message() {
    let items = vec![
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}),
        json!({"id":"rs_test_detached_after_1","type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]}),
        json!({"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"}),
    ];
    let reordered = reorder_detached_reasoning(items);
    let got = string(&reordered[0], "role");
    assert_eq!(
        got, "user",
        "detached reasoning crossed user boundary: first role={got:?}"
    );
    let got = string(&reordered[1], "type");
    assert_eq!(got, "reasoning", "item 1 = {got:?}, want reasoning");
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_reasoning_and_signature_to_function_call()
{
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[{"type":"summary_text","text":"hidden thought"}]},
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"true\"}"},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.1.parts");
    assert!(
        parts.len() == 2 && boolean(&parts[0], "thought"),
        "reasoning/function parts malformed: {result}"
    );
    let got = string(&parts[1], "functionCall.name");
    assert_eq!(
        got, "run_command",
        "function name = {got:?}; result={result}"
    );
    let got = string(&parts[1], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "function signature = {got:?}, want {GEMINI_SIGNATURE:?}; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_synthetic_parallel_calls_only_first_gets_sentinel() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"}
        ]
    });

    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.0.parts");
    assert_eq!(
        parts.len(),
        2,
        "parts = {}, want 2 parallel calls; result={result}",
        parts.len()
    );
    let got = string(&parts[0], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "first synthetic call signature = {got:?}, want sentinel; result={result}"
    );
    assert!(
        !exists(&parts[1], "thoughtSignature"),
        "second synthetic sibling should remain unsigned; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_native_parallel_calls_preserve_unsigned_sibling() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run twice"}]},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"}
        ]
    });

    let result = convert("gemini-3.6-flash-high", &input);
    let calls: Vec<&Value> = all_parts(&result)
        .filter(|part| exists(part, "functionCall"))
        .collect();
    assert_eq!(
        calls.len(),
        2,
        "calls = {}, want 2; result={result}",
        calls.len()
    );
    let got = string(calls[0], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "first call signature = {got:?}, want native signature; result={result}"
    );
    assert!(
        !exists(calls[1], "thoughtSignature"),
        "native unsigned sibling should remain unsigned; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_multiple_leading_tool_signatures() {
    let mut second_raw = STANDARD
        .decode(GEMINI_SIGNATURE)
        .expect("the test signature is base64");
    let last = second_raw.len() - 1;
    second_raw[last] ^= 1;
    let second_signature = STANDARD.encode(second_raw);
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run twice"}]},
            {"id":"rs_before_1","type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"id":"rs_before_2","type":"reasoning","encrypted_content":second_signature,"summary":[]},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"},
            {"type":"function_call_output","call_id":"call-1","output":"one"},
            {"type":"function_call_output","call_id":"call-2","output":"two"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let mut signatures = Vec::new();
    let mut sequence = Vec::new();
    for part in all_parts(&result) {
        if exists(part, "functionCall") {
            signatures.push(string(part, "thoughtSignature"));
            sequence.push(format!("call:{}", string(part, "functionCall.id")));
        }
        if exists(part, "functionResponse") {
            sequence.push(format!("output:{}", string(part, "functionResponse.id")));
        }
    }
    assert!(
        signatures.len() == 2
            && signatures[0] == GEMINI_SIGNATURE
            && signatures[1] == second_signature,
        "tool signatures = {signatures:?}; result={result}"
    );
    let got = sequence.join(",");
    assert_eq!(
        got, "call:call-1,call:call-2,output:call-1,output:call-2",
        "parallel tool call/output sequence = {got:?}; result={result}"
    );
    assert_pairing(&result, "parallel tool history is invalid");
}

#[test]
fn convert_openai_responses_request_to_gemini_groups_reversed_parallel_tool_outputs() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"},
            {"type":"function_call_output","call_id":"call-2","output":"two"},
            {"type":"function_call_output","call_id":"call-1","output":"one"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    assert_pairing(&result, "parallel tool history is invalid");
    let contents = array(&result, "contents");
    assert!(
        contents.len() == 2
            && string(&contents[0], "role") == "model"
            && string(&contents[1], "role") == "user",
        "parallel tool roles malformed; result={result}"
    );
    let responses = array(&contents[1], "parts");
    assert_eq!(
        responses.len(),
        2,
        "function response count = {}, want 2; result={result}",
        responses.len()
    );
    let got = string(&responses[0], "functionResponse.id");
    assert_eq!(
        got, "call-1",
        "first function response = {got:?}, want call-1; result={result}"
    );
    let got = string(&responses[0], "functionResponse.response.result");
    assert_eq!(
        got, "one",
        "first function result = {got:?}, want one; result={result}"
    );
    let got = string(&responses[1], "functionResponse.id");
    assert_eq!(
        got, "call-2",
        "second function response = {got:?}, want call-2; result={result}"
    );
    let got = string(&responses[1], "functionResponse.response.result");
    assert_eq!(
        got, "two",
        "second function result = {got:?}, want two; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_groups_non_contiguous_parallel_tool_outputs() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"},
            {"type":"function_call_output","call_id":"call-1","output":"one"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"between outputs"}]},
            {"type":"function_call_output","call_id":"call-2","output":"two"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let contents = array(&result, "contents");
    assert!(
        contents.len() == 4
            && string(&contents[0], "role") == "model"
            && string(&contents[1], "role") == "user"
            && string(&contents[2], "role") == "user"
            && string(&contents[3], "role") == "user",
        "non-contiguous tool output roles malformed; result={result}"
    );
    let got = string(&contents[1], "parts.0.functionResponse.id");
    assert_eq!(
        got, "call-1",
        "first function response = {got:?}, want call-1; result={result}"
    );
    let got = string(&contents[2], "parts.0.text");
    assert_eq!(
        got, "between outputs",
        "intervening user message = {got:?}; result={result}"
    );
    let got = string(&contents[3], "parts.0.functionResponse.id");
    assert_eq!(
        got, "call-2",
        "second function response crossed user boundary: got {got:?}; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_reasoning_before_paired_function_signature()
{
    let second_signature = different_gemini_signature();
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[{"type":"summary_text","text":"first"}]},
            {"type":"reasoning","encrypted_content":second_signature,"summary":[{"type":"summary_text","text":"second"}]},
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"true\"}"},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let signatures: Vec<String> = all_parts(&result)
        .map(|part| string(part, "thoughtSignature"))
        .filter(|signature| !signature.is_empty())
        .collect();
    assert!(
        signatures.len() == 2
            && signatures[0] == GEMINI_SIGNATURE
            && signatures[1] == second_signature,
        "reasoning/function signatures = {signatures:?}; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_function_output_order_across_model_text() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"between"}]},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"},
            {"type":"function_call_output","call_id":"call-1","output":"one"},
            {"type":"function_call_output","call_id":"call-2","output":"two"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let mut sequence = Vec::new();
    for part in all_parts(&result) {
        let id = string(part, "functionCall.id");
        if !id.is_empty() {
            sequence.push(format!("call:{id}"));
        }
        let id = string(part, "functionResponse.id");
        if !id.is_empty() {
            sequence.push(format!("output:{id}"));
        }
    }
    let got = sequence.join(",");
    assert_eq!(
        got, "call:call-1,call:call-2,output:call-1,output:call-2",
        "function output order = {got:?}; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_trailing_detached_signature_to_text() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"turn one"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"visible answer"}]},
            {"id":"rs_text_detached_after_1","type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"turn two"}]}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.1.parts");
    assert_eq!(
        parts.len(),
        1,
        "model parts = {}, want one signed visible part; result={result}",
        parts.len()
    );
    let got = string(&parts[0], "text");
    assert_eq!(
        got, "visible answer",
        "visible text = {got:?}; result={result}"
    );
    let got = string(&parts[0], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "signature = {got:?}, want detached signature; result={result}"
    );
    assert!(
        !boolean(&parts[0], "thought"),
        "detached visible carrier must not emit an empty thought part; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_unmarked_trailing_signature_to_text() {
    let input = json!({
        "model":"gemini-3.5-flash",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"turn one"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"visible answer"}]},
            {"id":"rs_client_rewritten","type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"turn two"}]}
        ]
    });
    let result = convert("gemini-3.5-flash", &input);
    let parts = array(&result, "contents.1.parts");
    assert_eq!(
        parts.len(),
        1,
        "model parts = {}, want one signed visible part after client rewrites carrier ID; result={result}",
        parts.len()
    );
    let got = string(&parts[0], "text");
    assert_eq!(
        got, "visible answer",
        "visible text = {got:?}; result={result}"
    );
    let got = string(&parts[0], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "signature = {got:?}, want unmarked trailing signature; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_unmarked_reasoning_before_function_call_still_pairs_call()
 {
    let input = json!({
        "model":"gemini-3.5-flash",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"I will run it."}]},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"true\"}"},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.5-flash", &input);
    let model_parts = array(&result, "contents.1.parts");
    assert_eq!(
        model_parts.len(),
        2,
        "model parts = {}, want unsigned preamble plus signed call; result={result}",
        model_parts.len()
    );
    assert!(
        !exists(&model_parts[0], "thoughtSignature"),
        "function-call signature was retargeted to preamble; result={result}"
    );
    let got = string(&model_parts[1], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "function signature = {got:?}, want unmarked reasoning signature; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_detached_signature_to_function_call() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]},
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"true\"}"},
            {"id":"rs_function_detached_after_1","type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    // gjson `contents.#(role=="model")#.parts`.
    let mut found = false;
    for content in array(&result, "contents")
        .iter()
        .filter(|content| string(content, "role") == "model")
    {
        for part in array(content, "parts") {
            if string(part, "functionCall.name") != "run_command" {
                continue;
            }
            found = true;
            let got = string(part, "thoughtSignature");
            assert_eq!(
                got, GEMINI_SIGNATURE,
                "function signature = {got:?}, want detached signature; result={result}"
            );
        }
    }
    assert!(found, "function call not found; result={result}");
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_unmarked_post_call_signature_with_matching_output()
 {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"true\"}"},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let got = string(&result, "contents.0.parts.0.thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "unmarked post-call signature = {got:?}, want native signature; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_directional_function_carriers_without_ids()
{
    struct Case {
        name: &'static str,
        direction: &'static str,
        input: fn(&str) -> Value,
    }
    let cases = [
        Case {
            name: "leading",
            direction: NEXT,
            input: |carrier| {
                json!([
                    {"type":"reasoning","encrypted_content":carrier,"summary":[]},
                    {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call-1","output":"ok"}
                ])
            },
        },
        Case {
            name: "post-call",
            direction: PREVIOUS,
            input: |carrier| {
                json!([
                    {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"},
                    {"type":"reasoning","encrypted_content":carrier,"summary":[]},
                    {"type":"function_call_output","call_id":"call-1","output":"ok"}
                ])
            },
        },
    ];
    for Case {
        name,
        direction: carrier_direction,
        input,
    } in cases
    {
        let carrier = signature_carrier::encode(GEMINI_SIGNATURE, carrier_direction, FUNCTION);
        let request = json!({"model":"gemini-3.6-flash-high","input":input(&carrier)});
        let result = convert("gemini-3.6-flash-high", &request);
        let got = string(&result, "contents.0.parts.0.thoughtSignature");
        assert_eq!(
            got, GEMINI_SIGNATURE,
            "{name}: directional function signature = {got:?}, want native signature; result={result}"
        );
        assert!(
            !result.to_string().contains(signature_carrier::PREFIX),
            "{name}: directional function carrier leaked to Gemini wire: {result}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_does_not_retarget_extra_previous_carrier() {
    // Each assertion's message names its case ("text" or "function").
    struct Case {
        target_kind: &'static str,
        input: fn(&str, &str) -> Value,
        assert: fn(&[Value], &str),
    }
    let signature2 = different_gemini_signature();
    let cases = [
        Case {
            target_kind: TEXT,
            input: |first, extra| {
                json!([
                    {"type":"reasoning","encrypted_content":first,"summary":[]},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"signed"}]},
                    {"type":"reasoning","encrypted_content":extra,"summary":[]},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"unsigned"}]}
                ])
            },
            assert: |parts, signature2| {
                assert!(
                    parts.len() == 3
                        && string(&parts[0], "text") == "signed"
                        && string(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
                        && exists(&parts[1], "text")
                        && string(&parts[1], "text").is_empty()
                        && string(&parts[1], "thoughtSignature") == signature2
                        && string(&parts[2], "text") == "unsigned"
                        && string(&parts[2], "thoughtSignature").is_empty(),
                    "extra previous text carrier retargeted: {}",
                    Value::from(parts.to_vec())
                );
            },
        },
        Case {
            target_kind: FUNCTION,
            input: |first, extra| {
                json!([
                    {"type":"reasoning","encrypted_content":first,"summary":[]},
                    {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"},
                    {"type":"reasoning","encrypted_content":extra,"summary":[]},
                    {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{}"}
                ])
            },
            assert: |parts, signature2| {
                assert!(
                    parts.len() == 3
                        && string(&parts[0], "functionCall.id") == "call-1"
                        && string(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
                        && exists(&parts[1], "text")
                        && string(&parts[1], "text").is_empty()
                        && string(&parts[1], "thoughtSignature") == signature2
                        && string(&parts[2], "functionCall.id") == "call-2"
                        && string(&parts[2], "thoughtSignature").is_empty(),
                    "extra previous function carrier retargeted: {}",
                    Value::from(parts.to_vec())
                );
            },
        },
    ];
    for case in cases {
        let first = signature_carrier::encode(GEMINI_SIGNATURE, NEXT, case.target_kind);
        let extra = signature_carrier::encode(&signature2, PREVIOUS, case.target_kind);
        let request = json!({"model":"gemini-3.6-flash-high","input":(case.input)(&first, &extra)});
        let translated = convert("gemini-3.6-flash-high", &request);
        (case.assert)(array(&translated, "contents.0.parts"), &signature2);
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_does_not_bind_standalone_function_carrier() {
    let carrier =
        signature_carrier::encode(GEMINI_SIGNATURE, signature_carrier::STANDALONE, FUNCTION);
    let input = json!({"model":"gemini-3.6-flash-high","input":[
        {"type":"reasoning","encrypted_content":carrier,"summary":[]},
        {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"}
    ]});
    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.0.parts");
    assert!(
        parts.len() == 2
            && string(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && string(&parts[1], "thoughtSignature") == BYPASS,
        "standalone carrier was bound to function call: {result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_unmarked_parallel_post_call_signature() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call_output","call_id":"call-1","output":"one"},
            {"type":"function_call_output","call_id":"call-2","output":"two"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.0.parts");
    assert!(
        parts.len() == 2
            && string(&parts[0], "thoughtSignature") == BYPASS
            && string(&parts[1], "thoughtSignature") == GEMINI_SIGNATURE,
        "parallel post-call signature was not attached to call-2: {result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reattaches_alternating_parallel_post_call_signatures()
{
    let signature2 = different_gemini_signature();
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{\"command\":\"one\"}"},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call","call_id":"call-2","name":"run_command","arguments":"{\"command\":\"two\"}"},
            {"type":"reasoning","encrypted_content":signature2,"summary":[]},
            {"type":"function_call_output","call_id":"call-1","output":"one"},
            {"type":"function_call_output","call_id":"call-2","output":"two"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.0.parts");
    assert!(
        parts.len() == 2
            && string(&parts[0], "functionCall.id") == "call-1"
            && string(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && string(&parts[1], "functionCall.id") == "call-2"
            && string(&parts[1], "thoughtSignature") == signature2,
        "alternating parallel post-call signatures shifted: {result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_extra_consecutive_post_call_carrier() {
    let signature2 = different_gemini_signature();
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"reasoning","encrypted_content":signature2,"summary":[]},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let parts = array(&result, "contents.0.parts");
    assert!(
        parts.len() == 2
            && string(&parts[0], "functionCall.id") == "call-1"
            && string(&parts[0], "thoughtSignature") == GEMINI_SIGNATURE
            && string(&parts[1], "thoughtSignature") == signature2,
        "consecutive post-call carriers malformed: {result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_does_not_pair_unmarked_post_call_signature_across_mismatch()
 {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"function_call_output","call_id":"other-call","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let got = string(&result, "contents.0.parts.0.thoughtSignature");
    assert_eq!(
        got, BYPASS,
        "mismatched output paired signature {got:?}; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_does_not_pair_unmarked_post_call_signature_across_user_message()
 {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[
            {"type":"function_call","call_id":"call-1","name":"run_command","arguments":"{}"},
            {"type":"reasoning","encrypted_content":GEMINI_SIGNATURE,"summary":[]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"boundary"}]},
            {"type":"function_call_output","call_id":"call-1","output":"ok"}
        ]
    });
    let result = convert("gemini-3.6-flash-high", &input);
    let got = string(&result, "contents.0.parts.0.thoughtSignature");
    assert_eq!(
        got, BYPASS,
        "user-boundary carrier paired signature {got:?}; result={result}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_strips_trailing_assistant_prefill() {
    let input = json!({
        "model": "gpt-5.4",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "previous answer"}]
            }
        ]
    });

    let result = convert("gemini-3.1-pro-high", &input);
    let contents = array(&result, "contents");

    assert_eq!(
        contents.len(),
        1,
        "contents length = {}, want 1. contents={}",
        contents.len(),
        result["contents"]
    );
    let got = string(&contents[0], "role");
    assert_eq!(got, "user", "final remaining role = {got:?}, want \"user\"");
}

#[test]
fn convert_openai_responses_request_to_gemini_text_format_json_schema() {
    let input = json!({
        "model": "gemini-flash-lite",
        "temperature": 0.2,
        "input": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Return structured JSON."
                    }
                ]
            }
        ],
        "text": {
            "format": {
                "type": "json_schema",
                "strict": true,
                "name": "response",
                "schema": {
                    "type": "object",
                    "properties": {
                        "cleanedContent": {
                            "type": "string"
                        }
                    },
                    "required": [
                        "cleanedContent"
                    ],
                    "additionalProperties": false
                }
            }
        }
    });

    let output = convert("gemini-3.1-flash-lite", &input);
    let gen_config = &output["generationConfig"];

    let got = string(gen_config, "responseMimeType");
    assert_eq!(
        got, "application/json",
        "responseMimeType = {got:?}, want application/json. Output: {output}"
    );
    let schema = gen_config
        .get("responseJsonSchema")
        .unwrap_or_else(|| panic!("responseJsonSchema missing. Output: {output}"));
    assert!(
        !exists(gen_config, "responseSchema"),
        "responseSchema should not be set with responseJsonSchema. Output: {output}"
    );
    let got = string(schema, "type");
    assert_eq!(
        got, "object",
        "schema type = {got:?}, want object. Output: {output}"
    );
    let got = string(schema, "properties.cleanedContent.type");
    assert_eq!(
        got, "string",
        "cleanedContent type = {got:?}, want string. Output: {output}"
    );
    let additional_properties = schema.get("additionalProperties");
    assert!(
        additional_properties.is_some_and(|value| !bool_of(value)),
        "additionalProperties = {additional_properties:?}, want false. Output: {output}"
    );
    let got = gen_config["temperature"].as_f64();
    assert_eq!(
        got,
        Some(0.2),
        "temperature = {got:?}, want 0.2. Output: {output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_text_format_json_object() {
    let input = json!({
        "model": "gemini-flash-lite",
        "input": "Return a JSON object.",
        "text": {
            "format": {
                "type": "json_object"
            }
        }
    });

    let output = convert("gemini-3.1-flash-lite", &input);
    let gen_config = &output["generationConfig"];

    let got = string(gen_config, "responseMimeType");
    assert_eq!(
        got, "application/json",
        "responseMimeType = {got:?}, want application/json. Output: {output}"
    );
    assert!(
        !exists(gen_config, "responseJsonSchema"),
        "responseJsonSchema should not be set for json_object. Output: {output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_reasoning_only_history() {
    let input = json!({
        "model": "gpt-5",
        "input": [{
            "type": "reasoning",
            "encrypted_content": format!("gemini#{GEMINI_SIGNATURE}"),
            "summary": [{"type": "summary_text", "text": "reasoning summary"}]
        }]
    });

    let output = convert("gemini-3.5-flash", &input);
    let parts = array(&output, "contents.0.parts");
    let got = array(&output, "contents");
    assert_eq!(
        got.len(),
        1,
        "contents length = {}, want 1. Output: {output}",
        got.len()
    );
    assert_eq!(
        parts.len(),
        1,
        "parts length = {}, want 1. Output: {output}",
        parts.len()
    );
    assert!(
        boolean(&parts[0], "thought"),
        "parts[0] should be thought. Output: {output}"
    );
    let got = string(&parts[0], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "parts[0].thoughtSignature = {got:?}, want {GEMINI_SIGNATURE:?}. Output: {output}"
    );
    let got = string(&parts[0], "text");
    assert_eq!(
        got, "reasoning summary",
        "thought text = {got:?}, want reasoning summary. Output: {output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_drops_empty_unsigned_reasoning_carrier() {
    let input = json!({
        "model":"gemini-3.6-flash-high",
        "input":[{"type":"reasoning","encrypted_content":"","summary":[]}]
    });

    let output = convert("gemini-3.6-flash-high", &input);
    let got = array(&output, "contents").len();
    assert_eq!(
        got, 0,
        "contents = {got}, want no empty unsigned model content; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_unbound_detached_carrier_without_empty_thought()
 {
    let input = json!({
        "model": "gemini-3.6-flash-high",
        "input": [{
            "id": "rs_unbound_detached_after_1",
            "type": "reasoning",
            "encrypted_content": GEMINI_SIGNATURE,
            "summary": []
        }]
    });

    let output = convert("gemini-3.6-flash-high", &input);
    let parts = array(&output, "contents.0.parts");
    assert_eq!(
        parts.len(),
        1,
        "unbound carrier parts = {}, want one signed carrier; output={output}",
        parts.len()
    );
    assert!(
        !(boolean(&parts[0], "thought")
            || !exists(&parts[0], "text")
            || !string(&parts[0], "text").is_empty()),
        "unbound carrier emitted an empty thought part: {output}"
    );
    let got = string(&parts[0], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "unbound carrier signature = {got:?}, want {GEMINI_SIGNATURE:?}; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_reasoning_before_trailing_assistant_prefill()
 {
    let input = json!({
        "model": "gpt-5.4",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            },
            {
                "type": "reasoning",
                "encrypted_content": format!("gemini#{GEMINI_SIGNATURE}"),
                "summary": [{"type": "summary_text", "text": "reasoning summary"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "previous answer"}]
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        2,
        "contents length = {}, want 2. Output: {output}",
        contents.len()
    );
    let got = string(&contents[0], "role");
    assert_eq!(got, "user", "contents[0].role = {got:?}, want user");
    let got = string(&contents[1], "parts.1.thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "reasoning visible thoughtSignature = {got:?}, want preserved signature"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_reasoning_signature_compatibility() {
    let tests = [
        (
            "GPT encrypted_content is dropped from Gemini thought",
            valid_responses_gpt_reasoning_signature(),
            "",
        ),
        (
            "Gemini encrypted_content is preserved",
            format!("gemini#{GEMINI_SIGNATURE}"),
            GEMINI_SIGNATURE,
        ),
        (
            "Missing encrypted_content leaves Gemini thought unsigned",
            String::new(),
            "",
        ),
    ];

    for (name, encrypted, want_signature) in tests {
        let input = json!({
            "model": "gpt-5",
            "input": [{
                "type": "reasoning",
                "encrypted_content": encrypted,
                "summary": [{"type": "summary_text", "text": "reasoning summary"}]
            }]
        });

        let output = convert("gemini-3.5-flash", &input);
        let parts = array(&output, "contents.0.parts");
        assert_eq!(
            parts.len(),
            1,
            "{name}: parts length = {}, want 1. Output: {output}",
            parts.len()
        );
        let got = string(&parts[0], "thoughtSignature");
        assert_eq!(
            got, want_signature,
            "{name}: thoughtSignature = {got:?}, want {want_signature:?}. Output: {output}"
        );
        let got = string(&parts[0], "text");
        assert_eq!(
            got, "reasoning summary",
            "{name}: thought text = {got:?}, want reasoning summary. Output: {output}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_merges_reasoning_with_assistant_visible_answer() {
    let input = json!({
        "model": "gemini-3.5-flash",
        "input": [
            {
                "type": "reasoning",
                "encrypted_content": format!("gemini#{GEMINI_SIGNATURE}"),
                "summary": [{"type": "summary_text", "text": "internal reasoning"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "visible answer"}]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "continue"}]
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        2,
        "contents length = {}, want 2. Output: {output}",
        contents.len()
    );
    let parts = array(&contents[0], "parts");
    assert_eq!(
        parts.len(),
        2,
        "model parts length = {}, want 2. Output: {output}",
        parts.len()
    );
    assert!(
        boolean(&parts[0], "thought"),
        "parts[0] should be thought. Output: {output}"
    );
    let got = string(&parts[0], "thoughtSignature");
    assert_eq!(
        got, "",
        "parts[0].thoughtSignature = {got:?}, want empty. Output: {output}"
    );
    let got = string(&parts[1], "text");
    assert_eq!(
        got, "visible answer",
        "visible text = {got:?}, want visible answer. Output: {output}"
    );
    let got = string(&parts[1], "thoughtSignature");
    assert_eq!(
        got, GEMINI_SIGNATURE,
        "visible thoughtSignature = {got:?}, want preserved signature"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_merges_reasoning_with_user_role_output_text() {
    let input = json!({
        "model": "gemini-3.5-flash",
        "input": [
            {
                "type": "reasoning",
                "encrypted_content": format!("gemini#{GEMINI_SIGNATURE}"),
                "summary": [{"type": "summary_text", "text": "reasoning summary"}]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "output_text", "text": "visible from user role"}]
            }
        ]
    });
    let output = convert("gemini-3.5-flash", &input);
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        1,
        "contents length = {}, want 1. Output: {output}",
        contents.len()
    );
    let got = string(&contents[0], "parts.1.text");
    assert_eq!(got, "visible from user role", "visible text = {got:?}");
}

#[test]
fn convert_openai_responses_request_to_gemini_merges_reasoning_with_assistant_string_content() {
    let input = json!({
        "model": "gemini-3.5-flash",
        "input": [
            {
                "type": "reasoning",
                "encrypted_content": format!("gemini#{GEMINI_SIGNATURE}"),
                "summary": [{"type": "summary_text", "text": "reasoning summary"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": "string visible answer"
            }
        ]
    });
    let output = convert("gemini-3.5-flash", &input);
    let got = string(&output, "contents.0.parts.1.text");
    assert_eq!(got, "string visible answer", "visible text = {got:?}");
}

#[test]
fn convert_openai_responses_request_to_gemini_preserves_whitespace_when_merging_reasoning() {
    let input = json!({
        "model": "gemini-3.5-flash",
        "input": [
            {
                "type": "reasoning",
                "encrypted_content": format!("gemini#{GEMINI_SIGNATURE}"),
                "summary": [{"type": "summary_text", "text": "reasoning summary"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "  lead trail  "}]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "next"}]
            }
        ]
    });
    let output = convert("gemini-3.5-flash", &input);
    let got = string(&output, "contents.0.parts.1.text");
    assert_eq!(
        got, "  lead trail  ",
        "visible text = {got:?}, want preserved whitespace"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_system_and_developer_roles() {
    let tests = [
        ("system role", "system", "System message text"),
        ("developer role", "developer", "Developer message text"),
    ];

    for (name, role, want_text) in tests {
        let input = json!({
            "instructions": "Be a helpful assistant",
            "input": [
                {
                    "type": "message",
                    "role": role,
                    "content": [
                        {
                            "type": "input_text",
                            "text": want_text
                        }
                    ]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "Hello"
                        }
                    ]
                }
            ]
        });

        let output = convert("gemini-3.5-flash", &input);

        let system_instruction = output
            .get("systemInstruction")
            .unwrap_or_else(|| panic!("{name}: systemInstruction missing. Output: {output}"));
        let parts = array(system_instruction, "parts");
        assert_eq!(
            parts.len(),
            2,
            "{name}: systemInstruction parts = {}, want 2. Output: {output}",
            parts.len()
        );
        let got = string(&parts[0], "text");
        assert_eq!(
            got, "Be a helpful assistant",
            "{name}: first systemInstruction part = {got:?}, want \"Be a helpful assistant\". Output: {output}"
        );
        let got = string(&parts[1], "text");
        assert_eq!(
            got, want_text,
            "{name}: second systemInstruction part = {got:?}, want {want_text:?}. Output: {output}"
        );

        for content in array(&output, "contents") {
            assert_ne!(
                string(content, "role"),
                role,
                "{name}: role {role:?} leaked into contents array. Output: {output}"
            );
        }
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_mid_session_developer_message_does_not_mutate_system_instruction()
 {
    let input = json!({
        "model": "gemini-3.5-flash",
        "instructions": "Be a helpful assistant",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Turn 1 user"}
                ]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Turn 1 assistant"}
                ]
            },
            {
                "type": "message",
                "role": "developer",
                "content": "<image_resize_notice>Image 1 was resized to 800x600</image_resize_notice>"
            },
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Turn 2 user"}
                ]
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);

    // systemInstruction must remain strictly unchanged (only original
    // instructions, not developer notice).
    let system_instruction = output
        .get("systemInstruction")
        .unwrap_or_else(|| panic!("systemInstruction missing; output={output}"));
    let parts = array(system_instruction, "parts");
    assert_eq!(
        parts.len(),
        1,
        "systemInstruction parts count = {}, want 1; output={output}",
        parts.len()
    );
    let got = string(&parts[0], "text");
    assert_eq!(
        got, "Be a helpful assistant",
        "systemInstruction part = {got:?}, want \"Be a helpful assistant\"; output={output}"
    );

    // contents should contain user, model, user (with merged developer
    // notice + turn 2 user text).
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "contents count = {}, want 3; output={output}",
        contents.len()
    );
    assert!(
        string(&contents[0], "role") == "user"
            && string(&contents[0], "parts.0.text") == "Turn 1 user",
        "turn 1 user content malformed; output={output}"
    );
    assert!(
        string(&contents[1], "role") == "model"
            && string(&contents[1], "parts.0.text") == "Turn 1 assistant",
        "turn 1 model content malformed; output={output}"
    );
    let got = string(&contents[2], "role");
    assert_eq!(
        got, "user",
        "turn 2 user content role = {got:?}, want user; output={output}"
    );
    let turn2_parts = array(&contents[2], "parts");
    assert_eq!(
        turn2_parts.len(),
        2,
        "turn 2 parts count = {}, want 2; output={output}",
        turn2_parts.len()
    );
    let expected_dev_text = "<system-reminder>\n<image_resize_notice>Image 1 was resized to 800x600</image_resize_notice>\n</system-reminder>";
    let got = string(&turn2_parts[0], "text");
    assert_eq!(
        got, expected_dev_text,
        "turn 2 part 0 = {got:?}, want {expected_dev_text:?}; output={output}"
    );
    let got = string(&turn2_parts[1], "text");
    assert_eq!(
        got, "Turn 2 user",
        "turn 2 part 1 = {got:?}, want Turn 2 user; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_mid_session_system_reminder_envelope() {
    let input = json!({
        "model": "gemini-3.5-flash",
        "instructions": "Be a helpful assistant",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Turn 1 user"}
                ]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Turn 1 assistant"}
                ]
            },
            {
                "type": "message",
                "role": "developer",
                "content": "Please decide which tool to call next."
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);

    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "contents count = {}, want 3; output={output}",
        contents.len()
    );
    let expected_reminder =
        "<system-reminder>\nPlease decide which tool to call next.\n</system-reminder>";
    let got = string(&contents[2], "parts.0.text");
    assert_eq!(
        got, expected_reminder,
        "mid-session system reminder mismatch:\ngot:  {got:?}\nwant: {expected_reminder:?}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_mid_session_developer_multi_part_content_wrapped_once()
 {
    let input = json!({
        "model": "gemini-3.5-flash",
        "instructions": "Be a helpful assistant",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": "Turn 1"
            },
            {
                "type": "message",
                "role": "assistant",
                "content": "Reply 1"
            },
            {
                "type": "message",
                "role": "developer",
                "content": [
                    {"type": "input_text", "text": "Rule line 1"},
                    {"type": "input_text", "text": "Rule line 2"}
                ]
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);

    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "contents count = {}, want 3; output={output}",
        contents.len()
    );
    let expected = "<system-reminder>\nRule line 1\nRule line 2\n</system-reminder>";
    let got = string(&contents[2], "parts.0.text");
    assert_eq!(
        got, expected,
        "multi-part developer reminder mismatch:\ngot:  {got:?}\nwant: {expected:?}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_multiple_mid_session_developer_messages_array_content()
 {
    let input = json!({
        "model": "gemini-3.5-flash",
        "instructions": "Be a helpful assistant",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Turn 1"}
                ]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Reply 1"}
                ]
            },
            {
                "type": "message",
                "role": "developer",
                "content": [
                    {"type": "input_text", "text": "<permissions instructions>\nApproved: git\n</permissions instructions>"}
                ]
            },
            {
                "type": "message",
                "role": "developer",
                "content": [
                    {"type": "input_text", "text": "<collaboration_mode>\nPlan\n</collaboration_mode>"}
                ]
            },
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Proceed"}
                ]
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);

    // systemInstruction only contains original instructions.
    let parts = array(&output, "systemInstruction.parts");
    assert!(
        parts.len() == 1 && string(&parts[0], "text") == "Be a helpful assistant",
        "systemInstruction corrupted: {output}"
    );

    // All mid-session developer messages coalesced into the final user turn.
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "contents count = {}, want 3; output={output}",
        contents.len()
    );
    let turn2_parts = array(&contents[2], "parts");
    assert_eq!(
        turn2_parts.len(),
        3,
        "turn 2 parts count = {}, want 3; output={output}",
        turn2_parts.len()
    );
    assert!(
        string(&turn2_parts[0], "text").contains("permissions instructions"),
        "part 0 mismatch; output={output}"
    );
    assert!(
        string(&turn2_parts[1], "text").contains("collaboration_mode"),
        "part 1 mismatch; output={output}"
    );
    assert_eq!(
        string(&turn2_parts[2], "text"),
        "Proceed",
        "part 2 mismatch; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_intervening_developer_message_preserves_tool_pairing()
{
    let input = json!({
        "model": "gemini-3.5-flash",
        "instructions": "Be a helpful assistant",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Run tool"}
                ]
            },
            {
                "type": "function_call",
                "call_id": "call-1",
                "name": "run_command",
                "arguments": "{\"command\":\"echo test\"}"
            },
            {
                "type": "message",
                "role": "developer",
                "content": "<permissions instructions>\nApproved: echo\n</permissions instructions>"
            },
            {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "test"
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);

    // Validate function call pairing passes strictly (no content turn before
    // pending functionResponse).
    assert_pairing(&output, "ValidateGeminiFunctionCallPairing failed");

    // systemInstruction only contains original instructions.
    let parts = array(&output, "systemInstruction.parts");
    assert!(
        parts.len() == 1 && string(&parts[0], "text") == "Be a helpful assistant",
        "systemInstruction corrupted: {output}"
    );

    // Function response should have matching call id and name.
    let found_fr =
        all_parts(&output).any(|part| string(part, "functionResponse.name") == "run_command");
    assert!(
        found_fr,
        "functionResponse run_command not found or lost pairing: {output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_intervening_developer_and_user_message_flushes_in_order()
 {
    let input = json!({
        "model": "gemini-3.5-flash",
        "instructions": "Be a helpful assistant",
        "input": [
            {
                "type": "function_call",
                "call_id": "call-1",
                "name": "run_command",
                "arguments": "{\"command\":\"test\"}"
            },
            {
                "type": "message",
                "role": "developer",
                "content": "<permissions instructions>\nApproved: test\n</permissions instructions>"
            },
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Wait, also check this"}
                ]
            },
            {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "done"
            }
        ]
    });

    let output = convert("gemini-3.5-flash", &input);

    // Pairing should be valid.
    assert_pairing(&output, "ValidateGeminiFunctionCallPairing failed");

    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "contents count = {}, want 3; output={output}",
        contents.len()
    );
    let got = string(&contents[0], "role");
    assert_eq!(got, "model", "turn 0 role = {got:?}, want model");
    let mid_parts = array(&contents[1], "parts");
    assert_eq!(
        mid_parts.len(),
        2,
        "turn 1 parts count = {}, want 2; output={output}",
        mid_parts.len()
    );
    assert!(
        string(&mid_parts[0], "text").contains("permissions instructions"),
        "turn 1 part 0 should be developer notice; got {}",
        mid_parts[0]
    );
    assert_eq!(
        string(&mid_parts[1], "text"),
        "Wait, also check this",
        "turn 1 part 1 should be user text; got {}",
        mid_parts[1]
    );
    assert!(
        exists(&contents[2], "parts.0.functionResponse"),
        "turn 2 should be functionResponse; got {}",
        contents[2]
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_cleans_tool_schema_required_fields() {
    let input = json!({
        "model": "gemini-2.0-flash",
        "input": "hi",
        "tools": [{
            "type": "function",
            "name": "search_company",
            "description": "Search",
            "parameters": {
                "type": "object",
                "title": "SearchCompany",
                "properties": {
                    "country": {"type": "string"},
                    "industry": {"type": "string"}
                },
                "required": ["country", "industry", "stale_field", "another_stale"]
            }
        }]
    });

    let output = convert("gemini-2.0-flash", &input);
    let schema = at(
        &output,
        "tools.0.functionDeclarations.0.parametersJsonSchema",
    )
    .unwrap_or_else(|| panic!("parametersJsonSchema missing. Output: {output}"));

    assert!(
        !exists(schema, "title"),
        "schema title should be removed. Output: {output}"
    );
    let required = array(schema, "required");
    assert_eq!(
        required.len(),
        2,
        "required length = {}, want 2. Schema: {schema}",
        required.len()
    );
    let got = str_of(Some(&required[0]));
    assert_eq!(
        got, "country",
        "required[0] = {got:?}, want country. Schema: {schema}"
    );
    let got = str_of(Some(&required[1]));
    assert_eq!(
        got, "industry",
        "required[1] = {got:?}, want industry. Schema: {schema}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_function_call_output_with_images() {
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Below is the image from tool. Reply IMAGE_SEEN."
                    }
                ]
            },
            {
                "type": "function_call",
                "id": "fc_test",
                "call_id": "call_test",
                "name": "read",
                "arguments": "{}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_test",
                "output": [
                    {
                        "type": "input_text",
                        "text": "Read image file [image/png]"
                    },
                    {
                        "type": "input_image",
                        "detail": "auto",
                        "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
                    }
                ]
            }
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    let user_content = &output["contents"][2];
    assert_eq!(
        string(user_content, "role"),
        "user",
        "expected role user in third content, got {user_content}"
    );

    let parts = array(user_content, "parts");
    assert_eq!(
        parts.len(),
        1,
        "expected 1 part (functionResponse with nested inlineData), got {}; raw: {user_content}",
        parts.len()
    );

    let fr = parts[0].get("functionResponse").unwrap_or_else(|| {
        panic!(
            "expected first part to be functionResponse, got {}",
            parts[0]
        )
    });
    let got = string(fr, "name");
    assert_eq!(
        got, "read",
        "expected functionResponse.name = \"read\", got {got:?}"
    );
    let got = string(fr, "id");
    assert_eq!(
        got, "call_test",
        "expected functionResponse.id = \"call_test\", got {got:?}"
    );
    let got = string(fr, "response.result");
    assert_eq!(
        got, "Read image file [image/png]",
        "expected functionResponse.response.result = \"Read image file [image/png]\", got {got:?}"
    );

    let img = at(fr, "parts.0.inlineData").unwrap_or_else(|| {
        panic!("expected functionResponse.parts.0 to have inlineData, got {fr}")
    });
    let got = string(img, "mimeType");
    assert_eq!(
        got, "image/png",
        "expected mimeType = \"image/png\", got {got:?}"
    );
    let got = string(img, "data");
    assert_eq!(
        got, "iVBORw0KGgoAAAANSUhEUg==",
        "expected data = \"iVBORw0KGgoAAAANSUhEUg==\", got {got:?}"
    );
}

/// One tool call and its output, as the subtests of
/// `TestConvertOpenAIResponsesRequestToGemini_FunctionCallOutputVariations`
/// send them.
fn single_call_with_output(name: &str, output: Value) -> Value {
    json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": name,
                "arguments": "{}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": output
            }
        ]
    })
}

#[test]
fn convert_openai_responses_request_to_gemini_function_call_output_variations() {
    // stringified JSON array with image
    {
        let input = single_call_with_output(
            "screenshot",
            json!(
                "[{\"type\":\"input_text\",\"text\":\"done\"},{\"type\":\"input_image\",\"image_url\":\"data:image/jpeg;base64,/9j/4AAQSkZJRg==\"}]"
            ),
        );
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "stringified JSON array with image: expected 1 part, got {}; raw: {user_content}",
            parts.len()
        );
        let expected_result = r#"[{"type":"input_text","text":"done"},{"type":"input_image","image_url":"data:image/jpeg;base64,/9j/4AAQSkZJRg=="}]"#;
        let got = string(&parts[0], "functionResponse.response.result");
        assert_eq!(
            got, expected_result,
            "stringified JSON array with image: expected result {expected_result:?}, got {got:?}"
        );
    }

    // plain structured JSON array without images
    {
        let input = single_call_with_output(
            "list_items",
            json!([{"id": 1, "name": "first"}, {"id": 2, "name": "second"}]),
        );
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "plain structured JSON array without images: expected 1 part, got {}; raw: {user_content}",
            parts.len()
        );
        let result_arr = array(&parts[0], "functionResponse.response.result");
        assert_eq!(
            result_arr.len(),
            2,
            "plain structured JSON array without images: expected 2 array items in result, got {}; raw: {}",
            result_arr.len(),
            parts[0]
        );
        let got = string(&result_arr[0], "name");
        assert_eq!(
            got, "first",
            "plain structured JSON array without images: expected item 0 name 'first', got {got:?}"
        );
    }

    // plain string output
    {
        let input = single_call_with_output("echo", json!("plain string result"));
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "plain string output: expected 1 part, got {}; raw: {user_content}",
            parts.len()
        );
        let got = string(&parts[0], "functionResponse.response.result");
        assert_eq!(
            got, "plain string result",
            "plain string output: expected 'plain string result', got {got:?}"
        );
    }

    // structured JSON object with image_url property not an image block
    {
        let input = single_call_with_output(
            "get_hero",
            json!({
                "ok": true,
                "caption": "hero",
                "image_url": "https://example.com/hero.png"
            }),
        );
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "structured JSON object with image_url property not an image block: expected 1 part, got {}; raw: {user_content}",
            parts.len()
        );
        let got = string(&parts[0], "functionResponse.response.result.caption");
        assert_eq!(
            got, "hero",
            "structured JSON object with image_url property not an image block: expected caption 'hero', got {got:?}"
        );
    }

    // mixed array with text and non-image structured object
    {
        let input = single_call_with_output(
            "query",
            json!([
                {"type": "input_text", "text": "summary header"},
                {"id": 1, "status": "active"}
            ]),
        );
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "mixed array with text and non-image structured object: expected 1 part, got {}; raw: {user_content}",
            parts.len()
        );
        let result_arr = array(&parts[0], "functionResponse.response.result");
        assert_eq!(
            result_arr.len(),
            2,
            "mixed array with text and non-image structured object: expected raw JSON array with 2 items, got {}; raw: {}",
            result_arr.len(),
            parts[0]
        );
        let got = string(&result_arr[1], "status");
        assert_eq!(
            got, "active",
            "mixed array with text and non-image structured object: expected item 1 status 'active', got {got:?}"
        );
    }

    // stringified single-element object array preserved as string
    {
        let input = single_call_with_output("lookup", json!("[{\"id\":1}]"));
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "stringified single-element object array preserved as string: expected 1 part, got {}; raw: {user_content}",
            parts.len()
        );
        let got = string(&parts[0], "functionResponse.response.result");
        assert_eq!(
            got, r#"[{"id":1}]"#,
            "stringified single-element object array preserved as string: expected result to be {:?}, got {got:?}",
            r#"[{"id":1}]"#
        );
    }

    // nested image_url object with detail
    {
        let input = single_call_with_output(
            "photo",
            json!([
                {"type": "input_image", "image_url": {"url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="}, "detail": "high"}
            ]),
        );
        let output = convert("gemini-3.7-flash-high", &input);
        let user_content = &output["contents"][1];
        let parts = array(user_content, "parts");
        assert_eq!(
            parts.len(),
            1,
            "nested image_url object with detail: expected 1 part (functionResponse with nested inlineData), got {}; raw: {user_content}",
            parts.len()
        );
        let fr = parts[0].get("functionResponse").unwrap_or_else(|| {
            panic!(
                "nested image_url object with detail: expected functionResponse part, got {}",
                parts[0]
            )
        });
        let img = at(fr, "parts.0.inlineData").unwrap_or_else(|| {
            panic!(
                "nested image_url object with detail: expected functionResponse.parts.0 to have inlineData, got {fr}"
            )
        });
        let got = string(img, "mimeType");
        assert_eq!(
            got, "image/png",
            "nested image_url object with detail: expected mimeType 'image/png', got {got:?}"
        );
        let got = string(img, "data");
        assert_eq!(
            got, "iVBORw0KGgoAAAANSUhEUg==",
            "nested image_url object with detail: expected data 'iVBORw0KGgoAAAANSUhEUg==', got {got:?}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_parallel_function_call_outputs_with_images() {
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {
                "role": "user",
                "content": [{"type": "input_text", "text": "read both images"}]
            },
            {
                "type": "function_call",
                "id": "fc_a",
                "call_id": "call_a",
                "name": "read_a",
                "arguments": "{}"
            },
            {
                "type": "function_call",
                "id": "fc_b",
                "call_id": "call_b",
                "name": "read_b",
                "arguments": "{}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_a",
                "output": [
                    {"type": "input_text", "text": "file A"},
                    {"type": "input_image", "image_url": "data:image/png;base64,QUJD"}
                ]
            },
            {
                "type": "function_call_output",
                "call_id": "call_b",
                "output": [
                    {"type": "input_text", "text": "file B"},
                    {"type": "input_image", "image_url": "data:image/jpeg;base64,REVm"}
                ]
            }
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    let user_content = &output["contents"][2];
    assert_eq!(
        string(user_content, "role"),
        "user",
        "expected role user in tool response content, got {user_content}"
    );

    let parts = array(user_content, "parts");
    assert_eq!(
        parts.len(),
        2,
        "expected 2 functionResponse parts, got {}; raw: {user_content}",
        parts.len()
    );

    let mut got_by_id = HashMap::new();
    for part in parts {
        let fr = part
            .get("functionResponse")
            .unwrap_or_else(|| panic!("expected each part to be functionResponse, got {part}"));
        got_by_id.insert(string(fr, "id"), fr);
    }

    let fr_a = got_by_id
        .get("call_a")
        .unwrap_or_else(|| panic!("missing functionResponse for call_a; raw: {user_content}"));
    let got = string(fr_a, "parts.0.inlineData.mimeType");
    assert_eq!(
        got, "image/png",
        "expected call_a mimeType image/png, got {got:?}"
    );
    let got = string(fr_a, "parts.0.inlineData.data");
    assert_eq!(got, "QUJD", "expected call_a data QUJD, got {got:?}");

    let fr_b = got_by_id
        .get("call_b")
        .unwrap_or_else(|| panic!("missing functionResponse for call_b; raw: {user_content}"));
    let got = string(fr_b, "parts.0.inlineData.mimeType");
    assert_eq!(
        got, "image/jpeg",
        "expected call_b mimeType image/jpeg, got {got:?}"
    );
    let got = string(fr_b, "parts.0.inlineData.data");
    assert_eq!(got, "REVm", "expected call_b data REVm, got {got:?}");
}

#[test]
fn convert_openai_responses_request_to_gemini_function_call_output_with_multiple_images() {
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {
                "role": "user",
                "content": [{"type": "input_text", "text": "show two screenshots"}]
            },
            {
                "type": "function_call",
                "id": "fc_multi",
                "call_id": "call_multi",
                "name": "take_screenshots",
                "arguments": "{}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_multi",
                "output": [
                    {"type": "input_text", "text": "captured 2 images"},
                    {"type": "input_image", "image_url": "data:image/png;base64,QUJD"},
                    {"type": "input_image", "image_url": "data:image/jpeg;base64,REVm"}
                ]
            }
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    let user_content = &output["contents"][2];
    let parts = array(user_content, "parts");
    assert_eq!(
        parts.len(),
        1,
        "expected 1 functionResponse part, got {}; raw: {user_content}",
        parts.len()
    );

    let fr = parts[0]
        .get("functionResponse")
        .unwrap_or_else(|| panic!("expected functionResponse, got {}", parts[0]));
    let got = string(fr, "id");
    assert_eq!(got, "call_multi", "expected id call_multi, got {got:?}");
    let got = string(fr, "response.result");
    assert_eq!(
        got, "captured 2 images",
        "expected result 'captured 2 images', got {got:?}"
    );

    let image_parts = array(fr, "parts");
    assert_eq!(
        image_parts.len(),
        2,
        "expected 2 nested inlineData parts, got {}; raw: {fr}",
        image_parts.len()
    );
    let got = string(&image_parts[0], "inlineData.mimeType");
    assert_eq!(
        got, "image/png",
        "expected first image mimeType image/png, got {got:?}"
    );
    let got = string(&image_parts[0], "inlineData.data");
    assert_eq!(got, "QUJD", "expected first image data QUJD, got {got:?}");
    let got = string(&image_parts[1], "inlineData.mimeType");
    assert_eq!(
        got, "image/jpeg",
        "expected second image mimeType image/jpeg, got {got:?}"
    );
    let got = string(&image_parts[1], "inlineData.data");
    assert_eq!(got, "REVm", "expected second image data REVm, got {got:?}");
}

#[test]
fn convert_openai_responses_request_to_gemini_additional_tools_namespace_and_custom() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [
                    {
                        "type": "namespace",
                        "name": "functions",
                        "tools": [
                            {
                                "type": "custom",
                                "name": "exec",
                                "description": "Execute a command"
                            },
                            {
                                "type": "function",
                                "name": "continuity_probe",
                                "description": "Return a continuity probe",
                                "parameters": {
                                    "type": "object",
                                    "properties": {
                                        "value": {"type": "string"}
                                    },
                                    "required": ["value"]
                                }
                            }
                        ]
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Run probe"
                    }
                ]
            }
        ],
        "tool_choice": {
            "type": "function",
            "name": "continuity_probe",
            "namespace": "functions"
        }
    });

    let output = convert("gemini-2.5-flash", &input);
    let decls = array(&output, "tools.0.functionDeclarations");
    assert_eq!(
        decls.len(),
        2,
        "expected 2 functionDeclarations, got {}; raw: {output}",
        decls.len()
    );

    let exec_decl = &decls[0];
    let got = string(exec_decl, "name");
    assert_eq!(
        got, "functions__exec",
        "decl 0 name = {got:?}, want functions__exec"
    );
    assert_eq!(
        string(exec_decl, "parametersJsonSchema.properties.input.type"),
        "string",
        "decl 0 custom input schema missing: {exec_decl}"
    );

    let probe_decl = &decls[1];
    let got = string(probe_decl, "name");
    assert_eq!(
        got, "functions__continuity_probe",
        "decl 1 name = {got:?}, want functions__continuity_probe"
    );

    let mode = string(&output, "toolConfig.functionCallingConfig.mode");
    assert_eq!(mode, "ANY", "toolConfig mode = {mode:?}, want ANY");
    let allowed = string(
        &output,
        "toolConfig.functionCallingConfig.allowedFunctionNames.0",
    );
    assert_eq!(
        allowed, "functions__continuity_probe",
        "allowedFunctionNames = {allowed:?}, want functions__continuity_probe"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_replays_custom_tool_call_and_output() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "additional_tools",
                "tools": [
                    {
                        "type": "namespace",
                        "name": "functions",
                        "tools": [
                            {"type": "custom", "name": "exec"}
                        ]
                    }
                ]
            },
            {
                "type": "custom_tool_call",
                "call_id": "call_1",
                "name": "exec",
                "namespace": "functions",
                "input": "pwd"
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": "/workspace"
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let contents = array(&output, "contents");
    assert!(
        contents.len() >= 2,
        "expected at least 2 contents, got {}; raw: {output}",
        contents.len()
    );

    let call_part = at(&contents[0], "parts.0.functionCall")
        .unwrap_or_else(|| panic!("missing functionCall in content 0: {}", contents[0]));
    let got = string(call_part, "name");
    assert_eq!(
        got, "functions__exec",
        "functionCall name = {got:?}, want functions__exec"
    );
    let got = string(call_part, "args.input");
    assert_eq!(got, "pwd", "functionCall args.input = {got:?}, want pwd");

    let resp_part = at(&contents[1], "parts.0.functionResponse")
        .unwrap_or_else(|| panic!("missing functionResponse in content 1: {}", contents[1]));
    let got = string(resp_part, "name");
    assert_eq!(
        got, "functions__exec",
        "functionResponse name = {got:?}, want functions__exec"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_two_turn_custom_tool_roundtrip_with_reasoning() {
    // Turn 2 request: includes reasoning carrier before custom_tool_call, then
    // custom_tool_call_output.
    let input = json!({
        "model": "gemini-3.6-flash-high",
        "input": [
            {
                "type": "additional_tools",
                "tools": [
                    {
                        "type": "namespace",
                        "name": "functions",
                        "tools": [
                            {"type": "custom", "name": "exec"}
                        ]
                    }
                ]
            },
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Run pwd"}]},
            {"type": "reasoning", "encrypted_content": GEMINI_SIGNATURE, "summary": [{"type": "summary_text", "text": "executing pwd"}]},
            {
                "type": "custom_tool_call",
                "call_id": "call_1",
                "name": "exec",
                "namespace": "functions",
                "input": "pwd"
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": "/workspace"
            }
        ]
    });

    let output = convert("gemini-3.6-flash-high", &input);
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "expected 3 contents (user, model, user), got {}; raw: {output}",
        contents.len()
    );

    let model_parts = array(&contents[1], "parts");
    assert_eq!(
        model_parts.len(),
        2,
        "expected 2 parts in model content (thought + functionCall), got {}; raw: {}",
        model_parts.len(),
        contents[1]
    );
    assert!(
        boolean(&model_parts[0], "thought") && string(&model_parts[0], "text") == "executing pwd",
        "expected thought part with 'executing pwd', got: {}",
        model_parts[0]
    );
    assert_eq!(
        string(&model_parts[1], "functionCall.name"),
        "functions__exec",
        "expected functionCall name 'functions__exec', got: {}",
        model_parts[1]
    );
    assert_eq!(
        string(&model_parts[1], "thoughtSignature"),
        GEMINI_SIGNATURE,
        "expected thoughtSignature on functionCall, got: {}",
        model_parts[1]
    );

    let user_resp_parts = array(&contents[2], "parts");
    assert_eq!(
        user_resp_parts.len(),
        1,
        "expected 1 part in user tool response, got {}; raw: {}",
        user_resp_parts.len(),
        contents[2]
    );
    assert_eq!(
        string(&user_resp_parts[0], "functionResponse.name"),
        "functions__exec",
        "expected functionResponse name 'functions__exec', got: {}",
        user_resp_parts[0]
    );
    assert_eq!(
        string(&user_resp_parts[0], "functionResponse.response.result"),
        "/workspace",
        "expected functionResponse result '/workspace', got: {}",
        user_resp_parts[0]
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_function_call_output_alternate_ids_and_queue_fallback()
 {
    let test_cases = [
        (
            "call_id standard",
            Some(("call_id", "call_123")),
            "call_123",
            "Bash",
        ),
        (
            "id alternate field",
            Some(("id", "call_123")),
            "call_123",
            "Bash",
        ),
        (
            "tool_call_id alternate field",
            Some(("tool_call_id", "call_123")),
            "call_123",
            "Bash",
        ),
        (
            "callId alternate field",
            Some(("callId", "call_123")),
            "call_123",
            "Bash",
        ),
        (
            "missing call_id with name fallback to pending queue",
            Some(("name", "Bash")),
            "call_123",
            "Bash",
        ),
        (
            "missing call_id completely fallback to pending queue",
            None,
            "call_123",
            "Bash",
        ),
    ];

    for (name, output_field, want_call_id, want_name) in test_cases {
        let mut output_item = json!({"type":"function_call_output","output":"result"});
        if let Some((key, value)) = output_field {
            output_item[key] = json!(value);
        }

        let input = json!({
            "model": "gemini-3.7-flash-high",
            "input": [
                {"role":"user","content":"run bash"},
                {"type":"function_call","call_id":"call_123","name":"Bash","arguments":"{\"command\":\"pwd\"}"},
                output_item
            ]
        });

        let output = convert("gemini-3.7-flash-high", &input);
        let contents = array(&output, "contents");
        assert_eq!(
            contents.len(),
            3,
            "{name}: expected 3 contents, got {}; output={output}",
            contents.len()
        );

        let user_content = &contents[2];
        let parts = array(user_content, "parts");
        assert!(
            !parts.is_empty(),
            "{name}: expected at least 1 part in user response, got 0; output={output}"
        );

        let fr = parts[0]
            .get("functionResponse")
            .unwrap_or_else(|| panic!("{name}: missing functionResponse: {user_content}"));
        let got_id = string(fr, "id");
        assert_eq!(
            got_id, want_call_id,
            "{name}: functionResponse.id = {got_id:?}, want {want_call_id:?}; output={output}"
        );
        let got_name = string(fr, "name");
        assert_eq!(
            got_name, want_name,
            "{name}: functionResponse.name = {got_name:?}, want {want_name:?}; output={output}"
        );

        assert_pairing(
            &output,
            &format!("{name}: ValidateGeminiFunctionCallPairing failed"),
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_parallel_function_call_outputs_alternate_ids() {
    // Two tool calls: call-1 and call-2.
    // Two outputs: reversed order with tool_call_id and id.
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"role":"user","content":"run tools"},
            {"type":"function_call","call_id":"call-1","name":"tool_a","arguments":"{}"},
            {"type":"function_call","call_id":"call-2","name":"tool_b","arguments":"{}"},
            {"type":"function_call_output","tool_call_id":"call-2","output":"result_b"},
            {"type":"function_call_output","id":"call-1","output":"result_a"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    assert_pairing(&output, "parallel tool pairing validation failed");

    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "expected 3 contents (user, model, user), got {}; output={output}",
        contents.len()
    );

    let responses = array(&contents[2], "parts");
    assert_eq!(
        responses.len(),
        2,
        "expected 2 response parts, got {}; output={output}",
        responses.len()
    );

    // Must be ordered call-1 then call-2 to match model functionCall order.
    let got_id = string(&responses[0], "functionResponse.id");
    assert_eq!(
        got_id, "call-1",
        "first response id = {got_id:?}, want call-1"
    );
    let got_name = string(&responses[0], "functionResponse.name");
    assert_eq!(
        got_name, "tool_a",
        "first response name = {got_name:?}, want tool_a"
    );
    let got_id = string(&responses[1], "functionResponse.id");
    assert_eq!(
        got_id, "call-2",
        "second response id = {got_id:?}, want call-2"
    );
    let got_name = string(&responses[1], "functionResponse.name");
    assert_eq!(
        got_name, "tool_b",
        "second response name = {got_name:?}, want tool_b"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_dedicated_call_id_takes_precedence_over_item_id() {
    // Items have item IDs (id: "item_b", "item_a") in addition to tool_call_id
    // ("call_b", "call_a") in reverse order. Dedicated tool_call_id must take
    // precedence over item id so content results are not swapped.
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"role":"user","content":"run tasks"},
            {"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},
            {"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},
            {"type":"function_call_output","id":"item_b","tool_call_id":"call_b","output":"content_b"},
            {"type":"function_call_output","id":"item_a","tool_call_id":"call_a","output":"content_a"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    assert_pairing(&output, "pairing validation failed");

    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "expected 3 contents, got {}; output={output}",
        contents.len()
    );

    let responses = array(&contents[2], "parts");
    assert_eq!(
        responses.len(),
        2,
        "expected 2 responses, got {}; output={output}",
        responses.len()
    );

    // First response must pair with call_a and have content_a.
    let got_id = string(&responses[0], "functionResponse.id");
    assert_eq!(
        got_id, "call_a",
        "first response id = {got_id:?}, want call_a"
    );
    let got_result = string(&responses[0], "functionResponse.response.result");
    assert_eq!(
        got_result, "content_a",
        "first response result = {got_result:?}, want content_a (tool_call_id precedence check failed)"
    );

    // Second response must pair with call_b and have content_b.
    let got_id = string(&responses[1], "functionResponse.id");
    assert_eq!(
        got_id, "call_b",
        "second response id = {got_id:?}, want call_b"
    );
    let got_result = string(&responses[1], "functionResponse.response.result");
    assert_eq!(
        got_result, "content_b",
        "second response result = {got_result:?}, want content_b (tool_call_id precedence check failed)"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_explicit_unmatched_call_id_not_rebound() {
    // Pending call is call_1, but output has an explicit call_id:
    // "call_other". It must NOT be hijacked and rewritten to call_1; emit it
    // as user text.
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"role":"user","content":"run bash"},
            {"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_other","output":"other_result"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    assert_pairing(&output, "pairing validation failed");

    let mut unmatched_text_found = false;
    for content in array(&output, "contents") {
        for part in array(content, "parts") {
            if let Some(fr) = part.get("functionResponse") {
                assert!(
                    string(fr, "id") != "call_other"
                        && string(fr, "response.result") != "other_result",
                    "unmatched explicit call_id emitted as functionResponse: {output}"
                );
            }
            if string(content, "role") == "user" && string(part, "text") == "other_result" {
                unmatched_text_found = true;
            }
        }
    }
    assert!(
        unmatched_text_found,
        "expected unmatched call_other output as user text; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_mixed_missing_and_explicit_parallel_outputs_across_user_message()
 {
    // Call A, Call B.
    // Output 1 has NO ID (result B).
    // Intervening user message.
    // Output 2 explicitly has call_id: call_a (result A).
    // Call A must NOT be stolen by Output 1; Output 1 must get Call B.
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]},
            {"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},
            {"type":"function_call","call_id":"call_b","name":"tool_b","arguments":"{}"},
            {"type":"function_call_output","output":"result_b"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"intervening"}]},
            {"type":"function_call_output","call_id":"call_a","output":"result_a"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    let contents = array(&output, "contents");

    let mut result_map = HashMap::new();
    for content in contents {
        if string(content, "role") == "user" {
            for part in array(content, "parts") {
                if let Some(fr) = part.get("functionResponse") {
                    result_map.insert(string(fr, "id"), string(fr, "response.result"));
                }
            }
        }
    }

    let got = result_map.get("call_a").map_or("", String::as_str);
    assert_eq!(
        got, "result_a",
        "result for call_a = {got:?}, want result_a"
    );
    let got = result_map.get("call_b").map_or("", String::as_str);
    assert_eq!(
        got, "result_b",
        "result for call_b = {got:?}, want result_b"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_all_pending_calls_reserved_by_future_explicit_outputs_does_not_duplicate()
 {
    // Call A is the only pending call.
    // Output 1 has NO ID.
    // Intervening user message.
    // Output 2 explicitly has call_id: call_a.
    // Output 1 must NOT be bound to call_a; call_a must not be duplicated.
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"run"}]},
            {"type":"function_call","call_id":"call_a","name":"tool_a","arguments":"{}"},
            {"type":"function_call_output","output":"result_1"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]},
            {"type":"function_call_output","call_id":"call_a","output":"result_2"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    let contents = array(&output, "contents");

    let mut response_ids = Vec::new();
    let mut response_results = Vec::new();
    for content in contents {
        if string(content, "role") == "user" {
            for part in array(content, "parts") {
                if let Some(fr) = part.get("functionResponse") {
                    response_ids.push(string(fr, "id"));
                    response_results.push(string(fr, "response.result"));
                }
            }
        }
    }

    // Verify call_a is not duplicated.
    let call_a_count = response_ids.iter().filter(|id| *id == "call_a").count();
    assert_eq!(
        call_a_count, 1,
        "call_a appeared {call_a_count} times in responseIDs {response_ids:?}, want exactly 1"
    );

    // The response that carries call_a must be result_2 (the explicit one),
    // not result_1.
    for (idx, id) in response_ids.iter().enumerate() {
        if id == "call_a" {
            assert_eq!(
                response_results[idx], "result_2",
                "call_a was bound to result {:?}, want result_2",
                response_results[idx]
            );
        }
    }
}

/// Fails unless `response` answers `id` from `shell` with `result`.
fn assert_shell_response(response: Option<&Value>, id: &str, result: &str, context: &str) {
    let response = response.unwrap_or(&Value::Null);
    assert!(
        string(response, "id") == id
            && string(response, "name") == "shell"
            && string(response, "response.result") == result,
        "{context}: {response}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_interrupted_function_call_preserves_pairing() {
    let input = json!({
        "model": "gemini-3.8-flash-high",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"List the files."}]},
            {"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":[\"ls\"]}"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Stop, do something else instead."}]},
            {"type":"function_call","call_id":"c2","name":"shell","arguments":"{\"command\":[\"pwd\"]}"},
            {"type":"function_call_output","call_id":"c2","output":"/tmp\n"}
        ]
    });

    let result = convert("gemini-3.8-flash-high", &input);
    assert_pairing(
        &result,
        "ValidateGeminiFunctionCallPairing failed on Gemini request",
    );

    // Verify synthesized response for c1.
    assert_shell_response(
        at(&result, "contents.2.parts.0.functionResponse"),
        "c1",
        "call interrupted, no output",
        "unexpected synthesized response for c1",
    );
    // Verify real response for c2.
    assert_shell_response(
        at(&result, "contents.5.parts.0.functionResponse"),
        "c2",
        "/tmp\n",
        "unexpected real response for c2",
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_parallel_interrupted_function_call_preserves_pairing_and_order()
 {
    let input = json!({
        "model": "gemini-3.8-flash-high",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"List and print."}]},
            {"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":[\"ls\"]}"},
            {"type":"function_call","call_id":"c2","name":"shell","arguments":"{\"command\":[\"pwd\"]}"},
            {"type":"function_call_output","call_id":"c2","output":"/tmp\n"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Stop, do something else instead."}]},
            {"type":"function_call","call_id":"c3","name":"shell","arguments":"{\"command\":[\"whoami\"]}"},
            {"type":"function_call_output","call_id":"c3","output":"root\n"}
        ]
    });

    let result = convert("gemini-3.8-flash-high", &input);
    assert_pairing(
        &result,
        "ValidateGeminiFunctionCallPairing failed on parallel interrupted request",
    );

    // In the response turn for [c1, c2], c1 must be first (synthesized) and
    // c2 must be second (real).
    assert_shell_response(
        at(&result, "contents.2.parts.0.functionResponse"),
        "c1",
        "call interrupted, no output",
        "unexpected response part 0 for c1",
    );
    assert_shell_response(
        at(&result, "contents.2.parts.1.functionResponse"),
        "c2",
        "/tmp\n",
        "unexpected response part 1 for c2",
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_trailing_partial_parallel_calls_preserves_pairing_and_order()
 {
    let input = json!({
        "model": "gemini-3.8-flash-high",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"List and print."}]},
            {"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":[\"ls\"]}"},
            {"type":"function_call","call_id":"c2","name":"shell","arguments":"{\"command\":[\"pwd\"]}"},
            {"type":"function_call_output","call_id":"c2","output":"/tmp\n"}
        ]
    });

    let result = convert("gemini-3.8-flash-high", &input);
    assert_pairing(
        &result,
        "ValidateGeminiFunctionCallPairing failed on trailing partial parallel request",
    );

    // In the response turn for [c1, c2], c1 must be first (synthesized) and
    // c2 must be second (real).
    assert_shell_response(
        at(&result, "contents.2.parts.0.functionResponse"),
        "c1",
        "call interrupted, no output",
        "unexpected response part 0 for c1",
    );
    assert_shell_response(
        at(&result, "contents.2.parts.1.functionResponse"),
        "c2",
        "/tmp\n",
        "unexpected response part 1 for c2",
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_interrupted_message_before_real_output_preserves_pairing_and_order()
 {
    let input = json!({
        "model": "gemini-3.8-flash-high",
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"List and print."}]},
            {"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":[\"ls\"]}"},
            {"type":"function_call","call_id":"c2","name":"shell","arguments":"{\"command\":[\"pwd\"]}"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Stop, do something else instead."}]},
            {"type":"function_call_output","call_id":"c2","output":"/tmp\n"}
        ]
    });

    let result = convert("gemini-3.8-flash-high", &input);
    assert_pairing(
        &result,
        "ValidateGeminiFunctionCallPairing failed on interrupted message before real output request",
    );

    // The user message precedes the completed tool response turn.
    let stop_text = string(&result, "contents.2.parts.0.text");
    assert_eq!(
        stop_text, "Stop, do something else instead.",
        "unexpected text in contents[2]: {stop_text:?}"
    );
    // In the response turn for [c1, c2], c1 must be first (synthesized) and
    // c2 must be second (real).
    assert_shell_response(
        at(&result, "contents.3.parts.0.functionResponse"),
        "c1",
        "call interrupted, no output",
        "unexpected response part 0 for c1",
    );
    assert_shell_response(
        at(&result, "contents.3.parts.1.functionResponse"),
        "c2",
        "/tmp\n",
        "unexpected response part 1 for c2",
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_function_call_output_with_fco_item_id() {
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"role":"user","content":"run command"},
            {"type":"function_call","call_id":"call_1788961125480214178_817","name":"Bash","arguments":"{\"command\":\"pwd\"}"},
            {"type":"function_call_output","id":"fco_01a08664-2d16-7a91-8ab2-2eccd49e4c3e","output":"/tmp"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    assert_pairing(&output, "pairing validation failed");

    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        3,
        "expected 3 contents, got {}; output={output}",
        contents.len()
    );

    let responses = array(&contents[2], "parts");
    assert_eq!(
        responses.len(),
        1,
        "expected 1 response part, got {}; output={output}",
        responses.len()
    );

    let got_id = string(&responses[0], "functionResponse.id");
    assert_eq!(
        got_id, "call_1788961125480214178_817",
        "response id = {got_id:?}, want call_1788961125480214178_817"
    );
    let got_name = string(&responses[0], "functionResponse.name");
    assert_eq!(got_name, "Bash", "response name = {got_name:?}, want Bash");
}

#[test]
fn convert_openai_responses_request_to_gemini_orphan_function_call_output_becomes_user_text() {
    // Codex multi-agent sub-threads inject a send_message_to_thread card as
    // function_call_output with an fco_ item id and no preceding
    // function_call.
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"Task initialization"}]},
            {"type":"function_call_output","id":"fco_01a09fca-8d33-73a1-97fd-4d83ecc02f9d","name":"send_message_to_thread","output":"<codex_delegation>\n  <source_thread_id>01a022d7-d4d0-72b2-8571-4590484ccaee</source_thread_id>\n  <input>Execute sub-task</input>\n</codex_delegation>"},
            {"type":"function_call","call_id":"call_1789387253098037589_85","name":"Bash","arguments":"{\"command\":\"pwd\"}"},
            {"type":"function_call_output","call_id":"call_1789387253098037589_85","id":"fco_01a09fca-a5f0-7b40-9943-21fbc923c537","output":"/Users/developer"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    assert_pairing(&output, "pairing validation failed");

    let mut delegation_found = false;
    let mut bash_call_id = String::new();
    let mut bash_response_id = String::new();
    for content in array(&output, "contents") {
        for part in array(content, "parts") {
            if let Some(fr) = part.get("functionResponse") {
                assert!(
                    !string(fr, "id").is_empty(),
                    "orphan output emitted as functionResponse with empty id: {output}"
                );
                if string(fr, "name") == "Bash" {
                    bash_response_id = string(fr, "id");
                }
            }
            if string(part, "functionCall.name") == "Bash" {
                bash_call_id = string(part, "functionCall.id");
            }
            if string(content, "role") == "user"
                && string(part, "text").contains("<codex_delegation>")
            {
                delegation_found = true;
            }
        }
    }
    assert!(
        delegation_found,
        "expected orphan send_message_to_thread output as user text; output={output}"
    );
    assert_eq!(
        bash_call_id, "call_1789387253098037589_85",
        "bash functionCall.id = {bash_call_id:?}; output={output}"
    );
    assert_eq!(
        bash_response_id, "call_1789387253098037589_85",
        "bash functionResponse.id = {bash_response_id:?}; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_unpaired_explicit_call_id_becomes_user_text() {
    let input = json!({
        "model": "gemini-3.7-flash-high",
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"Task initialization"}]},
            {"type":"function_call_output","call_id":"call_missing","name":"send_message_to_thread","output":"<codex_delegation>Execute sub-task</codex_delegation>"},
            {"type":"function_call","call_id":"call_1789387253098037589_85","name":"Bash","arguments":"{\"command\":\"pwd\"}"},
            {"type":"function_call_output","call_id":"call_1789387253098037589_85","output":"/Users/developer"}
        ]
    });

    let output = convert("gemini-3.7-flash-high", &input);
    assert_pairing(&output, "pairing validation failed");

    let mut delegation_found = false;
    let mut bash_response_id = String::new();
    for content in array(&output, "contents") {
        for part in array(content, "parts") {
            if let Some(fr) = part.get("functionResponse") {
                assert_ne!(
                    string(fr, "id"),
                    "call_missing",
                    "unpaired output emitted as functionResponse: {output}"
                );
                if string(fr, "name") == "Bash" {
                    bash_response_id = string(fr, "id");
                }
            }
            if string(content, "role") == "user"
                && string(part, "text").contains("<codex_delegation>")
            {
                delegation_found = true;
            }
        }
    }
    assert!(
        delegation_found,
        "expected unpaired send_message_to_thread output as user text; output={output}"
    );
    assert_eq!(
        bash_response_id, "call_1789387253098037589_85",
        "bash functionResponse.id = {bash_response_id:?}; output={output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_parameters_json_schema_preserves_additional_properties_and_pattern_issue5959()
 {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": "hi",
        "tools": [{
            "type": "function",
            "name": "submit",
            "description": "Submit a bounded schema test value.",
            "parameters": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "recipient": {
                        "type": "string",
                        "pattern": "^(alice|bob)$"
                    },
                    "amount": {
                        "type": "number"
                    },
                    "nested": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "tag": {
                                "type": "string",
                                "pattern": "^[a-z]+$"
                            }
                        }
                    }
                },
                "required": ["recipient", "amount"]
            }
        }]
    });

    let output = convert("gemini-2.5-flash", &input);
    let schema = at(
        &output,
        "tools.0.functionDeclarations.0.parametersJsonSchema",
    )
    .unwrap_or_else(|| panic!("parametersJsonSchema missing. Output: {output}"));

    let got = at(schema, "additionalProperties");
    assert_eq!(
        got,
        Some(&Value::Bool(false)),
        "root additionalProperties should be preserved as false, got: {got:?}. Schema: {schema}"
    );
    let got = at(schema, "properties.recipient.pattern");
    assert!(
        got.is_some() && str_of(got) == "^(alice|bob)$",
        "pattern should be preserved, got: {got:?}. Schema: {schema}"
    );
    let got = at(schema, "properties.nested.additionalProperties");
    assert_eq!(
        got,
        Some(&Value::Bool(false)),
        "nested additionalProperties should be preserved as false, got: {got:?}. Schema: {schema}"
    );
    let got = at(schema, "properties.nested.properties.tag.pattern");
    assert!(
        got.is_some() && str_of(got) == "^[a-z]+$",
        "nested pattern should be preserved, got: {got:?}. Schema: {schema}"
    );
    assert!(
        !(exists(schema, "description")
            && string(schema, "description").contains("No extra properties allowed")),
        "additionalProperties: false should not be converted to description hint. Schema: {schema}"
    );
    let got = at(schema, "properties.recipient.description");
    assert!(
        !(got.is_some() && str_of(got).contains("pattern:")),
        "pattern should not be converted to description hint. Schema: {schema}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_audio_input() {
    let tests = [
        (
            "standard nested input_audio object",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}},
                            {"type": "input_text", "text": "Transcribe this audio"}
                        ]
                    }
                ]
            }),
            "audio/wav",
            "UklGRg==",
        ),
        (
            "flat input_audio fields",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "data": "SUQzBA==", "format": "mp3"}
                        ]
                    }
                ]
            }),
            "audio/mpeg",
            "SUQzBA==",
        ),
        (
            "audio_url data URI",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "audio_url": "data:audio/ogg;base64,T2dnUw=="}
                        ]
                    }
                ]
            }),
            "audio/ogg",
            "T2dnUw==",
        ),
        (
            "top-level input item audio",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}
                ]
            }),
            "audio/wav",
            "UklGRg==",
        ),
        (
            "nested audio alias object",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "audio", "audio": {"data": "SUQzBA==", "format": "mp3"}}
                        ]
                    }
                ]
            }),
            "audio/mpeg",
            "SUQzBA==",
        ),
        (
            "nested audio alias object with wav",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "audio", "audio": {"data": "UklGRg==", "format": "wav"}}
                        ]
                    }
                ]
            }),
            "audio/wav",
            "UklGRg==",
        ),
    ];

    for (name, input, want_mime, want_data) in tests {
        let output = convert("gemini-2.5-flash", &input);
        let contents = array(&output, "contents");
        assert!(
            !contents.is_empty(),
            "{name}: expected at least 1 content, got 0. Output: {output}"
        );
        let parts = array(&contents[0], "parts");
        assert!(
            !parts.is_empty(),
            "{name}: expected at least 1 part, got 0. Output: {output}"
        );
        let Some(inline_data) = parts.iter().find_map(|part| part.get("inline_data")) else {
            panic!("{name}: did not find inline_data in parts. Output: {output}");
        };
        let got = string(inline_data, "mime_type");
        assert_eq!(
            got, want_mime,
            "{name}: inline_data.mime_type = {got:?}, want {want_mime:?}"
        );
        let got = string(inline_data, "data");
        assert_eq!(
            got, want_data,
            "{name}: inline_data.data = {got:?}, want {want_data:?}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_video_input() {
    let tests = [
        (
            "openresponses input_video with data URL string",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_video", "video_url": "data:video/mp4;base64,AAAAIGZ0eXBtcDQy"},
                            {"type": "input_text", "text": "Describe the video"}
                        ]
                    }
                ]
            }),
            "video/mp4",
            "AAAAIGZ0eXBtcDQy",
        ),
        (
            "openresponses input_video with video_url object",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_video", "video_url": {"url": "data:video/webm;base64,GkXfo59ChoEBQveBAULygQ8="}}
                        ]
                    }
                ]
            }),
            "video/webm",
            "GkXfo59ChoEBQveBAULygQ8=",
        ),
        (
            "input_file with video filename",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_file", "filename": "sample.mp4", "file_data": "AAAAIGZ0eXA="}
                        ]
                    }
                ]
            }),
            "video/mp4",
            "AAAAIGZ0eXA=",
        ),
        (
            "top-level input item video and text combined",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {"type": "input_text", "text": "Check video"},
                    {"type": "input_video", "video_url": "data:video/mp4;base64,AAAAIGZ0eXA="}
                ]
            }),
            "video/mp4",
            "AAAAIGZ0eXA=",
        ),
        (
            "nested video object with format webm",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "video", "video": {"data": "GkXfo59ChoEBQveBAULygQ8=", "format": "webm"}}
                        ]
                    }
                ]
            }),
            "video/webm",
            "GkXfo59ChoEBQveBAULygQ8=",
        ),
    ];

    for (name, input, want_mime, want_data) in tests {
        let output = convert("gemini-2.5-flash", &input);
        let contents = array(&output, "contents");
        assert!(
            !contents.is_empty(),
            "{name}: expected at least 1 content, got 0. Output: {output}"
        );
        let mut found_video = false;
        for content in contents {
            // The first matching part of each content.
            if let Some(inline_data) = array(content, "parts")
                .iter()
                .filter_map(|part| part.get("inline_data"))
                .find(|inline_data| string(inline_data, "mime_type") == want_mime)
            {
                let got = string(inline_data, "data");
                assert_eq!(
                    got, want_data,
                    "{name}: inline_data.data = {got:?}, want {want_data:?}"
                );
                found_video = true;
            }
        }
        assert!(
            found_video,
            "{name}: did not find inline_data with mime {want_mime:?}. Output: {output}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_top_level_video_and_text_combined() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {"type": "input_text", "text": "Check video"},
            {"type": "input_video", "video_url": "data:video/mp4;base64,AAAAIGZ0eXA="}
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        1,
        "expected exactly 1 content, got {}. Output: {output}",
        contents.len()
    );
    let got = string(&contents[0], "role");
    assert_eq!(got, "user", "role = {got:?}, want user. Output: {output}");

    let parts = array(&contents[0], "parts");
    assert_eq!(
        parts.len(),
        2,
        "expected 2 parts, got {}. Output: {output}",
        parts.len()
    );
    let got = string(&parts[0], "text");
    assert_eq!(
        got, "Check video",
        "parts[0].text = {got:?}, want Check video. Output: {output}"
    );
    let got = string(&parts[1], "inline_data.mime_type");
    assert_eq!(
        got, "video/mp4",
        "parts[1].inline_data.mime_type = {got:?}, want video/mp4. Output: {output}"
    );
    let got = string(&parts[1], "inline_data.data");
    assert_eq!(
        got, "AAAAIGZ0eXA=",
        "parts[1].inline_data.data = {got:?}, want AAAAIGZ0eXA=. Output: {output}"
    );
}

/// Fails unless each part's `path` reads as the MIME type wanted for it.
fn assert_part_mimes(output: &Value, parts: &[Value], path: &str, wants: &[(&str, &str)]) {
    for (index, (want, label)) in wants.iter().enumerate() {
        let got = string(&parts[index], path);
        assert_eq!(
            got, *want,
            "parts[{index}] ({label}) {path} = {got:?}, want {want}. Output: {output}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_fallback_mime_on_generic_data_url() {
    // Image data URL with generic MIME and explicit mime_type or filename.
    let img_input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_image", "image_url": "data:;base64,AAAA", "mime_type": "image/jpeg"},
                    {"type": "input_file", "file_data": "data:;base64,BBBB", "mime_type": "application/pdf"},
                    {"type": "input_image", "image_url": "data:;base64,CCCC", "filename": "photo.jpeg"},
                    {"type": "input_file", "file_data": "data:;base64,DDDD", "format": "pdf"},
                    {"type": "input_image", "image_url": "data:;base64,EEEE", "filename": "photo"},
                    {"type": "input_image", "image_url": "data:;base64,FFFF", "filename": "photo.unknownext"},
                    {"type": "input_image", "image_url": "data:application/octet-stream;base64,GGGG", "filename": "photo.jpeg"},
                    {"type": "input_image", "image_url": "data:binary/octet-stream;base64,HHHH", "filename": "photo.jpeg"},
                    {"type": "input_file", "file_data": "QUJD", "filename": "report.pdf", "mime_type": "binary/octet-stream"}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &img_input);
    let parts = array(&output, "contents.0.parts");
    assert!(
        parts.len() >= 9,
        "expected 9 parts, got {}. Output: {output}",
        parts.len()
    );

    assert_part_mimes(
        &output,
        parts,
        "inline_data.mime_type",
        &[
            ("image/jpeg", "explicit mime_type"),
            ("application/pdf", "explicit mime_type"),
            ("image/jpeg", "filename fallback"),
            ("application/pdf", "format fallback"),
            ("image/png", "no extension fallback"),
            ("image/png", "unknown extension fallback"),
            (
                "image/jpeg",
                "application/octet-stream with filename fallback",
            ),
            ("image/jpeg", "binary/octet-stream with filename fallback"),
            (
                "application/pdf",
                "raw base64 file with binary/octet-stream fallback",
            ),
        ],
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_remote_url_does_not_override_explicit_format() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_audio", "audio_url": "https://example.com/download.bin", "format": "wav"},
                    {"type": "input_video", "video_url": "https://example.com/stream.bin", "format": "mp4"}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let parts = array(&output, "contents.0.parts");
    assert!(
        parts.len() >= 2,
        "expected 2 parts, got {}. Output: {output}",
        parts.len()
    );

    assert_part_mimes(
        &output,
        parts,
        "file_data.mime_type",
        &[("audio/wav", "audio"), ("video/mp4", "video")],
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_unknown_extension_falls_back_to_defaults() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_audio", "audio_url": "https://example.com/download.bin", "mime_type": "application/octet-stream"},
                    {"type": "input_video", "video_url": "https://example.com/download.bin", "mime_type": "application/octet-stream"}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let parts = array(&output, "contents.0.parts");
    assert!(
        parts.len() >= 2,
        "expected 2 parts, got {}. Output: {output}",
        parts.len()
    );

    assert_part_mimes(
        &output,
        parts,
        "file_data.mime_type",
        &[("audio/wav", "audio"), ("video/mp4", "video")],
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_inline_explicit_format_not_overridden_by_filename() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_audio", "data": "UklGRg==", "format": "wav", "filename": "download.bin"},
                    {"type": "input_video", "data": "AAAAIGZ0eXA=", "format": "mp4", "filename": "download.bin"}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let parts = array(&output, "contents.0.parts");
    assert!(
        parts.len() >= 2,
        "expected 2 parts, got {}. Output: {output}",
        parts.len()
    );

    assert_part_mimes(
        &output,
        parts,
        "inline_data.mime_type",
        &[("audio/wav", "audio"), ("video/mp4", "video")],
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_remote_nested_format_not_overridden_by_filename() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_video", "video_url": "https://example.com/download.bin", "input_video": {"format": "mp4"}},
                    {"type": "input_audio", "audio_url": "https://example.com/download.bin", "input_audio": {"mime_type": "audio/wav"}},
                    {"type": "input_file", "file_url": "https://example.com/download.bin", "file": {"format": "pdf"}},
                    {"type": "input_file", "file_url": "https://example.com/download.bin", "file": {"format": "jpeg"}},
                    {"type": "input_image", "image_url": "https://example.com/download.bin", "image": {"format": "jpeg"}}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let parts = array(&output, "contents.0.parts");
    assert!(
        parts.len() >= 5,
        "expected 5 parts, got {}. Output: {output}",
        parts.len()
    );

    assert_part_mimes(
        &output,
        parts,
        "file_data.mime_type",
        &[
            ("video/mp4", "video"),
            ("audio/wav", "audio"),
            ("application/pdf", "file"),
            ("image/jpeg", "jpeg file"),
            ("image/jpeg", "image.format jpeg"),
        ],
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_generic_mime_with_nested_format_not_overridden_by_filename()
 {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_video", "video_url": "https://example.com/download.bin", "mime_type": "application/octet-stream", "input_video": {"format": "mp4"}},
                    {"type": "input_audio", "audio_url": "https://example.com/download.bin", "mime_type": "binary/octet-stream", "input_audio": {"format": "wav"}},
                    {"type": "input_file", "file_url": "https://example.com/download.bin", "mime_type": "application/octet-stream", "file": {"format": "pdf"}}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let parts = array(&output, "contents.0.parts");
    assert!(
        parts.len() >= 3,
        "expected 3 parts, got {}. Output: {output}",
        parts.len()
    );

    assert_part_mimes(
        &output,
        parts,
        "file_data.mime_type",
        &[
            ("video/mp4", "video"),
            ("audio/wav", "audio"),
            ("application/pdf", "file"),
        ],
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_remote_media() {
    let input = json!({
        "model": "gemini-2.5-flash",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_video", "video_url": "https://example.com/stream.mp4"},
                    {"type": "input_audio", "audio_url": "https://example.com/recording.wav"},
                    {"type": "input_image", "image_url": "https://example.com/photo.jpeg"},
                    {"type": "input_image", "image_url": "https://example.com/legacy.bin", "format": "jpg"},
                    {"type": "input_video", "video_url": "https://example.com/clip.webm", "format": "binary/octet-stream"},
                    {"type": "input_audio", "audio_url": "https://example.com/recording.mp3", "mime_type": "application/octet-stream"}
                ]
            }
        ]
    });

    let output = convert("gemini-2.5-flash", &input);
    let contents = array(&output, "contents");
    assert!(
        !contents.is_empty(),
        "expected at least 1 content, got 0. Output: {output}"
    );

    let parts = array(&contents[0], "parts");
    assert!(
        parts.len() >= 6,
        "expected at least 6 parts, got {}. Output: {output}",
        parts.len()
    );

    let wanted = [
        (
            "video/mp4",
            "https://example.com/stream.mp4",
            "remote video",
        ),
        (
            "audio/wav",
            "https://example.com/recording.wav",
            "remote audio",
        ),
        (
            "image/jpeg",
            "https://example.com/photo.jpeg",
            "remote image (photo.jpeg)",
        ),
        (
            "image/jpeg",
            "https://example.com/legacy.bin",
            "remote image (legacy.bin with format jpg)",
        ),
        (
            "video/webm",
            "https://example.com/clip.webm",
            "remote video (clip.webm with generic format fallback)",
        ),
        (
            "audio/mpeg",
            "https://example.com/recording.mp3",
            "remote audio (recording.mp3 with generic format fallback)",
        ),
    ];
    let mut missing = Vec::new();
    for (mime, uri, label) in wanted {
        let found = parts
            .iter()
            .filter_map(|part| part.get("file_data"))
            .any(|file_data| {
                string(file_data, "mime_type") == mime && string(file_data, "file_uri") == uri
            });
        if !found {
            missing.push(label);
        }
    }
    assert!(
        missing.is_empty(),
        "expected {missing:?} part(s) not found in output: {output}"
    );
}

#[test]
fn convert_openai_responses_request_to_gemini_invalid_data_urls_rejected() {
    let tests = [
        (
            "empty base64 audio payload",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "data": "data:audio/wav;base64,"}
                        ]
                    }
                ]
            }),
        ),
        (
            "non-base64 data URL",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_video", "video_url": "data:text/plain,hello"}
                        ]
                    }
                ]
            }),
        ),
        (
            "corrupted base64 payload",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "data": "data:audio/wav;base64,!!!"}
                        ]
                    }
                ]
            }),
        ),
        (
            "uppercase DATA scheme with invalid payload",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "data": "DATA:audio/wav,hello"}
                        ]
                    }
                ]
            }),
        ),
        (
            "leading whitespace on data URL with corrupted base64",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_audio", "data": " DATA:audio/wav;base64,!!!"}
                        ]
                    }
                ]
            }),
        ),
        (
            "source base64 with invalid data URL",
            json!({
                "model": "gemini-2.5-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_image", "source": {"type": "base64", "data": " DATA:image/png;base64,!!!"}}
                        ]
                    }
                ]
            }),
        ),
    ];

    for (name, input) in tests {
        let output = convert("gemini-2.5-flash", &input);
        // No inline_data should be created for invalid data URLs.
        for part in all_parts(&output) {
            assert!(
                !exists(part, "inline_data"),
                "{name}: expected no inline_data for invalid data URL, got: {part}"
            );
        }
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_function_response_json_ref() {
    // nested $ref object output is serialized as string result
    {
        let input = json!({
            "model": "gemini-3.8-flash",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_openapi_operation",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": {
                        "responses": {
                            "400": {
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "$ref": "#/components/schemas/ErrorModel"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            ]
        });
        let output = convert("gemini-3.8-flash", &input);
        let result = at(
            &output,
            "contents.1.parts.0.functionResponse.response.result",
        );
        let Some(Value::String(result)) = result else {
            panic!(
                "nested $ref object: expected functionResponse.response.result to be string, got {result:?}"
            );
        };
        assert!(
            result.contains("#/components/schemas/ErrorModel"),
            "nested $ref object: expected string result to contain ref target, got {result:?}"
        );
    }

    // array with nested $ref object is serialized as string result
    {
        let input = json!({
            "model": "gemini-3.8-flash",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_openapi_schemas",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [
                        {
                            "schema": {
                                "$ref": "#/components/schemas/ErrorModel"
                            }
                        }
                    ]
                }
            ]
        });
        let output = convert("gemini-3.8-flash", &input);
        let result = at(
            &output,
            "contents.1.parts.0.functionResponse.response.result",
        );
        let Some(Value::String(result)) = result else {
            panic!(
                "array with nested $ref object: expected functionResponse.response.result to be string, got {result:?}"
            );
        };
        assert!(
            result.contains("#/components/schemas/ErrorModel"),
            "array with nested $ref object: expected string result to contain ref target, got {result:?}"
        );
    }

    // ordinary structured object without $ref remains raw JSON object
    {
        let input = json!({
            "model": "gemini-3.8-flash",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": {
                        "temperature": 72,
                        "condition": "sunny"
                    }
                }
            ]
        });
        let output = convert("gemini-3.8-flash", &input);
        let result = at(
            &output,
            "contents.1.parts.0.functionResponse.response.result",
        );
        let Some(result @ Value::Object(_)) = result else {
            panic!(
                "ordinary structured object: expected functionResponse.response.result to remain JSON object, got {result:?}"
            );
        };
        assert_eq!(
            int_of(&result["temperature"]),
            72,
            "ordinary structured object: expected temperature 72, got {}",
            result["temperature"]
        );
    }

    // array with $ref object and media block preserves inlineData and
    // stringifies result
    {
        let input = json!({
            "model": "gemini-3.8-flash",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "render_schema_diagram",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [
                        {
                            "type": "input_image",
                            "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
                        },
                        {
                            "schema": {
                                "$ref": "#/components/schemas/ErrorModel"
                            }
                        }
                    ]
                }
            ]
        });
        let output = convert("gemini-3.8-flash", &input);
        let fr = at(&output, "contents.1.parts.0.functionResponse").unwrap_or_else(|| {
            panic!("array with $ref object and media block: expected functionResponse part, got {output}")
        });
        let img = at(fr, "parts.0.inlineData").unwrap_or_else(|| {
            panic!(
                "array with $ref object and media block: expected functionResponse.parts.0 to have inlineData, got {fr}"
            )
        });
        let got = string(img, "mimeType");
        assert_eq!(
            got, "image/png",
            "array with $ref object and media block: expected mimeType 'image/png', got {got:?}"
        );
        let result = at(fr, "response.result");
        let Some(Value::String(result)) = result else {
            panic!(
                "array with $ref object and media block: expected string result, got {result:?}"
            );
        };
        assert!(
            result.contains("#/components/schemas/ErrorModel"),
            "array with $ref object and media block: expected string result to contain ref target, got {result:?}"
        );
    }
}

#[test]
fn convert_openai_responses_request_to_gemini_unsigned_model_text_does_not_synthesize_bypass_signature()
 {
    // When reasoning item has no signature (empty encrypted_content), the
    // following assistant text should not be injected with the synthetic
    // "skip_thought_signature_validator".
    let input = json!({
        "input": [
            {
                "type": "reasoning",
                "encrypted_content": "",
                "summary": [{"type": "summary_text", "text": "my thinking process"}]
            },
            {
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "my visible answer"}]
            }
        ]
    });

    let output = convert("gemini-3.8-flash", &input);
    let contents = array(&output, "contents");
    assert_eq!(
        contents.len(),
        1,
        "expected 1 content, got {} (raw: {output})",
        contents.len()
    );

    let parts = array(&contents[0], "parts");
    assert_eq!(
        parts.len(),
        2,
        "expected 2 parts (thought + text), got {}: {output}",
        parts.len()
    );

    let thought_part = &parts[0];
    assert!(
        boolean(thought_part, "thought") && string(thought_part, "text") == "my thinking process",
        "unexpected thought part: {thought_part}"
    );
    assert!(
        !exists(thought_part, "thoughtSignature"),
        "thought part should not have thoughtSignature when unsigned, got: {thought_part}"
    );

    let text_part = &parts[1];
    assert!(
        !boolean(text_part, "thought") && string(text_part, "text") == "my visible answer",
        "unexpected text part: {text_part}"
    );
    assert!(
        !exists(text_part, "thoughtSignature"),
        "visible text part should not have synthetic thoughtSignature, got: {text_part}"
    );

    // Verify with sanitizer: this payload must NOT trigger any drop or bypass
    // replacement.
    let mut sanitized = output.clone();
    sanitize_gemini_request_thought_signatures(&mut sanitized, "contents");
    let sanitized_parts = array(&sanitized, "contents.0.parts");
    assert_eq!(
        sanitized_parts.len(),
        2,
        "sanitizer altered parts count: {sanitized}"
    );
    assert!(
        !exists(&sanitized_parts[1], "thoughtSignature"),
        "sanitizer left unexpected thoughtSignature on text part: {}",
        sanitized_parts[1]
    );
}
