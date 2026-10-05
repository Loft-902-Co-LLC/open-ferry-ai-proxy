// Ported from CLIProxyAPI internal/translator/gemini/claude/gemini_claude_response_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests after them are new; their expected output
// comes from upstream, with tool call numbers masked: the counter is shared
// by every stream in the process.

use serde_json::{Value, json};

use super::*;

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
    assert_eq!(
        output["content"],
        json!([
            {"type": "thinking", "thinking": "inferred reasoning", "signature": "sig-snake-case"},
            {"type": "text", "text": "final answer"}
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
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"y\":2}"}}"#,
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
fn stream_without_content_has_no_stop() {
    let output = stream(&json!({}), &["not json", "[DONE]"]);
    assert_eq!(
        output,
        [
            message_start(DEFAULT_MESSAGE_ID, DEFAULT_MODEL),
            String::new()
        ]
    );
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
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
                ),
                (
                    "content_block_stop",
                    r#"{"type":"content_block_stop","index":0}"#,
                ),
                (
                    "content_block_start",
                    r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                ),
                (
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"t"}}"#,
                ),
            ]),
        String::new(),
        events(&[
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
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
    let expected = [
        message_start(DEFAULT_MESSAGE_ID, DEFAULT_MODEL),
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
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"x"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"z"}}"#,
            ),
        ]),
    ];
    assert_eq!(output, expected);
}

#[test]
fn non_stream_edge_cases() {
    let request = json!({"tools": [{"name": "a b"}, {"name": "Read"}]});
    assert_eq!(
        non_stream(
            &request,
            concat!(
                r#"{"candidates":[{"content":{"parts":[{"text":"a","thought":true},{"thoughtSignature":"s"},{"text":"b"},"#,
                r#"{"functionCall":{"name":"a_b","args":"{}"}},{"functionCall":{"name":"read","args":{"k":[1]}}},"#,
                r#"{"text":"c","thought_signature":"t"}]},"finishReason":"MAX_TOKENS"}]}"#
            )
        )
        .to_string(),
        concat!(
            r#"{"id":"","type":"message","role":"assistant","model":"","content":[{"type":"thinking","thinking":"a","signature":"s"},"#,
            r#"{"type":"text","text":"b"},{"type":"tool_use","id":"a_b-1","name":"a b","input":{}},"#,
            r#"{"type":"tool_use","id":"read-2","name":"Read","input":{"k":[1]}},{"type":"thinking","thinking":"c","signature":"t"}],"#,
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
            r#""content":[{"type":"thinking","thinking":"","signature":"q"},{"type":"text","text":"x"}],"#,
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
