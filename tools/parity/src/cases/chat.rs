//! Hand-written cases for the Chat Completions translators.

use serde_json::{Value, json};

use super::{Case, escaped};

/// A name over Codex's 64-byte limit, which the request translator shortens.
const LONG_NAME: &str = "mcp__a_server_with_a_name_long_enough_to_need_shortening__search_files";

/// Chat Completions requests the generator is unlikely to build in one piece.
pub fn requests() -> Vec<Case> {
    let mut cases = vec![
        Case::new(
            "typical-sdk-request",
            "gpt-5",
            json!({
                "model": "gpt-5",
                "messages": [
                    { "role": "system", "content": "You are terse." },
                    { "role": "user", "content": "Weather in Paris?" },
                    { "role": "assistant", "content": null, "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call_1", "content": "18°C" },
                    { "role": "assistant", "content": "It is 18°C." }
                ],
                "tools": [{ "type": "function", "function": {
                    "name": "get_weather",
                    "description": "Look up the weather.",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] },
                    "strict": true
                }}],
                "tool_choice": "auto",
                "stream": true,
                "stream_options": { "include_usage": true }
            })
            .to_string(),
        ),
        Case::new(
            "apply-patch-as-function",
            "gpt-5",
            json!({
                "tools": [{ "type": "custom", "name": "apply_patch" }],
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\\n\"}" } },
                        { "id": "call_2", "type": "function", "function": { "name": "apply_patch", "arguments": "{\"input\":5}" } },
                        { "id": "call_3", "type": "custom", "custom": { "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call_3", "content": "Done!" },
                    { "role": "tool", "tool_call_id": "call_1", "content": "Done!" },
                    { "role": "tool", "tool_call_id": "call_2", "content": [{ "type": "text", "text": "Failed." }] }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "apply-patch-declared-twice",
            "gpt-5",
            json!({
                "tools": [
                    { "type": "function", "function": { "name": "apply_patch" } },
                    { "type": "custom", "name": "apply_patch" }
                ],
                "tool_choice": { "type": "function", "function": { "name": "apply_patch" } },
                "messages": [{ "role": "assistant", "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "apply_patch", "arguments": "{\"input\":\"x\"}" } }
                ]}]
            })
            .to_string(),
        ),
        Case::new(
            "custom-only-tool-choice",
            "gpt-5",
            json!({
                "tools": [{ "type": "custom", "name": "shell" }, { "type": "function", "function": { "name": "f" } }],
                "tool_choice": { "type": "function", "function": { "name": "shell" } }
            })
            .to_string(),
        ),
        Case::new(
            "long-and-colliding-names",
            "gpt-5",
            json!({
                "tools": [
                    { "type": "function", "function": { "name": LONG_NAME } },
                    { "type": "function", "function": { "name": "mcp__another_server_with_a_long_name_for_shortening_purposes__search_files" } },
                    { "type": "function", "function": { "name": "a".repeat(70) } },
                    { "type": "function", "function": { "name": format!("{}b", "a".repeat(70)) } },
                    { "type": "function", "function": { "name": "outil météo" } },
                    { "type": "custom", "name": "mcp.server:search tool" }
                ],
                "tool_choice": { "type": "function", "function": { "name": LONG_NAME } },
                "messages": [{ "role": "assistant", "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": LONG_NAME, "arguments": "{}" } },
                    { "id": "call_2", "type": "custom", "custom": { "name": "mcp.server:search tool", "input": "q" } }
                ]}]
            })
            .to_string(),
        ),
        Case::new(
            "tool-call-ids",
            "gpt-5",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "f", "arguments": "{}" } },
                        { "id": "call_1", "type": "function", "function": { "name": "g", "arguments": "{}" } },
                        { "type": "function", "function": { "name": "h", "arguments": "{}" } },
                        { "id": "", "type": "function", "function": { "name": "i", "arguments": "{}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call_1", "content": "first" },
                    { "role": "tool", "content": "no id" },
                    { "role": "tool", "tool_call_id": "call_1", "content": "second" },
                    { "role": "tool", "tool_call_id": "call_missing_0_2", "content": "generated id" },
                    { "role": "tool", "tool_call_id": "call_unknown", "content": "unknown" }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "tool-output-image-as-string",
            "gpt-5",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "screenshot", "arguments": "{}" } },
                        { "id": "call_2", "type": "function", "function": { "name": "screenshot", "arguments": "{}" } },
                        { "id": "call_3", "type": "function", "function": { "name": "screenshot", "arguments": "{}" } }
                    ]},
                    { "role": "tool", "tool_call_id": "call_1", "content": json!([
                        { "type": "text", "text": "Here:" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=", "detail": "low" } }
                    ]).to_string() },
                    { "role": "tool", "tool_call_id": "call_2", "content": "[{\"type\":\"text\",\"text\":\"no image\"}]" },
                    { "role": "tool", "tool_call_id": "call_3", "content": [
                        { "type": "input_image", "image_url": "https://example.com/a.png", "file_id": "file_1" },
                        { "type": "file", "file": { "file_id": "file_2", "filename": "a.pdf" } },
                        { "type": "audio", "data": "UklGRg==" }
                    ]}
                ]
            })
            .to_string(),
        ),
        Case::new(
            "tool-output-image-in-malformed-json",
            "gpt-5",
            json!({
                "messages": [
                    { "role": "assistant", "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "f", "arguments": "{}" } }] },
                    { "role": "tool", "tool_call_id": "call_1", "content": "[{\"type\":\"image_url\",\"image_url\":{\"url\":\"https://example.com/a.png\"}}" }
                ]
            })
            .to_string(),
        )
        .known_difference("gjson reads what it can from malformed JSON"),
        Case::new(
            "user-parts",
            "gpt-5",
            json!({
                "messages": [{ "role": "user", "content": [
                    { "type": "text", "text": "Look:" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/cat.png", "detail": "high" } },
                    { "type": "image_url", "image_url": "https://example.com/bare.png" },
                    { "type": "file", "file": { "file_data": "data:application/pdf;base64,JVBERi0=", "filename": "a.pdf" } },
                    { "type": "file", "file": { "file_id": "file_1" } },
                    { "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": "wav" } },
                    { "type": "refusal", "refusal": "No." },
                    "bare string"
                ]}]
            })
            .to_string(),
        ),
        Case::new(
            "roles",
            "gpt-5",
            json!({
                "messages": [
                    { "role": "system", "content": [{ "type": "text", "text": "sys" }] },
                    { "role": "developer", "content": "dev" },
                    { "role": "System", "content": "mixed case" },
                    { "role": "function", "name": "f", "content": "legacy" },
                    { "content": "no role" },
                    { "role": "assistant", "content": [{ "type": "text", "text": "parts" }] },
                    { "role": "assistant", "content": "" },
                    "hi",
                    5
                ]
            })
            .to_string(),
        ),
        Case::new(
            "response-format-and-verbosity",
            "gpt-5",
            json!({
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "answer",
                        "strict": true,
                        "description": "not copied",
                        "schema": { "type": "object", "properties": { "n": { "type": "number", "minimum": 1.50 } } }
                    }
                },
                "text": { "verbosity": "low" }
            })
            .to_string(),
        ),
        Case::new(
            "verbosity-without-format",
            "gpt-5",
            r#"{"text":{"verbosity":"high"},"response_format":{"type":"json_object"}}"#,
        ),
        Case::new(
            "builtin-tools",
            "gpt-5",
            json!({
                "tools": [
                    { "type": "web_search" },
                    { "type": "image_generation", "output_format": "png" },
                    { "type": "" },
                    { "type": 5 },
                    "tool",
                    { "function": { "name": "untyped" } }
                ],
                "tool_choice": { "type": "web_search" }
            })
            .to_string(),
        ),
        Case::new(
            "escaped-text",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":"café / 🚀"}],"tools":[{"type":"function","function":{"name":"outil_météo"}}]}"#
                .replace('é', &escaped('é'))
                .replace('/', "\\/"),
        ),
        Case::new(
            "huge-numbers",
            "gpt-5",
            r#"{"reasoning_effort":123456789012345678901234567890,"tools":[{"type":"function","function":{"name":"f","parameters":{"minimum":1e400}}}]}"#,
        ),
        Case::new("effort-beyond-f64", "gpt-5", r#"{"reasoning_effort":1e400}"#)
            .known_difference("Go writes a number too large for f64 as +Inf, which isn't JSON"),
        Case::new("empty-object", "gpt-5", "{}"),
        Case::new("array-body", "gpt-5", "[]"),
        Case::new("null-body", "gpt-5", "null"),
        Case::new("string-body", "gpt-5", r#""hi""#),
        Case::new("number-body", "gpt-5", "5"),
        Case::new(
            "duplicate-keys",
            "gpt-5",
            r#"{"service_tier":"fast","service_tier":"flex"}"#,
        )
        .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
    ];
    for (index, tier) in [
        json!("priority"),
        json!("fast"),
        json!(" FAST "),
        json!("ultrafast"),
        json!("UltraFast"),
        json!("flex"),
        json!(""),
        json!(5),
        Value::Null,
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(Case::new(
            format!("service-tier-{index}"),
            "gpt-5",
            json!({ "service_tier": tier }).to_string(),
        ));
    }
    for (index, effort) in [
        json!("high"),
        json!(" Low "),
        json!(""),
        json!(5),
        json!(1.50),
        json!({ "b": 1, "a": 2 }),
        Value::Null,
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(Case::new(
            format!("reasoning-effort-{index}"),
            "gpt-5",
            json!({ "reasoning_effort": effort }).to_string(),
        ));
    }
    cases
}

fn data(event: &Value) -> String {
    format!("data: {event}")
}

fn created(fields: Value) -> Value {
    let mut response = json!({ "id": "resp_1", "object": "response", "created_at": 1_700_000_000, "model": "gpt-5", "status": "in_progress", "output": [] });
    if let (Value::Object(response), Value::Object(fields)) = (&mut response, fields) {
        response.extend(fields);
    }
    json!({ "type": "response.created", "response": response })
}

fn completed(output: Value) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 1_700_000_000,
            "model": "gpt-5",
            "status": "completed",
            "output": output,
            "usage": {
                "input_tokens": 10,
                "input_tokens_details": { "cached_tokens": 4 },
                "output_tokens": 5,
                "output_tokens_details": { "reasoning_tokens": 2 },
                "total_tokens": 15
            }
        }
    })
}

fn stream(name: &str, request: &Value, events: &[Value]) -> Case {
    Case::new(name, "gpt-5", request.to_string()).with_events(events.iter().map(data).collect())
}

fn apply_patch_events(namespace: Option<&str>) -> Vec<Value> {
    let mut item = json!({ "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "apply_patch", "input": "", "status": "in_progress" });
    if let Some(namespace) = namespace {
        item["namespace"] = namespace.into();
    }
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+héllo\n*** End Patch\n";
    let mut done = item.clone();
    done["input"] = patch.into();
    done["status"] = "completed".into();
    vec![
        created(json!({})),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": item }),
        json!({ "type": "response.custom_tool_call_input.delta", "output_index": 0, "item_id": "ctc_1", "delta": "*** Begin Patch\n*** Add File: a.txt\n+h" }),
        json!({ "type": "response.custom_tool_call_input.delta", "output_index": 0, "item_id": "ctc_1", "delta": "éllo\n*** End Patch\n" }),
        json!({ "type": "response.custom_tool_call_input.done", "output_index": 0, "item_id": "ctc_1", "input": patch }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": done }),
        completed(json!([done])),
    ]
}

fn image_events(format: &str) -> Vec<Value> {
    let partial = |index: u64, b64: &str| json!({ "type": "response.image_generation_call.partial_image", "output_index": 0, "item_id": "ig_1", "partial_image_index": index, "partial_image_b64": b64, "output_format": format });
    let done = json!({ "type": "image_generation_call", "id": "ig_1", "status": "completed", "result": "AAAB", "output_format": format });
    vec![
        created(json!({})),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "image_generation_call", "id": "ig_1", "status": "in_progress" } }),
        partial(0, "AAAA"),
        partial(1, "AAAA"),
        partial(2, "AAAB"),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": done }),
        completed(json!([done])),
    ]
}

/// Codex event streams for the Chat Completions streaming translator.
pub fn streams() -> Vec<Case> {
    let patch_tool = json!({ "type": "custom", "name": "apply_patch" });
    let custom = json!({ "tools": [patch_tool] });
    let function =
        json!({ "tools": [{ "type": "function", "function": { "name": "apply_patch" } }] });
    let both = json!({ "tools": [{ "type": "function", "function": { "name": "apply_patch" } }, patch_tool] });
    let namespaced =
        json!({ "tools": [{ "type": "namespace", "name": "functions", "tools": [patch_tool] }] });
    let additional = json!({
        "input": [{ "type": "additional_tools", "tools": [{ "type": "namespace", "name": "ns", "tools": [patch_tool] }] }]
    });
    let long = json!({ "tools": [{ "type": "function", "function": { "name": LONG_NAME } }] });
    let text = |delta: &str| json!({ "type": "response.output_text.delta", "output_index": 0, "item_id": "msg_1", "delta": delta });

    let mut cases = vec![
        stream("apply-patch-custom", &custom, &apply_patch_events(None)),
        stream("apply-patch-function", &function, &apply_patch_events(None)),
        stream(
            "apply-patch-declared-twice",
            &both,
            &apply_patch_events(None),
        ),
        stream(
            "apply-patch-in-namespace",
            &namespaced,
            &apply_patch_events(Some("functions")),
        ),
        stream(
            "apply-patch-in-additional-tools",
            &additional,
            &apply_patch_events(Some("ns")),
        ),
        stream(
            "apply-patch-other-namespace",
            &namespaced,
            &apply_patch_events(Some("other")),
        ),
        stream(
            "apply-patch-undeclared",
            &json!({}),
            &apply_patch_events(None),
        ),
        stream(
            "long-name-restored",
            &long,
            &[
                created(json!({})),
                json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "mcp__search_files", "arguments": "" } }),
                json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_1", "delta": "{\"q\":" }),
                json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_1", "delta": "\"x\"}" }),
                json!({ "type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_1", "arguments": "{\"q\":\"x\"}" }),
                json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "mcp__search_files", "arguments": "{\"q\":\"x\"}" } }),
                completed(json!([])),
            ],
        ),
        stream(
            "function-call-done-only",
            &json!({}),
            &[
                created(json!({})),
                json!({ "type": "response.output_item.done", "output_index": 1, "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "f", "arguments": "{}" } }),
                completed(json!([])),
            ],
        ),
        stream(
            "reasoning-text-and-summary",
            &json!({}),
            &[
                created(json!({})),
                json!({ "type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "Summary" }),
                json!({ "type": "response.reasoning_summary_text.done", "item_id": "rs_1", "text": "Summary" }),
                json!({ "type": "response.reasoning_text.delta", "item_id": "rs_1", "delta": "Raw " }),
                json!({ "type": "response.reasoning_text.delta", "item_id": "rs_1", "delta": "thought" }),
                json!({ "type": "response.reasoning_text.done", "item_id": "rs_1", "text": "Raw thought" }),
                text("Answer"),
                completed(json!([])),
            ],
        ),
        stream(
            "service-tier-and-model",
            &json!({}),
            &[
                created(json!({ "service_tier": "priority", "model": "gpt-5-2026-09" })),
                json!({ "type": "response.output_text.delta", "model": "gpt-5-codex", "delta": "Hi" }),
                json!({ "type": "response.output_text.delta", "model": null, "delta": "!" }),
                json!({ "type": "response.output_text.delta", "service_tier": "flex", "delta": "?" }),
                completed(json!([])),
            ],
        ),
        stream(
            "created-at-forms",
            &json!({}),
            &[
                created(json!({ "created_at": "1700000000" })),
                text("a"),
                created(json!({ "created_at": 1_700_000_000.9 })),
                text("b"),
                created(json!({ "created_at": null })),
                text("c"),
            ],
        ),
        stream(
            "no-created-event",
            &json!({}),
            &[text("Hi"), completed(json!([]))],
        ),
        stream(
            "incomplete-max-output-tokens",
            &json!({}),
            &[
                created(json!({})),
                text("Cut"),
                json!({ "type": "response.incomplete", "response": { "id": "resp_1", "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" } } }),
            ],
        ),
        stream(
            "incomplete-content-filter",
            &json!({}),
            &[
                created(json!({})),
                json!({ "type": "response.incomplete", "response": { "incomplete_details": { "reason": "content_filter" } } }),
            ],
        ),
        Case::new("line-formats", "gpt-5", "{}").with_events(vec![
            format!("data:{}", created(json!({}))),
            format!("data:\t {} \r", text("a")),
            text("bare").to_string(),
            "event: response.output_text.delta".into(),
            ": ping".into(),
            String::new(),
            "data:".into(),
            "data: [DONE]".into(),
            "data: {}".into(),
        ]),
        Case::new("escaped-event", "gpt-5", "{}").with_events(vec![
            r#"data: {"type":"response.output_text.delta","delta":"café a\/b"}"#
                .replace("output_text", &format!("output{}text", escaped('_'))),
        ]),
        Case::new("malformed-event", "gpt-5", "{}")
            .with_events(vec![
                r#"data: {"type":"response.output_text.delta","delta":"x""#.into(),
            ])
            .known_difference("a data line that isn't valid JSON gives no chunk"),
    ];
    for format in ["png", "jpeg", "webp", "gif", "image/avif", ""] {
        cases.push(stream(
            &format!("image-{}", format.replace('/', "-")),
            &json!({}),
            &image_events(format),
        ));
    }
    cases
}

/// Final events for the Chat Completions non-streaming translator.
pub fn finals() -> Vec<Case> {
    let message = json!({ "type": "message", "id": "msg_1", "role": "assistant", "content": [{ "type": "output_text", "text": "Hi" }] });
    let reasoning = json!({ "type": "reasoning", "id": "rs_1", "summary": [{ "type": "summary_text", "text": "Thinking" }], "content": [{ "type": "reasoning_text", "text": "Raw" }] });
    let call = json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "mcp__search_files", "arguments": "{\"q\":\"x\"}" });
    let patch = json!({ "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch\n" });
    let image = json!({ "type": "image_generation_call", "id": "ig_1", "status": "completed", "result": "AAAA", "output_format": "webp" });
    let request = json!({
        "tools": [
            { "type": "function", "function": { "name": LONG_NAME } },
            { "type": "custom", "name": "apply_patch" }
        ]
    })
    .to_string();
    let final_case = |name: &str, body: Value| {
        Case::new(name, "gpt-5", request.clone()).with_events(vec![body.to_string()])
    };
    let mut everything = completed(json!([reasoning, message, call, patch, image]));
    everything["response"]["service_tier"] = "priority".into();
    let mut no_created_at = completed(json!([message]));
    no_created_at["response"]
        .as_object_mut()
        .expect("an object")
        .shift_remove("created_at");
    let mut incomplete = completed(json!([message]));
    incomplete["type"] = "response.incomplete".into();
    incomplete["response"]["incomplete_details"] = json!({ "reason": "max_output_tokens" });

    vec![
        final_case("everything", everything),
        final_case("no-created-at", no_created_at),
        final_case("incomplete", incomplete),
        final_case("text-only", completed(json!([message]))),
        final_case("empty-output", completed(json!([]))),
        final_case("output-not-an-array", completed(json!({ "a": message }))),
        final_case(
            "string-created-at",
            json!({ "type": "response.completed", "response": { "created_at": "1700000000", "output": [] } }),
        ),
        final_case("missing-response", json!({ "type": "response.completed" })),
        final_case(
            "failed",
            json!({ "type": "response.failed", "response": { "error": { "message": "boom" } } }),
        ),
        final_case("not-final", created(json!({}))),
        Case::new("not-json", "gpt-5", "{}").with_events(vec!["not json".into()]),
        Case::new("empty", "gpt-5", "{}").with_events(vec![String::new()]),
    ]
}
