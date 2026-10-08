// Ported from CLIProxyAPI internal/translator/codex/claude/codex_claude_response_test.go
// and codex_claude_parallel_function_calls_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;

const EMPTY_REQUEST: &str = r#"{"messages":[]}"#;

fn request(json: &str) -> Value {
    serde_json::from_str(json).expect("test request is valid JSON")
}

/// Feeds `lines` through one stream and returns everything it emits.
fn run_stream(original_request: &str, lines: &[&str]) -> String {
    let mut stream = CodexToClaudeStream::new(&request(original_request));
    lines
        .iter()
        .map(|line| stream.translate_line(line.as_bytes()))
        .collect()
}

fn convert_non_stream(original_request: &str, response: &str) -> Value {
    let response: Value = serde_json::from_str(response).expect("test response is valid JSON");
    convert_codex_response_to_claude_non_stream(&request(original_request), &response)
        .expect("terminal event converts")
}

/// Splits Claude SSE output into `(event, payload)` frames.
fn frames(output: &str) -> Vec<(String, Value)> {
    let mut frames = Vec::new();
    let mut event = "";
    for line in output.split('\n') {
        if let Some(name) = line.strip_prefix("event: ") {
            event = name;
        } else if let Some(data) = line.strip_prefix("data: ") {
            let payload = serde_json::from_str(data).expect("data line is valid JSON");
            frames.push((event.to_owned(), payload));
        }
    }
    frames
}

fn payloads(output: &str) -> Vec<Value> {
    frames(output)
        .into_iter()
        .map(|(_, payload)| payload)
        .collect()
}

fn first_payload_for_event(output: &str, event: &str) -> Option<Value> {
    frames(output)
        .into_iter()
        .find(|(name, _)| name == event)
        .map(|(_, payload)| payload)
}

fn find_message_delta(output: &str) -> Option<Value> {
    payloads(output)
        .into_iter()
        .find(|payload| text_at(payload, "type") == "message_delta")
}

fn find_stop_reason(output: &str) -> Option<String> {
    find_message_delta(output).map(|delta| text_at(&delta, "delta.stop_reason"))
}

/// Looks up a dotted path such as `content.0.id`, like a plain gjson path.
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

/// The value at `path` as gjson's `Int()` would return it.
fn int_at(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
}

#[test]
fn stream_thinking_includes_signature() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_123","model":"gpt-5"}}"#,
            r#"data: {"type":"response.reasoning_summary_part.added"}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
            r#"data: {"type":"response.reasoning_summary_part.done"}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_123"}}"#,
        ],
    );

    let mut start_found = false;
    let mut signature_delta_found = false;
    let mut stop_found = false;
    for data in payloads(&output) {
        match &*text_at(&data, "type") {
            "content_block_start" => {
                if text_at(&data, "content_block.type") == "thinking" {
                    start_found = true;
                    assert!(
                        at(&data, "content_block.signature").is_none(),
                        "thinking start block should not have a signature field when the signature is unknown: {data}"
                    );
                }
            }
            "content_block_delta" => {
                if text_at(&data, "delta.type") == "signature_delta" {
                    signature_delta_found = true;
                    assert_eq!(text_at(&data, "delta.signature"), "enc_sig_123");
                }
            }
            "content_block_stop" => stop_found = true,
            _ => {}
        }
    }

    assert!(start_found, "expected thinking content_block_start event");
    assert!(
        signature_delta_found,
        "expected signature_delta event for thinking block"
    );
    assert!(
        stop_found,
        "expected content_block_stop event for thinking block"
    );
}

#[test]
fn stream_cyber_policy_error() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"This content was flagged for possible cybersecurity risk.","param":null},"sequence_number":3}"#,
        ],
    );
    assert!(
        output.contains("event: error\n"),
        "expected Claude SSE error event, got: {output:?}"
    );

    let payload = first_payload_for_event(&output, "error")
        .unwrap_or_else(|| panic!("missing error event payload: {output:?}"));
    assert_eq!(text_at(&payload, "type"), "error", "{payload}");
    assert_eq!(
        text_at(&payload, "error.type"),
        "invalid_request_error",
        "{payload}"
    );
    assert_eq!(
        text_at(&payload, "error.message"),
        "This content was flagged for possible cybersecurity risk.",
        "{payload}"
    );
}

#[test]
fn stream_error_type_fallback_message() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[r#"data: {"type":"error","error":{},"error_type":"overloaded_error"}"#],
    );

    let payload = first_payload_for_event(&output, "error")
        .unwrap_or_else(|| panic!("missing error event payload: {output:?}"));
    assert_eq!(
        text_at(&payload, "error.type"),
        "overloaded_error",
        "{payload}"
    );
    assert_eq!(
        text_at(&payload, "error.message"),
        "overloaded_error",
        "{payload}"
    );
}

#[test]
fn stream_thinking_without_reasoning_item_still_includes_signature_field() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"response.reasoning_summary_part.added"}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
            r#"data: {"type":"response.reasoning_summary_part.done"}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ],
    );

    let mut thinking_start_found = false;
    let mut thinking_stop_found = false;
    let mut signature_delta_found = false;
    for data in payloads(&output) {
        let kind = text_at(&data, "type");
        if kind == "content_block_start" && text_at(&data, "content_block.type") == "thinking" {
            thinking_start_found = true;
            assert!(
                at(&data, "content_block.signature").is_none(),
                "thinking start block should not have a signature field without encrypted_content: {data}"
            );
        }
        if kind == "content_block_stop" && int_at(&data, "index") == 0 {
            thinking_stop_found = true;
        }
        if kind == "content_block_delta" && text_at(&data, "delta.type") == "signature_delta" {
            signature_delta_found = true;
        }
    }

    assert!(
        thinking_start_found,
        "expected thinking content_block_start event"
    );
    assert!(
        thinking_stop_found,
        "expected thinking content_block_stop event"
    );
    assert!(
        !signature_delta_found,
        "did not expect signature_delta without encrypted_content"
    );
}

/// The thinking-related events of a stream: block and signature counts and
/// the reassembled thinking text.
#[derive(Default)]
struct ThinkingDigest {
    starts: usize,
    stops: usize,
    signatures: Vec<String>,
    thinking: String,
    raw: String,
}

fn digest_thinking_stream(lines: &[&str]) -> ThinkingDigest {
    let raw = run_stream(EMPTY_REQUEST, lines);
    let mut digest = ThinkingDigest::default();
    for data in payloads(&raw) {
        match &*text_at(&data, "type") {
            "content_block_start" => {
                if text_at(&data, "content_block.type") == "thinking" {
                    digest.starts += 1;
                }
            }
            "content_block_delta" => match &*text_at(&data, "delta.type") {
                "thinking_delta" => digest.thinking.push_str(&text_at(&data, "delta.thinking")),
                "signature_delta" => digest.signatures.push(text_at(&data, "delta.signature")),
                _ => {}
            },
            "content_block_stop" => digest.stops += 1,
            _ => {}
        }
    }
    digest.raw = raw;
    digest
}

#[test]
fn stream_thinking_keeps_single_block_across_summary_parts() {
    let digest = digest_thinking_stream(&[
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"First part"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Second part"}"#,
    ]);

    assert_eq!(
        digest.starts, 1,
        "expected a single thinking block start for one reasoning item"
    );
    assert_eq!(
        digest.stops, 0,
        "expected the thinking block to stay open until output_item.done"
    );
    assert_eq!(digest.thinking, "First part\n\nSecond part");
}

#[test]
fn stream_thinking_emits_single_signature_across_multipart_reasoning() {
    let digest = digest_thinking_stream(&[
        r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_multipart"}}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"First part"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Second part"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning"}}"#,
    ]);

    assert!(
        digest.starts == 1 && digest.stops == 1,
        "expected exactly one thinking block, got {} starts and {} stops",
        digest.starts,
        digest.stops
    );
    // output_item.done omits encrypted_content here, so the pre-content fallback is expected.
    assert_eq!(
        digest.signatures,
        ["enc_sig_multipart"],
        "expected one signature_delta for one reasoning item"
    );
    assert_eq!(digest.thinking, "First part\n\nSecond part");
}

// output_item.added carries a pre-content snapshot of encrypted_content that
// always differs from the final value on output_item.done. Emitting the
// snapshot makes the client replay bogus reasoning items.
#[test]
fn stream_thinking_never_emits_pre_content_encrypted_content() {
    let digest = digest_thinking_stream(&[
        r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_pre_content_snapshot"}}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Part A"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Part B"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Part C"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_final"}}"#,
    ]);

    assert!(
        digest.starts == 1 && digest.stops == 1,
        "expected one thinking block for one reasoning item with three summary parts, got {} starts and {} stops",
        digest.starts,
        digest.stops
    );
    assert_eq!(
        digest.signatures,
        ["enc_sig_final"],
        "expected exactly one signature_delta carrying the final encrypted_content"
    );
    assert!(
        !digest.raw.contains("enc_sig_pre_content_snapshot"),
        "pre-content encrypted_content snapshot leaked into the Claude stream"
    );
    assert_eq!(digest.thinking, "Part A\n\nPart B\n\nPart C");
}

#[test]
fn stream_thinking_emits_one_block_per_reasoning_item() {
    let digest = digest_thinking_stream(&[
        r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_pre_1"}}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"First item"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_final_1"}}"#,
        r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_pre_2"}}"#,
        r#"data: {"type":"response.reasoning_summary_part.added"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Second item"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done"}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_final_2"}}"#,
    ]);

    assert!(
        digest.starts == 2 && digest.stops == 2,
        "expected two thinking blocks for two reasoning items, got {} starts and {} stops",
        digest.starts,
        digest.stops
    );
    assert_eq!(
        digest.signatures,
        ["enc_final_1", "enc_final_2"],
        "expected each block signed with its own final encrypted_content"
    );
    assert!(
        !digest.raw.contains("enc_pre_1") && !digest.raw.contains("enc_pre_2"),
        "pre-content encrypted_content snapshot leaked into the Claude stream"
    );
}

#[test]
fn stream_thinking_uses_early_captured_signature_when_done_omits_it() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_early"}}"#,
            r#"data: {"type":"response.reasoning_summary_part.added"}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning"}}"#,
        ],
    );

    let mut signature_delta_count = 0;
    for data in payloads(&output) {
        if text_at(&data, "type") == "content_block_delta"
            && text_at(&data, "delta.type") == "signature_delta"
        {
            signature_delta_count += 1;
            assert_eq!(text_at(&data, "delta.signature"), "enc_sig_early");
        }
    }

    assert_eq!(
        signature_delta_count, 1,
        "expected signature_delta from early-captured signature"
    );
}

#[test]
fn stream_thinking_uses_final_done_signature() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_initial"}}"#,
            r#"data: {"type":"response.reasoning_summary_part.added"}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Let me think"}"#,
            r#"data: {"type":"response.reasoning_summary_part.done"}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_final"}}"#,
        ],
    );

    let mut signature_delta_count = 0;
    let mut events = Vec::new();
    for data in payloads(&output) {
        let kind = text_at(&data, "type");
        let delta_type = text_at(&data, "delta.type");
        if kind == "content_block_start" && text_at(&data, "content_block.type") == "thinking" {
            events.push("thinking_start");
        }
        if kind == "content_block_delta" && delta_type == "thinking_delta" {
            events.push("thinking_delta");
        }
        if kind == "content_block_stop" && int_at(&data, "index") == 0 {
            events.push("thinking_stop");
        }
        if kind != "content_block_delta" || delta_type != "signature_delta" {
            continue;
        }
        events.push("signature_delta");
        signature_delta_count += 1;
        assert_eq!(
            text_at(&data, "delta.signature"),
            "enc_sig_final",
            "signature delta should be the final done signature"
        );
    }

    assert_eq!(signature_delta_count, 1, "expected one signature_delta");
    assert_eq!(
        events.join(","),
        "thinking_start,thinking_delta,signature_delta,thinking_stop",
        "thinking event order"
    );
}

#[test]
fn stream_signature_only_reasoning_emits_thinking_signature() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_123","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_sig_initial"}}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_sig_only"}}"#,
            r#"data: {"type":"response.content_part.added"}"#,
            r#"data: {"type":"response.output_text.delta","delta":"ok"}"#,
        ],
    );

    let mut thinking_start_found = false;
    let mut thinking_delta_found = false;
    let mut signature_delta_found = false;
    let mut thinking_stop_found = false;
    let mut text_start_index = -1;
    let mut events = Vec::new();
    for data in payloads(&output) {
        match &*text_at(&data, "type") {
            "content_block_start" => {
                if text_at(&data, "content_block.type") == "thinking" {
                    events.push("thinking_start");
                    thinking_start_found = true;
                    assert_eq!(int_at(&data, "index"), 0, "thinking block index");
                }
                if text_at(&data, "content_block.type") == "text" {
                    events.push("text_start");
                    text_start_index = int_at(&data, "index");
                }
            }
            "content_block_delta" => match &*text_at(&data, "delta.type") {
                "thinking_delta" => thinking_delta_found = true,
                "signature_delta" => {
                    events.push("signature_delta");
                    signature_delta_found = true;
                    assert_eq!(int_at(&data, "index"), 0, "signature delta index");
                    assert_eq!(text_at(&data, "delta.signature"), "enc_sig_only");
                }
                _ => {}
            },
            "content_block_stop" if int_at(&data, "index") == 0 => {
                events.push("thinking_stop");
                thinking_stop_found = true;
            }
            _ => {}
        }
    }

    assert!(
        thinking_start_found,
        "expected signature-only reasoning to start a thinking block"
    );
    assert!(
        !thinking_delta_found,
        "did not expect thinking_delta when upstream omitted summary text"
    );
    assert!(
        signature_delta_found,
        "expected signature_delta from encrypted_content-only reasoning"
    );
    assert!(
        thinking_stop_found,
        "expected signature-only thinking block to stop"
    );
    assert_eq!(
        text_start_index, 1,
        "text block index should be 1 after signature-only thinking block"
    );
    assert_eq!(
        events.join(","),
        "thinking_start,signature_delta,thinking_stop,text_start",
        "signature-only event order"
    );
}

#[test]
fn non_stream_thinking_includes_signature() {
    let out = convert_non_stream(
        EMPTY_REQUEST,
        r#"{
            "type":"response.completed",
            "response":{
                "id":"resp_123",
                "model":"gpt-5",
                "usage":{"input_tokens":10,"output_tokens":20},
                "output":[
                    {
                        "type":"reasoning",
                        "encrypted_content":"enc_sig_nonstream",
                        "summary":[{"type":"summary_text","text":"internal reasoning"}]
                    },
                    {
                        "type":"message",
                        "content":[{"type":"output_text","text":"final answer"}]
                    }
                ]
            }
        }"#,
    );

    let thinking = &out["content"][0];
    assert_eq!(
        text_at(thinking, "type"),
        "thinking",
        "expected first content block to be thinking: {thinking}"
    );
    assert_eq!(text_at(thinking, "signature"), "enc_sig_nonstream");
    assert_eq!(text_at(thinking, "thinking"), "internal reasoning");
}

#[test]
fn stream_text_before_tool_calls_does_not_emit_ghost_stop() {
    let output = run_stream(
        r#"{"tools":[{"name":"Read","description":"read"}]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"grok-composer-2.5-fast"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"message","status":"in_progress"},"output_index":1}"#,
            r#"data: {"type":"response.content_part.added","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
            r#"data: {"type":"response.output_text.delta","delta":"查看项目的 README 和核心入口，以便准确说明项目用途。\n","output_index":1}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read","status":"in_progress"},"output_index":2}"#,
            r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"path\":\"/tmp/README.md\"}","output_index":2}"#,
            r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp/README.md\"}","output_index":2}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"path\":\"/tmp/README.md\"}"},"output_index":2}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read","status":"in_progress"},"output_index":3}"#,
            r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"path\":\"/tmp/main.go\"}","output_index":3}"#,
            r#"data: {"type":"response.content_part.done","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"message","status":"completed"},"output_index":1}"#,
            r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp/main.go\"}","output_index":3}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{\"path\":\"/tmp/main.go\"}"},"output_index":3}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ],
    );

    let mut start_indices = Vec::new();
    let mut stop_indices = Vec::new();
    for data in payloads(&output) {
        match &*text_at(&data, "type") {
            "content_block_start" => start_indices.push(int_at(&data, "index")),
            "content_block_stop" => stop_indices.push(int_at(&data, "index")),
            _ => {}
        }
    }

    assert_eq!(
        start_indices,
        [0, 1, 2],
        "expected 3 content_block_start events (text + 2 tools)"
    );
    assert_eq!(
        stop_indices,
        [0, 1, 2],
        "expected 3 content_block_stop events"
    );
}

#[test]
fn stream_function_call_defers_start_until_done_name() {
    let mut stream = CodexToClaudeStream::new(&request(
        r#"{"tools":[{"name":"web_search","description":"search"}]}"#,
    ));

    stream.translate_line(
        br#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
    );
    let added = stream.translate_line(
        br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1"},"output_index":1}"#,
    );
    let arguments = stream.translate_line(
        br#"data: {"type":"response.function_call_arguments.done","arguments":"{\"query\":\"example\"}","output_index":1}"#,
    );
    let done = stream.translate_line(
        br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"web_search","arguments":"{\"query\":\"example\"}"},"output_index":1}"#,
    );

    assert!(
        !added.contains(r#""content_block_start""#),
        "function_call without name must not emit content_block_start: {added:?}"
    );
    assert!(
        !arguments.contains(r#""input_json_delta""#),
        "arguments must be buffered until the tool name is available: {arguments:?}"
    );

    let mut tool_start_count = 0;
    let mut tool_stop_count = 0;
    let mut argument_deltas = Vec::new();
    for data in payloads(&done) {
        match &*text_at(&data, "type") {
            "content_block_start" => {
                if text_at(&data, "content_block.type") != "tool_use" {
                    continue;
                }
                tool_start_count += 1;
                assert_eq!(
                    text_at(&data, "content_block.name"),
                    "web_search",
                    "unexpected tool_use name in {data}"
                );
            }
            "content_block_delta" => {
                if text_at(&data, "delta.type") == "input_json_delta" {
                    argument_deltas.push(text_at(&data, "delta.partial_json"));
                }
            }
            "content_block_stop" => tool_stop_count += 1,
            _ => {}
        }
    }

    assert_eq!(
        tool_start_count, 1,
        "expected one deferred tool_use start in {done:?}"
    );
    assert_eq!(
        argument_deltas,
        [r#"{"query":"example"}"#],
        "unexpected buffered argument deltas"
    );
    assert_eq!(
        tool_stop_count, 1,
        "expected one deferred tool_use stop in {done:?}"
    );
}

#[test]
fn stream_unnamed_function_call_done_by_call_id_keeps_pending_slots() {
    let output = run_stream(
        r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_first"},"output_index":1}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_second"},"output_index":2}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_first","name":"lookup","arguments":"{\"id\":1}"}}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_second","name":"lookup","arguments":"{\"id\":2}"}}"#,
        ],
    );

    let mut tool_ids = Vec::new();
    let mut start_indices = Vec::new();
    let mut stop_indices = Vec::new();
    let mut argument_deltas = Vec::new();
    for data in payloads(&output) {
        match &*text_at(&data, "type") {
            "content_block_start" => {
                if text_at(&data, "content_block.type") == "tool_use" {
                    tool_ids.push(text_at(&data, "content_block.id"));
                    start_indices.push(int_at(&data, "index"));
                }
            }
            "content_block_delta" => {
                if text_at(&data, "delta.type") == "input_json_delta" {
                    argument_deltas.push(text_at(&data, "delta.partial_json"));
                }
            }
            "content_block_stop" => stop_indices.push(int_at(&data, "index")),
            _ => {}
        }
    }

    assert_eq!(
        tool_ids,
        ["call_first", "call_second"],
        "unexpected tool IDs; output={output:?}"
    );
    assert_eq!(
        start_indices,
        [0, 1],
        "unexpected start indices; output={output:?}"
    );
    assert_eq!(
        stop_indices,
        [0, 1],
        "unexpected stop indices; output={output:?}"
    );
    assert_eq!(
        argument_deltas,
        [r#"{"id":1}"#, r#"{"id":2}"#],
        "unexpected argument deltas; output={output:?}"
    );
}

#[test]
fn stream_deferred_unnamed_function_call_does_not_reserve_block_index() {
    let output = run_stream(
        r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_hidden"},"output_index":1}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":2}"#,
        ],
    );

    let text_start = payloads(&output).into_iter().find(|data| {
        text_at(data, "type") == "content_block_start"
            && text_at(data, "content_block.type") == "text"
    });
    let text_start =
        text_start.unwrap_or_else(|| panic!("missing text content_block_start; output={output:?}"));
    assert_eq!(
        int_at(&text_start, "index"),
        0,
        "text block index; output={output:?}"
    );
}

#[test]
fn stream_terminal_output_hydrates_open_function_call_arguments() {
    let output = run_stream(
        r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"lookup"},"output_index":1}"#,
            r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"example\"}"}]}}"#,
        ],
    );

    let mut final_argument_position = None;
    let mut stop_position = None;
    let mut message_delta_position = None;
    for (position, data) in payloads(&output).iter().enumerate() {
        match &*text_at(data, "type") {
            "content_block_delta" => {
                if text_at(data, "delta.type") == "input_json_delta"
                    && text_at(data, "delta.partial_json") == r#"{"query":"example"}"#
                {
                    final_argument_position = Some(position);
                }
            }
            "content_block_stop" => {
                if int_at(data, "index") == 0 {
                    stop_position = Some(position);
                }
            }
            "message_delta" => message_delta_position = Some(position),
            _ => {}
        }
    }

    let arguments = final_argument_position
        .unwrap_or_else(|| panic!("missing terminal argument delta; output={output:?}"));
    let stop = stop_position.unwrap_or_else(|| {
        panic!("missing content_block_stop for open function call; output={output:?}")
    });
    let message_delta = message_delta_position
        .unwrap_or_else(|| panic!("missing message_delta; output={output:?}"));
    assert!(
        arguments < stop && stop < message_delta,
        "unexpected event order: args={arguments} stop={stop} message_delta={message_delta}; output={output:?}"
    );
}

#[test]
fn stream_terminal_output_emits_pending_unnamed_function_call() {
    let output = run_stream(
        r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1"},"output_index":1}"#,
            r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"query\":\"example\"}","output_index":1}"#,
            r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"example\"}"}]}}"#,
        ],
    );

    assert_eq!(
        output.matches(r#""type":"tool_use""#).count(),
        1,
        "expected one terminal tool_use block, got output:\n{output}"
    );
    assert!(
        output.contains(r#""name":"lookup""#)
            && output.contains(r#""partial_json":"{\"query\":\"example\"}""#),
        "expected terminal tool name and arguments, got output:\n{output}"
    );
    let reason = find_stop_reason(&output)
        .unwrap_or_else(|| panic!("missing message_delta; output={output:?}"));
    assert_eq!(reason, "tool_use", "stop_reason; output={output:?}");
    let tool_use_position = output.find(r#""type":"tool_use""#);
    let message_delta_position = output.find(r#""type":"message_delta""#);
    assert!(
        matches!((tool_use_position, message_delta_position), (Some(tool_use), Some(message_delta)) if tool_use < message_delta),
        "terminal tool_use must be emitted before message_delta:\n{output}"
    );
}

#[test]
fn stream_unresolved_pending_function_call_does_not_force_tool_use_stop_reason() {
    let mut stream = CodexToClaudeStream::new(&request(
        r#"{"tools":[{"name":"lookup","description":"lookup"}]}"#,
    ));
    let output: String = [
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
        r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_hidden"},"output_index":1}"#,
        r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[]}}"#,
    ]
    .iter()
    .map(|line| stream.translate_line(line.as_bytes()))
    .collect();

    assert!(
        !output.contains(r#""type":"tool_use""#),
        "unresolved pending function_call must not emit tool_use:\n{output}"
    );
    let reason = find_stop_reason(&output)
        .unwrap_or_else(|| panic!("missing message_delta; output={output:?}"));
    assert_eq!(reason, "end_turn", "stop_reason; output={output:?}");
    assert!(
        stream.calls.is_empty()
            && stream.call_keys.is_empty()
            && stream.queue.is_empty()
            && stream.last_call.is_none(),
        "pending function calls were not cleared"
    );
}

#[test]
fn stream_empty_output_uses_output_item_done_message_fallback() {
    let output = run_stream(
        r#"{"tools":[]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},"output_index":0}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ],
    );

    let found_text = payloads(&output).iter().any(|data| {
        text_at(data, "type") == "content_block_delta"
            && text_at(data, "delta.type") == "text_delta"
            && text_at(data, "delta.text") == "ok"
    });
    assert!(
        found_text,
        "expected fallback content from response.output_item.done message; output={output:?}"
    );
}

#[test]
fn stream_web_search_call_emits_claude_server_tool_blocks() {
    let output = run_stream(
        r#"{
            "tools":[{"type":"web_search_20250305","name":"web_search"}],
            "messages":[{"role":"user","content":"search weather"}]
        }"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.4"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"id":"ws_123","type":"web_search_call","status":"in_progress"}}"#,
            r#"data: {"type":"response.web_search_call.searching","item_id":"ws_123"}"#,
            r#"data: {"type":"response.web_search_call.completed","item_id":"ws_123"}"#,
            r#"data: {"type":"response.output_item.done","item":{"id":"ws_123","type":"web_search_call","status":"completed","action":{"type":"search","query":"search weather"}}}"#,
            r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2}}}"#,
        ],
    );

    for needle in [
        r#""type":"server_tool_use""#,
        r#""id":"ws_123""#,
        r#""type":"web_search_tool_result""#,
        "event: message_stop",
    ] {
        assert!(
            output.contains(needle),
            "stream output missing {needle}:\n{output}"
        );
    }
    let server_tool = output.find(r#""type":"server_tool_use""#);
    let result = output.find(r#""type":"web_search_tool_result""#);
    assert!(
        matches!((server_tool, result), (Some(server_tool), Some(result)) if result >= server_tool),
        "web_search_tool_result must follow server_tool_use:\n{output}"
    );
    assert!(
        output.contains("partial_json") && output.contains("search weather"),
        "expected web search query delta after populated output_item.done:\n{output}"
    );
}

#[test]
fn stream_web_search_call_reuses_fallback_tool_use_id() {
    let output = run_stream(
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"search weather"}]}"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.4"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"web_search_call","status":"in_progress"}}"#,
            r#"data: {"type":"response.web_search_call.completed","item_id":"ws_from_upstream"}"#,
            r#"data: {"type":"response.output_item.done","item":{"id":"ws_from_upstream","type":"web_search_call","status":"completed","action":{"type":"search","query":"search weather"}}}"#,
            r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2}}}"#,
        ],
    );

    assert_eq!(
        output.matches(r#""type":"server_tool_use""#).count(),
        1,
        "expected exactly one server_tool_use block, got output:\n{output}"
    );
    assert!(
        output.contains(r#""tool_use_id":"ws_from_upstream""#),
        "expected web_search_tool_result to reuse fallback tool_use_id:\n{output}"
    );
}

/// Checks the `web_search_tool_result` made from an `action.sources` list:
/// blank URLs are left out, and a missing title is the URL.
fn assert_action_sources_content(content: &Value) {
    let content = content.as_array().expect("content is an array");
    assert_eq!(
        content.len(),
        2,
        "web_search_tool_result.content: {content:?}"
    );
    assert_eq!(
        text_at(&content[0], "url"),
        "https://docs.x.ai/developers/tools/web-search"
    );
    assert_eq!(text_at(&content[0], "title"), "xAI Docs");
    assert_eq!(text_at(&content[1], "url"), "https://example.com/notitle");
    assert_eq!(
        text_at(&content[1], "title"),
        "https://example.com/notitle",
        "fallback to url"
    );
}

// TestConvertCodexResponseToClaude_StreamWebSearchCallActionSources
#[test]
fn stream_web_search_call_action_sources() {
    let output = run_stream(
        r#"{
            "tools":[{"type":"web_search_20250305","name":"web_search"}],
            "messages":[{"role":"user","content":"search xai docs"}]
        }"#,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"grok-4"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"id":"ws_123","type":"web_search_call","status":"in_progress"}}"#,
            r#"data: {"type":"response.output_item.done","item":{"id":"ws_123","type":"web_search_call","status":"completed","action":{"type":"search","query":"xAI web search docs","sources":[{"type":"url","url":"https://docs.x.ai/developers/tools/web-search","title":"xAI Docs"},{"type":"url","url":"https://example.com/notitle"},{"type":"url","url":""},{"type":"url","url":"   "}]}}}"#,
            r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":5,"output_tokens":10}}}"#,
        ],
    );
    let block = payloads(&output)
        .into_iter()
        .rfind(|data| {
            text_at(data, "type") == "content_block_start"
                && text_at(data, "content_block.type") == "web_search_tool_result"
        })
        .expect("web_search_tool_result block in stream output");
    assert_action_sources_content(&block["content_block"]["content"]);
}

#[test]
fn shortens_long_tool_use_ids() {
    let long_call_id = format!("call_{}", "a".repeat(62));
    assert!(
        long_call_id.len() > 64,
        "test setup error: long_call_id length = {}, want > 64",
        long_call_id.len()
    );
    let original_request =
        r#"{"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{}}}]}"#;

    // stream
    let line = format!(
        r#"data: {{"type":"response.output_item.added","item":{{"type":"function_call","call_id":"{long_call_id}","name":"lookup"}}}}"#
    );
    let output = run_stream(original_request, &[&line]);
    let tool_id = payloads(&output)
        .iter()
        .filter(|data| {
            text_at(data, "type") == "content_block_start"
                && text_at(data, "content_block.type") == "tool_use"
        })
        .map(|data| text_at(data, "content_block.id"))
        .next_back()
        .unwrap_or_default();
    assert!(
        !tool_id.is_empty(),
        "missing stream tool_use block. Output={output:?}"
    );
    assert!(
        tool_id.len() <= 64,
        "stream tool_use id length = {}, want <= 64: {tool_id:?}",
        tool_id.len()
    );
    assert_ne!(
        tool_id, long_call_id,
        "stream tool_use id was not shortened"
    );

    // nonstream
    let response = format!(
        r#"{{
            "type":"response.completed",
            "response":{{
                "id":"resp_1",
                "model":"gpt-5",
                "usage":{{"input_tokens":1,"output_tokens":1}},
                "output":[{{"type":"function_call","call_id":"{long_call_id}","name":"lookup","arguments":"{{}}"}}]
            }}
        }}"#
    );
    let out = convert_non_stream(original_request, &response);
    let tool_id = text_at(&out, "content.0.id");
    assert!(
        !tool_id.is_empty(),
        "missing nonstream tool_use id. Output: {out}"
    );
    assert!(
        tool_id.len() <= 64,
        "nonstream tool_use id length = {}, want <= 64: {tool_id:?}",
        tool_id.len()
    );
    assert_ne!(
        tool_id, long_call_id,
        "nonstream tool_use id was not shortened"
    );
}

#[test]
fn stream_stop_reason_mapping() {
    let cases: [(&str, &[&str], &str); 4] = [
        (
            "Stop maps to end_turn",
            &[
                r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
            "end_turn",
        ),
        (
            "Incomplete max output maps to max_tokens",
            &[
                r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
            "max_tokens",
        ),
        (
            "Tool call wins over stop",
            &[
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"lookup"}}"#,
                r#"data: {"type":"response.completed","response":{"stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
            "tool_use",
        ),
        (
            "Content filter maps to Claude refusal",
            &[
                r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
            "refusal",
        ),
    ];

    for (name, lines, want_reason) in cases {
        let output = run_stream(
            r#"{"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{}}}]}"#,
            lines,
        );
        let reason = find_stop_reason(&output).unwrap_or_else(|| {
            panic!("{name}: did not find message_delta stop_reason; output={output:?}")
        });
        assert_eq!(reason, want_reason, "{name}: output={output:?}");
    }
}

#[test]
fn stream_stop_sequence_mapping() {
    let output = run_stream(
        EMPTY_REQUEST,
        &[
            r#"data: {"type":"response.completed","response":{"stop_reason":"stop","stop_sequence":"\nEND","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ],
    );
    let message_delta = find_message_delta(&output)
        .unwrap_or_else(|| panic!("did not find message_delta; output={output:?}"));
    assert_eq!(
        text_at(&message_delta, "delta.stop_reason"),
        "stop_sequence",
        "output={output:?}"
    );
    assert_eq!(
        text_at(&message_delta, "delta.stop_sequence"),
        "\nEND",
        "output={output:?}"
    );
}

#[test]
fn non_stream_web_search_call_emits_server_tool_blocks() {
    let out = convert_non_stream(
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"search weather"}]}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.3-codex-spark","stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2},"output":[{"type":"web_search_call","id":"ws_123","status":"completed","action":{"type":"search","query":"search weather"}},{"type":"message","content":[{"type":"output_text","text":"done"}]}]}}"#,
    );
    let raw = out.to_string();
    let types: Vec<String> = out["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| text_at(block, "type"))
        .collect();
    for want in ["server_tool_use", "web_search_tool_result", "text"] {
        let found =
            types.iter().any(|got| got == want) || raw.contains(&format!(r#""type":"{want}""#));
        assert!(found, "missing content type {want} in {raw}");
    }
    if text_at(&out, "content.0.input.query") != "search weather" {
        assert!(
            raw.contains("search weather"),
            "expected web search query in non-stream output: {raw}"
        );
    }
}

#[test]
fn non_stream_web_search_stop_reason_end_turn() {
    let out = convert_non_stream(
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"search weather"}]}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.3-codex-spark","stop_reason":"stop","usage":{"input_tokens":3,"output_tokens":2},"output":[{"type":"web_search_call","id":"ws_123","status":"completed","action":{"type":"search","query":"search weather"}},{"type":"message","content":[{"type":"output_text","text":"done"}]}]}}"#,
    );
    assert_eq!(
        text_at(&out, "stop_reason"),
        "end_turn",
        "stop_reason should be end_turn when only server web_search and text are present"
    );
}

#[test]
fn non_stream_web_search_dedupes_empty_open_page_items() {
    let out = convert_non_stream(
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"q"}]}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.3-codex-spark","stop_reason":"stop","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"open_page"}},{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"weather"}},{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}}"#,
    );
    let raw = out.to_string();
    assert_eq!(
        raw.matches(r#""type":"server_tool_use""#).count(),
        1,
        "expected one server_tool_use after dedupe, got {raw}"
    );
    assert!(
        raw.contains("weather"),
        "expected populated query item to be kept: {raw}"
    );
}

// TestConvertCodexResponseToClaudeNonStream_WebSearchCallActionSources
#[test]
fn non_stream_web_search_call_action_sources() {
    let out = convert_non_stream(
        r#"{"tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"search xai docs"}]}"#,
        r#"{
            "type":"response.completed",
            "response":{
                "id":"resp_1",
                "model":"grok-4",
                "stop_reason":"stop",
                "usage":{"input_tokens":5,"output_tokens":10},
                "output":[
                    {
                        "type":"web_search_call",
                        "id":"ws_123",
                        "status":"completed",
                        "action":{
                            "type":"search",
                            "query":"xAI web search docs",
                            "sources":[
                                {"type":"url","url":"https://docs.x.ai/developers/tools/web-search","title":"xAI Docs"},
                                {"type":"url","url":"https://example.com/notitle"},
                                {"type":"url","url":""},
                                {"type":"url","url":"   "}
                            ]
                        }
                    },
                    {
                        "type":"message",
                        "content":[{"type":"output_text","text":"here are the docs"}]
                    }
                ]
            }
        }"#,
    );
    let block = out["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|block| text_at(block, "type") == "web_search_tool_result")
        .unwrap_or_else(|| panic!("missing web_search_tool_result in non-stream content: {out}"));
    assert_action_sources_content(&block["content"]);
}

#[test]
fn non_stream_stop_reason_mapping() {
    let cases = [
        (
            "Stop maps to end_turn",
            r#"{
                "type":"response.completed",
                "response":{
                    "id":"resp_1",
                    "model":"gpt-5",
                    "stop_reason":"stop",
                    "usage":{"input_tokens":1,"output_tokens":1},
                    "output":[]
                }
            }"#,
            "end_turn",
        ),
        (
            "Incomplete max output maps to max_tokens",
            r#"{
                "type":"response.incomplete",
                "response":{
                    "id":"resp_1",
                    "model":"gpt-5",
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "usage":{"input_tokens":1,"output_tokens":1},
                    "output":[]
                }
            }"#,
            "max_tokens",
        ),
        (
            "Tool call wins over stop",
            r#"{
                "type":"response.completed",
                "response":{
                    "id":"resp_1",
                    "model":"gpt-5",
                    "stop_reason":"stop",
                    "usage":{"input_tokens":1,"output_tokens":1},
                    "output":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}]
                }
            }"#,
            "tool_use",
        ),
        (
            "Content filter maps to Claude refusal",
            r#"{
                "type":"response.incomplete",
                "response":{
                    "id":"resp_1",
                    "model":"gpt-5",
                    "incomplete_details":{"reason":"content_filter"},
                    "usage":{"input_tokens":1,"output_tokens":1},
                    "output":[]
                }
            }"#,
            "refusal",
        ),
    ];

    for (name, response, want_reason) in cases {
        let out = convert_non_stream(
            r#"{"tools":[{"name":"lookup","input_schema":{"type":"object","properties":{}}}]}"#,
            response,
        );
        assert_eq!(
            text_at(&out, "stop_reason"),
            want_reason,
            "{name}: output: {out}"
        );
    }
}

#[test]
fn non_stream_stop_sequence_mapping() {
    let out = convert_non_stream(
        EMPTY_REQUEST,
        r#"{
            "type":"response.completed",
            "response":{
                "id":"resp_1",
                "model":"gpt-5",
                "stop_reason":"stop",
                "stop_sequence":"\nEND",
                "usage":{"input_tokens":1,"output_tokens":1},
                "output":[]
            }
        }"#,
    );
    assert_eq!(
        text_at(&out, "stop_reason"),
        "stop_sequence",
        "output: {out}"
    );
    assert_eq!(text_at(&out, "stop_sequence"), "\nEND", "output: {out}");
}

/// Input, output, cache read and cache write tokens.
type TokenCounts = (i64, i64, i64, i64);

/// Checks Claude usage against wanted counts. A zero cache write count must
/// leave `cache_creation_input_tokens` out.
fn assert_cache_usage(name: &str, usage: &Value, want: TokenCounts) {
    let (input, output, cache_read, cache_write) = want;
    assert_eq!(int_at(usage, "input_tokens"), input, "{name}: input_tokens");
    assert_eq!(
        int_at(usage, "output_tokens"),
        output,
        "{name}: output_tokens"
    );
    assert_eq!(
        int_at(usage, "cache_read_input_tokens"),
        cache_read,
        "{name}: cache_read_input_tokens"
    );
    if cache_write == 0 {
        assert!(
            at(usage, "cache_creation_input_tokens").is_none(),
            "{name}: cache_creation_input_tokens should not be emitted when zero; got {usage}"
        );
    } else {
        assert_eq!(
            int_at(usage, "cache_creation_input_tokens"),
            cache_write,
            "{name}: cache_creation_input_tokens"
        );
    }
}

// (name, usage JSON, wanted counts)
const CACHE_USAGE_CASES: [(&str, &str, TokenCounts); 6] = [
    (
        "cache_write_tokens field",
        r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
        (50, 200, 800, 150),
    ),
    (
        "cache_creation_tokens field alias",
        r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_creation_tokens":150}}"#,
        (50, 200, 800, 150),
    ),
    (
        "cached_tokens greater than input_tokens clamps input_tokens to zero",
        r#"{"input_tokens":500,"output_tokens":100,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":50}}"#,
        (0, 100, 800, 50),
    ),
    (
        "zero cache_write_tokens does not emit cache_creation_input_tokens",
        r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":0}}"#,
        (200, 200, 800, 0),
    ),
    (
        "cache_write_tokens only deducts from input_tokens",
        r#"{"input_tokens":4022,"output_tokens":462,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":4019}}"#,
        (3, 462, 0, 4019),
    ),
    (
        "combined cached and cache_write greater than input_tokens clamps to zero",
        r#"{"input_tokens":500,"output_tokens":100,"input_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
        (0, 100, 300, 300),
    ),
];

#[test]
fn stream_preserves_cache_write_usage() {
    for (name, usage, want) in CACHE_USAGE_CASES {
        let terminal = format!(
            r#"data: {{"type":"response.completed","response":{{"stop_reason":"stop","usage":{usage}}}}}"#
        );
        let output = run_stream(
            EMPTY_REQUEST,
            &[
                r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#,
                &terminal,
            ],
        );
        let delta = find_message_delta(&output)
            .unwrap_or_else(|| panic!("{name}: missing message_delta event; output={output:?}"));
        assert_cache_usage(name, &delta["usage"], want);
    }
}

#[test]
fn non_stream_preserves_cache_write_usage() {
    for (name, usage, want) in CACHE_USAGE_CASES {
        let response = format!(
            r#"{{
                "type":"response.completed",
                "response":{{
                    "id":"resp_1",
                    "model":"gpt-5",
                    "stop_reason":"stop",
                    "usage":{usage},
                    "output":[{{"type":"message","content":[{{"type":"output_text","text":"ok"}}]}}]
                }}
            }}"#
        );
        let out = convert_non_stream(EMPTY_REQUEST, &response);
        assert_cache_usage(name, &out["usage"], want);
    }
}

/// Checks `output_tokens` and the optional `output_tokens_details.thinking_tokens`.
fn assert_reasoning_usage(name: &str, usage: &Value, output: i64, reasoning: Option<i64>) {
    assert_eq!(
        int_at(usage, "output_tokens"),
        output,
        "{name}: output_tokens"
    );
    let thinking = at(usage, "output_tokens_details.thinking_tokens");
    match reasoning {
        Some(want) => {
            let thinking = thinking.unwrap_or_else(|| {
                panic!(
                    "{name}: expected output_tokens_details.thinking_tokens to exist, got none in {usage}"
                )
            });
            assert_eq!(int_of(thinking), want, "{name}: thinking_tokens");
        }
        None => assert!(
            thinking.is_none(),
            "{name}: expected output_tokens_details.thinking_tokens to be absent, got {thinking:?}"
        ),
    }
}

// (name, usage JSON, output tokens, thinking tokens if present)
const REASONING_USAGE_CASES: [(&str, &str, i64, Option<i64>); 10] = [
    (
        "preserves positive reasoning tokens",
        r#"{"input_tokens":420,"output_tokens":518,"output_tokens_details":{"reasoning_tokens":163},"total_tokens":938}"#,
        518,
        Some(163),
    ),
    (
        "preserves explicit zero reasoning tokens",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":0}}"#,
        50,
        Some(0),
    ),
    (
        "omits reasoning detail when absent",
        r#"{"input_tokens":100,"output_tokens":50}"#,
        50,
        None,
    ),
    (
        "clamps oversized reasoning tokens to output tokens",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":999}}"#,
        50,
        Some(50),
    ),
    (
        "rejects negative reasoning tokens",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":-5}}"#,
        50,
        None,
    ),
    (
        "rejects negative float reasoning tokens",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":-0.5}}"#,
        50,
        None,
    ),
    (
        "clamps oversized int64 overflow reasoning tokens to output tokens",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":9223372036854775808}}"#,
        50,
        Some(50),
    ),
    (
        "omits string reasoning detail",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":"163"}}"#,
        50,
        None,
    ),
    (
        "omits boolean reasoning detail",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":true}}"#,
        50,
        None,
    ),
    (
        "omits null reasoning detail",
        r#"{"input_tokens":100,"output_tokens":50,"output_tokens_details":{"reasoning_tokens":null}}"#,
        50,
        None,
    ),
];

#[test]
fn preserves_reasoning_usage() {
    for (name, usage, output_tokens, reasoning) in REASONING_USAGE_CASES {
        let terminal = format!(
            r#"data: {{"type":"response.completed","response":{{"stop_reason":"stop","usage":{usage}}}}}"#
        );
        let output = run_stream(
            EMPTY_REQUEST,
            &[
                r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#,
                &terminal,
            ],
        );
        let delta = find_message_delta(&output)
            .unwrap_or_else(|| panic!("{name}: missing message_delta event; output={output:?}"));
        assert_reasoning_usage(name, &delta["usage"], output_tokens, reasoning);
    }
}

#[test]
fn non_stream_preserves_reasoning_usage() {
    for (name, usage, output_tokens, reasoning) in REASONING_USAGE_CASES {
        let response = format!(
            r#"{{
                "type":"response.completed",
                "response":{{
                    "id":"resp_1",
                    "model":"gpt-5",
                    "stop_reason":"stop",
                    "usage":{usage},
                    "output":[{{"type":"message","content":[{{"type":"output_text","text":"ok"}}]}}]
                }}
            }}"#
        );
        let out = convert_non_stream(EMPTY_REQUEST, &response);
        assert_reasoning_usage(name, &out["usage"], output_tokens, reasoning);
    }
}

// Upstream's extractResponsesUsage returns the four counts; the port folds it
// into claude_usage, which writes the cache counts only when they are positive.
#[test]
fn extract_responses_usage() {
    // (name, raw usage, wanted counts)
    let cases: [(&str, &str, TokenCounts); 12] = [
        ("nil / absent usage", "", (0, 0, 0, 0)),
        ("null usage", "null", (0, 0, 0, 0)),
        (
            "only input and output tokens without cache details",
            r#"{"input_tokens":100,"output_tokens":50}"#,
            (100, 50, 0, 0),
        ),
        (
            "deducts cache_read_tokens only",
            r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":30}}"#,
            (70, 50, 30, 0),
        ),
        (
            "deducts cache_write_tokens only (issue 5956)",
            r#"{"input_tokens":4022,"output_tokens":462,"input_tokens_details":{"cache_write_tokens":4019}}"#,
            (3, 462, 0, 4019),
        ),
        (
            "deducts cache_creation_tokens alias only",
            r#"{"input_tokens":4022,"output_tokens":462,"input_tokens_details":{"cache_creation_tokens":4019}}"#,
            (3, 462, 0, 4019),
        ),
        (
            "deducts both cached_tokens and cache_write_tokens",
            r#"{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
            (50, 200, 800, 150),
        ),
        (
            "clamps input_tokens to zero when cache exceeds input",
            r#"{"input_tokens":500,"output_tokens":100,"input_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
            (0, 100, 300, 300),
        ),
        (
            "handles negative cache numbers safely without corrupting input",
            r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":-10,"cache_write_tokens":-5}}"#,
            (100, 50, -10, 0),
        ),
        (
            "clamps raw negative input_tokens to zero",
            r#"{"input_tokens":-10,"output_tokens":50}"#,
            (0, 50, 0, 0),
        ),
        (
            "negative cache_write_tokens falls back to cache_creation_tokens alias",
            r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cache_write_tokens":-1,"cache_creation_tokens":40}}"#,
            (60, 50, 0, 40),
        ),
        (
            "prevents int64 overflow when cached_tokens and cache_write_tokens are huge",
            r#"{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":9223372036854775800,"cache_write_tokens":100}}"#,
            (0, 50, 9_223_372_036_854_775_800, 100),
        ),
    ];

    for (name, raw, (input, output, cached, cache_write)) in cases {
        let parsed: Option<Value> =
            (!raw.is_empty()).then(|| serde_json::from_str(raw).expect("usage is valid JSON"));
        let usage = claude_usage(parsed.as_ref());

        assert_eq!(
            int_at(&usage, "input_tokens"),
            input,
            "{name}: input_tokens"
        );
        assert_eq!(
            int_at(&usage, "output_tokens"),
            output,
            "{name}: output_tokens"
        );
        for (key, want) in [
            ("cache_read_input_tokens", cached),
            ("cache_creation_input_tokens", cache_write),
        ] {
            if want > 0 {
                assert_eq!(usage.get(key), Some(&json!(want)), "{name}: {key}");
            } else {
                assert!(usage.get(key).is_none(), "{name}: {key} in {usage}");
            }
        }
    }
}

// Parallel function calls.

const PARALLEL_REQUEST: &str = r#"{"stream":true,"tools":[{"name":"Read"}]}"#;

#[derive(Debug, Default)]
struct ContentBlock {
    index: i64,
    kind: String,
    id: String,
    name: String,
    text: String,
    arguments: String,
}

/// Checks that blocks open one at a time, never reuse an index and all close
/// before `message_delta`, then returns the blocks in order.
fn assert_content_block_lifecycle(output: &str) -> Vec<ContentBlock> {
    let mut open: HashMap<i64, usize> = HashMap::new();
    let mut started = HashSet::new();
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut message_state = 0;
    for event in payloads(output) {
        assert_ne!(
            message_state, 2,
            "event emitted after message_stop: {event}"
        );
        let index = int_at(&event, "index");
        match &*text_at(&event, "type") {
            "content_block_start" => {
                assert_eq!(
                    message_state, 0,
                    "content block started after message terminal events: {event}"
                );
                assert!(
                    open.is_empty(),
                    "content block start emitted while another block remains open: {open:?}"
                );
                assert!(
                    started.insert(index),
                    "content block index {index} was reused"
                );
                open.insert(index, blocks.len());
                blocks.push(ContentBlock {
                    index,
                    kind: text_at(&event, "content_block.type"),
                    id: text_at(&event, "content_block.id"),
                    name: text_at(&event, "content_block.name"),
                    ..ContentBlock::default()
                });
            }
            "content_block_delta" => {
                let position = *open.get(&index).unwrap_or_else(|| {
                    panic!("content block delta targets unopened index {index}")
                });
                let block = &mut blocks[position];
                match &*text_at(&event, "delta.type") {
                    "input_json_delta" => block
                        .arguments
                        .push_str(&text_at(&event, "delta.partial_json")),
                    "text_delta" => block.text.push_str(&text_at(&event, "delta.text")),
                    _ => {}
                }
            }
            "content_block_stop" => {
                assert!(
                    open.remove(&index).is_some(),
                    "content block stop targets unopened index {index}"
                );
            }
            "message_delta" => {
                assert!(
                    open.is_empty(),
                    "message_delta emitted while content blocks remain open: {open:?}"
                );
                assert_eq!(
                    message_state, 0,
                    "duplicate or out-of-order message_delta: {event}"
                );
                message_state = 1;
            }
            "message_stop" => {
                assert!(
                    open.is_empty(),
                    "message_stop emitted while content blocks remain open: {open:?}"
                );
                assert_eq!(
                    message_state, 1,
                    "message_stop emitted before message_delta: {event}"
                );
                message_state = 2;
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "content blocks remain open: {open:?}");
    blocks
}

fn assert_parallel_tool_calls(case: &str, blocks: &[ContentBlock]) {
    assert_eq!(blocks.len(), 2, "{case}: content block count");
    let expected_ids = ["call_a", "call_b"];
    let expected_arguments = [r#"{"file_path":"a"}"#, r#"{"file_path":"b"}"#];
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(block.index, index as i64, "{case}: block {index} index");
        assert!(
            block.kind == "tool_use" && block.name == "Read",
            "{case}: block {index} = {block:?}, want Read tool_use"
        );
        assert_eq!(block.id, expected_ids[index], "{case}: block {index} ID");
        assert_eq!(
            block.arguments, expected_arguments[index],
            "{case}: block {index} arguments"
        );
    }
}

#[test]
fn stream_serializes_interleaved_named_function_calls() {
    let cases: [(&str, &[&str]); 2] = [
        (
            "first call finishes first",
            &[
                r#"data: {"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":1}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read"},"output_index":2}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"a\"}","output_index":1}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"b\"}","output_index":2}"#,
                r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":1}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":1}"#,
                r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"b\"}","output_index":2}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{\"file_path\":\"b\"}"},"output_index":2}"#,
            ],
        ),
        (
            "second call finishes first",
            &[
                r#"data: {"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":1}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read"},"output_index":2}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"b\"}","output_index":2}"#,
                r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"b\"}","output_index":2}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{\"file_path\":\"b\"}"},"output_index":2}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"file_path\":\"a\"}","output_index":1}"#,
                r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":1}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":1}"#,
            ],
        ),
    ];

    for (name, lines) in cases {
        let output = run_stream(PARALLEL_REQUEST, lines);
        let blocks = assert_content_block_lifecycle(&output);
        assert_parallel_tool_calls(name, &blocks);
    }
}

#[test]
fn stream_defers_other_content_until_function_calls_close() {
    // (name, function call line, first block type, second block type)
    let cases = [
        (
            "named active call",
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":0}"#,
            "tool_use",
            "text",
        ),
        (
            "unnamed pending call",
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a"},"output_index":0}"#,
            "text",
            "tool_use",
        ),
    ];

    for (name, function_call, first_block, second_block) in cases {
        let output = run_stream(
            PARALLEL_REQUEST,
            &[
                r#"data: {"type":"response.created","response":{"id":"resp_mixed","model":"gpt-5"}}"#,
                function_call,
                r#"data: {"type":"response.output_item.added","item":{"type":"message","status":"in_progress"},"output_index":1}"#,
                r#"data: {"type":"response.content_part.added","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                r#"data: {"type":"response.output_text.delta","delta":"done","output_index":1}"#,
                r#"data: {"type":"response.content_part.done","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"message","status":"completed"},"output_index":1}"#,
                r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":0}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":0}"#,
                r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );

        let blocks = assert_content_block_lifecycle(&output);
        assert_eq!(blocks.len(), 2, "{name}: content block count");
        assert!(
            blocks[0].index == 0 && blocks[0].kind == first_block,
            "{name}: unexpected first block: {:?}",
            blocks[0]
        );
        assert!(
            blocks[1].index == 1 && blocks[1].kind == second_block,
            "{name}: unexpected second block: {:?}",
            blocks[1]
        );
        for block in &blocks {
            match block.kind.as_str() {
                "tool_use" => assert_eq!(
                    block.arguments, r#"{"file_path":"a"}"#,
                    "{name}: unexpected tool block: {block:?}"
                ),
                "text" => assert_eq!(
                    block.text, "done",
                    "{name}: unexpected text block: {block:?}"
                ),
                _ => {}
            }
        }
    }
}

#[test]
fn stream_deferred_text_closes_before_thinking_starts() {
    let output = run_stream(
        PARALLEL_REQUEST,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_mixed","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":0}"#,
            r#"data: {"type":"response.content_part.added","part":{"type":"output_text"},"content_index":0,"output_index":1}"#,
            r#"data: {"type":"response.output_text.delta","delta":"answer","output_index":1}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"enc_initial"},"output_index":2}"#,
            r#"data: {"type":"response.reasoning_summary_part.added","output_index":2}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thought","output_index":2}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc_final"},"output_index":2}"#,
            r#"data: {"type":"response.function_call_arguments.done","arguments":"{\"file_path\":\"a\"}","output_index":0}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{\"file_path\":\"a\"}"},"output_index":0}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ],
    );

    let blocks = assert_content_block_lifecycle(&output);
    assert_eq!(blocks.len(), 3, "content block count");
    assert!(
        blocks[0].index == 0
            && blocks[0].kind == "tool_use"
            && blocks[0].arguments == r#"{"file_path":"a"}"#,
        "unexpected tool block: {:?}",
        blocks[0]
    );
    assert!(
        blocks[1].index == 1 && blocks[1].kind == "text" && blocks[1].text == "answer",
        "unexpected text block: {:?}",
        blocks[1]
    );
    assert!(
        blocks[2].index == 2 && blocks[2].kind == "thinking",
        "unexpected thinking block: {:?}",
        blocks[2]
    );
}

#[test]
fn stream_terminal_matches_function_calls_by_output_index() {
    let output = run_stream(
        PARALLEL_REQUEST,
        &[
            r#"data: {"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","name":"Read"},"output_index":0}"#,
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","name":"Read"},"output_index":1}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"function_call","name":"Read","arguments":"{\"file_path\":\"a\"}"},{"type":"function_call","name":"Read","arguments":"{\"file_path\":\"b\"}"}]}}"#,
        ],
    );

    let blocks = assert_content_block_lifecycle(&output);
    assert_eq!(blocks.len(), 2, "content block count");
    assert!(
        blocks[0].index == 0 && blocks[0].arguments == r#"{"file_path":"a"}"#,
        "unexpected first function call: {:?}",
        blocks[0]
    );
    assert!(
        blocks[1].index == 1 && blocks[1].arguments == r#"{"file_path":"b"}"#,
        "unexpected second function call: {:?}",
        blocks[1]
    );
}

#[test]
fn stream_terminal_hydrates_interleaved_function_calls() {
    for terminal_type in ["response.completed", "response.incomplete"] {
        let terminal = format!(
            r#"data: {{"type":"{terminal_type}","response":{{"usage":{{"input_tokens":1,"output_tokens":1}},"output":[{{"type":"function_call","call_id":"call_a","name":"Read","arguments":"{{\"file_path\":\"a\"}}"}},{{"type":"function_call","call_id":"call_b","name":"Read","arguments":"{{\"file_path\":\"b\"}}"}}]}}}}"#
        );
        let output = run_stream(
            PARALLEL_REQUEST,
            &[
                r#"data: {"type":"response.created","response":{"id":"resp_parallel","model":"gpt-5"}}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"Read"},"output_index":0}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_b","name":"Read"},"output_index":1}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"file_path\":","output_index":0}"#,
                &terminal,
            ],
        );

        let blocks = assert_content_block_lifecycle(&output);
        assert_parallel_tool_calls(terminal_type, &blocks);
    }
}

// Not upstream's: upstream sets the arguments' text as the tool input
// (`SetRawBytes`), so each number keeps its spelling.
#[test]
fn non_stream_tool_input_keeps_number_text() {
    let response = r#"{"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"c","name":"f","arguments":"{\"n\":1e400,\"z\":-0,\"e\":1E20}"}]}}"#;
    let out = convert_non_stream(EMPTY_REQUEST, response);
    assert_eq!(
        out["content"][0]["input"].to_string(),
        r#"{"n":1e400,"z":-0,"e":1E20}"#
    );
}
