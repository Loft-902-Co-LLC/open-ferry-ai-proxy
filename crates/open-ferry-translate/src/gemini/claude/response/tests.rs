// Ported from CLIProxyAPI internal/translator/gemini/claude/gemini_claude_response_test.go
// (v8.0.38, MIT), and its v8.0.38 tests (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests without an upstream name are new; their
// expected output comes from upstream, with tool call numbers masked: the
// counter is shared by every response in the process.

use serde_json::{Value, json};

use super::*;

// Not upstream's: the partial JSON must keep Gemini's number spelling.
#[test]
fn tool_arguments_keep_number_text() {
    let body = br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{"n":1E+2,"nested":[1e2,-0]}}}]}}]}"#;
    let mut translator = GeminiToClaudeStream::new(&json!({}));
    let out = translator.translate(body);
    let delta = out
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["delta"]["type"] == "input_json_delta")
        .unwrap();
    assert_eq!(
        delta["delta"]["partial_json"],
        r#"{"n":1E+2,"nested":[1e2,-0]}"#
    );
    let response = crate::json::exact::from_slice(body).unwrap();
    let out = convert_gemini_response_to_claude_non_stream(&json!({}), &response);
    assert_eq!(
        out["content"][0]["input"].to_string(),
        r#"{"n":1E+2,"nested":[1e2,-0]}"#
    );
}

/// Feeds `chunks` through one stream and returns what each gave, with the
/// number at the end of every `tool_use` ID replaced by `N`.
fn stream(request: &Value, chunks: &[&str]) -> Vec<String> {
    let mut translator = GeminiToClaudeStream::new(request);
    chunks
        .iter()
        .map(|chunk| mask_tool_ids(&translator.translate(chunk.as_bytes())))
        .collect()
}

fn mask_tool_ids(text: &str) -> String {
    const KEY: &str = r#""id":""#;
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(KEY) {
        let (head, tail) = rest.split_at(start + KEY.len());
        out.push_str(head);
        let end = tail.find('"').unwrap();
        let id = &tail[..end];
        match id.rsplit_once('-') {
            Some((name, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                out.push_str(name);
                out.push_str("-N");
            }
            _ => out.push_str(id),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// One SSE event as upstream writes it.
fn event(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n\n")
}

fn events(list: &[(&str, &str)]) -> String {
    list.iter().map(|(name, data)| event(name, data)).collect()
}

fn message_start(id: &str, model: &str) -> String {
    event(
        "message_start",
        &format!(
            concat!(
                r#"{{"type":"message_start","message":{{"id":"{}","type":"message","role":"assistant","#,
                r#""content":[],"model":"{}","stop_reason":null,"stop_sequence":null,"#,
                r#""usage":{{"input_tokens":0,"output_tokens":0}}}}}}"#
            ),
            id, model
        ),
    )
}

fn non_stream(request: &Value, body: &str) -> Value {
    let response = serde_json::from_str(body).unwrap_or(Value::Null);
    convert_gemini_response_to_claude_non_stream(request, &response)
}

#[test]
fn signature_only_part_does_not_open_empty_text_block() {
    let request = json!({"model": "gemini-test", "messages": [
        {"role": "user", "content": [{"type": "text", "text": "hi"}]}
    ]});
    let thinking = r#"{
        "candidates": [{"content": {"parts": [{"text": "thinking text", "thought": true}]}}],
        "modelVersion": "gemini-test",
        "responseId": "resp-test"
    }"#;
    let signature = r#"{
        "candidates": [{
            "content": {"parts": [{"text": "", "thoughtSignature": "sig-test"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "thoughtsTokenCount": 2, "totalTokenCount": 12},
        "modelVersion": "gemini-test",
        "responseId": "resp-test"
    }"#;
    let output = stream(&request, &[thinking, signature, "[DONE]"]).concat();

    assert!(
        !output.contains(r#""content_block":{"type":"text""#),
        "{output}"
    );
    assert!(
        !output.contains(r#""type":"content_block_stop","index":1"#),
        "{output}"
    );
    assert!(output.contains(r#""type":"signature_delta""#), "{output}");
    assert!(output.contains(r#""signature":"sig-test""#), "{output}");
    assert_eq!(
        output
            .matches(r#""type":"content_block_stop","index":0"#)
            .count(),
        1,
        "{output}"
    );
    assert!(output.contains(r#""type":"message_delta""#), "{output}");
    assert!(output.contains(r#""output_tokens":2"#), "{output}");
    assert!(output.contains(r#""type":"message_stop""#), "{output}");
}

#[test]
fn non_stream_preserves_thought_signature() {
    let request =
        json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
    let output = non_stream(
        &request,
        r#"{
            "candidates": [{
                "content": {"parts": [
                    {"text": "thinking step 1\n", "thought": true},
                    {"text": "thinking step 2", "thought": true, "thoughtSignature": "sig-xyz-123"},
                    {"text": "visible answer"}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
            "modelVersion": "gemini-2.5-pro",
            "responseId": "resp-non-stream"
        }"#,
    );
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "thinking step 1\nthinking step 2", "signature": "sig-xyz-123"},
            {"type": "text", "text": "visible answer"}
        ])
    );
}

#[test]
fn non_stream_part_with_thought_signature_without_thought_bool() {
    let request =
        json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
    let output = non_stream(
        &request,
        r#"{
            "candidates": [{
                "content": {"parts": [
                    {"text": "inferred reasoning", "thought_signature": "sig-snake-case"},
                    {"text": "final answer"}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
            "modelVersion": "gemini-2.5-pro",
            "responseId": "resp-non-stream-2"
        }"#,
    );
    // v8.0.20: a signature on visible text goes in a carrier.
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "", "signature": "sig-snake-case"},
            {"type": "text", "text": "inferred reasoningfinal answer"}
        ])
    );
}

#[test]
fn non_stream_trailing_signature_only_part() {
    let request =
        json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
    let output = non_stream(
        &request,
        r#"{
            "candidates": [{
                "content": {"parts": [
                    {"text": "thinking step 1\n", "thought": true},
                    {"text": "", "thoughtSignature": "sig-trailing"},
                    {"text": "visible answer"}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
            "modelVersion": "gemini-2.5-pro",
            "responseId": "resp-non-stream-trailing"
        }"#,
    );
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "thinking step 1\n", "signature": "sig-trailing"},
            {"type": "text", "text": "visible answer"}
        ])
    );
}

#[test]
fn usage_with_cached_content_token_count() {
    let request =
        json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
    let chunk = r#"{
        "candidates": [{"content": {"parts": [{"text": "Hello world"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 7, "cachedContentTokenCount": 91},
        "modelVersion": "gemini-2.5-pro",
        "responseId": "resp-usage-cache"
    }"#;
    let output = stream(&request, &[chunk]).concat();
    let delta = output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find(|data| data.contains(r#""type":"message_delta""#))
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .unwrap();
    assert_eq!(
        delta["usage"],
        json!({"input_tokens": 9, "output_tokens": 7, "cache_read_input_tokens": 91})
    );
}

#[test]
fn non_stream_usage_with_cached_content_token_count() {
    let request =
        json!({"model": "gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]});
    let output = non_stream(
        &request,
        r#"{
            "candidates": [{"content": {"parts": [{"text": "Hello world"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 7, "cachedContentTokenCount": 91},
            "modelVersion": "gemini-2.5-pro",
            "responseId": "resp-usage-cache-nonstream"
        }"#,
    );
    assert_eq!(
        output["usage"],
        json!({"input_tokens": 9, "output_tokens": 7, "cache_read_input_tokens": 91})
    );
}

#[test]
fn token_count() {
    assert_eq!(claude_token_count(42), json!({"input_tokens": 42}));
}

#[test]
fn streams_text_thinking_and_tool_calls() {
    let request = json!({"tools": [{"name": "a b"}, {"name": "Read"}]});
    let output = stream(
        &request,
        &[
            r#"{"modelVersion":null,"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"hmm","thought":true,"thoughtSignature":"s1"},{"thought_signature":"s2"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a_b","args":{"x":1}}},{"functionCall":{"args":{"y":2}}},{"functionCall":{"name":"read"}}]}}]}"#,
            r#"{"candidates":[{"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"thoughtsTokenCount":2}}"#,
            r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{}}"#,
            "[DONE]",
        ],
    );
    let expected = [
        message_start(DEFAULT_MESSAGE_ID, "")
            + &events(&[
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                (
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
                ),
            ]),
        events(&[
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"s1"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"s2"}}"#,
            ),
        ]),
        events(&[
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"a_b-N","name":"a b","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"read-N","name":"Read","input":{}}}"#,
            ),
        ]),
        events(&[
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":3}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":5,"output_tokens":5}}"#,
            ),
        ]),
        String::new(),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ];
    assert_eq!(output, expected);
}

#[test]
fn stream_without_content_closes_with_an_empty_text_block() {
    let output = stream(&json!({}), &["not json", "[DONE]"]);
    assert_eq!(
        output,
        [
            message_start(DEFAULT_MESSAGE_ID, DEFAULT_MODEL),
            events(&[
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
                (
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#,
                ),
                ("message_stop", r#"{"type":"message_stop"}"#),
            ]),
        ]
    );
    // Without a first chunk, there's no message to close.
    assert_eq!(stream(&json!({}), &["[DONE]"]), [String::new()]);
}

#[test]
fn stream_reads_sse_lines() {
    // Vertex AI's executor passes each line as it came, `data:` and all.
    let output = stream(
        &json!({}),
        &[
            r#"data: {"responseId":"r1","modelVersion":"gemini-2.5-pro","candidates":[{"index":0,"content":{"parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}"#,
            "",
            ": keep-alive",
            "[DONE]",
        ],
    );
    let expected = [
        message_start("r1", "gemini-2.5-pro")
            + &events(&[
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                (
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
                (
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2,"output_tokens":1}}"#,
                ),
            ]),
        String::new(),
        String::new(),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ];
    assert_eq!(output, expected);
}

#[test]
fn stream_edge_cases() {
    let output = stream(
        &json!({}),
        &[
            r#"{"responseId":7,"candidates":{"0":{"content":{"parts":[{"thoughtSignature":"s0"},{"text":"","thought":true},{"text":"t"}]}}}}"#,
            r#"{"usageMetadata":{"promptTokenCount":1}}"#,
            r#"{"usageMetadata":{"promptTokenCount":3,"cachedContentTokenCount":9},"x":"finishReason"}"#,
            r#"{"usageMetadata":{"promptTokenCount":3,"cachedContentTokenCount":9},"finishReason":1}"#,
            "[DONE]",
        ],
    );
    let expected = [
        message_start("7", DEFAULT_MODEL)
            + &events(&[
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                ),
                (
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s0"}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
                ),
                (
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":""}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":1}"#,
                ),
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
                ),
                (
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"t"}}"#,
                ),
            ]),
        String::new(),
        events(&[
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":2}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":9}}"#,
            ),
        ]),
        String::new(),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ];
    assert_eq!(output, expected);

    let output = stream(
        &json!({}),
        &[
            r#"{"usageMetadata":{},"finishReason":1}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"","args":"s"}},{"text":"x","thoughtSignature":"z"}]}}]}"#,
        ],
    );
    // The empty text block's stop leaves the index where it was, as
    // upstream's does, so the call after the final events reuses index 0.
    let expected = [
        message_start(DEFAULT_MESSAGE_ID, DEFAULT_MODEL)
            + &events(&[
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
                (
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#,
                ),
            ]),
        events(&[
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"-N","name":"","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"s\""}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"z"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"x"}}"#,
            ),
        ]),
    ];
    assert_eq!(output, expected);
}

#[test]
fn non_stream_edge_cases() {
    let request = json!({"tools": [{"name": "a b"}, {"name": "Read"}]});
    let output = non_stream(
        &request,
        concat!(
            r#"{"candidates":[{"content":{"parts":[{"text":"a","thought":true},{"thoughtSignature":"s"},{"text":"b"},"#,
            r#"{"functionCall":{"name":"a_b","args":"{}"}},{"functionCall":{"name":"read","args":{"k":[1]}}},"#,
            r#"{"text":"c","thought_signature":"t"}]},"finishReason":"MAX_TOKENS"}]}"#
        ),
    );
    let calls: Vec<_> = output["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .collect();
    assert_eq!(calls.len(), 2);
    // The names differ, so compare the numbers the counter gave them.
    let number = |call: &Value| -> u64 {
        let id = call["id"].as_str().unwrap();
        id.rsplit_once('-').unwrap().1.parse().unwrap()
    };
    assert!(number(calls[1]) > number(calls[0]), "{calls:?}");
    assert_eq!(
        mask_tool_ids(&output.to_string()),
        concat!(
            r#"{"id":"","type":"message","role":"assistant","model":"","content":[{"type":"thinking","thinking":"a","signature":"s"},"#,
            r#"{"type":"text","text":"b"},{"type":"tool_use","id":"a_b-N","name":"a b","input":{}},"#,
            r#"{"type":"tool_use","id":"read-N","name":"Read","input":{}},{"type":"thinking","thinking":"","signature":"t"},"#,
            r#"{"type":"text","text":"c"}],"#,
            r#""stop_reason":"tool_use","stop_sequence":null}"#
        )
    );
    assert_eq!(
        non_stream(&request, "not json").to_string(),
        r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":"end_turn","stop_sequence":null}"#
    );
    assert_eq!(
        non_stream(
            &request,
            concat!(
                r#"{"responseId":1,"modelVersion":{"a":1},"candidates":{"0":{"content":{"parts":[{"text":"x"},{"thoughtSignature":"q"}]},"#,
                r#""finishReason":"MAX_TOKENS"}},"usageMetadata":{"promptTokenCount":2,"cachedContentTokenCount":5}}"#
            )
        )
        .to_string(),
        concat!(
            r#"{"id":"1","type":"message","role":"assistant","model":"{\"a\":1}","#,
            r#""content":[{"type":"text","text":"x"},{"type":"thinking","thinking":"","signature":"q"}],"#,
            r#""stop_reason":"max_tokens","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":5}}"#
        )
    );
    assert_eq!(
        non_stream(&request, r#"{"usageMetadata":null}"#).to_string(),
        concat!(
            r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"#,
            r#""stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#
        )
    );
}

/// Panics unless each of `names` is an event in `output`, in this order.
#[track_caller]
fn require_event_order(output: &str, names: &[&str]) {
    let mut last = None;
    for name in names {
        let index = output
            .find(&format!("event: {name}\n"))
            .unwrap_or_else(|| panic!("event {name:?} not found in output:\n{output}"));
        assert!(
            last.is_none_or(|last| index > last),
            "event {name:?} is out of order in output:\n{output}"
        );
        last = Some(index);
    }
}

/// The data of the stream's `message_delta` event.
#[track_caller]
fn message_delta(output: &str) -> Value {
    let data = output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find(|data| data.contains(r#""type":"message_delta""#))
        .unwrap_or_else(|| panic!("no message_delta in output:\n{output}"));
    serde_json::from_str(data).unwrap()
}

fn hi_request() -> Value {
    json!({"model": "claude-opus-5-5", "messages": [{"role": "user", "content": "hi"}]})
}

// TestConvertGeminiResponseToClaudeStream_PartlessSafetyClosesMessageWithRefusal
#[test]
fn partless_safety_closes_message_with_refusal() {
    let chunk = r#"{
        "candidates": [{"content": {"role": "model", "parts": []}, "index": 0, "finishReason": "SAFETY"}],
        "modelVersion": "m",
        "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 37, "totalTokenCount": 157}
    }"#;
    let output = stream(&hi_request(), &[chunk, "[DONE]"]).concat();
    require_event_order(
        &output,
        &[
            "message_start",
            "content_block_start",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
    );
    let delta = message_delta(&output);
    assert_eq!(delta["delta"]["stop_reason"], "refusal", "{delta}");
    assert_eq!(delta["usage"]["input_tokens"], 120, "{delta}");
    assert_eq!(delta["usage"]["output_tokens"], 37, "{delta}");
}

// TestConvertGeminiResponseToClaudeStream_PartlessMalformedFunctionCallClosesMessage
#[test]
fn partless_malformed_function_call_closes_message() {
    let chunk = r#"{
        "candidates": [{"content": {"role": "model", "parts": []}, "index": 0, "finishReason": "MALFORMED_FUNCTION_CALL"}],
        "modelVersion": "m",
        "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 10, "totalTokenCount": 60}
    }"#;
    let output = stream(&hi_request(), &[chunk, "[DONE]"]).concat();
    assert_eq!(output.matches("event: error\n").count(), 1);
    assert!(!output.contains("event: message_delta"));
    assert!(!output.contains("event: message_stop"));
    let error: Value = output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "error")
        .unwrap();
    assert_eq!(error["error"]["type"], "api_error");
    assert!(output.contains("MALFORMED_FUNCTION_CALL"));
}

// TestConvertGeminiResponseToClaudeStream_PartlessStopClosesMessageWithEndTurn
#[test]
fn partless_stop_closes_message_with_end_turn() {
    let chunk = r#"{
        "candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "index": 0, "finishReason": "STOP"}],
        "modelVersion": "m",
        "usageMetadata": {"promptTokenCount": 80, "candidatesTokenCount": 0, "totalTokenCount": 80}
    }"#;
    let output = stream(&hi_request(), &[chunk, "[DONE]"]).concat();
    require_event_order(
        &output,
        &[
            "message_start",
            "content_block_start",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
    );
    assert_eq!(
        message_delta(&output)["delta"]["stop_reason"],
        "end_turn",
        "{output}"
    );
}

// TestConvertGeminiResponseToClaudeNonStream_SafetyAndMalformedFunctionCallRefusal
#[test]
fn non_stream_safety_and_malformed_function_call_refusal() {
    for (finish_reason, want) in [
        ("SAFETY", "refusal"),
        ("IMAGE_SAFETY", "refusal"),
        ("RECITATION", "refusal"),
        ("PROHIBITED_CONTENT", "refusal"),
        ("SPII", "refusal"),
        ("BLOCKLIST", "refusal"),
        ("MALFORMED_RESPONSE", "refusal"),
        ("MAX_TOKENS", "max_tokens"),
        ("STOP", "end_turn"),
    ] {
        let output = non_stream(
            &hi_request(),
            &format!(
                r#"{{
                    "candidates": [{{"content": {{"role": "model", "parts": []}}, "finishReason": "{finish_reason}"}}],
                    "usageMetadata": {{"promptTokenCount": 120, "candidatesTokenCount": 37, "totalTokenCount": 157}},
                    "modelVersion": "m",
                    "responseId": "resp-test"
                }}"#
            ),
        );
        assert_eq!(output["stop_reason"], want, "{finish_reason}: {output}");
    }
}

#[test]
fn filtered_finish_malformed_response_streams_refusal_without_content() {
    let output = stream(
        &hi_request(),
        &[
            r#"{"candidates":[{"finishReason":"MALFORMED_RESPONSE"}]}"#,
            r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2}}"#,
            "[DONE]",
        ],
    )
    .concat();
    assert_eq!(message_delta(&output)["delta"]["stop_reason"], "refusal");
    assert_eq!(message_delta(&output)["usage"]["output_tokens"], 2);
    assert_eq!(output.matches("event: message_delta\n").count(), 1);
    assert_eq!(output.matches("event: message_stop\n").count(), 1);
}

fn lite_request() -> Value {
    json!({"model": "gemini-3.5-flash-lite", "messages": [{"role": "user", "content": "hi"}]})
}

// TestConvertGeminiResponseToClaudeNonStream_Issue6409_SignedVisibleText
#[test]
fn non_stream_issue_6409_signed_visible_text() {
    let output = non_stream(
        &lite_request(),
        r#"{
            "candidates": [{
                "content": {"parts": [{"text": "ok", "thoughtSignature": "EmAKXgFpFH0Tb/MkBw="}], "role": "model"},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1},
            "modelVersion": "gemini-3.5-flash-lite",
            "responseId": "5Qm-as2qBuLI-sAP76KVkAk"
        }"#,
    );
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "", "signature": "EmAKXgFpFH0Tb/MkBw="},
            {"type": "text", "text": "ok"}
        ]),
        "{output}"
    );
}

// TestConvertGeminiResponseToClaudeNonStream_Issue6409_ThinkingFollowedBySignedVisibleText
#[test]
fn non_stream_issue_6409_thinking_followed_by_signed_visible_text() {
    let output = non_stream(
        &lite_request(),
        r#"{
            "candidates": [{
                "content": {"parts": [
                    {"text": "reasoning step", "thought": true, "thoughtSignature": "sig-think"},
                    {"text": "final answer", "thoughtSignature": "sig-visible"}
                ], "role": "model"},
                "finishReason": "STOP"
            }],
            "modelVersion": "gemini-3.5-flash-lite",
            "responseId": "resp-mixed"
        }"#,
    );
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "reasoning step", "signature": "sig-think"},
            {"type": "thinking", "thinking": "", "signature": "sig-visible"},
            {"type": "text", "text": "final answer"}
        ]),
        "{output}"
    );
}

// TestConvertGeminiResponseToClaudeNonStream_Issue6409_ConsecutiveSignedVisibleTexts
#[test]
fn non_stream_issue_6409_consecutive_signed_visible_texts() {
    let output = non_stream(
        &lite_request(),
        r#"{
            "candidates": [{
                "content": {"parts": [
                    {"text": "part A", "thoughtSignature": "sig-A"},
                    {"text": "part B", "thoughtSignature": "sig-B"}
                ], "role": "model"},
                "finishReason": "STOP"
            }],
            "modelVersion": "gemini-3.5-flash-lite",
            "responseId": "resp-consecutive"
        }"#,
    );
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "", "signature": "sig-A"},
            {"type": "text", "text": "part A"},
            {"type": "thinking", "thinking": "", "signature": "sig-B"},
            {"type": "text", "text": "part B"}
        ]),
        "{output}"
    );
}

#[track_caller]
fn require_contains(output: &str, wants: &[&str]) {
    for want in wants {
        assert!(output.contains(want), "expected {want} in:\n{output}");
    }
}

// TestConvertGeminiResponseToClaudeStream_Issue6409_SignedVisibleText
#[test]
fn stream_issue_6409_signed_visible_text() {
    let chunk = r#"{
        "candidates": [{
            "content": {"parts": [{"text": "ok", "thoughtSignature": "sig-stream-1"}], "role": "model"},
            "finishReason": "STOP",
            "index": 0
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1},
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-s1"
    }"#;
    let output = stream(&lite_request(), &[chunk, "[DONE]"]).concat();
    assert!(!output.contains(r#""thinking_delta""#), "{output}");
    require_contains(
        &output,
        &[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-stream-1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
        ],
    );
}

// TestConvertGeminiResponseToClaudeStream_Issue6409_SplitTrailingSignature
#[test]
fn stream_issue_6409_split_trailing_signature() {
    let text = r#"{
        "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-s2"
    }"#;
    let signature = r#"{
        "candidates": [{
            "content": {"parts": [{"text": "", "thoughtSignature": "sig-stream-trailing"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1},
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-s2"
    }"#;
    let output = stream(&lite_request(), &[text, signature, "[DONE]"]).concat();
    assert!(!output.contains(r#""thinking_delta""#), "{output}");
    require_contains(
        &output,
        &[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig-stream-trailing"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
        ],
    );
}

// TestConvertGeminiResponseToClaudeStream_Issue6409_SignedFunctionCall
#[test]
fn stream_issue_6409_signed_function_call() {
    let chunk = r#"{
        "candidates": [{
            "content": {"parts": [{"thoughtSignature": "sig-fc-stream", "functionCall": {"name": "test_tool", "args": {}}}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1},
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-fc"
    }"#;
    let output = stream(&lite_request(), &[chunk, "[DONE]"]).concat();
    require_contains(
        &output,
        &[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-fc-stream"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use""#,
            r#"{"type":"content_block_stop","index":1}"#,
        ],
    );
}

// TestConvertGeminiResponseToClaudeStream_Issue6409_FunctionCallContinuationSignature
#[test]
fn stream_issue_6409_function_call_continuation_signature() {
    let call = r#"{
        "candidates": [{"content": {"parts": [{"functionCall": {"name": "test_tool", "args": {}}}]}}],
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-fc-cont"
    }"#;
    let continuation = r#"{
        "candidates": [{
            "content": {"parts": [{"thoughtSignature": "sig-fc-cont-done", "functionCall": {"args": {"key": "val"}}}]},
            "finishReason": "STOP"
        }],
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-fc-cont"
    }"#;
    let output = stream(&lite_request(), &[call, continuation, "[DONE]"]).concat();
    require_contains(
        &output,
        &[
            r#""type":"input_json_delta""#,
            r#""signature":"sig-fc-cont-done""#,
        ],
    );
}

// TestConvertGeminiResponseToClaudeStream_Issue6409_ThinkingFollowedBySignedVisibleText
#[test]
fn stream_issue_6409_thinking_followed_by_signed_visible_text() {
    let thinking = r#"{
        "candidates": [{"content": {"parts": [{"text": "thinking step", "thought": true, "thoughtSignature": "sig-stream-think"}]}}],
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-mixed-stream"
    }"#;
    let visible = r#"{
        "candidates": [{
            "content": {"parts": [{"text": "visible text", "thoughtSignature": "sig-stream-visible"}]},
            "finishReason": "STOP"
        }],
        "modelVersion": "gemini-3.5-flash-lite",
        "responseId": "resp-mixed-stream"
    }"#;
    let output = stream(&lite_request(), &[thinking, visible, "[DONE]"]).concat();
    require_contains(
        &output,
        &[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-stream-think"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig-stream-visible"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"visible text"}}"#,
        ],
    );
}

// Not upstream's: signatures on thoughts with function calls, a function
// call's continuation and null text; output tokens from the total; and a
// finish reason that a later chunk leaves out.
#[test]
fn stream_carriers_in_detail() {
    let output = stream(
        &json!({}),
        &[
            concat!(
                r#"{"candidates":[{"content":{"parts":[{"text":"a"},{"thought":true,"functionCall":{"name":"f"},"thoughtSignature":"p"},"#,
                r#"{"thought":true,"functionCall":{"name":"g"}},{"functionCall":{"name":"h","args":{}},"thoughtSignature":"q"},"#,
                r#"{"functionCall":{"args":{"k":1}},"thoughtSignature":"r"},{"text":""},{"text":null,"thoughtSignature":"u"}]},"#,
                r#""finishReason":"IMAGE_SAFETY"}],"usageMetadata":{"promptTokenCount":4,"totalTokenCount":10}}"#
            ),
            "[DONE]",
        ],
    );
    let block = |index: usize, content_block: &str, deltas: &[&str]| {
        let mut list = vec![event(
            "content_block_start",
            &format!(
                r#"{{"type":"content_block_start","index":{index},"content_block":{content_block}}}"#
            ),
        )];
        for delta in deltas {
            list.push(event(
                "content_block_delta",
                &format!(r#"{{"type":"content_block_delta","index":{index},"delta":{delta}}}"#),
            ));
        }
        list.push(event(
            "content_block_stop",
            &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
        ));
        list.concat()
    };
    let thinking = r#"{"type":"thinking","thinking":""}"#;
    let expected = [
        message_start(DEFAULT_MESSAGE_ID, DEFAULT_MODEL)
            + &block(
                0,
                r#"{"type":"text","text":""}"#,
                &[r#"{"type":"text_delta","text":"a"}"#],
            )
            + &block(
                1,
                thinking,
                &[r#"{"type":"signature_delta","signature":"p"}"#],
            )
            + &block(2, thinking, &[r#"{"type":"thinking_delta","thinking":""}"#])
            + &block(
                3,
                thinking,
                &[r#"{"type":"signature_delta","signature":"q"}"#],
            )
            + &block(
                4,
                r#"{"type":"tool_use","id":"h-N","name":"h","input":{}}"#,
                &[r#"{"type":"input_json_delta","partial_json":"{\"k\":1}"}"#],
            )
            + &block(
                5,
                thinking,
                &[r#"{"type":"signature_delta","signature":"r"}"#],
            )
            + &block(
                6,
                r#"{"type":"text","text":""}"#,
                &[r#"{"type":"text_delta","text":""}"#],
            )
            + &block(
                7,
                thinking,
                &[r#"{"type":"signature_delta","signature":"u"}"#],
            )
            + &event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":4,"output_tokens":6}}"#,
            ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ];
    assert_eq!(output, expected);

    let output = stream(
        &json!({}),
        &[
            r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"RECITATION"}]}"#,
            r#"{"candidates":[{"finishReason":""}]}"#,
            "[DONE]",
        ],
    )
    .concat();
    assert_eq!(
        message_delta(&output),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_sequence": null}, "usage": {"input_tokens": 0, "output_tokens": 0}})
    );
}

// Not upstream's: a signature on a thought without text, then one alone,
// a thought's function call (dropped, as upstream does), and a signed call.
#[test]
fn non_stream_carriers_in_detail() {
    let output = non_stream(
        &json!({}),
        concat!(
            r#"{"candidates":[{"content":{"parts":[{"thought":true,"thoughtSignature":"a"},{"thoughtSignature":"b"},"#,
            r#"{"text":"x","thought":true},{"thoughtSignature":"c"},{"functionCall":{"name":"f"},"thought":true},"#,
            r#"{"functionCall":{"name":"g"},"thoughtSignature":"d"},{"text":"y"},{"text":"z","thoughtSignature":"e"}]},"#,
            r#""finishReason":"SPII"}]}"#
        ),
    );
    assert_eq!(
        mask_tool_ids(&output.to_string()),
        concat!(
            r#"{"id":"","type":"message","role":"assistant","model":"","content":[{"type":"thinking","thinking":"","signature":"a"},"#,
            r#"{"type":"thinking","thinking":"","signature":"b"},{"type":"thinking","thinking":"x","signature":"c"},"#,
            r#"{"type":"thinking","thinking":"","signature":"d"},{"type":"tool_use","id":"g-N","name":"g","input":{}},"#,
            r#"{"type":"text","text":"y"},{"type":"thinking","thinking":"","signature":"e"},{"type":"text","text":"z"}],"#,
            r#""stop_reason":"tool_use","stop_sequence":null}"#
        )
    );
}

fn input_deltas(output: &str) -> String {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter(|event| event["delta"]["type"] == "input_json_delta")
        .map(|event| event["delta"]["partial_json"].as_str().unwrap().to_owned())
        .collect()
}

// Ported from TestConvertGeminiResponseToClaude_SanitizesParameterlessToolArguments,
// TestConvertGeminiResponseToClaude_CanonicalNameCollisionKeepsExactToolArguments,
// TestConvertGeminiResponseToClaude_AmbiguousCanonicalNameKeepsUpstreamNameAndArguments,
// TestConvertGeminiResponseToClaude_KeepsReasonWhenTranslatedSchemaHasNoPlaceholder.
#[test]
fn cleans_arguments_and_preserves_exact_or_ambiguous_names() {
    let request = json!({"tools": [
        {"name":"EnterPlanMode","input_schema":{"type":"object","properties":{}}},
        {"name":"Read","input_schema":{"type":"object","properties":{}}},
        {"name":"read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}},
        {"name":"Bash","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}
    ]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"Bash","parametersJsonSchema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]}]});
    for (name, args, want) in [
        (
            "EnterPlanMode",
            json!({"reason":"Plan mode switch"}),
            json!({}),
        ),
        ("read", json!({"path":"/tmp/a"}), json!({"path":"/tmp/a"})),
        ("Read", json!({"reason":"x"}), json!({})),
        ("READ", json!({"path":"/tmp/a"}), json!({"path":"/tmp/a"})),
        (
            "Bash",
            json!({"command":"ls","reason":"why"}),
            json!({"command":"ls","reason":"why"}),
        ),
    ] {
        let response = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":name,"args":args}}]},"finishReason":"STOP"}]});
        let out = convert_gemini_response_to_claude_non_stream_with_translated(
            &request, &sent, &response,
        );
        assert_eq!(out["content"][0]["name"], name);
        assert_eq!(out["content"][0]["input"], want);
        let mut translator = GeminiToClaudeStream::new(&request).with_translated_request(&sent);
        let output = translator.translate(response.to_string().as_bytes());
        assert_eq!(
            serde_json::from_str::<Value>(&input_deltas(&output)).unwrap(),
            want
        );
    }
}

#[test]
#[ignore = "needs the p42 argument helper"]
fn implicit_open_cleaned_placeholders_are_deferred() {
    for closed in [false, true] {
        let mut request = json!({"tools":[
            {"name":"EnterPlanMode","input_schema":{"type":"object","properties":{}}},
            {"name":"Read","input_schema":{"type":"object","properties":{}}},
            {"name":"read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}
        ]});
        if closed {
            for index in [0, 1] {
                request["tools"][index]["input_schema"]["additionalProperties"] = json!(false);
            }
        }
        for name in ["EnterPlanMode", "Read"] {
            let mut translator = GeminiToClaudeStream::new(&request);
            let first = json!({"candidates":[{"content":{"parts":[{"functionCall":{
                "name":name,"args":{"reason":"switch"}
            }}]}}]});
            let before = translator.translate(first.to_string().as_bytes());
            assert_eq!(
                input_deltas(&before),
                if closed { "{}" } else { "" },
                "{name}, closed={closed}: {before}"
            );
            let after = translator.translate(br#"{"candidates":[{"finishReason":"STOP"}]}"#)
                + &translator.translate(b"[DONE]");
            assert_eq!(
                input_deltas(&after),
                if closed { "" } else { "{}" },
                "{name}, closed={closed}: {after}"
            );
            assert_eq!(input_deltas(&(before + &after)), "{}");
        }
    }
}

// Ported from TestConvertGeminiResponseToClaude_ParameterlessToolContinuationDoesNotDuplicateEmptyObject.
#[test]
fn parameterless_continuation_emits_one_empty_object() {
    for tool in [
        json!({"name":"EnterPlanMode"}),
        json!({"name":"EnterPlanMode","input_schema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"EnterPlanMode","input_schema":{"type":"object","properties":{}}}),
    ] {
        let request = json!({"tools":[tool]});
        let output = stream(&request, &[
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"EnterPlanMode","args":{"reason":"switch"}}}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":{"reason":"more"}}}]},"finishReason":"STOP"}]}"#,
        ]).concat();
        assert_eq!(input_deltas(&output), "{}", "{request}: {output}");
    }
}

#[test]
fn deferred_empty_tool_input_preserves_real_arguments_once() {
    let request = json!({"tools":[{"name":"echo","input_schema":{
        "type":"object","properties":{"user":{"type":"string"}}
    }}]});
    check_empty_tool_continuations(&request);
}

// Ported from TestConvertGeminiResponseToClaude_ImplicitOpenToolContinuationOrdering.
#[test]
#[ignore = "needs the p42 argument helper"]
fn implicit_open_tool_continuation_ordering() {
    let request =
        json!({"tools":[{"name":"echo","input_schema":{"type":"object","properties":{}}}]});
    check_empty_tool_continuations(&request);
}

#[test]
#[ignore = "needs the p42 argument helper"]
fn implicit_open_final_empty_chunk() {
    let request =
        json!({"tools":[{"name":"echo","input_schema":{"type":"object","properties":{}}}]});
    for split in [false, true] {
        let mut translator = GeminiToClaudeStream::new(&request);
        if split {
            let first = translator.translate(br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"echo","args":{}}}]}}]}"#);
            assert!(input_deltas(&first).is_empty(), "{first}");
        }
        let call = if split {
            json!({"args":{}})
        } else {
            json!({"name":"echo","args":{}})
        };
        let last = json!({"candidates":[{"content":{"parts":[{"functionCall":call}]},"finishReason":"STOP"}]});
        let mut output = translator.translate(last.to_string().as_bytes());
        output.push_str(&translator.translate(b"[DONE]"));
        assert_eq!(input_deltas(&output), "{}", "{output}");
        assert_eq!(
            output.matches("\"type\":\"input_json_delta\"").count(),
            1,
            "{output}"
        );
    }
}

fn check_empty_tool_continuations(request: &Value) {
    for (first, continuation) in [
        (json!({}), json!({"user":"v"})),
        (json!({"reason":"thinking"}), json!({"user":"v"})),
        (json!({"user":"v"}), json!({})),
        (json!({"user":"v"}), json!({"reason":"done"})),
    ] {
        // The translated schema proves that reason is a normalizer placeholder.
        let sent = json!({"tools":[{"functionDeclarations":[{"name":"echo",
            "parametersJsonSchema":{"type":"object","properties":{
                "user":{"type":"string"},"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}}}}]}]});
        let mut translator = GeminiToClaudeStream::new(request).with_translated_request(&sent);
        let first_chunk = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"echo","args":first}}]}}]});
        let first_out = translator.translate(first_chunk.to_string().as_bytes());
        if first.get("user").is_none() {
            assert!(input_deltas(&first_out).is_empty(), "{first_out}");
        }
        let continuation_chunk = json!({"candidates":[{"content":{"parts":[{"functionCall":{"args":continuation}}]},"finishReason":"STOP"}]});
        let output = first_out
            + &translator.translate(continuation_chunk.to_string().as_bytes())
            + &translator.translate(b"[DONE]");
        let input = input_deltas(&output);
        assert_eq!(
            serde_json::from_str::<Value>(&input).unwrap(),
            json!({"user":"v"}),
            "{output}"
        );
        assert_eq!(input, r#"{"user":"v"}"#);
    }
}

#[test]
fn deferred_empty_tool_input_flushes_before_each_completion() {
    for ending in [
        "[DONE]",
        r#"{"candidates":[{"finishReason":"STOP"}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"done"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"thinking"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"next","args":{"x":1}}}]}}]}"#,
    ] {
        let mut translator = GeminiToClaudeStream::new(&Value::Null);
        let first = translator.translate(br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"echo","args":{}}}]}}]}"#);
        assert!(!first.contains("input_json_delta"), "{first}");
        let out = translator.translate(ending.as_bytes());
        let events: Vec<Value> = out
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        let deltas: Vec<_> = events
            .iter()
            .filter(|e| e["index"] == 0 && e["delta"]["type"] == "input_json_delta")
            .collect();
        assert_eq!(deltas.len(), 1, "{ending}: {out}");
        assert_eq!(deltas[0]["delta"]["partial_json"], "{}");
        if let Some(stop) = events
            .iter()
            .position(|e| e["index"] == 0 && e["type"] == "content_block_stop")
        {
            assert!(
                events
                    .iter()
                    .position(|e| e["delta"]["type"] == "input_json_delta")
                    .unwrap()
                    < stop
            );
        }
        assert!(!translator.translate(b"[DONE]").contains("input_json_delta"));
    }
}

// Ported from TestConvertGeminiResponseToClaudeStream_MalformedFunctionCallAfterThinkingEmitsError
// and TestConvertGeminiResponseToClaudeStream_MalformedFunctionCallWithoutUsageAndDuplicateTerminalChunk.
#[test]
fn malformed_call_closes_thinking_and_suppresses_late_chunks() {
    let mut translator = GeminiToClaudeStream::new(&hi_request());
    let mut output = translator.translate(
        br#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"planning tool call"}]}}]}"#,
    );
    output.push_str(
        &translator.translate(br#"{"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#),
    );
    assert!(
        output.find("event: content_block_stop").unwrap() < output.find("event: error").unwrap()
    );
    assert!(translator.translate(br#"{"candidates":[{"content":{"parts":[{"text":"late text"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}],"usageMetadata":{"promptTokenCount":50}}"#).is_empty());
    assert!(translator.translate(b"[DONE]").is_empty());
    assert!(!output.contains("event: message_delta"));
    assert!(!output.contains("event: message_stop"));
    assert_eq!(output.matches("event: error").count(), 1);
}

// Not upstream's: a malformed call reported after the message has ended
// leaves the block its parts open, with no error, as upstream does.
#[test]
fn malformed_call_after_the_message_ended_leaves_the_block_open() {
    let mut translator = GeminiToClaudeStream::new(&hi_request());
    let ended = translator.translate(br#"{"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1}}"#)
        + &translator.translate(b"[DONE]");
    assert!(ended.contains("event: message_stop"));
    let late = translator.translate(br#"{"candidates":[{"content":{"parts":[{"text":"late","thought":true}]},"finishReason":"MALFORMED_FUNCTION_CALL"}],"usageMetadata":{"promptTokenCount":1}}"#);
    assert!(late.contains("event: content_block_start"));
    assert!(late.contains("event: content_block_delta"));
    assert!(!late.contains("event: content_block_stop"));
    assert!(!late.contains("event: error"));
    assert!(
        translator
            .translate(br#"{"candidates":[{"content":{"parts":[{"text":"later"}]}}]}"#)
            .is_empty()
    );
}

// Ported from TestConvertGeminiResponseToClaudeNonStream_MalformedFunctionCallReturnsAPIError
// and TestConvertGeminiResponseToClaude_MalformedFunctionCallWithToolUsePreservesToolUse.
#[test]
fn malformed_call_is_api_error_unless_a_tool_call_was_returned() {
    let mut request = hi_request();
    request["tools"] = json!([{"name":"Bash","input_schema":{"type":"object"}}]);
    for tool_call in [false, true] {
        let parts = if tool_call {
            json!([{"functionCall":{"name":"Bash","args":{"command":"ls"}}}])
        } else {
            json!([])
        };
        let response = json!({"candidates":[{"content":{"parts":parts},"finishReason":"MALFORMED_FUNCTION_CALL"}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":37}});
        let output = convert_gemini_response_to_claude_non_stream(&request, &response);
        if tool_call {
            assert_eq!(output["stop_reason"], "tool_use");
        } else {
            assert_eq!(output, malformed_error());
        }
        let output = stream(&request, &[&response.to_string(), "[DONE]"]).concat();
        assert_eq!(output.contains("event: error"), !tool_call);
        if tool_call {
            assert!(output.contains(r#""stop_reason":"tool_use""#));
        }
    }
}

// Ported from TestConvertGeminiResponseToClaudeNonStream_UniqueToolUseIDsAcrossTurns.
#[test]
fn non_stream_tool_use_ids_are_unique_across_turns_and_streams() {
    let request =
        json!({"model":"gemini-test","messages":[{"role":"user","content":"run command"}]});
    let response = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}}}],"role":"model"},"finishReason":"STOP"}],"modelVersion":"gemini-test","responseId":"resp-tool"});
    let mut translator = GeminiToClaudeStream::new(&request);
    let stream =
        translator.translate(response.to_string().as_bytes()) + &translator.translate(b"[DONE]");
    let streamed = stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["content_block"]["type"] == "tool_use")
        .unwrap();
    let first = convert_gemini_response_to_claude_non_stream(&request, &response);
    let second = convert_gemini_response_to_claude_non_stream(&request, &response);
    let ids = [
        streamed["content_block"]["id"].as_str().unwrap(),
        first["content"][0]["id"].as_str().unwrap(),
        second["content"][0]["id"].as_str().unwrap(),
    ];
    for id in ids {
        assert!(id.strip_prefix("Bash-").unwrap().parse::<u64>().unwrap() > 0);
    }
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);
}

#[test]
fn cached_tool_names_resolve_multiple_calls_in_both_response_modes() {
    let request = json!({"tools":[{"name":"Read"},{"name":"read"},
        {"name":"Write"},{"name":"Write"},{"function":{"name":"__Nested"}},
        {"name":"odd name","input_schema":{"type":"object","properties":{}}}]});
    let response = json!({"candidates":[{"content":{"parts":[
        {"functionCall":{"name":"READ","args":{}}},
        {"functionCall":{"name":"Read","args":{}}},
        {"functionCall":{"name":"write","args":{}}},
        {"functionCall":{"name":"NESTED","args":{}}},
        {"functionCall":{"name":"odd_name","args":{"_":""}}}
    ]},"finishReason":"STOP"}]});
    let expected = ["READ", "Read", "Write", "__Nested", "odd name"];
    let out = convert_gemini_response_to_claude_non_stream(&request, &response);
    let names: Vec<_> = out["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, expected);
    assert_eq!(out["content"][4]["input"], json!({}));
    let mut translator = GeminiToClaudeStream::new(&request);
    let events = translator.translate(response.to_string().as_bytes());
    let names: Vec<String> = events
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event["content_block"]["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(names, expected);
}
