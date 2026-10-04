// Ported from CLIProxyAPI internal/translator/interactions/claude/interactions_claude_test.go
// (the response tests) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests after them are new; their expected output
// comes from upstream.

use serde_json::{Value, json};

use super::*;

/// Feeds `chunks` through one stream and returns every frame.
fn stream(model: &str, chunks: &[&str]) -> Vec<String> {
    let mut translator = InteractionsToClaudeStream::new(model);
    chunks
        .iter()
        .flat_map(|chunk| translator.translate(chunk.as_bytes()))
        .collect()
}

/// `findClaudeEventPayload`: the data of the first frame for `event`.
fn find_event(frames: &[String], event: &str) -> Option<Value> {
    let frame = frames
        .iter()
        .find(|frame| frame.contains(&format!("event: {event}")))?;
    let data = frame
        .lines()
        .find_map(|line| line.trim().strip_prefix("data:"))?;
    Some(serde_json::from_str(data.trim()).unwrap())
}

fn non_stream(model: &str, body: &str) -> Value {
    convert_interactions_response_to_claude_non_stream(model, body.as_bytes())
}

// TestConvertInteractionsResponseToClaudeStream
#[test]
fn response_to_claude_stream() {
    let out = stream(
        "gemini-3.1-flash-lite",
        &[
            "event: interaction.created\ndata: {\"interaction\":{\"id\":\"interaction_1\",\"model\":\"gemini-3.1-flash-lite\"},\"event_type\":\"interaction.created\"}",
            "event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"model_output\"},\"event_type\":\"step.start\"}",
            "event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"北京今天晴\"},\"event_type\":\"step.delta\"}",
            "event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}",
            "event: interaction.completed\ndata: {\"interaction\":{\"id\":\"interaction_1\",\"model\":\"gemini-3.1-flash-lite\",\"usage\":{\"total_input_tokens\":3,\"total_output_tokens\":4}},\"event_type\":\"interaction.completed\"}",
            "event: done\ndata: [DONE]",
        ],
    );
    let start = find_event(&out, "message_start").unwrap();
    assert_eq!(start["message"]["model"], "gemini-3.1-flash-lite");
    let delta = find_event(&out, "content_block_delta").unwrap();
    assert_eq!(delta["delta"]["text"], "北京今天晴");
    let message_delta = find_event(&out, "message_delta").unwrap();
    assert_eq!(message_delta["usage"]["output_tokens"], 4);
    let stop = find_event(&out, "message_stop").unwrap();
    assert_eq!(stop["type"], "message_stop");
}

// TestConvertInteractionsResponseToClaudeStreamToolCall
#[test]
fn stream_tool_call() {
    let out = stream(
        "gemini-3.1-flash-lite",
        &[
            r#"data: {"interaction":{"id":"interaction_1","model":"gemini-3.1-flash-lite"},"event_type":"interaction.created"}"#,
            r#"data: {"index":0,"step":{"type":"function_call","id":"toolu_1","signature":"sig_1","name":"get_weather","arguments":{}},"event_type":"step.start"}"#,
            r#"data: {"index":0,"delta":{"type":"arguments_delta","arguments":"{\"location\":\"北京\"}"},"event_type":"step.delta"}"#,
            r#"data: {"index":0,"event_type":"step.stop"}"#,
            r#"data: {"interaction":{"usage":{"total_input_tokens":1,"total_output_tokens":2}},"event_type":"interaction.completed"}"#,
        ],
    );
    let start = find_event(&out, "content_block_start").unwrap();
    assert_eq!(start["content_block"]["type"], "tool_use");
    assert_eq!(start["content_block"]["signature"], "sig_1");
    let delta = find_event(&out, "content_block_delta").unwrap();
    assert_eq!(delta["delta"]["partial_json"], r#"{"location":"北京"}"#);
    let message_delta = find_event(&out, "message_delta").unwrap();
    assert_eq!(message_delta["delta"]["stop_reason"], "tool_use");
}

// TestConvertInteractionsResponseToClaudeStreamFinishMetadataUsage
#[test]
fn stream_finish_metadata_usage() {
    let out = stream(
        "claude-test",
        &[
            r#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_tokens":8}}}"#,
        ],
    );
    let payload = find_event(&out, "message_delta").unwrap();
    assert_eq!(payload["usage"]["input_tokens"], 2, "{payload}");
    assert_eq!(payload["usage"]["output_tokens"], 6, "{payload}");
}

// TestConvertInteractionsResponseToClaudeNonStream
#[test]
fn response_to_claude_non_stream() {
    let out = non_stream(
        "gemini-3.1-flash-lite",
        r#"{"id":"interaction_1","model":"gemini-3.1-flash-lite","steps":[{"type":"model_output","content":[{"type":"text","text":"ok"}]},{"type":"function_call","call_id":"toolu_1","signature":"sig_1","name":"lookup","arguments":{"q":"x"}}],"usage":{"total_input_tokens":3,"total_output_tokens":4}}"#,
    );
    assert_eq!(out["content"][0]["text"], "ok", "{out}");
    assert_eq!(out["content"][1]["type"], "tool_use", "{out}");
    assert_eq!(out["content"][1]["signature"], "sig_1", "{out}");
    assert_eq!(out["stop_reason"], "tool_use", "{out}");
    assert_eq!(out["usage"]["input_tokens"], 3, "{out}");
}

// TestConvertInteractionsResponseToClaude_IncompleteMaxTokens
#[test]
fn incomplete_max_tokens() {
    let out = non_stream(
        "devin/swe-2",
        r#"{"id":"interaction_1","model":"devin/swe-2","status":"incomplete","finish_reason":"length","steps":[{"type":"model_output","content":[{"type":"text","text":"cut short"}]}],"usage":{"total_input_tokens":3,"total_output_tokens":4}}"#,
    );
    assert_eq!(out["stop_reason"], "max_tokens", "{out}");

    let out = stream(
        "devin/swe-2",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"cut short"}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"incomplete","finish_reason":"length"}}"#,
            "data: [DONE]",
        ],
    );
    let message_delta = find_event(&out, "message_delta").unwrap();
    assert_eq!(message_delta["delta"]["stop_reason"], "max_tokens");
}

// TestConvertInteractionsResponseToClaude_PreservesCacheReadUsage/streaming_cache_hit
#[test]
fn preserves_cache_read_usage_streaming() {
    let out = stream(
        "devin/swe-2",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"hello"}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","model":"devin/swe-2","status":"completed","usage":{"total_input_tokens":10411,"total_output_tokens":76,"total_cached_tokens":10340,"total_tokens":10487}}}"#,
            "data: [DONE]",
        ],
    );
    let usage = &find_event(&out, "message_delta").unwrap()["usage"];
    assert_eq!(usage["input_tokens"], 71, "{usage}");
    assert_eq!(usage["output_tokens"], 76, "{usage}");
    assert_eq!(usage["cache_read_input_tokens"], 10340, "{usage}");
}

/// The non-streaming subtests of
/// TestConvertInteractionsResponseToClaude_PreservesCacheReadUsage: the
/// usage given, and the Claude usage fields expected (`None` for absent).
const NON_STREAM_USAGE_CASES: &[(&str, &str, [Option<i64>; 4])] = &[
    (
        "non_streaming_cache_hit",
        r#"{"total_input_tokens":96724,"total_output_tokens":269,"total_cached_tokens":30784,"total_tokens":96993}"#,
        [Some(65940), Some(269), Some(30784), None],
    ),
    (
        "zero_cache_tokens",
        r#"{"total_input_tokens":100,"total_output_tokens":50,"total_cached_tokens":0,"total_tokens":150}"#,
        [Some(100), Some(50), None, None],
    ),
    (
        "explicit_uncached_and_cache_creation",
        r#"{"input_tokens":71,"total_input_tokens":10411,"total_output_tokens":76,"cache_read_input_tokens":10340,"cache_creation_input_tokens":25,"total_tokens":10487}"#,
        [Some(71), Some(76), Some(10340), Some(25)],
    ),
    (
        "explicit_uncached_with_cache_write_in_total",
        r#"{"input_tokens":71,"total_input_tokens":10436,"total_output_tokens":76,"cache_read_input_tokens":10340,"cache_creation_input_tokens":25,"total_tokens":10512}"#,
        [Some(71), Some(76), Some(10340), Some(25)],
    ),
    (
        "full_cache_hit",
        r#"{"total_input_tokens":500,"total_output_tokens":50,"total_cached_tokens":500,"total_tokens":550}"#,
        [Some(0), Some(50), Some(500), None],
    ),
    (
        "cached_tokens_exceeds_input",
        r#"{"total_input_tokens":50,"total_output_tokens":50,"total_cached_tokens":100,"total_tokens":150}"#,
        [Some(0), Some(50), Some(100), None],
    ),
    (
        "explicit_uncached_without_total_input_tokens",
        r#"{"input_tokens":71,"output_tokens":76,"cache_read_input_tokens":10340}"#,
        [Some(71), Some(76), Some(10340), None],
    ),
    (
        "explicit_uncached_without_total_input_tokens_greater",
        r#"{"input_tokens":100,"output_tokens":30,"cache_read_input_tokens":80}"#,
        [Some(100), Some(30), Some(80), None],
    ),
    (
        "explicit_uncached_without_total_input_tokens_equal",
        r#"{"input_tokens":80,"output_tokens":30,"cache_read_input_tokens":80}"#,
        [Some(80), Some(30), Some(80), None],
    ),
    (
        "explicit_uncached_zero_with_total",
        r#"{"input_tokens":0,"total_input_tokens":500,"output_tokens":30,"cache_read_input_tokens":500}"#,
        [Some(0), Some(30), Some(500), None],
    ),
    (
        "explicit_uncached_equal_to_total_input",
        r#"{"input_tokens":100,"total_input_tokens":100,"output_tokens":30,"cache_read_input_tokens":20}"#,
        [Some(100), Some(30), Some(20), None],
    ),
    (
        "explicit_uncached_zero_with_partial_cache",
        r#"{"input_tokens":0,"total_input_tokens":500,"output_tokens":30,"cache_read_input_tokens":200}"#,
        [Some(0), Some(30), Some(200), None],
    ),
    (
        "inclusive_total_without_input_tokens_large_scale",
        r#"{"total_input_tokens":10436,"output_tokens":76,"cache_read_input_tokens":10340,"cache_creation_input_tokens":25}"#,
        [Some(71), Some(76), Some(10340), Some(25)],
    ),
    (
        "prompt_tokens_fallback",
        r#"{"prompt_tokens":100,"output_tokens":20,"cached_tokens":30}"#,
        [Some(70), Some(20), Some(30), None],
    ),
];

// TestConvertInteractionsResponseToClaude_PreservesCacheReadUsage (the
// non-streaming subtests). Upstream checks some of the fields of each; we
// check all four.
#[test]
fn preserves_cache_read_usage_non_streaming() {
    for (name, usage, want) in NON_STREAM_USAGE_CASES {
        let body = format!(
            r#"{{"id":"i","model":"devin/swe-2","status":"completed","steps":[{{"type":"model_output","content":[{{"type":"text","text":"response text"}}]}}],"usage":{usage}}}"#
        );
        let out = non_stream("devin/swe-2", &body);
        let got = [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .map(|key| out["usage"].get(key).and_then(Value::as_i64));
        assert_eq!(&got, want, "{name}: {out}");
    }
}

// TestConvertInteractionsResponseToClaude_ResponseFailed
#[test]
fn response_failed() {
    for (name, payload, want) in [
        (
            "response_failed_top_level",
            r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error (permission_denied): Unable to process request due to an MCP configuration issue.","code":"403"}}"#,
            "permission_denied",
        ),
        (
            "interaction_failed_nested",
            r#"data: {"event_type":"interaction.failed","interaction":{"error":{"message":"service unavailable","type":"server_error"}}}"#,
            "service unavailable",
        ),
        (
            "fallback_defaults",
            r#"data: {"event_type":"response.failed"}"#,
            "upstream error occurred",
        ),
    ] {
        let events = stream("devin/kimi-k3", &[payload]);
        assert!(!events.is_empty(), "{name}");
        let error = find_event(&events, "error").unwrap();
        assert!(
            error["error"]["message"].as_str().unwrap().contains(want),
            "{name}: {error}"
        );
    }
}

// Not upstream's: every frame of a stream with thinking, a signature, and a
// call whose start never came, then a second [DONE] that gives nothing.
#[test]
fn stream_frames() {
    let out = stream(
        "m",
        &[
            r#"{"event_type":"step.start","index":0,"step":{"type":"thought"}}"#,
            r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"text":"hm"}}}"#,
            r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"s"}}"#,
            r#"{"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta","arguments":""},"step":{"name":"f"}}"#,
            "[DONE]",
            "[DONE]",
        ],
    );
    let mut frames = out.iter();
    let start = frames.next().unwrap();
    assert!(start.starts_with("event: message_start\ndata: "), "{start}");
    assert!(start.ends_with("\n\n\n"), "{start}");
    let rest: Vec<&str> = frames.map(String::as_str).collect();
    assert_eq!(
        rest,
        [
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"s\"}}\n\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_3\",\"name\":\"f\",\"input\":{}}}\n\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}\n\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n\n",
        ]
    );
    let start: Value = serde_json::from_str(
        start
            .trim()
            .strip_prefix("event: message_start\ndata: ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        start["message"]["id"]
            .as_str()
            .map(|id| id.starts_with("msg_")),
        Some(true)
    );
    assert_eq!(start["message"]["model"], "m");
}

// Not upstream's: a body with the interaction nested, steps given as an
// object, and thought texts from a string and from parts.
#[test]
fn non_stream_nested_interaction() {
    let out = non_stream(
        "m",
        r#"{"id":"outer","interaction":{"model":" ","steps":{
            "a":{"type":"thought","content":"plain","thought_signature":"sig"},
            "b":{"type":"thought","content":[{"text":" "},{"content":{"text":"part"}}]},
            "c":{"type":"function_call","name":"f","args":{"x":1},"arguments":null},
            "d":"skipped"}}}"#,
    );
    assert_eq!(
        out,
        json!({
            "id": "outer",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "thinking", "thinking": "plain", "signature": "sig"},
                {"type": "thinking", "thinking": "part"},
                {"type": "tool_use", "id": "toolu_interactions", "name": "f", "input": {}},
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": 0},
        })
    );
}
