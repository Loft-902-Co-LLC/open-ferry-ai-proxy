//! Hand-written cases for the Claude → Chat Completions translators.

use serde_json::{Value, json};

use super::openai_chat::{
    bodies, call, chunk, data, event_streams, finish, lines, response, text, usage_only, whole_call,
};
use super::{Case, GROK_SIGNATURE, gpt_signature};

const MODEL: &str = "gpt-4o";

/// The Claude requests written for the Codex translator, and requests aimed
/// at what only this one reads. Each runs in both the plain and
/// compatibility suites, half of them for a client that streams.
pub fn requests() -> Vec<Case> {
    let tools = json!([
        { "name": "Bash", "description": "Run a command.", "input_schema": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] } },
        { "name": "Read", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } } },
        { "name": "Glob", "input_schema": { "type": "object" } }
    ]);
    let user = |content: Value| json!({ "role": "user", "content": content });
    let assistant = |content: Value| json!({ "role": "assistant", "content": content });
    let tool_use = |id: &str, name: &str, input: Value| json!({ "type": "tool_use", "id": id, "name": name, "input": input });
    let result = |id: &str, content: Value| json!({ "type": "tool_result", "tool_use_id": id, "content": content });
    let image = json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } });
    let thinking = |signature: &str| json!({ "type": "thinking", "thinking": "Let me look.", "signature": signature });
    let request = |fields: Value| {
        let mut request = json!({ "model": "claude-sonnet-4-5", "max_tokens": 1024, "messages": [user(json!("Hi"))] });
        if let (Value::Object(request), Value::Object(fields)) = (&mut request, fields) {
            request.extend(fields);
        }
        request.to_string()
    };

    let mut cases: Vec<Case> = super::hand_written();
    cases.extend([
        Case::new(
            "claude-code-turn",
            MODEL,
            json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 32000,
                "stream": true,
                "system": [
                    { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1" },
                    { "type": "text", "text": "You are Claude Code.", "cache_control": { "type": "ephemeral" } }
                ],
                "tools": tools,
                "thinking": { "type": "enabled", "budget_tokens": 10000 },
                "metadata": { "user_id": "user_abc" },
                "messages": [
                    user(json!([{ "type": "text", "text": "List files." }])),
                    assistant(json!([
                        thinking(&gpt_signature()),
                        { "type": "text", "text": "Listing." },
                        tool_use("toolu_1", "Bash", json!({ "command": "ls" }))
                    ])),
                    user(json!([
                        result("toolu_1", json!("a.txt\nb.txt")),
                        { "type": "text", "text": "Now read a.txt." }
                    ]))
                ]
            })
            .to_string(),
        ),
        Case::new("top-p-only", MODEL, request(json!({ "top_p": 0.9 }))),
        Case::new(
            "temperature-and-top-p",
            MODEL,
            request(json!({ "temperature": 0.2, "top_p": 0.9 })),
        ),
        Case::new(
            "temperature-as-text",
            MODEL,
            request(json!({ "temperature": "0.5", "top_p": "0.9" })),
        ),
        Case::new(
            "temperature-not-finite",
            MODEL,
            request(json!({ "temperature": "NaN", "top_p": "Inf" })),
        )
        .known_difference("upstream writes temperature as NaN, which isn't JSON; we leave it out"),
        Case::new(
            "top-p-not-finite",
            MODEL,
            request(json!({ "top_p": "-Inf" })),
        )
        .known_difference("upstream writes top_p as -Inf, which isn't JSON; we leave it out"),
        Case::new(
            "stop-sequences",
            MODEL,
            request(json!({ "stop_sequences": ["END", "", 5, true, null] })),
        ),
        Case::new(
            "stop-sequences-empty",
            MODEL,
            request(json!({ "stop_sequences": [] })),
        ),
        Case::new(
            "stop-sequences-text",
            MODEL,
            request(json!({ "stop_sequences": "END" })),
        ),
        Case::new("user-text", MODEL, request(json!({ "user": "user_1" }))),
        Case::new("user-number", MODEL, request(json!({ "user": 5 }))),
        Case::new("user-null", MODEL, request(json!({ "user": null }))),
        Case::new(
            "thinking-adaptive",
            MODEL,
            request(json!({ "thinking": { "type": "adaptive" }, "output_config": { "effort": "max" } })),
        ),
        Case::new(
            "thinking-disabled",
            MODEL,
            request(json!({ "thinking": { "type": "disabled" } })),
        ),
        Case::new(
            "thinking-budget-zero",
            MODEL,
            request(json!({ "thinking": { "type": "enabled", "budget_tokens": 0 } })),
        ),
        Case::new(
            "thinking-no-budget",
            MODEL,
            request(json!({ "thinking": { "type": "enabled" } })),
        ),
        Case::new(
            "effort-without-thinking",
            MODEL,
            request(json!({ "output_config": { "effort": "low" } })),
        ),
    ]);
    for (name, choice) in [
        ("auto", json!({ "type": "auto" })),
        ("any", json!({ "type": "any" })),
        ("none", json!({ "type": "none" })),
        ("tool", json!({ "type": "tool", "name": "Read" })),
        ("tool-no-name", json!({ "type": "tool" })),
        (
            "auto-serial",
            json!({ "type": "auto", "disable_parallel_tool_use": true }),
        ),
        (
            "any-parallel",
            json!({ "type": "any", "disable_parallel_tool_use": false }),
        ),
        (
            "serial-as-text",
            json!({ "type": "auto", "disable_parallel_tool_use": "true" }),
        ),
        ("unknown", json!({ "type": "other" })),
        ("text", json!("auto")),
    ] {
        cases.push(Case::new(
            format!("tool-choice-{name}"),
            MODEL,
            request(json!({ "tools": tools, "tool_choice": choice })),
        ));
    }
    cases.extend([
        Case::new(
            "tool-result-with-images",
            MODEL,
            request(json!({ "messages": [
                user(json!("Look.")),
                assistant(json!([tool_use("toolu_1", "Read", json!({ "path": "a.png" }))])),
                user(json!([result("toolu_1", json!([{ "type": "text", "text": "Here." }, image]))]))
            ] })),
        ),
        Case::new(
            "tool-result-only-images",
            MODEL,
            request(json!({ "messages": [
                user(json!("Look.")),
                assistant(json!([
                    tool_use("toolu_1", "Read", json!({ "path": "a.png" })),
                    tool_use("toolu_2", "Read", json!({ "path": "b.png" }))
                ])),
                user(json!([result("toolu_1", json!([image])), result("toolu_2", json!([image]))]))
            ] })),
        ),
        Case::new(
            "tool-results-out-of-order",
            MODEL,
            request(json!({ "messages": [
                user(json!("Go.")),
                assistant(json!([
                    tool_use("toolu_1", "Bash", json!({ "command": "a" })),
                    tool_use("toolu_2", "Bash", json!({ "command": "b" }))
                ])),
                user(json!([
                    { "type": "text", "text": "Results:" },
                    result("toolu_2", json!("B")),
                    result("toolu_1", json!([{ "type": "text", "text": "A" }]))
                ]))
            ] })),
        ),
        Case::new(
            "tool-result-missing",
            MODEL,
            request(json!({ "messages": [
                user(json!("Go.")),
                assistant(json!([tool_use("toolu_1", "Bash", json!({ "command": "a" }))])),
                user(json!("Never mind."))
            ] })),
        ),
        Case::new(
            "tool-result-content-kinds",
            MODEL,
            request(json!({ "messages": [
                user(json!("Go.")),
                assistant(json!([
                    tool_use("toolu_1", "Bash", json!({})),
                    tool_use("toolu_2", "Bash", json!({})),
                    tool_use("toolu_3", "Bash", json!({}))
                ])),
                user(json!([
                    result("toolu_1", Value::Null),
                    result("toolu_2", json!({ "a": 1 })),
                    { "type": "tool_result", "tool_use_id": "toolu_3", "is_error": true, "content": "failed" }
                ]))
            ] })),
        ),
        Case::new(
            "tool-use-without-id-or-input",
            MODEL,
            request(json!({ "messages": [
                user(json!("Go.")),
                assistant(json!([{ "type": "tool_use", "name": "Bash" }, { "type": "tool_use", "id": "toolu_2", "name": "Read", "input": "text" }])),
            ] })),
        ),
        Case::new(
            "system-mid-conversation",
            MODEL,
            request(json!({ "system": "Top.", "messages": [
                user(json!("Hi")),
                { "role": "system", "content": "Be brief." },
                assistant(json!("Hello")),
                { "role": "system", "content": [{ "type": "text", "text": "Again." }] },
                user(json!("Bye"))
            ] })),
        ),
        Case::new(
            "thinking-gpt-signature",
            MODEL,
            request(json!({ "messages": [
                user(json!("Hi")),
                assistant(json!([thinking(&gpt_signature()), { "type": "text", "text": "Hello" }])),
                user(json!("Again"))
            ] })),
        ),
        Case::new(
            "thinking-claude-signature",
            MODEL,
            request(json!({ "messages": [
                user(json!("Hi")),
                assistant(json!([thinking("EqQBCkgIBRABGAIiQL"), { "type": "text", "text": "Hello" }])),
                user(json!("Again"))
            ] })),
        ),
        Case::new(
            "thinking-grok-signature",
            MODEL,
            request(json!({ "messages": [
                user(json!("Hi")),
                assistant(json!([thinking(GROK_SIGNATURE), { "type": "text", "text": "Hello" }])),
                user(json!("Again"))
            ] })),
        ),
        Case::new(
            "thinking-only-assistant",
            MODEL,
            request(json!({ "messages": [
                user(json!("Hi")),
                assistant(json!([thinking(&gpt_signature())])),
                user(json!("Again"))
            ] })),
        ),
        Case::new(
            "redacted-thinking",
            MODEL,
            request(json!({ "messages": [
                user(json!("Hi")),
                assistant(json!([{ "type": "redacted_thinking", "data": "abc" }, { "type": "text", "text": "Hello" }])),
            ] })),
        ),
        Case::new(
            "image-sources",
            MODEL,
            request(json!({ "messages": [user(json!([
                image,
                { "type": "image", "source": { "type": "url", "url": "https://example.com/a.png" } },
                { "type": "image", "source": { "type": "base64", "data": "AAAA" } },
                { "type": "image", "source": { "type": "file", "file_id": "file_1" } },
                { "type": "image" },
                { "type": "document", "source": { "type": "text", "data": "doc" } }
            ]))] })),
        ),
        Case::new(
            "max-tokens-loose",
            MODEL,
            request(json!({ "max_tokens": "2048" })),
        ),
        Case::new(
            "assistant-text-content",
            MODEL,
            request(json!({ "messages": [user(json!("Hi")), assistant(json!("Hello")), assistant(json!(""))] })),
        ),
    ]);
    cases
        .into_iter()
        .enumerate()
        .map(|(index, case)| case.with_options(json!({ "stream": index % 2 == 0 })))
        .collect()
}

/// The client's Claude request: tools in mixed case, some with leading
/// underscores or padding, and whether it streams.
fn original_request(stream: Value) -> String {
    json!({
        "model": "claude-sonnet-4-5",
        "stream": stream,
        "tools": [
            { "name": "get_weather", "input_schema": { "type": "object" } },
            { "name": "search", "input_schema": { "type": "object" } },
            { "name": "Bash", "input_schema": { "type": "object" } },
            { "name": " Read ", "input_schema": { "type": "object" } },
            { "name": "__TodoWrite", "input_schema": { "type": "object" } },
            { "name": "bash", "input_schema": { "type": "object" } },
            { "type": "function", "function": { "name": "mcp__github__Get_Me" } },
            { "name": "Ölçü" },
            { "name": "ΑΣ" }
        ],
        "messages": [{ "role": "user", "content": "Hi" }]
    })
    .to_string()
}

/// Streams that only this translator reads differently: calls by names the
/// client declared in another case, and arguments in single quotes.
fn claude_streams() -> Vec<(&'static str, Vec<String>)> {
    let named = |index: u64, name: &str| call(index, Some("call_1"), Some(name), "{}");
    vec![
        (
            "mapped-names",
            lines(&[
                named(0, "BASH"),
                named(1, " read "),
                named(2, "_todowrite"),
                named(3, "MCP__GITHUB__GET_ME"),
                named(4, "ölçü"),
                named(5, "ασ"),
                named(6, "Glob"),
                finish("tool_calls"),
            ]),
        ),
        (
            "single-quoted-arguments",
            lines(&[
                call(
                    0,
                    Some("call_1"),
                    Some("Bash"),
                    r#"{'command': 'echo "hi"', 'n': 'it\'s'}"#,
                ),
                call(1, Some("call_2"), Some("Bash"), "{'open"),
                finish("tool_calls"),
            ]),
        ),
        (
            "unsafe-call-ids",
            lines(&[
                call(0, Some("call 1/é"), Some("Bash"), "{}"),
                call(1, Some(""), Some("Bash"), "{}"),
                finish("tool_calls"),
            ]),
        ),
        (
            "call-closes-text",
            lines(&[
                text("Before."),
                call(0, Some("call_1"), Some("Bash"), ""),
                call(0, None, None, r#"{"command":"ls"}"#),
                text("After."),
                call(1, Some("call_2"), Some("search"), "{}"),
                text("End."),
                finish("tool_calls"),
                usage_only(super::openai_chat::usage(4, 4)),
            ]),
        ),
        (
            "empty-tool-call-delta",
            lines(&[
                chunk(json!({ "tool_calls": [] })),
                chunk(json!({ "tool_calls": [{ "index": 0 }] })),
                finish("stop"),
            ]),
        ),
    ]
}

/// Chat Completions streams answering a client that streams, one that
/// doesn't, and one whose request is missing.
pub fn streams() -> Vec<Case> {
    let streams = event_streams().into_iter().chain(claude_streams());
    let mut cases: Vec<Case> = streams
        .map(|(name, lines)| Case::response(name, original_request(json!(true)), lines))
        .collect();
    for (name, stream) in [
        ("not-streaming", json!(false)),
        ("stream-null", Value::Null),
        ("stream-as-text", json!("true")),
    ] {
        cases.push(Case::response(
            name,
            original_request(stream),
            lines(&[text("Hi"), finish("stop")]),
        ));
    }
    for (name, lines) in event_streams().into_iter().take(12) {
        cases.push(Case::response(format!("no-request-{name}"), "", lines));
    }
    cases.push(Case::response(
        "no-request-line-kinds",
        "",
        vec![
            ": keep-alive".to_owned(),
            "event: x".to_owned(),
            text("bare").to_string(),
            data(&text("Hi")),
            "data:".to_owned(),
            "data: {not json".to_owned(),
            "data: [DONE]".to_owned(),
        ],
    ));
    cases
}

/// Whole Chat Completions responses, for the non-streaming translator.
pub fn finals() -> Vec<Case> {
    let request = original_request(json!(false));
    let mut cases: Vec<Case> = bodies()
        .into_iter()
        .map(|(name, body)| Case::response(name, &request, vec![body]))
        .collect();
    let mapped = json!([
        whole_call("call_1", "BASH", "{}"),
        whole_call("call_2", " read ", "{'path': 'a'}"),
        whole_call("call 3/é", "__todowrite", r#"{"a":"#),
        whole_call("", "Glob", "[1]")
    ]);
    cases.extend([
        Case::response(
            "mapped-names",
            &request,
            vec![
                response(
                    json!({ "role": "assistant", "content": null, "tool_calls": mapped }),
                    "tool_calls",
                )
                .to_string(),
            ],
        ),
        Case::response(
            "no-request",
            "",
            vec![response(json!({ "content": "x" }), "stop").to_string()],
        ),
        Case::response("no-body", &request, Vec::new()),
    ]);
    cases
}
