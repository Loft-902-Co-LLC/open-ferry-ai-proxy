// Ported from CLIProxyAPI internal/translator/codex/openai/responses/codex_openai-responses_response_test.go
// (v8.0.20, MIT). https://github.com/router-for-me/CLIProxyAPI
//
// TestApplyPatchResponsesActualRequestGatesNativeCodex is split in two:
// apply_patch_call_passes_through and only_an_executor_bridge_converts.

use serde_json::{Value, json};

use super::*;

/// Looks up a dotted path such as `response.model`, like a plain gjson path.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` would return it.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The JSON payload of an SSE line. Upstream reads it with gjson, which skips
/// ahead to the first `{`, so it never strips `data:` itself.
fn payload(line: &[u8]) -> Value {
    let data = line.strip_prefix(b"data:").unwrap_or(line);
    serde_json::from_slice(data).expect("line carries valid JSON")
}

// translate_line returns exactly one line, so Go's output count check has no counterpart.
#[test]
fn created_includes_original_request_model() {
    let request = json!({"model": "original-codex-model"});
    let translated_request = json!({"model": "translated-codex-model"});
    for (event_name, raw) in [
        (
            "response.created",
            r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#,
        ),
        (
            "response.in_progress",
            r#"data: {"type":"response.in_progress","response":{"id":"resp_1"}}"#,
        ),
    ] {
        let stream =
            CodexToOpenAIResponsesStream::new("fallback-model", &request, &translated_request);
        let output = stream.translate_line(raw.as_bytes());
        assert_eq!(
            text_at(&payload(&output), "response.model"),
            "original-codex-model",
            "{event_name} model; payload={}",
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn non_stream_incomplete() {
    let raw = json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
            "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
        }
    });

    let out = convert_codex_response_to_openai_responses_non_stream(raw)
        .expect("terminal event converts");

    assert_eq!(text_at(&out, "status"), "incomplete", "payload={out}");
    assert_eq!(
        text_at(&out, "incomplete_details.reason"),
        "max_output_tokens",
        "payload={out}"
    );
}

const SEARCH_ITEM: &str = r#"{"id":"ws_fixture_0123456789abcdef0123456789abcdef","type":"web_search_call","status":"completed","action":{"type":"search","queries":["python asyncio documentation"],"query":"python asyncio documentation","sources":[{"type":"url","url":"https://docs.python.org/3.13/library/asyncio.html"},{"type":"url","url":"https://docs.python.org/3/library/asyncio-task.html?highlight=n"}]}}"#;
const MESSAGE_ITEM: &str = r#"{"id":"msg_fixture_0123456789abcdef0123456789abcdef","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Python docs","annotations":[{"type":"url_citation","start_index":0,"end_index":11,"title":"asyncio","url":"https://docs.python.org/3/library/asyncio.html?trk=article-ssr-frontend-pulse_little-text-block&utm_source=openai"}]}]}"#;
const CITATION_URL: &str = "https://docs.python.org/3/library/asyncio.html?trk=article-ssr-frontend-pulse_little-text-block&utm_source=openai";

// TestConvertCodexResponseToOpenAIResponses_PreservesWebSearchSources
#[test]
fn preserves_web_search_sources() {
    let request = json!({});
    let stream = CodexToOpenAIResponsesStream::new("gpt-6-luna", &request, &request);

    let done = format!(
        r#"data: {{"type":"response.output_item.done","output_index":0,"item":{SEARCH_ITEM}}}"#
    );
    let out = payload(&stream.translate_line(done.as_bytes()));
    assert_eq!(
        text_at(&out, "item.id"),
        "ws_fixture_0123456789abcdef0123456789abcdef"
    );
    let sources = at(&out, "item.action.sources")
        .and_then(Value::as_array)
        .expect("sources");
    assert_eq!(sources.len(), 2, "{out}");
    assert_eq!(
        text_at(&out, "item.action.sources.0.url"),
        "https://docs.python.org/3.13/library/asyncio.html"
    );
    assert_eq!(
        text_at(&out, "item.action.sources.1.url"),
        "https://docs.python.org/3/library/asyncio-task.html?highlight=n"
    );

    let completed = format!(
        r#"data: {{"type":"response.completed","response":{{"id":"resp_04f9","status":"completed","output":[{SEARCH_ITEM},{MESSAGE_ITEM}]}}}}"#
    );
    let out = payload(&stream.translate_line(completed.as_bytes()));
    assert_eq!(
        text_at(&out, "response.output.0.action.sources.1.url"),
        "https://docs.python.org/3/library/asyncio-task.html?highlight=n"
    );
    assert_eq!(
        text_at(&out, "response.output.1.id"),
        "msg_fixture_0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        text_at(&out, "response.output.1.content.0.annotations.0.url"),
        CITATION_URL
    );
}

// TestConvertCodexResponseToOpenAIResponsesNonStream_PreservesWebSearchSources
#[test]
fn non_stream_preserves_web_search_sources() {
    let raw = format!(
        r#"{{"type":"response.completed","response":{{"id":"resp_04f9","status":"completed","output":[{SEARCH_ITEM},{MESSAGE_ITEM}]}}}}"#
    );
    let out =
        convert_codex_response_to_openai_responses_non_stream(serde_json::from_str(&raw).unwrap())
            .expect("terminal event converts");

    assert_eq!(
        text_at(&out, "output.0.id"),
        "ws_fixture_0123456789abcdef0123456789abcdef"
    );
    let sources = at(&out, "output.0.action.sources")
        .and_then(Value::as_array)
        .expect("sources");
    assert_eq!(sources.len(), 2, "{out}");
    assert_eq!(
        text_at(&out, "output.0.action.sources.0.url"),
        "https://docs.python.org/3.13/library/asyncio.html"
    );
    assert_eq!(
        text_at(&out, "output.0.action.sources.1.url"),
        "https://docs.python.org/3/library/asyncio-task.html?highlight=n"
    );
    assert_eq!(
        text_at(&out, "output.1.content.0.annotations.0.type"),
        "url_citation"
    );
    assert_eq!(
        text_at(&out, "output.1.content.0.annotations.0.url"),
        CITATION_URL
    );
}

// TestConvertCodexResponseToOpenAIResponses_MissingSourcesStayAbsentWithCitation
#[test]
fn missing_sources_stay_absent_with_citation() {
    let search_item = r#"{"id":"ws_missing","type":"web_search_call","status":"completed","action":{"type":"search","query":"python asyncio documentation"}}"#;
    let message_item =
        MESSAGE_ITEM.replace("msg_fixture_0123456789abcdef0123456789abcdef", "msg_cited");
    let request = json!({});
    let stream = CodexToOpenAIResponsesStream::new("gpt-6-luna", &request, &request);

    let done = format!(
        r#"data: {{"type":"response.output_item.done","output_index":0,"item":{search_item}}}"#
    );
    let out = payload(&stream.translate_line(done.as_bytes()));
    assert!(
        at(&out, "item.action.sources").is_none(),
        "sources should not be fabricated on output_item.done: {out}"
    );

    let completed = format!(
        r#"{{"type":"response.completed","response":{{"id":"resp_missing","status":"completed","output":[{search_item},{message_item}]}}}}"#
    );
    let out = payload(&stream.translate_line(format!("data: {completed}").as_bytes()));
    assert!(
        at(&out, "response.output.0.action.sources").is_none(),
        "sources should not be fabricated on response.completed: {out}"
    );
    assert_eq!(
        text_at(&out, "response.output.1.content.0.annotations.0.type"),
        "url_citation",
        "{out}"
    );

    let out = convert_codex_response_to_openai_responses_non_stream(
        serde_json::from_str(&completed).unwrap(),
    )
    .expect("terminal event converts");
    assert!(
        at(&out, "output.0.action.sources").is_none(),
        "sources should not be fabricated on non-stream response: {out}"
    );
    assert_eq!(
        text_at(&out, "output.1.content.0.annotations.0.url"),
        CITATION_URL
    );
}

#[test]
fn apply_patch_call_passes_through() {
    let original = json!({"tools": [{"type": "custom", "name": "apply_patch"}]});
    let raw = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#;

    let stream = CodexToOpenAIResponsesStream::new("m", &original, &original);
    let out = stream.translate_line(raw.as_bytes());

    assert_eq!(
        &*out,
        raw.as_bytes(),
        "native Codex changed: {}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
fn only_an_executor_bridge_converts() {
    let original = json!({"tools": [{"type": "custom", "name": "apply_patch"}]});
    let mut bridged = original.clone();
    crate::apply_patch::responses::normalize_request(&mut bridged).unwrap();
    let raw = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#;
    let stream = CodexToOpenAIResponsesStream::new("m", &original, &bridged);

    let out = stream.translate_line(raw.as_bytes());
    assert_eq!(
        &*out,
        raw.as_bytes(),
        "configuration enabled native bridging"
    );

    let mut xai = Bridge::new(&original);
    let out = stream.translate_line_with_bridge(raw.as_bytes(), &mut xai);
    assert_eq!(out.len(), 3, "same Codex wire format bypassed bridge");
    assert!(out.iter().all(|line| line.starts_with(b"data: ")));
    assert_eq!(text_at(&payload(&out[2]), "item.input"), "p");

    let mut failed = Bridge::new(&original);
    let line = br#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"apply_patch","arguments":"{}"}}"#;
    let out = stream.translate_line_with_bridge(line, &mut failed);
    assert_eq!(out.len(), 1);
    assert_eq!(text_at(&payload(&out[0]), "type"), "response.failed");
    assert!(failed.tool_input_error().is_some());
}

// Not upstream's.

#[test]
fn bridge_sees_the_model_and_converts_non_stream() {
    let original = json!({"model": "m1", "tools": [{"type": "custom", "name": "apply_patch"}]});
    let stream = CodexToOpenAIResponsesStream::new("m", &original, &original);
    let mut bridge = Bridge::new(&original);
    let out = stream.translate_line_with_bridge(
        br#"data:{"type":"response.created","response":{"id":"r"}}"#,
        &mut bridge,
    );
    assert_eq!(
        out,
        [br#"data: {"type":"response.created","response":{"id":"r","model":"m1"}}"#]
    );
    let out = stream.translate_line_with_bridge(b"event: response.created", &mut bridge);
    assert_eq!(out, [b"event: response.created"]);

    let body = json!({"type": "response.completed", "response": {"output": [
        {"type": "function_call", "id": "a", "call_id": "c", "name": "apply_patch", "arguments": "{\"input\":\"p\"}"}
    ]}});
    let mut bridge = Bridge::new(&original);
    let response =
        convert_codex_response_to_openai_responses_non_stream_with_bridge(body, &mut bridge)
            .unwrap();
    assert_eq!(text_at(&response, "output.0.type"), "custom_tool_call");
    assert_eq!(text_at(&response, "output.0.input"), "p");

    let body = json!({"type": "response.completed", "response": {"output": [
        {"type": "function_call", "name": "apply_patch", "arguments": "{}"}
    ]}});
    let mut bridge = Bridge::new(&original);
    assert_eq!(
        convert_codex_response_to_openai_responses_non_stream_with_bridge(body, &mut bridge),
        None
    );
    assert!(bridge.tool_input_error().is_some());
}
