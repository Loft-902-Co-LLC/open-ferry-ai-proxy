//! Reasoning, ported from upstream's `xai_executor_test.go`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

/// `testValidGrokEncryptedContent`: 256 bytes that pass for xAI's.
fn valid_grok_encrypted_content() -> String {
    let mut buffer = Vec::with_capacity(256);
    for index in 0_u32..8 {
        let [low, middle, high, _] = index.to_le_bytes();
        buffer.extend_from_slice(&Sha256::digest([low, middle, high]));
    }
    STANDARD_NO_PAD.encode(buffer)
}

fn events(data: &str) -> Vec<Value> {
    normalize_summary_data_events(data.as_bytes().to_vec())
        .iter()
        .map(|event| serde_json::from_slice(event).expect("valid JSON"))
        .collect()
}

// TestSanitizeXAIInputEncryptedContent_DropsInvalidReasoningBlob.
#[test]
fn sanitize_drops_invalid_reasoning_blob() {
    let mut body = json!({"model": "grok-4.3", "input": [
        {"type": "reasoning", "summary": [], "encrypted_content": "bad"},
        {"type": "reasoning", "summary": [], "encrypted_content": "gAAAAABinvalid-gpt-shape"},
        {"role": "user", "content": "hi"}
    ]});
    sanitize_input_encrypted_content(&mut body);
    assert!(
        body["input"][0].get("encrypted_content").is_none(),
        "{body}"
    );
    assert!(
        body["input"][1].get("encrypted_content").is_none(),
        "{body}"
    );
}

// TestSanitizeXAIInputEncryptedContent_PreservesValidBlob.
#[test]
fn sanitize_preserves_valid_blob() {
    let sample = valid_grok_encrypted_content();
    let mut body = json!({"model": "grok-4.3", "input": [
        {"type": "reasoning", "summary": [], "encrypted_content": sample}
    ]});
    let before = body.clone();
    sanitize_input_encrypted_content(&mut body);
    assert_eq!(body, before);
}

// Not upstream's: a compaction item with an encrypted_content xAI can't
// take goes, null or not a string; a reasoning item loses only the field,
// and other items aren't looked at.
#[test]
fn sanitize_drops_compaction_items_and_non_string_contents() {
    let sample = valid_grok_encrypted_content();
    let mut body = json!({"input": [
        {"type": "compaction", "encrypted_content": null},
        {"type": " compaction ", "encrypted_content": 1},
        {"type": "compaction", "encrypted_content": sample},
        {"type": "compaction"},
        {"type": "reasoning", "summary": [], "encrypted_content": {"a": 1}},
        {"type": "message", "encrypted_content": "bad"}
    ]});
    sanitize_input_encrypted_content(&mut body);
    assert_eq!(
        body["input"],
        json!([
            {"type": "compaction", "encrypted_content": sample},
            {"type": "compaction"},
            {"type": "reasoning", "summary": []},
            {"type": "message", "encrypted_content": "bad"}
        ])
    );
    for (content, reason) in [
        (json!(true), "encrypted_content must be a string, got True"),
        (
            json!(false),
            "encrypted_content must be a string, got False",
        ),
        (json!(1.5), "encrypted_content must be a string, got Number"),
        (json!([]), "encrypted_content must be a string, got JSON"),
        (json!(null), "encrypted_content is null"),
    ] {
        assert_eq!(invalid_encrypted_content(&content).as_deref(), Some(reason));
    }
}

// Not upstream's: with nothing dropped, the input is left alone, summaries
// unmerged (normalize_input_reasoning_items merges them).
#[test]
fn sanitize_leaves_valid_input_unmerged() {
    let mut body = json!({"input": [
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "a"}]},
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "b"}]}
    ]});
    let before = body.clone();
    sanitize_input_encrypted_content(&mut body);
    assert_eq!(body, before);
}

// Not upstream's (upstream checks this through the request it sends; see
// the request's tests): null content and encrypted_content go, and a
// summary-only reasoning item joins the one before.
#[test]
fn normalize_input_reasoning_items_drops_nulls_and_merges() {
    let mut body = json!({"input": [
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "a"}], "content": null, "encrypted_content": null},
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "b"}], "content": null},
        {"type": "reasoning", "summary": []},
        {"type": "reasoning", "id": "rs_2", "summary": [{"type": "summary_text", "text": "c"}]},
        {"type": "reasoning", "summary": "d"},
        {"role": "user", "content": null}
    ]});
    normalize_input_reasoning_items(&mut body);
    assert_eq!(
        body["input"],
        json!([
            {"type": "reasoning", "summary": [
                {"type": "summary_text", "text": "a"},
                {"type": "summary_text", "text": "b"}
            ]},
            {"type": "reasoning", "summary": []},
            {"type": "reasoning", "id": "rs_2", "summary": [{"type": "summary_text", "text": "c"}]},
            {"type": "reasoning", "summary": "d"},
            {"role": "user", "content": null}
        ])
    );

    let mut not_a_list = json!({"input": "hi"});
    normalize_input_reasoning_items(&mut not_a_list);
    assert_eq!(not_a_list, json!({"input": "hi"}));
}

// Not upstream's, the unit half of
// TestXAIExecutorExecuteStreamNormalizesReasoningTextEvents (which runs the
// executor): each reasoning_text event becomes its summary event.
#[test]
fn normalizes_reasoning_text_events() {
    assert_eq!(
        events(
            r#"{"type":"response.content_part.added","item_id":"rs_1","output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":""}}"#
        ),
        [
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "part": {"type": "summary_text", "text": ""}, "summary_index": 0})
        ]
    );
    assert_eq!(
        events(
            r#"{"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":0,"delta":"thinking"}"#
        ),
        [
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "delta": "thinking", "summary_index": 0})
        ]
    );
    assert_eq!(
        events(
            r#"{"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":0,"text":"thinking"}"#
        ),
        [
            json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "output_index": 0, "text": "thinking", "summary_index": 0}),
            json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "output_index": 0, "part": {"type": "summary_text", "text": "thinking"}, "summary_index": 0})
        ]
    );
    assert_eq!(
        events(
            r#"{"type":"response.content_part.done","content_index":1,"summary_index":3,"part":{"type":"reasoning_text"}}"#
        ),
        [
            json!({"type": "response.reasoning_summary_part.done", "summary_index": 3, "part": {"type": "summary_text"}})
        ]
    );
    // An output text part isn't reasoning, and invalid JSON is passed on.
    let text_part =
        r#"{"type":"response.content_part.added","content_index":0,"part":{"type":"output_text"}}"#;
    assert_eq!(
        normalize_summary_data_events(text_part.as_bytes().to_vec()),
        [text_part.as_bytes().to_vec()]
    );
    assert_eq!(
        normalize_summary_data_events(b"{".to_vec()),
        [b"{".to_vec()]
    );
}

// Not upstream's, after TestXAIExecutorExecuteStreamNormalizesReasoningTextEvents
// and TestXAIExecutorExecuteNormalizesReasoningOutputForNonStreamTranslation
// (which run the executor): a reasoning item's reasoning_text content
// becomes its summary, in an event's item and a response's output.
#[test]
fn normalizes_reasoning_output_items() {
    let done = normalize_summary_data(
        br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"thinking"},{"type":"other"}]}}"#.to_vec(),
    );
    assert_eq!(
        String::from_utf8(done).expect("UTF-8"),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"thinking"}]}}"#
    );
    let completed = normalize_summary_data(
        br#"{"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"type":"reasoning_text","text":"a"}]},{"type":"message","content":[{"type":"reasoning_text"}]}]}}"#.to_vec(),
    );
    assert_eq!(
        String::from_utf8(completed).expect("UTF-8"),
        r#"{"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"a"}]},{"type":"message","content":[{"type":"reasoning_text"}]}]}}"#
    );
    let untouched =
        br#"{"type":"response.output_item.done", "item":{"type":"reasoning","summary":[]}}"#;
    assert_eq!(
        normalize_summary_data(untouched.to_vec()),
        untouched.to_vec()
    );
}

// Not upstream's: event lines are renamed as their data is.
#[test]
fn normalizes_event_lines() {
    let line = |line: &str, name: &str| {
        String::from_utf8(normalize_summary_event_line(line.as_bytes(), name)).expect("UTF-8")
    };
    assert_eq!(
        line("event:  response.reasoning_text.delta ", ""),
        "event: response.reasoning_summary_text.delta"
    );
    assert_eq!(
        line("event: response.reasoning_text.done", ""),
        "event: response.reasoning_summary_part.done"
    );
    assert_eq!(
        line("event: response.created", ""),
        "event: response.created"
    );
    assert_eq!(
        line(
            "event: response.reasoning_text.done",
            "response.reasoning_summary_text.done"
        ),
        "event: response.reasoning_summary_text.done"
    );
    assert_eq!(line(": comment", ""), ": comment");
    assert_eq!(line("event:", ""), "event:");
}
