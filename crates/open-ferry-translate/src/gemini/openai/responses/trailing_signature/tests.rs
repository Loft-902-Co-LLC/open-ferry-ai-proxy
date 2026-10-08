// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/trailing_signature_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests for signatures that trail a message's text: the response stream
//! keeps them in the replay cache instead of sending carriers, and the request
//! translator puts them back after the unchanged message, in their order.
//!
//! Dropped or changed tests:
//! - gemini_responses_cache_recovery_preserves_fallback_signature_order:
//!   upstream makes the first cache write fail for a moment by switching to a
//!   disabled Home store, so a later write stores the first signature along
//!   with the second, and checks that replay keeps them in order. There is no
//!   Home store here: a write fails only for a signature, message ID or text
//!   the cache can't take, and a later write can't store that signature
//!   either. So the test runs the same stream with both writes kept: no
//!   fallback carrier follows the message, and replay puts both signatures
//!   back in order.

use serde_json::{Value, json};

use super::super::test_support::convert_openai_responses_request_to_gemini;
use super::super::test_support::{GEMINI_SIGNATURE, different_gemini_signature, sse_events};
use super::super::{GeminiToOpenAIResponsesStream, array_of, at};
use super::*;

const MODEL: &str = "gemini-3.8-flash-high";

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|err| panic!("invalid test JSON {raw:?}: {err}"))
}

/// A stream line holding `chunk`.
fn data_line(chunk: Value) -> String {
    format!("data: {chunk}")
}

/// Runs `lines` through a new stream translator with no request and returns
/// every event.
fn run_stream(model: &str, lines: &[String]) -> Vec<(String, Value)> {
    let mut stream = GeminiToOpenAIResponsesStream::new(model, &Value::Null, &Value::Null);
    lines
        .iter()
        .flat_map(|line| sse_events(&stream.translate_line(line.as_bytes())))
        .collect()
}

/// The `response.output` of the last `response.completed` event, or `null`.
fn completed_output(events: &[(String, Value)]) -> Value {
    events
        .iter()
        .rev()
        .find(|(name, _)| name == "response.completed")
        .and_then(|(_, data)| at(data, "response.output"))
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn gemini_responses_late_signature_preserves_summary() {
    let lines = [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Let me think.","thought":true}]}}],"responseId":"issue-5513"}"#.to_owned(),
        r#"data: {"candidates":[{"content":{"parts":[{"text":"The answer is 42."}]}}]}"#.to_owned(),
        data_line(json!({
            "candidates": [{
                "content": {"parts": [{"text": "", "thoughtSignature": GEMINI_SIGNATURE}]},
                "finishReason": "STOP",
            }],
        })),
        "data: [DONE]".to_owned(),
    ];
    let events = run_stream(MODEL, &lines);
    for (name, data) in &events {
        if (name == "response.output_item.added" || name == "response.output_item.done")
            && str_of(at(data, "item.type")) == "reasoning"
            && array_of(at(data, "item.summary")).is_empty()
            && !str_of(at(data, "item.encrypted_content")).is_empty()
        {
            panic!("late signature emitted an empty reasoning item: {data}");
        }
    }
    let completed = completed_output(&events);
    assert!(
        array_of(Some(&completed)).len() == 2
            && str_of(at(&completed, "0.summary.0.text")) == "Let me think."
            && str_of(at(&completed, "1.type")) == "message",
        "unexpected completed output: {completed}"
    );
    let replayed =
        convert_openai_responses_request_to_gemini(MODEL, &json!({"input": completed}), false);
    assert!(
        str_of(at(&replayed, "contents.0.parts.1.thoughtSignature")) == GEMINI_SIGNATURE,
        "late text signature was not restored: {replayed}"
    );
}

#[test]
fn gemini_responses_cached_text_signature_replay_boundaries() {
    const MESSAGE_ID: &str = "msg_resp_issue-5513-boundaries_0";
    const ANSWER: &str = "An exact signed answer.";
    assert!(
        cache_text_signatures(MODEL, MESSAGE_ID, ANSWER, &[GEMINI_SIGNATURE.to_owned()]),
        "could not seed text signature replay cache"
    );
    let message = format!(
        r#"{{"id":"{MESSAGE_ID}","type":"message","role":"assistant","content":[{{"type":"output_text","text":"{ANSWER}"}}]}}"#
    );
    let cases = [
        ("exact", MODEL, message.clone(), true),
        ("different model", "gemini-other", message.clone(), false),
        (
            "edited text",
            MODEL,
            message.replace(ANSWER, "Edited answer."),
            false,
        ),
        (
            "different ID",
            MODEL,
            message.replace(MESSAGE_ID, &format!("{MESSAGE_ID}-other")),
            false,
        ),
        (
            "no ID",
            MODEL,
            message.replace(&format!(r#""id":"{MESSAGE_ID}","#), ""),
            false,
        ),
    ];
    for (name, model, message, want_signature) in cases {
        let request = parse(&format!(r#"{{"input":[{message}]}}"#));
        let output = convert_openai_responses_request_to_gemini(model, &request, false);
        let got = output.to_string().contains(GEMINI_SIGNATURE);
        assert_eq!(
            got, want_signature,
            "{name}: signature restored={got}, want {want_signature}: {output}"
        );
    }
    let carrier = json!({
        "type": "reasoning",
        "summary": [],
        "encrypted_content": encode(GEMINI_SIGNATURE, PREVIOUS, TEXT),
    });
    let request = parse(&format!(r#"{{"input":[{message},{carrier}]}}"#));
    let output = convert_openai_responses_request_to_gemini(MODEL, &request, false).to_string();
    assert!(
        output.matches(GEMINI_SIGNATURE).count() == 1,
        "explicit carrier duplicated cached signature: {output}"
    );
}

#[test]
fn gemini_responses_text_signature_cache_rejects_invalid_signature() {
    assert!(
        !cache_text_signatures(
            "gemini-test",
            "msg_resp_invalid_0",
            "answer",
            &["invalid".to_owned()]
        ),
        "invalid signature was accepted into replay cache"
    );
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"answer"},{"text":"","thoughtSignature":"invalid"}]},"finishReason":"STOP"}],"responseId":"invalid-cache-fallback"}"#;
    let completed = completed_output(&run_stream(
        "gemini-test",
        &[line.to_owned(), "[DONE]".to_owned()],
    ));
    assert!(
        array_of(Some(&completed)).len() == 2 && str_of(at(&completed, "1.type")) == "reasoning",
        "uncacheable signature lost its fallback carrier: {completed}"
    );
}

#[test]
fn gemini_responses_late_signature_replay_with_thinking_suffix() {
    let line = data_line(json!({
        "candidates": [{
            "content": {"parts": [
                {"text": "suffix answer"},
                {"text": "", "thoughtSignature": GEMINI_SIGNATURE},
            ]},
            "finishReason": "STOP",
        }],
        "responseId": "issue-5513-suffix",
    }));
    let completed = completed_output(&run_stream(
        "gemini-2.5-pro(8192)",
        &[line, "[DONE]".to_owned()],
    ));
    let output = convert_openai_responses_request_to_gemini(
        "gemini-2.5-pro",
        &json!({"input": completed}),
        false,
    );
    assert!(
        str_of(at(&output, "contents.0.parts.0.thoughtSignature")) == GEMINI_SIGNATURE,
        "executor base model could not restore suffixed response signature: {output}"
    );
}

#[test]
fn gemini_responses_cached_signatures_merge_explicit_prefix_in_order() {
    const MESSAGE_ID: &str = "msg_resp_issue-5513-merge_0";
    let signature2 = different_gemini_signature();
    assert!(
        cache_text_signatures(
            MODEL,
            MESSAGE_ID,
            "answer",
            &[GEMINI_SIGNATURE.to_owned(), signature2.clone()]
        ),
        "could not seed replay cache"
    );
    for explicit_signature in [GEMINI_SIGNATURE, signature2.as_str()] {
        let request = json!({
            "input": [
                {
                    "id": MESSAGE_ID,
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}],
                },
                {
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": encode(explicit_signature, PREVIOUS, TEXT),
                },
            ],
        });
        let output = convert_openai_responses_request_to_gemini(MODEL, &request, false);
        let parts = array_of(at(&output, "contents.0.parts"));
        assert!(
            parts.len() == 2
                && str_of(parts[0].get("text")) == "answer"
                && str_of(parts[0].get("thoughtSignature")) == GEMINI_SIGNATURE
                && str_of(parts[1].get("thoughtSignature")) == signature2,
            "partial explicit carrier changed cached signature order: {output}"
        );
    }
}

#[test]
fn gemini_responses_late_thought_signature_does_not_bind_earlier_message() {
    let signature2 = different_gemini_signature();
    let lines = [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"earlier answer"}]}}],"responseId":"issue-5513-thought-boundary"}"#.to_owned(),
        data_line(json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "later thought", "thought": true, "thoughtSignature": GEMINI_SIGNATURE},
                ]},
            }],
        })),
        data_line(json!({
            "candidates": [{
                "content": {"parts": [{"text": "", "thoughtSignature": signature2}]},
                "finishReason": "STOP",
            }],
        })),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run_stream(MODEL, &lines));
    let output =
        convert_openai_responses_request_to_gemini(MODEL, &json!({"input": completed}), false);
    let parts = array_of(at(&output, "contents.0.parts"));
    assert!(
        parts.len() >= 2
            && str_of(parts[0].get("thoughtSignature")).is_empty()
            && str_of(parts[1].get("thoughtSignature")) == GEMINI_SIGNATURE
            && output.to_string().contains(&signature2),
        "late thought signature rebound to the earlier message: {output}"
    );
}

#[test]
fn gemini_responses_cache_recovery_preserves_fallback_signature_order() {
    let signature2 = different_gemini_signature();
    let lines = [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"recovery answer"}]}}],"responseId":"issue-5513-cache-recovery"}"#.to_owned(),
        data_line(json!({
            "candidates": [{
                "content": {"parts": [{"text": "", "thoughtSignature": GEMINI_SIGNATURE}]},
            }],
        })),
        data_line(json!({
            "candidates": [{
                "content": {"parts": [{"text": "", "thoughtSignature": signature2}]},
                "finishReason": "STOP",
            }],
        })),
        "data: [DONE]".to_owned(),
    ];
    let completed = completed_output(&run_stream(MODEL, &lines));
    // Both cache writes succeed, so no fallback carrier follows the message.
    assert!(
        array_of(Some(&completed)).len() == 1 && str_of(at(&completed, "0.type")) == "message",
        "expected the message alone: {completed}"
    );
    let output =
        convert_openai_responses_request_to_gemini(MODEL, &json!({"input": completed}), false);
    let parts = array_of(at(&output, "contents.0.parts"));
    assert!(
        parts.len() == 2
            && str_of(parts[0].get("thoughtSignature")) == GEMINI_SIGNATURE
            && str_of(parts[1].get("thoughtSignature")) == signature2,
        "cache recovery changed fallback signature order: {output}"
    );
}
