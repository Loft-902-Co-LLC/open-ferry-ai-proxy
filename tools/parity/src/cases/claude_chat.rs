//! Hand-written cases for the Chat Completions → Claude translators.

use serde_json::{Value, json};

use super::Case;

/// Chat Completions requests the generator is unlikely to build in one piece.
/// Each runs in both the plain and compatibility suites.
pub fn requests() -> Vec<Case> {
    let tools = json!([
        { "type": "function", "function": { "name": "get_weather", "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } } },
        { "type": "function", "function": { "name": "search", "description": "Search the web." } },
        { "type": "function", "function": { "name": "mcp.server:read file" } }
    ]);
    let with_tools = |choice: Value, parallel: Value| {
        let mut request = json!({
            "messages": [{ "role": "user", "content": "Go." }],
            "tools": tools,
            "tool_choice": choice,
            "parallel_tool_calls": parallel
        });
        let fields = request.as_object_mut().expect("an object");
        fields.retain(|_, value| !value.is_null());
        request.to_string()
    };

    let mut cases = vec![
        Case::new(
            "typical-sdk-request",
            "claude-sonnet-4-6",
            json!({
                "model": "claude-sonnet-4-6",
                "messages": [
                    { "role": "system", "content": "You are terse." },
                    { "role": "user", "content": "Weather in Paris?" },
                    { "role": "assistant", "content": null, "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call_1", "content": "18°C" },
                    { "role": "assistant", "content": "It is 18°C." },
                    { "role": "user", "content": "Thanks!" }
                ],
                "tools": tools,
                "tool_choice": "auto",
                "max_tokens": 1024,
                "temperature": 0.2,
                "stream": true,
                "stream_options": { "include_usage": true }
            })
            .to_string(),
        ),
        Case::new(
            "system-cache-control",
            "claude-opus-4-6",
            json!({
                "messages": [
                    { "role": "system", "content": [
                        { "type": "text", "text": "Rules.", "cache_control": { "type": "ephemeral" } },
                        { "type": "text", "text": "More rules." }
                    ], "cache_control": { "type": "ephemeral", "ttl": "1h" } },
                    { "role": "developer", "content": "Be brief.", "cache_control": { "type": "ephemeral" } },
                    { "role": "system", "content": [{ "type": "text", "text": "Last." }], "cache_control": { "type": "persistent" } },
                    { "role": "user", "content": [
                        { "type": "text", "text": "One." },
                        { "type": "text", "text": "Two.", "cache_control": { "type": "ephemeral", "ttl": "5m" } }
                    ], "cache_control": { "type": "ephemeral", "ttl": "1h" } },
                    { "role": "user", "content": "Plain.", "cache_control": { "type": "ephemeral" } }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "tool-result-cache-control",
            "claude-opus-4-6",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "search", "arguments": "{}" } },
                        { "id": "call_2", "type": "function", "function": { "name": "search", "arguments": "{}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call_1", "content": [
                        { "type": "text", "text": "a" },
                        { "type": "text", "text": "b", "cache_control": { "type": "ephemeral", "ttl": "1h" } }
                    ], "cache_control": { "type": "ephemeral" } },
                    { "role": "tool", "tool_call_id": "call_2", "content": "c", "cache_control": { "type": "ephemeral" } }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "tool-cache-control-and-strict",
            "claude-opus-4-6",
            json!({
                "tools": [
                    { "type": "function", "cache_control": { "type": "ephemeral" }, "strict": true, "function": { "name": "a", "parameters": { "type": "object" } } },
                    { "type": "function", "function": { "name": "b", "cache_control": { "type": "ephemeral", "ttl": "1h" }, "strict": false, "parametersJsonSchema": { "type": "object", "properties": { "x": { "type": "integer" } } } } },
                    { "type": "function", "strict": "true", "function": { "name": "c" } },
                    { "type": "custom", "name": "shell" }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "client-metadata-user-id",
            "claude-opus-4-6",
            json!({ "metadata": { "user_id": " user-1 " }, "user": "other", "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "openai-user-field",
            "claude-opus-4-6",
            json!({ "metadata": { "user_id": "  " }, "user": "abc ", "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "no-client-user-id",
            "claude-opus-4-6",
            json!({ "metadata": { "user_id": 5 }, "user": "", "prompt_cache_key": "k", "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "allowed-tools-required",
            "claude-opus-4-6",
            with_tools(
                json!({ "type": "allowed_tools", "allowed_tools": { "mode": "required", "tools": [
                    { "type": "function", "function": { "name": "get_weather" } },
                    { "type": "function", "name": " mcp.server:read file " }
                ]}}),
                Value::Null,
            ),
        ),
        Case::new(
            "allowed-tools-none-mode",
            "claude-opus-4-6",
            with_tools(
                json!({ "type": "allowed_tools", "mode": "None", "tools": [{ "name": "search" }] }),
                json!(false),
            ),
        ),
        Case::new(
            "allowed-tools-unknown-names",
            "claude-opus-4-6",
            with_tools(
                json!({ "type": "allowed_tools", "allowed_tools": { "tools": [{ "name": "missing" }] } }),
                json!(false),
            ),
        ),
        Case::new(
            "allowed-tools-single-object",
            "claude-opus-4-6",
            with_tools(
                json!({ "type": "allowed_tools", "allowed_tools": { "mode": "auto", "tools": { "type": "function", "function": { "name": "search" } } } }),
                Value::Null,
            ),
        ),
        Case::new(
            "parallel-off-with-choice-none",
            "claude-opus-4-6",
            with_tools(json!("none"), json!(false)),
        ),
        Case::new(
            "parallel-off-with-named-tool",
            "claude-opus-4-6",
            with_tools(
                json!({ "type": "function", "function": { "name": "mcp.server:read file" } }),
                json!(false),
            ),
        ),
        Case::new(
            "parallel-off-without-choice",
            "claude-opus-4-6",
            with_tools(Value::Null, json!(false)),
        ),
        Case::new(
            "parallel-off-without-tools",
            "claude-opus-4-6",
            json!({ "parallel_tool_calls": false, "tool_choice": "required", "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "parallel-off-as-string",
            "claude-opus-4-6",
            with_tools(json!("auto"), json!("false")),
        ),
        Case::new(
            "response-format-json-schema",
            "claude-opus-4-6",
            json!({
                "messages": [{ "role": "system", "content": "Answer in JSON." }, { "role": "user", "content": "Weather?" }],
                "response_format": { "type": "json_schema", "json_schema": {
                    "name": "weather",
                    "description": "The weather.",
                    "schema": { "type": "object", "properties": { "temp": { "type": "number" } }, "required": ["temp"] }
                }}
            })
            .to_string(),
        ),
        Case::new(
            "response-format-only",
            "claude-opus-4-6",
            json!({ "response_format": { "type": "json_object" } }).to_string(),
        ),
        Case::new(
            "system-only",
            "claude-opus-4-6",
            json!({ "messages": [{ "role": "system", "content": "Say hi." }] }).to_string(),
        ),
        Case::new(
            "assistant-reasoning-content",
            "claude-opus-4-6",
            json!({
                "messages": [
                    { "role": "user", "content": "Plan it." },
                    { "role": "assistant", "reasoning_content": "Think first.", "content": "Plan.", "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "search", "arguments": "{\"q\":\"x\"}" } }
                    ]},
                    { "role": "assistant", "reasoning_content": " ", "content": "Blank reasoning." },
                    { "role": "tool", "tool_call_id": "call_1", "content": "found" }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "duplicate-tool-results",
            "claude-opus-4-6",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "id": "call 1", "type": "function", "function": { "name": "search", "arguments": "{}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call 1", "content": "first" },
                    { "role": "tool", "tool_call_id": "", "content": "no id" },
                    { "role": "tool", "tool_call_id": "call 1", "content": "second" },
                    { "role": "user", "content": "next" }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "tool-calls-without-ids",
            "claude-opus-4-6",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "type": "function", "function": { "name": "search", "arguments": "{\"q\":1}" } },
                        { "id": "", "type": "function", "function": { "name": "search", "arguments": "[1]" } },
                        { "id": "call_3", "type": "custom", "custom": { "name": "shell", "input": "ls" } }
                    ]}
                ]
            })
            .to_string(),
        ),
        Case::new(
            "images-and-files",
            "claude-opus-4-6",
            json!({
                "messages": [{ "role": "user", "content": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" } },
                    { "type": "image_url", "image_url": { "url": "https://example.com/cat.png", "detail": "high" } },
                    { "type": "image_url", "image_url": { "url": "data:;base64,AAAA" } },
                    { "type": "image_url", "image_url": "https://example.com/string.png" },
                    { "type": "file", "file": { "file_data": "data:application/pdf;base64,JVBERi0=", "filename": "a.pdf" } },
                    { "type": "file", "file": { "file_data": "data:text/plain;charset=utf-8;base64,aGk=" } },
                    { "type": "file", "file": { "file_data": "data:application/pdf,JVBE;Ri0=" } },
                    { "type": "file", "file": { "file_id": "file_123" } },
                    { "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": "wav" } }
                ]}]
            })
            .to_string(),
        ),
        Case::new(
            "sampling-and-limits",
            "claude-opus-4-6",
            json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "top_p": "0.25",
                "stop": ["\n\nHuman:", 5, null],
                "max_completion_tokens": 2048,
                "temperature": 0
            })
            .to_string(),
        ),
        Case::new(
            "stop-as-string-and-max-tokens-first",
            "claude-opus-4-6",
            json!({
                "messages": [{ "role": "user", "content": "hi" }],
                "top_p": 1,
                "stop": "END",
                "max_tokens": "100",
                "max_completion_tokens": 2048
            })
            .to_string(),
        ),
        Case::new(
            "tool-content-forms",
            "claude-opus-4-6",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "id": "a", "type": "function", "function": { "name": "search", "arguments": "{}" } },
                        { "id": "b", "type": "function", "function": { "name": "search", "arguments": "{}" } },
                        { "id": "c", "type": "function", "function": { "name": "search", "arguments": "{}" } },
                        { "id": "d", "type": "function", "function": { "name": "search", "arguments": "{}" } },
                        { "id": "e", "type": "function", "function": { "name": "search", "arguments": "{}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "a", "content": [
                        "plain",
                        { "type": "text", "text": "text" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" } }
                    ]},
                    { "role": "tool", "tool_call_id": "b", "content": { "type": "text", "text": "object" } },
                    { "role": "tool", "tool_call_id": "c", "content": { "result": 1.50 } },
                    { "role": "tool", "tool_call_id": "d", "content": [{ "type": "audio" }] },
                    { "role": "tool", "tool_call_id": "e", "content": 1.50 }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "summary-shown-without-effort",
            "claude-opus-4-6",
            json!({ "reasoning": { "summary": "auto" }, "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "summary-shown-on-budget-model",
            "claude-sonnet-4-5-20250929",
            json!({ "include_reasoning": true, "max_tokens": 4096, "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "summary-hidden-with-effort",
            "claude-opus-4-6",
            json!({ "reasoning_effort": "high", "reasoning": { "exclude": true }, "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ),
        Case::new(
            "summary-hidden-thinking-off",
            "claude-opus-4-6",
            json!({ "reasoning_effort": "none", "extra_body": { "google": { "thinking_config": { "include_thoughts": false } } } })
                .to_string(),
        ),
        Case::new(
            "top-p-out-of-range",
            "claude-opus-4-6",
            r#"{"top_p":1e400,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .known_difference("upstream writes top_p as +Inf, which isn't JSON; we leave it out"),
        Case::new(
            "huge-max-tokens-with-summary",
            "claude-sonnet-4-5-20250929",
            r#"{"max_tokens":1e30,"include_reasoning":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .known_difference("Go's int64(1e30) depends on the CPU; Rust saturates, so a budget fits"),
    ];

    // Effort levels, on models with levels, with budgets only, and unknown.
    let efforts = [
        ("none", "none"),
        ("auto", "auto"),
        ("minimal", "minimal"),
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("xhigh", "xhigh"),
        ("max", "max"),
        ("padded", " High "),
        ("unknown", "ultra"),
    ];
    for model in [
        "claude-opus-4-6",
        "claude-sonnet-4-5-20250929",
        "claude-3-5-haiku-20241022",
    ] {
        for (name, effort) in efforts {
            cases.push(Case::new(
                format!("effort-{name}-{model}"),
                model,
                json!({ "reasoning_effort": effort, "max_tokens": 8192, "messages": [{ "role": "user", "content": "hi" }] })
                    .to_string(),
            ));
        }
    }
    cases
}

/// A message start, with usage, as Claude sends it.
fn message_start(usage: Value) -> Value {
    json!({ "type": "message_start", "message": {
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
        "content": [], "stop_reason": null, "usage": usage
    }})
}

fn block_start(index: u64, block: Value) -> Value {
    json!({ "type": "content_block_start", "index": index, "content_block": block })
}

fn block_delta(index: u64, delta: Value) -> Value {
    json!({ "type": "content_block_delta", "index": index, "delta": delta })
}

fn block_stop(index: u64) -> Value {
    json!({ "type": "content_block_stop", "index": index })
}

fn message_delta(stop_reason: &str, usage: Value) -> Value {
    json!({ "type": "message_delta", "delta": { "stop_reason": stop_reason, "stop_sequence": null }, "usage": usage })
}

/// Named Claude event streams.
fn event_streams() -> Vec<(&'static str, Vec<Value>)> {
    let stop = json!({ "type": "message_stop" });
    vec![
        (
            "text",
            vec![
                message_start(json!({ "input_tokens": 10, "output_tokens": 1 })),
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "Hel" })),
                json!({ "type": "ping" }),
                block_delta(0, json!({ "type": "text_delta", "text": "lo 🚀" })),
                block_stop(0),
                message_delta("end_turn", json!({ "output_tokens": 5 })),
                stop.clone(),
            ],
        ),
        (
            "tool-use",
            vec![
                message_start(json!({ "input_tokens": 10, "output_tokens": 1 })),
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "Checking." })),
                block_stop(0),
                block_start(
                    1,
                    json!({ "type": "tool_use", "id": "toolu_01A", "name": "get_weather", "input": {} }),
                ),
                block_delta(
                    1,
                    json!({ "type": "input_json_delta", "partial_json": "{\"city\":" }),
                ),
                block_delta(
                    1,
                    json!({ "type": "input_json_delta", "partial_json": "\"Paris\"}" }),
                ),
                block_stop(1),
                block_start(
                    2,
                    json!({ "type": "tool_use", "id": "toolu_01B", "name": "search", "input": {} }),
                ),
                block_stop(2),
                message_delta("tool_use", json!({ "output_tokens": 40 })),
                stop.clone(),
            ],
        ),
        (
            "interleaved-tools",
            vec![
                message_start(json!({ "input_tokens": 3 })),
                block_start(
                    0,
                    json!({ "type": "tool_use", "id": "toolu_01A", "name": "a", "input": {} }),
                ),
                block_start(
                    1,
                    json!({ "type": "tool_use", "id": "toolu_01B", "name": "b", "input": {} }),
                ),
                block_delta(
                    1,
                    json!({ "type": "input_json_delta", "partial_json": "{\"b\":1}" }),
                ),
                block_delta(
                    0,
                    json!({ "type": "input_json_delta", "partial_json": "{\"a\":1}" }),
                ),
                block_stop(1),
                block_stop(0),
                message_delta("tool_use", json!({ "output_tokens": 9 })),
                stop.clone(),
            ],
        ),
        (
            "thinking",
            vec![
                message_start(json!({ "input_tokens": 10, "output_tokens": 1 })),
                block_start(0, json!({ "type": "thinking", "thinking": "" })),
                block_delta(
                    0,
                    json!({ "type": "thinking_delta", "thinking": "Let me think." }),
                ),
                block_delta(
                    0,
                    json!({ "type": "signature_delta", "signature": "EqQBCkYIBxgCKkA=" }),
                ),
                block_stop(0),
                block_start(
                    1,
                    json!({ "type": "redacted_thinking", "data": "EmwKAhgB" }),
                ),
                block_stop(1),
                block_start(2, json!({ "type": "text", "text": "" })),
                block_delta(2, json!({ "type": "text_delta", "text": "Done." })),
                block_stop(2),
                message_delta("max_tokens", json!({ "output_tokens": 20 })),
                stop.clone(),
            ],
        ),
        (
            "cached-usage",
            vec![
                message_start(json!({
                    "input_tokens": 44, "output_tokens": 1,
                    "cache_creation_input_tokens": 31, "cache_read_input_tokens": 22000
                })),
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "ok" })),
                block_stop(0),
                message_delta(
                    "end_turn",
                    json!({ "input_tokens": 44, "output_tokens": 4, "cache_read_input_tokens": 22000 }),
                ),
                stop.clone(),
            ],
        ),
        (
            "refusal",
            vec![
                message_start(json!({ "input_tokens": 1 })),
                message_delta("refusal", json!({ "output_tokens": 0 })),
                stop.clone(),
            ],
        ),
        (
            "error",
            vec![
                message_start(json!({ "input_tokens": 1 })),
                json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
            ],
        ),
        (
            "no-message-start",
            vec![
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "orphan" })),
                block_stop(0),
                message_delta("end_turn", json!({ "output_tokens": 1 })),
                stop,
            ],
        ),
    ]
}

/// Claude event streams, one `data:` line per event, plus lines that aren't
/// events.
pub fn streams() -> Vec<Case> {
    let mut cases: Vec<Case> = event_streams()
        .into_iter()
        .map(|(name, events)| {
            let lines = events
                .iter()
                .map(|event| format!("data: {event}"))
                .collect();
            Case::new(name, "claude-sonnet-4-6", "").with_events(lines)
        })
        .collect();
    cases.push(
        Case::new("sse-framing", "claude-sonnet-4-6", "").with_events(vec![
            "event: message_start".to_owned(),
            format!("data:{}", message_start(json!({ "input_tokens": 1 }))),
            String::new(),
            ": keep-alive".to_owned(),
            format!("  data: {}", block_start(0, json!({ "type": "text" }))),
            format!(
                "data: {} \r",
                block_delta(0, json!({ "type": "text_delta", "text": "x" }))
            ),
            block_delta(0, json!({ "type": "text_delta", "text": "bare" })).to_string(),
            "data: [DONE]".to_owned(),
            "data: {not json".to_owned(),
        ]),
    );
    cases
}

/// The same streams as SSE bodies, for the non-streaming translator.
pub fn finals() -> Vec<Case> {
    event_streams()
        .into_iter()
        .map(|(name, events)| {
            let body: String = events
                .iter()
                .map(|event| {
                    let kind = event["type"].as_str().unwrap_or_default();
                    format!("event: {kind}\ndata: {event}\n\n")
                })
                .collect();
            Case::new(name, "claude-sonnet-4-6", "").with_events(vec![body])
        })
        .chain([
            Case::new("empty-body", "claude-sonnet-4-6", "").with_events(vec![String::new()]),
            Case::new("no-body", "claude-sonnet-4-6", ""),
        ])
        .collect()
}
