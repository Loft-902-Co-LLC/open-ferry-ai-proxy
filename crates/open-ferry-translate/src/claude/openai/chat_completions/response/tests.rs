// Ported from CLIProxyAPI internal/translator/claude/openai/chat-completions/claude_openai_response_test.go,
// claude_openai_native_response_test.go and noop_optimization_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;

const MODEL: &str = "claude-opus-4-6";

/// A `data:` line carrying `event`.
fn data(event: &Value) -> String {
    format!("data: {event}")
}

/// Sends `event` to `stream` as a `data:` line.
fn send(stream: &mut ClaudeToOpenAIChatCompletionsStream, event: &Value) -> Option<Value> {
    stream.translate_line(data(event).as_bytes())
}

/// Feeds `events` through one stream and returns the chunks it emits.
fn run_stream(events: &[Value]) -> Vec<Value> {
    let mut stream = ClaudeToOpenAIChatCompletionsStream::new(MODEL);
    events
        .iter()
        .filter_map(|event| send(&mut stream, event))
        .collect()
}

/// Converts `events`, one `data:` line each, as a complete response.
fn non_stream(events: &[Value]) -> Value {
    let sse: String = events.iter().map(|event| data(event) + "\n").collect();
    convert_claude_response_to_openai_chat_completions_non_stream(sse.as_bytes())
}

/// Looks up a dotted path such as `choices.0.delta.content`, like a plain
/// gjson path.
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

/// The text at `path` in each chunk, joined.
fn streamed_text(chunks: &[Value], path: &str) -> String {
    chunks.iter().map(|chunk| text_at(chunk, path)).collect()
}

/// The usage the cached token tests expect: 13 input, 22000 cache read and 31
/// cache write tokens in, 4 out.
fn cached_usage() -> Value {
    json!({
        "prompt_tokens": 22044,
        "completion_tokens": 4,
        "total_tokens": 22048,
        "prompt_tokens_details": {
            "cached_tokens": 22000,
            "cached_creation_tokens": 31,
            "cache_write_tokens": 31,
        },
    })
}

#[test]
fn stream_usage_includes_cached_tokens() {
    let mut stream = ClaudeToOpenAIChatCompletionsStream::new(MODEL);
    let chunk = send(
        &mut stream,
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {
                "input_tokens": 13,
                "output_tokens": 4,
                "cache_read_input_tokens": 22000,
                "cache_creation_input_tokens": 31,
            },
        }),
    )
    .expect("expected 1 chunk");
    assert_eq!(chunk["usage"], cached_usage(), "chunk={chunk}");
}

#[test]
fn stream_usage_merges_message_start_usage() {
    let mut stream = ClaudeToOpenAIChatCompletionsStream::new(MODEL);
    send(
        &mut stream,
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": MODEL,
                "usage": {
                    "input_tokens": 13,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 22000,
                    "cache_creation_input_tokens": 31,
                },
            },
        }),
    );
    let chunk = send(
        &mut stream,
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 4},
        }),
    )
    .expect("expected 1 chunk");
    assert_eq!(chunk["usage"], cached_usage(), "chunk={chunk}");
}

#[test]
fn non_stream_usage_includes_cached_tokens() {
    let out = non_stream(&[
        json!({"type": "message_start", "message": {"id": "msg_123", "model": MODEL}}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {
                "input_tokens": 13,
                "output_tokens": 4,
                "cache_read_input_tokens": 22000,
                "cache_creation_input_tokens": 31,
            },
        }),
    ]);
    assert_eq!(out["usage"], cached_usage(), "out={out}");
}

#[test]
fn non_stream_usage_merges_message_start_usage() {
    let out = non_stream(&[
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": MODEL,
                "usage": {
                    "input_tokens": 13,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 22000,
                    "cache_creation_input_tokens": 31,
                },
            },
        }),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 4},
        }),
    ]);
    assert_eq!(out["usage"], cached_usage(), "out={out}");
}

#[test]
fn refusal_stop_reason() {
    for (stop_reason, want) in [
        ("refusal", "content_filter"),
        ("sensitive", "content_filter"),
    ] {
        let mut stream = ClaudeToOpenAIChatCompletionsStream::new(MODEL);
        let chunk = send(
            &mut stream,
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": {"output_tokens": 10},
            }),
        )
        .unwrap_or_else(|| panic!("{stop_reason}: expected 1 chunk"));
        assert_eq!(
            text_at(&chunk, "choices.0.finish_reason"),
            want,
            "{stop_reason} maps to {want}; payload={chunk}"
        );
    }
}

#[test]
fn non_stream_refusal_stop_reason() {
    for (stop_reason, want) in [
        ("refusal", "content_filter"),
        ("sensitive", "content_filter"),
    ] {
        let out = non_stream(&[
            json!({"type": "message_start", "message": {"id": "msg_123", "model": MODEL}}),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": {"input_tokens": 10, "output_tokens": 20},
            }),
        ]);
        assert_eq!(
            text_at(&out, "choices.0.finish_reason"),
            want,
            "{stop_reason} maps to {want}; payload={out}"
        );
    }
}

#[test]
fn non_stream_reasoning_content() {
    let out = non_stream(&[
        json!({"type": "message_start", "message": {"id": "msg_123", "model": MODEL}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me analyze the problem."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": " Step 2 is clear."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Here is the solution."}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 10, "output_tokens": 20},
        }),
    ]);
    // No old-style `reasoning` field either.
    assert_eq!(
        out["choices"][0]["message"],
        json!({
            "role": "assistant",
            "content": "Here is the solution.",
            "reasoning_content": "Let me analyze the problem. Step 2 is clear.",
        }),
        "payload={out}"
    );
}

#[test]
fn non_stream_omits_reasoning_content_when_absent() {
    let out = non_stream(&[
        json!({"type": "message_start", "message": {"id": "msg_123", "model": MODEL}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Just plain text."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 10, "output_tokens": 20},
        }),
    ]);
    assert_eq!(
        out["choices"][0]["message"],
        json!({"role": "assistant", "content": "Just plain text."}),
        "payload={out}"
    );
}

#[test]
fn stream_and_non_stream_parity() {
    let events = [
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": MODEL,
                "usage": {"input_tokens": 15, "output_tokens": 1},
            },
        }),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "First thought. "}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Second thought."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Final "}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "answer."}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 25},
        }),
        json!({"type": "message_stop"}),
    ];

    let chunks = run_stream(&events);
    let stream_reasoning = streamed_text(&chunks, "choices.0.delta.reasoning_content");
    let stream_content = streamed_text(&chunks, "choices.0.delta.content");
    let stream_finish_reason = chunks
        .iter()
        .rev()
        .map(|chunk| text_at(chunk, "choices.0.finish_reason"))
        .find(|reason| !reason.is_empty())
        .unwrap_or_default();

    let out = non_stream(&events);

    assert_eq!(stream_reasoning, "First thought. Second thought.");
    assert_eq!(
        text_at(&out, "choices.0.message.reasoning_content"),
        stream_reasoning,
        "reasoning_content parity"
    );
    assert_eq!(stream_content, "Final answer.");
    assert_eq!(
        text_at(&out, "choices.0.message.content"),
        stream_content,
        "content parity"
    );
    assert_eq!(stream_finish_reason, "stop");
    assert_eq!(
        text_at(&out, "choices.0.finish_reason"),
        stream_finish_reason,
        "finish_reason parity"
    );
}

#[test]
fn redacted_thinking_ignored() {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_123", "model": MODEL}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "redacted_thinking", "data": "encrypted_blob"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Visible reply."}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 10, "output_tokens": 20},
        }),
    ];

    let out = non_stream(&events);
    assert_eq!(
        out["choices"][0]["message"],
        json!({"role": "assistant", "content": "Visible reply."}),
        "redacted_thinking must not map to reasoning_content or reasoning in non-stream"
    );

    let chunks = run_stream(&events);
    for chunk in &chunks {
        for path in [
            "choices.0.delta.reasoning_content",
            "choices.0.delta.reasoning",
        ] {
            assert!(
                at(chunk, path).is_none(),
                "redacted_thinking must not produce {path} in stream, got {chunk}"
            );
        }
    }
    assert_eq!(
        streamed_text(&chunks, "choices.0.delta.content"),
        "Visible reply."
    );
}

#[test]
fn stream_tool_call_index_is_zero_based() {
    let events = [
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": MODEL,
                "usage": {"input_tokens": 15, "output_tokens": 1},
            },
        }),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Thinking..."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": r#"{"city": "Paris"}"#}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "get_time"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": r#"{"city":"Tokyo"}"#}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 25},
        }),
        json!({"type": "message_stop"}),
    ];

    let calls: Vec<Value> = run_stream(&events)
        .iter()
        .filter_map(|chunk| at(chunk, "choices.0.delta.tool_calls.0").cloned())
        .collect();
    assert_eq!(
        calls,
        [
            json!({
                "index": 0,
                "id": "toolu_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": r#"{"city": "Paris"}"#},
            }),
            json!({
                "index": 1,
                "id": "toolu_2",
                "type": "function",
                "function": {"name": "get_time", "arguments": r#"{"city":"Tokyo"}"#},
            }),
        ]
    );
}

#[test]
fn stream_emits_trailing_usage_chunk_with_cache_details() {
    let chunks = run_stream(&[
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": MODEL,
                "usage": {
                    "input_tokens": 100,
                    "cache_creation_input_tokens": 20,
                    "cache_read_input_tokens": 50,
                    "output_tokens": 1,
                },
            },
        }),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 15},
        }),
        json!({"type": "message_stop"}),
        // A duplicate message_stop gives no second usage chunk.
        json!({"type": "message_stop"}),
    ]);

    // One trailing usage chunk with `choices: []`, as OpenAI streaming and
    // LiteLLM expect, after the finish_reason chunk.
    let is_trailing_usage = |chunk: &Value| {
        at(chunk, "choices")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            && chunk.get("usage").is_some()
    };
    let trailing: Vec<usize> = (0..chunks.len())
        .filter(|&i| is_trailing_usage(&chunks[i]))
        .collect();
    let all: Vec<String> = chunks.iter().map(Value::to_string).collect();
    let [trailing_index] = trailing[..] else {
        panic!(
            "expected exactly 1 trailing usage chunk with empty choices array, got {}; chunks:\n{}",
            trailing.len(),
            all.join("\n")
        );
    };
    let finish_reason_index = chunks
        .iter()
        .rposition(|chunk| !text_at(chunk, "choices.0.finish_reason").is_empty());
    assert!(
        finish_reason_index < Some(trailing_index),
        "expected finish_reason chunk ({finish_reason_index:?}) before trailing usage chunk ({trailing_index})"
    );

    assert_eq!(
        chunks[trailing_index]["usage"],
        json!({
            "prompt_tokens": 170,
            "completion_tokens": 15,
            "total_tokens": 185,
            "prompt_tokens_details": {
                "cached_tokens": 50,
                "cached_creation_tokens": 20,
                "cache_write_tokens": 20,
            },
        })
    );
}

#[test]
fn non_stream_finish_reasons() {
    for (name, stop_reason, want) in [
        ("missing", "", "stop"),
        ("end_turn", "end_turn", "stop"),
        ("stop_sequence", "stop_sequence", "stop"),
        ("max_tokens", "max_tokens", "length"),
        ("refusal", "refusal", "content_filter"),
        ("sensitive", "sensitive", "content_filter"),
    ] {
        let line = data(&json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}}));
        let out = convert_claude_response_to_openai_chat_completions_non_stream(line.as_bytes());
        assert_eq!(
            text_at(&out, "choices.0.finish_reason"),
            want,
            "{name}: payload={out}"
        );
    }
}

// Ports TestConvertClaudeResponseToOpenAINonStream_NativeMessagesJSON.
#[test]
fn non_stream_native_messages_json() {
    for (name, content, stop_reason, finish_reason, tools) in [
        (
            "text",
            r#"[{"type":"text","text":"Hello "},{"type":"text","text":"world!"}]"#,
            "end_turn",
            "stop",
            false,
        ),
        (
            "tools",
            r#"[{"type":"text","text":"Hello world!"},{"type":"tool_use","id":"toolu_weather","name":"get_weather","input":{"city":"Paris","days":2}},{"type":"tool_use","id":"toolu_clock","name":"get_time","input":{}}]"#,
            "tool_use",
            "tool_calls",
            true,
        ),
        (
            "max_tokens",
            r#"[{"type":"text","text":"Hello world!"}]"#,
            "max_tokens",
            "length",
            false,
        ),
    ] {
        let raw = format!(
            r#"{{"id":"msg_native","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":{content},"stop_reason":"{stop_reason}","stop_sequence":null,"usage":{{"input_tokens":13,"cache_read_input_tokens":7,"cache_creation_input_tokens":3,"output_tokens":5}}}}"#
        );
        let out = convert_claude_response_to_openai_chat_completions_non_stream(raw.as_bytes());
        for (path, want) in [
            ("id", "msg_native"),
            ("object", "chat.completion"),
            ("model", "claude-sonnet-4-6"),
            ("choices.0.message.role", "assistant"),
            ("choices.0.message.content", "Hello world!"),
            ("choices.0.finish_reason", finish_reason),
        ] {
            assert_eq!(text_at(&out, path), want, "{name}: {path} in {out}");
        }
        assert_eq!(
            at(&out, "choices").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
        for (path, want) in [
            ("usage.prompt_tokens", 23),
            ("usage.completion_tokens", 5),
            ("usage.total_tokens", 28),
            ("usage.prompt_tokens_details.cached_tokens", 7),
            ("usage.prompt_tokens_details.cached_creation_tokens", 3),
        ] {
            assert_eq!(
                at(&out, path).and_then(Value::as_i64),
                Some(want),
                "{name}: {path} in {out}"
            );
        }
        let calls = at(&out, "choices.0.message.tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !tools {
            assert!(calls.is_empty(), "{name}: unexpected tool calls in {out}");
            continue;
        }
        let calls = Value::Array(calls);
        assert_eq!(calls.as_array().map(Vec::len), Some(2), "{name}: {out}");
        for (path, want) in [
            ("0.id", "toolu_weather"),
            ("0.type", "function"),
            ("0.function.name", "get_weather"),
            ("1.id", "toolu_clock"),
            ("1.type", "function"),
            ("1.function.name", "get_time"),
        ] {
            assert_eq!(text_at(&calls, path), want, "{name}: tool_calls.{path}");
        }
        let arguments: Value =
            serde_json::from_str(&text_at(&calls, "0.function.arguments")).expect("JSON");
        assert_eq!(arguments, json!({"city": "Paris", "days": 2}), "{name}");
        let empty: Value =
            serde_json::from_str(&text_at(&calls, "1.function.arguments")).expect("JSON");
        assert_eq!(empty, json!({}), "{name}");
    }
}
