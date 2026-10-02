//! Hand-written cases for the Responses translators.

use serde_json::{Value, json};

use super::{Case, escaped};

/// Responses requests the generator is unlikely to build in one piece.
pub fn requests() -> Vec<Case> {
    let mut cases = vec![
        Case::new(
            "string-input",
            "gpt-5",
            r#"{"model":"gpt-5","input":"Hello, world!"}"#,
        ),
        Case::new(
            "already-normalized",
            "gpt-5",
            json!({
                "model": "gpt-5",
                "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }],
                "stream": true,
                "store": false,
                "parallel_tool_calls": true,
                "include": ["reasoning.encrypted_content"]
            })
            .to_string(),
        ),
        Case::new("empty-object", "gpt-5", "{}"),
        Case::new("array-body", "gpt-5", "[]"),
        Case::new("array-of-numbers-body", "gpt-5", "[1,2]"),
        Case::new("null-body", "gpt-5", "null"),
        Case::new("string-body", "gpt-5", r#""hi""#),
        Case::new("number-body", "gpt-5", "5"),
        Case::new(
            "breakpoints-everywhere",
            "gpt-5",
            json!({
                "prompt_cache_breakpoint": { "type": "ephemeral" },
                "tools": [{ "type": "function", "name": "f", "parameters": {}, "prompt_cache_breakpoint": true }],
                "input": [
                    { "type": "message", "role": "user", "prompt_cache_breakpoint": { "type": "ephemeral" }, "content": [
                        { "type": "input_text", "text": "a", "prompt_cache_breakpoint": { "type": "ephemeral" } },
                        "loose",
                        { "type": "input_text", "text": "b" }
                    ]},
                    { "type": "function_call_output", "call_id": "call_1", "output": [
                        { "type": "input_text", "text": "out", "prompt_cache_breakpoint": null }
                    ]},
                    { "type": "message", "role": "assistant", "content": "plain", "prompt_cache_breakpoint": false },
                    { "type": "message", "role": "user", "content": { "prompt_cache_breakpoint": 1 } },
                    ["prompt_cache_breakpoint"]
                ]
            })
            .to_string(),
        ),
        Case::new(
            "breakpoint-only-outside-input",
            "gpt-5",
            r#"{"input":[{"role":"user","content":"hi"}],"metadata":{"prompt_cache_breakpoint":"x"}}"#,
        ),
        Case::new(
            "escaped-breakpoint-key",
            "gpt-5",
            r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"a","prompt_cache_breakpoint":{"type":"ephemeral"}}]}]}"#
                .replace("_breakpoint", &format!("_{}reakpoint", escaped('b'))),
        )
        .known_difference(
            "upstream only strips prompt_cache_breakpoint when the body holds the key unescaped",
        ),
        // Upstream rebuilds `input` with json.Marshal, which compacts it and
        // escapes HTML characters; the value is the same.
        Case::new(
            "system-roles",
            "gpt-5",
            serde_json::to_string_pretty(&json!({
                "input": [
                    { "role": "system", "content": "<b>a & b</b>" },
                    { "type": "message", "role": "system", "content": [{ "type": "input_text", "text": "café / 🚀", "n": 1.50 }] },
                    { "role": "System", "content": "x" },
                    { "role": ["system"] },
                    { "type": "function_call", "role": "system", "arguments": "" },
                    "system"
                ]
            }))
            .expect("serializable"),
        ),
        Case::new(
            "escaped-system-role",
            "gpt-5",
            r#"{"input":[{"role":"system","content":"x"},{"type":"function_call","arguments":" "}]}"#
                .replace("system", &format!("sys{}em", escaped('t')))
                .replace("function_call", &format!("function{}call", escaped('_'))),
        ),
        Case::new(
            "web-search-aliases",
            "gpt-5",
            json!({
                "tools": [
                    { "type": "web_search_preview" },
                    { "type": "web_search_preview_2025_03_11", "search_context_size": "low" },
                    { "type": "web_search" },
                    { "type": "WEB_SEARCH_PREVIEW" },
                    "web_search_preview",
                    { "type": "function", "name": "web_search_preview" }
                ],
                "tool_choice": {
                    "type": "web_search_preview_2025_03_11",
                    "tools": [{ "type": "web_search_preview" }, { "type": "function", "name": "f" }]
                }
            })
            .to_string(),
        ),
        Case::new(
            "tool-choice-array",
            "gpt-5",
            r#"{"tool_choice":[{"type":"web_search_preview"}],"tools":{"type":"web_search_preview"}}"#,
        ),
        Case::new(
            "blank-arguments",
            "gpt-5",
            json!({ "input": blank_arguments() }).to_string(),
        ),
        Case::new(
            "loose-required-fields",
            "gpt-5",
            r#"{"stream":"true","store":0,"parallel_tool_calls":null,"include":"reasoning.encrypted_content"}"#,
        ),
        Case::new(
            "dropped-and-kept-fields",
            "gpt-5",
            json!({
                "max_output_tokens": 1024,
                "max_completion_tokens": 1024,
                "temperature": 0.7,
                "top_p": 1,
                "truncation": "auto",
                "prompt_cache_options": { "retention": "24h" },
                "prompt_cache_retention": "24h",
                "context_management": [{ "type": "compaction" }],
                "user": "user_123",
                "prompt_cache_key": "key",
                "reasoning": { "effort": "low" },
                "text": { "verbosity": "low" },
                "metadata": { "a": "b" },
                "previous_response_id": "resp_1"
            })
            .to_string(),
        ),
        Case::new(
            "huge-numbers",
            "gpt-5",
            r#"{"temperature":1e400,"metadata":{"n":1e400,"m":123456789012345678901234567890}}"#,
        ),
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
        json!("PRİORİTY"),
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
            json!({ "service_tier": tier, "input": "hi" }).to_string(),
        ));
    }
    for (index, include) in [
        json!([]),
        json!(["reasoning.encrypted_content", "x"]),
        json!([" reasoning.encrypted_content"]),
        json!(["reasoning.encrypted_content"]),
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(Case::new(
            format!("include-{index}"),
            "gpt-5",
            json!({ "include": include }).to_string(),
        ));
    }
    cases
}

/// Function calls with arguments that are blank, nearly blank or not strings,
/// and other items with a blank field.
fn blank_arguments() -> Vec<Value> {
    ["", "  \n\t", "\u{a0}", "\u{3000}", "\u{200b}", " {} "]
        .iter()
        .map(|arguments| json!({ "type": "function_call", "call_id": "call_1", "name": "f", "arguments": arguments }))
        .chain([
            json!({ "type": "function_call", "arguments": null }),
            json!({ "type": "function_call", "arguments": 5 }),
            json!({ "type": "function_call" }),
            json!({ "type": "custom_tool_call", "input": "" }),
            json!({ "type": "function_call_output", "output": "" }),
        ])
        .collect()
}

fn data(event: &Value) -> String {
    format!("data: {event}")
}

fn created(response: Value) -> Value {
    json!({ "type": "response.created", "sequence_number": 0, "response": response })
}

fn stream(name: &str, model: &str, request: &str, translated: &str, lines: Vec<String>) -> Case {
    Case {
        translated_request: translated.into(),
        events: lines,
        ..Case::new(name, model, request)
    }
}

/// Codex event streams for the Responses streaming translator.
pub fn streams() -> Vec<Case> {
    let in_progress = json!({ "id": "resp_1", "status": "in_progress" });
    let typical = vec![
        data(&created(in_progress.clone())),
        data(
            &json!({ "type": "response.in_progress", "sequence_number": 1, "response": in_progress }),
        ),
        data(&json!({ "type": "response.output_text.delta", "sequence_number": 2, "delta": "Hi" })),
        data(
            &json!({ "type": "response.completed", "sequence_number": 3, "response": { "id": "resp_1", "status": "completed" } }),
        ),
        "data: [DONE]".into(),
    ];
    let model = |model: &str| json!({ "model": model }).to_string();
    let with_response = |responses: &[Value]| {
        responses
            .iter()
            .map(|response| data(&created(response.clone())))
            .collect::<Vec<_>>()
    };

    vec![
        stream(
            "fills-missing-model",
            "",
            &model("gpt-5"),
            "",
            typical.clone(),
        ),
        stream(
            "keeps-codex-model",
            "",
            &model("gpt-5"),
            "",
            vec![data(&created(
                json!({ "id": "resp_1", "model": "gpt-5-2026-09" }),
            ))],
        ),
        stream(
            "model-from-request-model",
            "",
            r#"{"request":{"model":"gpt-5-codex"}}"#,
            "",
            typical.clone(),
        ),
        stream(
            "model-from-translated-request",
            "",
            &model(" "),
            &model("gpt-5"),
            typical.clone(),
        ),
        stream(
            "invalid-original-request",
            "",
            "not json",
            &model("gpt-5"),
            typical.clone(),
        ),
        stream("model-param", "gpt-5-mini", "{}", "", typical.clone()),
        stream("no-model-anywhere", "", &model(""), "", typical.clone()),
        stream(
            "untrimmed-model",
            "",
            &model(" gpt-5 "),
            "",
            typical.clone(),
        ),
        stream(
            "non-string-model",
            "gpt-5",
            r#"{"model":5,"request":{"model":["x"]}}"#,
            "",
            typical,
        ),
        stream(
            "response-not-an-object",
            "",
            &model("gpt-5"),
            "",
            with_response(&[
                Value::Null,
                json!("resp_1"),
                json!(5),
                json!(true),
                json!([]),
                json!([{ "id": "resp_1" }]),
                json!({}),
                json!({ "model": null }),
                json!({ "model": "" }),
            ])
            .into_iter()
            .chain([data(&json!({ "type": "response.created" }))])
            .collect(),
        ),
        stream(
            "line-formats",
            "",
            &model("gpt-5"),
            "",
            vec![
                format!("data:{}", created(json!({}))),
                format!("data:\t {} \r", created(json!({}))),
                format!("data: {}\u{a0}", created(json!({}))),
                created(json!({ "id": "bare" })).to_string(),
                format!(" {} ", created(json!({ "id": "padded" }))),
                "event: response.created".into(),
                ": ping".into(),
                String::new(),
                "data:".into(),
                "data: [DONE]".into(),
                "data: {}".into(),
            ],
        ),
        stream(
            "escaped-event",
            "",
            &model("gpt-5"),
            "",
            vec![
                r#"data: {"type":"response.created","response":{"id":"resp_é","note":"a\/b"}}"#
                    .replace("created", &format!("cr{}ated", escaped('e')))
                    .replace('é', &escaped('é')),
            ],
        ),
        stream(
            "malformed-created-event",
            "",
            &model("gpt-5"),
            "",
            vec![r#"data: {"type":"response.created","response":{"id":"resp_1""#.into()],
        )
        .known_difference("a data line that isn't valid JSON passes through unchanged"),
    ]
}

/// Final events for the Responses non-streaming translator.
pub fn finals() -> Vec<Case> {
    let response = json!({
        "id": "resp_1",
        "object": "response",
        "model": "gpt-5",
        "status": "completed",
        "output": [{ "type": "message", "id": "msg_1", "role": "assistant", "content": [{ "type": "output_text", "text": "Hi" }] }],
        "usage": { "input_tokens": 3, "output_tokens": 1, "total_tokens": 4 }
    });
    let event = |kind: &str| json!({ "type": kind, "response": response.clone() });
    let final_case = |name: &str, body: String| Case::response(name, "{}", vec![body]);

    vec![
        final_case("completed", event("response.completed").to_string()),
        final_case("incomplete", event("response.incomplete").to_string()),
        final_case(
            "pretty-and-escaped",
            serde_json::to_string_pretty(&json!({ "type": "response.completed", "response": { "id": "resp_é", "note": "a/b", "n": 1.50 } }))
                .expect("serializable")
                .replace('é', &escaped('é'))
                .replace('/', "\\/"),
        ),
        final_case("already-a-response", response.to_string()),
        final_case("blank-type-with-output", r#"{"type":"","output":[]}"#.into()),
        final_case("null-type-with-output", r#"{"type":null,"output":[]}"#.into()),
        final_case("output-not-an-array", r#"{"output":{}}"#.into()),
        final_case("missing-response", r#"{"type":"response.completed"}"#.into()),
        final_case(
            "null-response",
            r#"{"type":"response.completed","response":null}"#.into(),
        ),
        final_case("failed", event("response.failed").to_string()),
        final_case("not-final", event("response.created").to_string()),
        final_case("not-json", "not json".into()),
        final_case("empty", String::new()),
    ]
}
