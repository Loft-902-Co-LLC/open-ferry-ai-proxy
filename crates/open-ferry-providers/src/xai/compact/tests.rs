//! Not upstream's: upstream tests compact calls only through the executor
//! (see the executor's tests); these pin the helpers.

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use open_ferry_core::exec::{Format, Options, Request};

use super::*;
use crate::codex::request::Context;
use crate::xai::request::prepare;

/// 2026-01-01T00:00:00.0000002Z (Windows keeps time in 100 ns steps).
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(1_767_225_600, 200)
}

fn prepared(payload: &str) -> Prepared {
    let request = Request {
        model: "grok-4.3".into(),
        payload: Bytes::from(payload.to_owned()),
    };
    let context = Context {
        auth: None,
        config: None,
        models: None,
    };
    prepare(
        context,
        &request,
        &Options::new(Format::OPENAI_RESPONSE),
        false,
        Format::OPENAI_RESPONSE,
    )
    .expect("prepares")
}

#[test]
fn finds_input_items_by_type() {
    let payload = br#"{"input":[{"role":"user","content":"hi"},{"type":"compaction_trigger"}]}"#;
    assert!(input_has_item_type(payload, COMPACTION_TRIGGER));
    assert!(!input_has_item_type(payload, "compaction"));
    assert!(!input_has_item_type(
        br#"{"input":"compaction_trigger"}"#,
        COMPACTION_TRIGGER
    ));
    assert!(!input_has_item_type(b"not json", COMPACTION_TRIGGER));
}

#[test]
fn shapes_the_compact_body() {
    let mut body = json!({
        "model": "grok-4.3",
        "stream": true,
        "tools": [{"type": "function", "name": "lookup"}],
        "tool_choice": {"type": "function", "name": "lookup"},
        "max_output_tokens": 64,
        "temperature": 0.3,
        "top_p": 0.8,
        "top_k": 10,
        "stop": ["END"],
        "reasoning": {"effort": "high"},
        "input": [{"role": "user", "content": "hi"}, {"type": "compaction_trigger"}],
    });
    shape_body(&mut body, br#"{"previous_response_id":"  resp_1  "}"#);
    assert_eq!(
        body,
        json!({
            "model": "grok-4.3",
            "reasoning": {"effort": "high"},
            "input": [{"role": "user", "content": "hi"}],
            "previous_response_id": "resp_1",
        })
    );
    let mut body = json!({"input": []});
    shape_body(&mut body, br#"{"previous_response_id":"   "}"#);
    assert_eq!(body, json!({"input": []}));
}

#[test]
fn response_and_item_ids() {
    let id = |compact: Value| response_id(&compact, now());
    assert_eq!(id(json!({"id": " resp_abc "})), "resp_abc");
    assert_eq!(id(json!({"id": "cmp_abc"})), "resp_abc");
    assert_eq!(id(json!({"id": "abc"})), "resp_abc");
    assert_eq!(id(json!({})), "resp_xai_compaction_1767225600000000200");
    assert_eq!(item_id("resp_abc"), "cmp_abc");
    assert_eq!(item_id("resp_"), "cmp_resp_");
    assert_eq!(item_id("other"), "cmp_other");
}

#[test]
fn output_item_gets_a_type_and_an_id() {
    assert_eq!(
        output_item(&json!({"output": [{"encrypted_content": "e"}]}), "resp_1"),
        json!({"encrypted_content": "e", "type": "compaction", "id": "cmp_1"})
    );
    assert_eq!(
        output_item(
            &json!({"output": [{"type": "message", "id": "m"}]}),
            "resp_1"
        ),
        json!({"type": "message", "id": "m"})
    );
    assert_eq!(
        output_item(&json!({"output": ["text"]}), "resp_1"),
        json!({"type": "compaction", "id": "cmp_1"})
    );
}

#[test]
fn trigger_stream_has_six_frames_of_one_response() {
    let prepared = prepared(
        r#"{"model":"grok-4.3","instructions":"be brief","input":[{"role":"user","content":"hi"}]}"#,
    );
    let chunks = trigger_stream_chunks(
        &prepared,
        br#"{"id":"cmp_9","output":[{"type":"compaction","encrypted_content":"e"}],"usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}"#,
        now(),
    );
    let events: Vec<(String, Value)> = chunks
        .iter()
        .map(|chunk| {
            let text = String::from_utf8(chunk.clone()).unwrap();
            let rest = text.strip_prefix("event: ").expect("event line");
            let (event, rest) = rest.split_once("\ndata: ").expect("data line");
            let data = rest.strip_suffix("\n\n").expect("frame end");
            (event.to_owned(), serde_json::from_str(data).unwrap())
        })
        .collect();
    let names: Vec<&str> = events.iter().map(|(event, _)| event.as_str()).collect();
    assert_eq!(
        names,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "keepalive",
            "response.output_item.done",
            "response.completed",
        ]
    );
    for (index, (event, data)) in events.iter().enumerate() {
        assert_eq!(data["type"], event.as_str());
        assert_eq!(data["sequence_number"], index);
    }
    let created = &events[0].1["response"];
    assert_eq!(created["id"], "resp_9");
    assert_eq!(created["status"], "in_progress");
    assert_eq!(created["created_at"], 1_767_225_600);
    assert_eq!(created["model"], "grok-4.3");
    assert_eq!(created["instructions"], "be brief");
    let item = json!({"type": "compaction", "encrypted_content": "e", "id": "cmp_9"});
    assert_eq!(events[2].1["item"], item);
    assert_eq!(events[4].1["item"], item);
    let completed = &events[5].1["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["completed_at"], 1_767_225_600);
    assert_eq!(completed["output"], json!([item]));
    assert_eq!(completed["usage"]["total_tokens"], 4);
    assert_eq!(
        completed["usage"]["input_tokens_details"]["cached_tokens"],
        0
    );
    assert_eq!(
        completed["usage"]["output_tokens_details"]["reasoning_tokens"],
        0
    );
}
