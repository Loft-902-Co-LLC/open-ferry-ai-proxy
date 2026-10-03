//! Hand-written cases for the Chat Completions passthrough, and the Chat
//! Completions streams and whole responses that the translators from Chat
//! Completions to Claude and Responses read too.

use serde_json::{Map, Value, json};

use super::{Case, escaped};

const MODEL: &str = "gpt-4o";

/// The Chat Completions requests written for the Codex translators that are
/// JSON, and requests with every kind of `model`.
pub fn requests() -> Vec<Case> {
    let mut cases: Vec<Case> = super::chat::requests()
        .into_iter()
        .filter(|case| serde_json::from_str::<Value>(&case.request).is_ok())
        .map(|mut case| {
            // Their known differences are in fields the passthrough doesn't
            // read.
            case.known_difference = None;
            case.model = MODEL.to_owned();
            case
        })
        .collect();
    let messages = json!([{ "role": "user", "content": "Hi" }]);
    for (name, model) in [
        ("model-matches", json!(MODEL)),
        ("model-differs", json!("gpt-4o-mini")),
        ("model-differs-in-case", json!("GPT-4o")),
        ("model-padded", json!(" gpt-4o ")),
        ("model-empty", json!("")),
        ("model-number", json!(5)),
        ("model-null", Value::Null),
        ("model-array", json!([MODEL])),
        ("model-object", json!({ "name": MODEL })),
    ] {
        let request = json!({ "model": model, "messages": messages, "stream": true });
        cases.push(Case::new(
            format!("passthrough-{name}"),
            MODEL,
            request.to_string(),
        ));
    }
    cases.extend([
        Case::new(
            "passthrough-model-missing",
            MODEL,
            json!({ "messages": messages }).to_string(),
        ),
        Case::new(
            "passthrough-model-last",
            MODEL,
            json!({ "messages": messages, "temperature": 0.50, "model": "other" }).to_string(),
        ),
        Case::new(
            "passthrough-pretty",
            MODEL,
            serde_json::to_string_pretty(&json!({ "model": "other", "messages": messages }))
                .expect("a Value always serializes"),
        ),
        Case::new(
            "passthrough-escaped-model",
            MODEL,
            format!(r#"{{"model":"gpt{}4o","messages":[]}}"#, escaped('-')),
        ),
        Case::new(
            "passthrough-escaped-key",
            MODEL,
            format!(r#"{{"m{}del":"other","messages":[]}}"#, escaped('o')),
        ),
        Case::new(
            "passthrough-empty-model-name",
            "",
            json!({ "model": MODEL }).to_string(),
        ),
        Case::new("passthrough-scalar", MODEL, "5"),
        Case::new("passthrough-string", MODEL, r#""text""#),
        Case::new(
            "passthrough-number-text",
            MODEL,
            r#"{"n":1.50e+3,"model":"x"}"#,
        ),
    ]);
    cases
}

/// The streams of [`event_streams`], for the passthrough.
pub fn streams() -> Vec<Case> {
    event_streams()
        .into_iter()
        .map(|(name, lines)| Case::new(name, MODEL, "").with_events(lines))
        .collect()
}

/// The responses of [`bodies`], for the passthrough.
pub fn finals() -> Vec<Case> {
    bodies()
        .into_iter()
        .map(|(name, body)| Case::new(name, MODEL, "").with_events(vec![body]))
        .chain([Case::new("no-body", MODEL, "")])
        .collect()
}

// --- Streams ---

/// A chunk with the usual fields and these choices.
pub(super) fn chunk_with(choices: Value) -> Value {
    json!({ "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000, "model": MODEL, "choices": choices })
}

/// A chunk with one choice, carrying `delta`.
pub(super) fn chunk(delta: Value) -> Value {
    chunk_with(json!([{ "index": 0, "delta": delta, "finish_reason": null }]))
}

/// A chunk with text.
pub(super) fn text(text: &str) -> Value {
    chunk(json!({ "content": text }))
}

/// A chunk with reasoning, as `reasoning_content`.
pub(super) fn reasoning(text: &str) -> Value {
    chunk(json!({ "reasoning_content": text }))
}

/// The first chunk, with the assistant's role and no content.
pub(super) fn role() -> Value {
    chunk(json!({ "role": "assistant", "content": "" }))
}

/// A chunk with one tool call delta: its index, with its ID, name and
/// arguments where given.
pub(super) fn call(index: u64, id: Option<&str>, name: Option<&str>, arguments: &str) -> Value {
    let mut call = Map::new();
    call.insert("index".into(), index.into());
    if let Some(id) = id {
        call.insert("id".into(), id.into());
        call.insert("type".into(), "function".into());
    }
    let mut function = Map::new();
    if let Some(name) = name {
        function.insert("name".into(), name.into());
    }
    function.insert("arguments".into(), arguments.into());
    call.insert("function".into(), function.into());
    chunk(json!({ "tool_calls": [call] }))
}

/// A chunk that finishes the choice.
pub(super) fn finish(reason: &str) -> Value {
    chunk_with(json!([{ "index": 0, "delta": {}, "finish_reason": reason }]))
}

/// `chunk` with `usage`.
pub(super) fn with_usage(mut chunk: Value, usage: Value) -> Value {
    chunk["usage"] = usage;
    chunk
}

/// A chunk with only usage, as providers send it after the finish reason.
pub(super) fn usage_only(usage: Value) -> Value {
    with_usage(chunk_with(json!([])), usage)
}

/// Token counts with a matching total.
pub(super) fn usage(prompt: u64, completion: u64) -> Value {
    json!({ "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion })
}

pub(super) fn data(chunk: &Value) -> String {
    format!("data: {chunk}")
}

pub(super) const DONE: &str = "data: [DONE]";

/// Each chunk as a `data:` line, then `[DONE]`.
pub(super) fn lines(chunks: &[Value]) -> Vec<String> {
    chunks.iter().map(data).chain([DONE.to_owned()]).collect()
}

/// Chat Completions streams, as their lines. Calls name `get_weather` and
/// `search`.
pub(super) fn event_streams() -> Vec<(&'static str, Vec<String>)> {
    let weather = |index| call(index, Some("call_1"), Some("get_weather"), "");
    vec![
        (
            "text",
            lines(&[
                role(),
                text("Hello"),
                text(", world"),
                finish("stop"),
                usage_only(usage(10, 5)),
            ]),
        ),
        (
            "text-usage-with-finish",
            lines(&[text("Hi"), with_usage(finish("stop"), usage(3, 1))]),
        ),
        (
            "usage-before-finish",
            lines(&[text("Hi"), usage_only(usage(3, 1)), finish("stop")]),
        ),
        (
            "unicode-split",
            lines(&[text("caf"), text("é 🚀 日本"), text(""), finish("stop")]),
        ),
        (
            "reasoning-content",
            lines(&[
                role(),
                reasoning("Think"),
                reasoning("ing."),
                text("Answer"),
                finish("stop"),
            ]),
        ),
        (
            "reasoning-field",
            lines(&[
                chunk(json!({ "reasoning": "Thinking." })),
                text("Answer"),
                finish("stop"),
            ]),
        ),
        (
            "reasoning-details",
            lines(&[
                chunk(
                    json!({ "reasoning_details": [{ "type": "reasoning.text", "text": "Step one." }, { "type": "reasoning.summary", "summary": "Sum." }] }),
                ),
                text("Answer"),
                finish("stop"),
            ]),
        ),
        (
            "reasoning-only",
            lines(&[reasoning("Thinking only."), finish("stop")]),
        ),
        (
            "text-after-reasoning-after-text",
            lines(&[
                text("One."),
                reasoning("Hmm."),
                text("Two."),
                finish("stop"),
            ]),
        ),
        (
            "tool-call-whole",
            lines(&[
                role(),
                call(
                    0,
                    Some("call_1"),
                    Some("get_weather"),
                    r#"{"city":"Paris"}"#,
                ),
                with_usage(finish("tool_calls"), usage(20, 8)),
            ]),
        ),
        (
            "tool-call-split",
            lines(&[
                weather(0),
                call(0, None, None, r#"{"ci"#),
                call(0, None, None, r#"ty":"Pa"#),
                call(0, None, None, r#"ris"}"#),
                finish("tool_calls"),
                usage_only(usage(20, 8)),
            ]),
        ),
        (
            "tool-call-name-before-id",
            lines(&[
                call(0, None, Some("get_weather"), ""),
                call(0, Some("call_1"), None, r#"{"city":"Paris"}"#),
                finish("tool_calls"),
            ]),
        ),
        (
            "tool-call-no-id",
            lines(&[
                call(0, None, Some("get_weather"), r#"{"city":"Paris"}"#),
                finish("tool_calls"),
            ]),
        ),
        (
            "tool-call-no-name",
            lines(&[
                call(0, Some("call_1"), None, r#"{"city":"Paris"}"#),
                finish("tool_calls"),
            ]),
        ),
        (
            "tool-call-name-repeated",
            lines(&[
                weather(0),
                call(0, None, Some("get_weather"), r#"{"city":"Paris"}"#),
                finish("tool_calls"),
            ]),
        ),
        (
            "tool-call-no-index",
            lines(&[
                chunk(
                    json!({ "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{}" } }] }),
                ),
                finish("tool_calls"),
            ]),
        ),
        (
            "two-tool-calls-interleaved",
            lines(&[
                weather(0),
                call(1, Some("call_2"), Some("search"), ""),
                call(0, None, None, r#"{"city":"#),
                call(1, None, None, r#"{"q":"rust"}"#),
                call(0, None, None, r#""Paris"}"#),
                finish("tool_calls"),
            ]),
        ),
        (
            "two-tool-calls-one-chunk",
            lines(&[
                chunk(json!({ "tool_calls": [
                    { "index": 0, "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{}" } },
                    { "index": 1, "id": "call_2", "type": "function", "function": { "name": "search", "arguments": "{}" } }
                ] })),
                finish("tool_calls"),
            ]),
        ),
        (
            "text-then-tool-call",
            lines(&[
                text("Let me check."),
                call(0, Some("call_1"), Some("get_weather"), "{}"),
                finish("tool_calls"),
            ]),
        ),
        (
            "text-during-tool-call",
            lines(&[
                weather(0),
                text("Held."),
                reasoning("Held thought."),
                call(0, None, None, "{}"),
                finish("tool_calls"),
            ]),
        ),
        (
            "invalid-arguments",
            lines(&[
                call(0, Some("call_1"), Some("get_weather"), r#"{"city":"#),
                finish("tool_calls"),
            ]),
        ),
        (
            "empty-arguments",
            lines(&[weather(0), finish("tool_calls")]),
        ),
        (
            "array-arguments",
            lines(&[
                call(0, Some("call_1"), Some("get_weather"), "[1,2]"),
                finish("tool_calls"),
            ]),
        ),
        (
            "tool-call-finish-stop",
            lines(&[
                call(0, Some("call_1"), Some("get_weather"), "{}"),
                finish("stop"),
            ]),
        ),
        (
            "length",
            lines(&[text("Cut"), finish("length")]),
        ),
        (
            "content-filter",
            lines(&[text("Bad"), finish("content_filter")]),
        ),
        (
            "function-call-finish",
            lines(&[text("x"), finish("function_call")]),
        ),
        (
            "unknown-finish",
            lines(&[text("x"), finish("STOP")]),
        ),
        (
            "cached-usage",
            lines(&[
                text("Hi"),
                finish("stop"),
                usage_only(json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 120,
                    "prompt_tokens_details": { "cached_tokens": 30 },
                    "completion_tokens_details": { "reasoning_tokens": 5 }
                })),
            ]),
        ),
        (
            "cache-write-usage",
            lines(&[
                text("Hi"),
                finish("stop"),
                usage_only(json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "prompt_tokens_details": { "cached_tokens": 10, "cache_write_tokens": 40, "cache_creation_tokens": 7 }
                })),
            ]),
        ),
        (
            "loose-usage",
            lines(&[
                text("Hi"),
                finish("stop"),
                usage_only(
                    json!({ "prompt_tokens": "12", "completion_tokens": 2.5, "total_tokens": null }),
                ),
            ]),
        ),
        (
            "usage-twice",
            lines(&[
                with_usage(text("Hi"), usage(1, 1)),
                finish("stop"),
                usage_only(usage(10, 5)),
            ]),
        ),
        (
            "no-finish",
            lines(&[text("Hi")]),
        ),
        (
            "no-done",
            vec![data(&text("Hi")), data(&finish("stop"))],
        ),
        ("done-only", vec![DONE.to_owned()]),
        ("no-lines", Vec::new()),
        (
            "done-twice-then-more",
            vec![
                data(&text("Hi")),
                data(&finish("stop")),
                DONE.to_owned(),
                DONE.to_owned(),
                data(&text("late")),
            ],
        ),
        (
            "line-formats",
            vec![
                ": keep-alive".to_owned(),
                String::new(),
                "event: chunk".to_owned(),
                format!("data:{}", text("a")),
                format!("data:  {} \r", text("b")),
                text("bare").to_string(),
                " data: {}".to_owned(),
                "data:".to_owned(),
                data(&finish("stop")),
                "data:[DONE]".to_owned(),
            ],
        ),
        (
            "escaped-chunk",
            lines(&[
                serde_json::from_str(
                    &text("café 🚀")
                        .to_string()
                        .replace('é', &escaped('é'))
                        .replace('🚀', &escaped('🚀')),
                )
                .expect("escapes are JSON"),
                finish("stop"),
            ]),
        ),
        (
            "choices-object",
            lines(&[
                chunk_with(
                    json!({ "0": { "index": 0, "delta": { "content": "x" }, "finish_reason": null } }),
                ),
                finish("stop"),
            ]),
        ),
        (
            "two-choices",
            lines(&[
                chunk_with(json!([
                    { "index": 0, "delta": { "content": "first" }, "finish_reason": null },
                    { "index": 1, "delta": { "content": "second" }, "finish_reason": null }
                ])),
                chunk_with(json!([
                    { "index": 0, "delta": {}, "finish_reason": "stop" },
                    { "index": 1, "delta": {}, "finish_reason": "length" }
                ])),
            ]),
        ),
        (
            "second-choice-only",
            lines(&[
                chunk_with(
                    json!([{ "index": 1, "delta": { "content": "second" }, "finish_reason": null }]),
                ),
                finish("stop"),
            ]),
        ),
        (
            "finish-with-content",
            lines(&[chunk_with(
                json!([{ "index": 0, "delta": { "content": "last" }, "finish_reason": "stop" }]),
            )]),
        ),
        (
            "content-not-string",
            lines(&[
                chunk(json!({ "content": 5 })),
                chunk(json!({ "content": null })),
                chunk(json!({ "content": true })),
                finish("stop"),
            ]),
        ),
        (
            "error-event",
            vec![
                data(&text("Hi")),
                r#"data: {"error":{"message":"Rate limit reached","type":"rate_limit_error","code":429}}"#
                    .to_owned(),
            ],
        ),
        (
            "not-chunks",
            lines(&[
                json!({ "id": "chatcmpl-1", "object": "chat.completion", "choices": [{ "index": 0, "message": { "content": "whole" } }] }),
                json!({ "object": "list", "data": [] }),
                text("Hi"),
                finish("stop"),
            ]),
        ),
        (
            "no-choices",
            lines(&[
                json!({ "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": MODEL }),
                text("Hi"),
                finish("stop"),
            ]),
        ),
        (
            "fields-missing",
            lines(&[
                json!({ "choices": [{ "delta": { "content": "x" } }] }),
                json!({ "choices": [{ "finish_reason": "stop" }] }),
            ]),
        ),
        (
            "fields-of-other-types",
            lines(&[
                json!({ "id": 5, "object": "chat.completion.chunk", "created": "1700000000", "model": 7, "choices": [{ "index": "0", "delta": { "content": "x" } }] }),
                json!({ "id": 5, "created": 1.5e9, "choices": [{ "index": 0, "delta": {}, "finish_reason": 5 }] }),
            ]),
        ),
        (
            "ids-change",
            lines(&[
                text("a"),
                json!({ "id": "chatcmpl-2", "object": "chat.completion.chunk", "created": 1_700_000_001, "model": "gpt-4o-mini", "choices": [{ "index": 0, "delta": { "content": "b" }, "finish_reason": "stop" }] }),
            ]),
        ),
        (
            "malformed-line",
            vec![
                "data: {not json".to_owned(),
                data(&text("Hi")),
                "data: [1,2]".to_owned(),
                "data: \"text\"".to_owned(),
                data(&finish("stop")),
                DONE.to_owned(),
            ],
        ),
    ]
}

// --- Whole responses ---

/// A whole response with these choices.
pub(super) fn response_with(choices: Value, usage: Value) -> Value {
    json!({ "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000, "model": MODEL, "choices": choices, "usage": usage })
}

/// A whole response with one choice, carrying `message`.
pub(super) fn response(message: Value, finish_reason: &str) -> Value {
    response_with(
        json!([{ "index": 0, "message": message, "finish_reason": finish_reason }]),
        usage(10, 5),
    )
}

/// A whole tool call.
pub(super) fn whole_call(id: &str, name: &str, arguments: &str) -> Value {
    json!({ "id": id, "type": "function", "function": { "name": name, "arguments": arguments } })
}

/// Whole Chat Completions responses, as their text. Calls name `get_weather`
/// and `search`.
pub(super) fn bodies() -> Vec<(&'static str, String)> {
    let assistant = |content: Value| json!({ "role": "assistant", "content": content });
    let with = |mut message: Value, key: &str, value: Value| {
        message[key] = value;
        message
    };
    let calls = json!([
        whole_call("call_1", "get_weather", r#"{"city":"Paris"}"#),
        whole_call("call_2", "search", r#"{"q":"rust"}"#)
    ]);
    let bodies: Vec<(&'static str, Value)> = vec![
        ("text", response(assistant(json!("Hello")), "stop")),
        ("empty-text", response(assistant(json!("")), "stop")),
        ("null-content", response(assistant(Value::Null), "stop")),
        (
            "reasoning-content",
            response(
                with(
                    assistant(json!("Answer")),
                    "reasoning_content",
                    json!("Thinking."),
                ),
                "stop",
            ),
        ),
        (
            "reasoning-field",
            response(
                with(assistant(json!("Answer")), "reasoning", json!("Thinking.")),
                "stop",
            ),
        ),
        (
            "reasoning-details",
            response(
                with(
                    assistant(json!("Answer")),
                    "reasoning_details",
                    json!([{ "type": "reasoning.text", "text": "Step." }]),
                ),
                "stop",
            ),
        ),
        (
            "tool-calls",
            response(
                with(assistant(Value::Null), "tool_calls", calls.clone()),
                "tool_calls",
            ),
        ),
        (
            "text-and-tool-calls",
            response(
                with(assistant(json!("Checking.")), "tool_calls", calls.clone()),
                "tool_calls",
            ),
        ),
        (
            "tool-call-invalid-arguments",
            response(
                with(
                    assistant(Value::Null),
                    "tool_calls",
                    json!([whole_call("call_1", "get_weather", r#"{"city":"#)]),
                ),
                "tool_calls",
            ),
        ),
        (
            "tool-call-missing-fields",
            response(
                with(
                    assistant(Value::Null),
                    "tool_calls",
                    json!([{ "function": { "name": "get_weather" } }, { "id": "call_2", "function": { "arguments": "{}" } }]),
                ),
                "tool_calls",
            ),
        ),
        (
            "content-parts",
            response(
                assistant(json!([
                    { "type": "reasoning", "text": "Hmm." },
                    { "type": "text", "text": "One." },
                    { "type": "text", "text": "Two." },
                    { "type": "tool_calls", "tool_calls": [whole_call("call_1", "get_weather", "{}")] },
                    { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
                    { "type": "text", "text": "Three." }
                ])),
                "tool_calls",
            ),
        ),
        ("length", response(assistant(json!("Cut")), "length")),
        (
            "content-filter",
            response(assistant(json!("Bad")), "content_filter"),
        ),
        ("unknown-finish", response(assistant(json!("x")), "STOP")),
        (
            "no-finish",
            response_with(
                json!([{ "index": 0, "message": { "role": "assistant", "content": "x" } }]),
                usage(1, 1),
            ),
        ),
        (
            "cached-usage",
            response_with(
                json!([{ "index": 0, "message": { "role": "assistant", "content": "x" }, "finish_reason": "stop" }]),
                json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 120,
                    "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 4 },
                    "completion_tokens_details": { "reasoning_tokens": 5 }
                }),
            ),
        ),
        (
            "no-usage",
            json!({ "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000, "model": MODEL, "choices": [{ "index": 0, "message": { "content": "x" }, "finish_reason": "stop" }] }),
        ),
        (
            "two-choices",
            response_with(
                json!([
                    { "index": 0, "message": { "content": "first" }, "finish_reason": "stop" },
                    { "index": 1, "message": { "content": "second" }, "finish_reason": "length" }
                ]),
                usage(1, 2),
            ),
        ),
        (
            "choices-object",
            response_with(
                json!({ "0": { "index": 0, "message": { "content": "x" }, "finish_reason": "stop" } }),
                usage(1, 2),
            ),
        ),
        ("empty-choices", response_with(json!([]), usage(1, 0))),
        (
            "no-id-or-created",
            json!({ "object": "chat.completion", "model": MODEL, "choices": [{ "index": 0, "message": { "content": "x" }, "finish_reason": "stop" }] }),
        ),
        (
            "fields-of-other-types",
            json!({ "id": 5, "created": "1700000000", "model": 7, "choices": [{ "index": 0, "message": { "content": 5 }, "finish_reason": 5 }], "usage": { "prompt_tokens": "3" } }),
        ),
        ("empty-object", json!({})),
        ("array", json!([])),
        ("null", Value::Null),
    ];
    let mut bodies: Vec<(&'static str, String)> = bodies
        .into_iter()
        .map(|(name, body)| (name, body.to_string()))
        .collect();
    bodies.extend([
        (
            "pretty",
            serde_json::to_string_pretty(&response(assistant(json!("Hello")), "stop"))
                .expect("a Value always serializes"),
        ),
        (
            "escaped",
            response(assistant(json!("café 🚀")), "stop")
                .to_string()
                .replace('é', &escaped('é'))
                .replace('🚀', &escaped('🚀')),
        ),
        ("empty-body", String::new()),
        ("not-json", "{not json".to_owned()),
        (
            "data-line",
            format!("data: {}", response(assistant(json!("x")), "stop")),
        ),
    ]);
    bodies
}
