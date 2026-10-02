//! Hand-written cases for the Responses → Claude translators.

use serde_json::{Value, json};

use super::{Case, escaped, gpt_signature};
use crate::generate::claude_responses::claude_signature;

/// Tools as a Codex client declares them: functions, a namespace, the
/// `apply_patch` custom tool and web search.
fn client_tools() -> Value {
    json!([
        { "type": "function", "name": "get_weather", "description": "Weather.", "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } },
        { "type": "function", "name": "search", "strict": false, "parameters": { "type": "object", "properties": {} } },
        { "type": "namespace", "name": "mcp__github", "description": "GitHub.", "tools": [
            { "type": "function", "name": "read_file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } },
            { "name": "mcp__github__list" }
        ]},
        { "type": "custom", "name": "apply_patch", "description": "Edit files.", "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" } },
        { "type": "web_search", "filters": { "allowed_domains": ["docs.rs"] }, "user_location": { "type": "approximate", "country": "FR" } }
    ])
}

fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

fn assistant(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
}

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({ "type": "function_call", "call_id": id, "name": name, "arguments": arguments })
}

fn output(id: &str, output: Value) -> Value {
    json!({ "type": "function_call_output", "call_id": id, "output": output })
}

fn reasoning(signature: Value) -> Value {
    json!({ "type": "reasoning", "encrypted_content": signature, "summary": [{ "type": "summary_text", "text": "Thought." }] })
}

/// A request with these input items, as JSON text.
fn items(items: Vec<Value>) -> String {
    json!({ "input": items }).to_string()
}

/// Responses requests the generator is unlikely to build in one piece. Each
/// runs in both the plain and compatibility suites.
pub fn requests() -> Vec<Case> {
    let signature = claude_signature("claude-sonnet-4-6");
    let mut cases = vec![
        Case::new(
            "typical-sdk-request",
            "claude-sonnet-4-6",
            json!({
                "model": "claude-sonnet-4-6",
                "instructions": "You are terse.",
                "input": [
                    user("Weather in Paris?"),
                    reasoning(json!(signature)),
                    call("call_1", "get_weather", "{\"city\":\"Paris\"}"),
                    output("call_1", json!("18°C")),
                    assistant("It is 18°C."),
                    user("Thanks!")
                ],
                "tools": client_tools(),
                "tool_choice": "auto",
                "max_output_tokens": 1024,
                "reasoning": { "effort": "medium", "summary": "auto" },
                "parallel_tool_calls": true,
                "store": false,
                "stream": true,
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "cache-1"
            })
            .to_string(),
        ),
        Case::new("string-input", "claude-opus-4-6", r#"{"input":"hi"}"#),
        Case::new(
            "system-cache-control",
            "claude-opus-4-6",
            json!({
                "instructions": "Rules.",
                "input": [
                    { "type": "message", "role": "system", "content": [
                        { "type": "input_text", "text": "One.", "cache_control": { "type": "ephemeral" } },
                        { "type": "input_text", "text": "Two." },
                        { "type": "input_image", "image_url": "https://example.com/a.png" }
                    ], "cache_control": { "type": "ephemeral", "ttl": "1h" } },
                    { "role": " Developer ", "content": "Be careful." },
                    { "role": "developer", "content": [{ "type": "output_text", "text": "Three.", "cache_control": "bad" }] },
                    { "type": "message", "role": "user", "content": [
                        { "type": "input_text", "text": "a", "cache_control": { "type": "ephemeral" } },
                        { "type": "input_text", "text": "b" }
                    ], "cache_control": { "type": "ephemeral" } }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "instructions-not-a-string",
            "claude-opus-4-6",
            json!({ "instructions": ["rules"], "input": "hi" }).to_string(),
        ),
        Case::new(
            "system-only",
            "claude-opus-4-6",
            json!({ "instructions": "Rules.", "input": [{ "role": "system", "content": "More." }] })
                .to_string(),
        ),
        Case::new(
            "agent-messages",
            "claude-opus-4-6",
            items(vec![
                user("Coordinate."),
                json!({ "type": "agent_message", "author": "worker-1", "content": [
                    { "type": "encrypted_content", "encrypted_content": "Worker says hi." },
                    { "type": " encrypted_content ", "encrypted_content": "Padded type." },
                    { "type": "encrypted_content", "encrypted_content": 5 },
                    { "type": "input_text", "text": "plain" }
                ]}),
                json!({ "type": " agent_message ", "role": "assistant", "content": "a string" }),
            ]),
        ),
        Case::new(
            "reasoning-signatures",
            "claude-sonnet-4-6",
            items(vec![
                user("Think."),
                reasoning(json!(signature)),
                assistant("First."),
                user("Again."),
                reasoning(json!(claude_signature("claude-opus-4-6"))),
                reasoning(json!(gpt_signature())),
                assistant("Second."),
                user("More."),
                reasoning(json!("")),
                reasoning(json!("not a signature")),
                assistant("Third."),
                user("Last."),
                json!({ "type": "reasoning", "encrypted_content": signature, "summary": [], "content": [{ "type": "reasoning_text", "text": "From content." }] }),
                json!({ "type": "reasoning", "encrypted_content": signature, "summary": ["bare", { "type": "summary_text", "text": "typed" }] }),
                call("call_1", "search", "{}"),
                output("call_1", json!("done")),
            ]),
        ),
        Case::new(
            "redacted-thinking",
            "claude-sonnet-4-6",
            items(vec![
                user("Think."),
                reasoning(json!("claude-redacted-thinking:EmwKAhgBEgwvYXRoZXJzdGVwcw==")),
                reasoning(json!(" claude-redacted-thinking: EmwK ")),
                reasoning(json!("claude-redacted-thinking:")),
                assistant("Done."),
                user("Next."),
            ]),
        ),
        Case::new(
            "trailing-thinking",
            "claude-opus-4-6",
            items(vec![user("Think."), reasoning(json!(signature))]),
        ),
        Case::new(
            "assistant-prefill",
            "claude-fable-5-1",
            items(vec![user("Start."), assistant("Prefilled")]),
        ),
        Case::new(
            "assistant-prefill-allowed",
            "claude-opus-4-6",
            items(vec![user("Start."), assistant("Prefilled")]),
        ),
        Case::new(
            "tool-call-ids",
            "claude-opus-4-6",
            items(vec![
                user("Go."),
                json!({ "type": "function_call", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call", "tool_call_id": "call.1:é/x y", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call", "callId": "call_camel", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call", "id": "call_by_id", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call", "id": "fco_skipped", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call", "call_id": format!("call_{}", "x".repeat(90)), "name": "search", "arguments": "{}" }),
                output("call.1:é/x y", json!("one")),
                json!({ "type": "function_call_output", "callId": "call_camel", "output": "two" }),
                json!({ "type": "function_call_output", "tool_call_id": "call_by_id", "output": "three" }),
                json!({ "type": "function_call_output", "output": "no id" }),
                json!({ "type": "function_call_output", "call_id": format!("call_{}", "x".repeat(90)), "output": "long" }),
            ]),
        ),
        Case::new(
            "duplicate-call-ids",
            "claude-opus-4-6",
            items(vec![
                user("Go."),
                call("call_1", "search", "{\"q\":1}"),
                call("call_1", "search", "{\"q\":2}"),
                output("call_1", json!("first")),
                output("call_1", json!("second")),
                output("call_1", json!("third")),
            ]),
        ),
        Case::new(
            "outputs-out-of-order",
            "claude-opus-4-6",
            items(vec![
                user("Go."),
                output("call_early", json!("before its call")),
                call("call_early", "search", "{}"),
                call("call_a", "get_weather", "{}"),
                call("call_b", "search", "{}"),
                output("call_b", json!("b")),
                output("call_a", json!("a")),
                output("call_orphan", json!("nobody asked")),
                json!({ "type": "function_call_output", "call_id": "call_empty", "output": "" }),
                assistant("Done."),
            ]),
        ),
        Case::new(
            "unanswered-calls",
            "claude-sonnet-4-6",
            items(vec![
                user("Go."),
                call("call_1", "search", "{}"),
                user("Never mind."),
                call("call_2", "search", "{}"),
            ]),
        ),
        Case::new(
            "call-arguments",
            "claude-opus-4-6",
            items(vec![
                user("Go."),
                call("call_1", "search", "[1,2]"),
                call("call_2", "search", "not json"),
                call("call_3", "search", ""),
                call("call_4", "search", "{ \"spaced\" : 1.50 }"),
                json!({ "type": "function_call", "call_id": "call_5", "name": "search", "arguments": { "object": true } }),
                output("call_1", json!("1")),
                output("call_2", json!("2")),
                output("call_3", json!("3")),
                output("call_4", json!("4")),
                output("call_5", json!("5")),
            ]),
        ),
        Case::new(
            "tool-output-forms",
            "claude-opus-4-6",
            items(vec![
                user("Go."),
                call("a", "search", "{}"),
                call("b", "search", "{}"),
                call("c", "search", "{}"),
                call("d", "search", "{}"),
                call("e", "search", "{}"),
                output("a", json!([
                    { "type": "input_text", "text": "text" },
                    { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo=" },
                    { "type": "input_file", "file_data": "data:application/pdf;base64,JVBERi0=", "filename": "a.pdf" },
                    { "type": "input_audio" }
                ])),
                output("b", json!({ "result": 1.50 })),
                output("c", json!([])),
                output("d", json!(1.50)),
                output("e", json!("  ")),
            ]),
        ),
        Case::new(
            "namespaced-calls",
            "claude-opus-4-6",
            json!({
                "tools": client_tools(),
                "input": [
                    user("Read it."),
                    { "type": "function_call", "call_id": "call_1", "name": "read_file", "namespace": "mcp__github", "arguments": "{\"path\":\"a\"}" },
                    { "type": "function_call", "call_id": "call_2", "name": "mcp__github__list", "namespace": "mcp__github", "arguments": "{}" },
                    { "type": "function_call", "call_id": "call_3", "name": "x", "namespace": "tools__", "arguments": "{}" },
                    output("call_1", json!("a")),
                    output("call_2", json!("b")),
                    output("call_3", json!("c"))
                ],
                "tool_choice": { "type": "function", "name": "read_file", "namespace": "mcp__github" }
            })
            .to_string(),
        ),
        Case::new(
            "apply-patch",
            "claude-opus-4-6",
            json!({
                "tools": client_tools(),
                "input": [
                    user("Add a file."),
                    { "type": "custom_tool_call", "call_id": "call_patch", "name": "apply_patch",
                      "input": "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch" },
                    { "type": "custom_tool_call_output", "call_id": "call_patch", "output": "Done!" },
                    { "type": "custom_tool_call", "call_id": "call_shell", "name": "shell", "input": 5 }
                ],
                "tool_choice": { "type": "custom", "name": "apply_patch" }
            })
            .to_string(),
        ),
        Case::new(
            "web-search-calls",
            "claude-opus-4-6",
            items(vec![
                user("Search."),
                json!({ "type": "web_search_call", "id": "ws_srvtoolu_01A", "status": "completed",
                        "action": { "type": "search", "query": "rust serde" },
                        "results": [
                            { "type": "web_search_result", "url": "https://serde.rs", "title": "Serde", "encrypted_content": "EqgfCioIARgBIiQ3" },
                            { "type": "web_search_result", "url": "https://x.example", "encrypted_content": " " },
                            { "type": "web_search_tool_result_error", "error_code": "unavailable" }
                        ] }),
                json!({ "type": "web_search_call", "id": "ws_é-日 x", "action": { "type": "search", "queries": ["first", "second"] } }),
                json!({ "type": "web_search_call", "id": "ws_page", "action": { "type": "open_page", "url": "https://example.com" },
                        "results": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" } }),
                json!({ "type": "web_search_call", "id": "ws_" }),
                json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Serde is fast.", "annotations": [
                    { "type": "url_citation", "url": "https://serde.rs", "title": "Serde", "encrypted_index": "Eo8BCioIBxgCIiQ4", "cited_text": "fast" },
                    { "type": "url_citation", "url": "https://serde.rs", "title": "Serde" }
                ] }] }),
                user("Thanks."),
            ]),
        ),
        Case::new(
            "web-search-tools",
            "claude-opus-4-6",
            json!({
                "input": "Search.",
                "tools": [
                    { "type": "web_search", "max_uses": 3, "filters": { "allowed_domains": ["docs.rs", "serde.rs"] },
                      "user_location": { "type": "approximate", "city": "Paris", "timezone": "Europe/Paris" } },
                    { "type": "web_search", "name": "search_web", "external_web_access": true },
                    { "type": "web_search", "name": "offline", "external_web_access": false },
                    { "type": "web_search", "name": "string_false", "external_web_access": "false" },
                    { "type": "web_search", "name": "", "max_uses": 1.5, "filters": { "allowed_domains": "docs.rs" } },
                    { "type": "image_generation" },
                    { "type": "file_search", "vector_store_ids": ["vs_1"] },
                    { "type": "mcp", "name": "deepwiki", "server_label": "deepwiki" },
                    { "type": "mcp", "server_label": "unnamed" }
                ],
                "tool_choice": { "type": "web_search" }
            })
            .to_string(),
        ),
        Case::new(
            "additional-tools",
            "claude-opus-4-6",
            json!({
                "tools": [{ "type": "function", "name": "search", "description": "Top level." }],
                "input": [
                    user("Go."),
                    { "type": "additional_tools", "tools": [
                        { "type": "function", "name": "search", "description": "Shadowed." },
                        { "type": "function", "name": "extra", "parameters": { "type": "object" } },
                        { "type": "namespace", "name": "browser", "tools": [{ "type": "function", "name": "open" }] }
                    ]},
                    { "type": "additional_tools", "tools": "none" }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "long-and-colliding-tool-names",
            "claude-opus-4-6",
            json!({
                "input": "Go.",
                "tools": [
                    { "type": "function", "name": "a".repeat(70) },
                    { "type": "function", "name": format!("{}b", "a".repeat(69)) },
                    { "type": "function", "name": "mcp.server:read file" },
                    { "type": "function", "name": "mcp_server_read_file" },
                    { "type": "namespace", "name": "namespace_with_a_rather_long_name_for_shortening", "tools": [
                        { "type": "function", "name": "and_a_child_with_a_long_name_too" }
                    ]},
                    { "type": "function", "name": "dup" },
                    { "type": "function", "name": "dup", "description": "second" }
                ],
                "tool_choice": { "type": "function", "name": "a".repeat(70) }
            })
            .to_string(),
        ),
        Case::new(
            "images-and-files",
            "claude-opus-4-6",
            items(vec![json!({ "type": "message", "role": "user", "content": [
                { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo=" },
                { "type": "input_image", "image_url": "data:;base64,iVBORw0KGgo=" },
                { "type": "input_image", "image_url": "data:image/png;base64," },
                { "type": "input_image", "image_url": "data:image/png,iVBORw0KGgo=" },
                { "type": "input_image", "url": "https://example.com/cat.png" },
                { "type": "input_image", "image_url": "", "url": "https://example.com/fallback.png" },
                { "type": "input_image", "image_url": { "url": "https://example.com/nested.png" } },
                { "type": "input_file", "file_data": "data:application/pdf;base64,JVBERi0xLjQK", "filename": "a.pdf" },
                { "type": "input_file", "file_data": "data:text/plain;charset=utf-8;base64,aGk=" },
                { "type": "input_file", "file_data": "JVBERi0xLjQK" },
                { "type": "input_file", "file_id": "file_123" },
                { "type": "input_text", "text": "What are these?" }
            ], "cache_control": { "type": "ephemeral" } })]),
        ),
        Case::new(
            "refusals-and-roles",
            "claude-opus-4-6",
            items(vec![
                json!({ "content": [{ "type": "input_text", "text": "no role" }] }),
                json!({ "type": "", "role": "user", "content": "empty type" }),
                json!({ "type": "message", "role": "tool", "content": [{ "type": "output_text", "text": "odd role" }] }),
                json!({ "type": "message", "role": "assistant", "content": [
                    { "type": "refusal", "refusal": "I can't." },
                    { "type": "refusal", "refusal": "" },
                    { "type": "output_text", "text": "But here." }
                ] }),
                json!({ "type": "message", "role": "USER", "content": [{ "type": "input_text", "text": "upper" }] }),
                json!({ "type": "item_reference", "id": "msg_1" }),
            ]),
        ),
        Case::new(
            "text-format",
            "claude-opus-4-6",
            json!({
                "input": "Give JSON.",
                "text": { "format": { "type": "json_schema", "name": "answer", "strict": true,
                    "schema": { "type": "object", "properties": { "a": { "type": "number" } }, "required": ["a"] } }, "verbosity": "low" }
            })
            .to_string(),
        ),
        Case::new(
            "response-format",
            "claude-opus-4-6",
            json!({ "input": "Give JSON.", "response_format": { "type": "json_object" } }).to_string(),
        ),
        Case::new(
            "text-format-wins",
            "claude-opus-4-6",
            json!({ "input": "x", "text": { "format": { "type": "text" } }, "response_format": { "type": "json_object" } })
                .to_string(),
        ),
        Case::new(
            "service-tier-priority",
            "claude-opus-4-6",
            json!({ "input": "x", "service_tier": "priority" }).to_string(),
        ),
        Case::new(
            "service-tier-other",
            "claude-opus-4-6",
            json!({ "input": "x", "service_tier": " Priority " }).to_string(),
        ),
        Case::new(
            "metadata-user-id",
            "claude-opus-4-6",
            json!({ "input": "x", "metadata": { "user_id": "user-123" }, "user": "ignored" }).to_string(),
        ),
        Case::new(
            "user-field",
            "claude-opus-4-6",
            json!({ "input": "x", "metadata": { "user_id": " " }, "user": "user-456" }).to_string(),
        ),
        Case::new(
            "token-limits",
            "claude-sonnet-4-5-20250929",
            json!({ "input": "x", "max_output_tokens": 999999, "reasoning": { "effort": "high" } }).to_string(),
        ),
        Case::new(
            "summary-settings",
            "claude-opus-4-6",
            json!({ "input": "x", "reasoning": { "effort": "low", "summary": "detailed", "generate_summary": "concise" } })
                .to_string(),
        ),
        Case::new("array-body", "claude-opus-4-6", "[]"),
        Case::new("null-body", "claude-opus-4-6", "null"),
        Case::new("number-body", "claude-opus-4-6", "5"),
        Case::new("string-body", "claude-opus-4-6", r#""hi""#),
        Case::new("empty-object-body", "claude-opus-4-6", "{}"),
        Case::new("empty-model", "", r#"{"input":"hi"}"#),
        Case::new(
            "escaped-keys",
            "claude-opus-4-6",
            r#"{"instructions":"Rules","input":[{"type":"function_call","call_id":"c","name":"search","arguments":"{}"},{"type":"function_call_output","call_id":"c","output":"é"}]}"#
                .replace("instructions", &format!("instruc{}ions", escaped('t')))
                .replace("function_call_output", &format!("function_call{}output", escaped('_')))
                .replace('é', &escaped('é')),
        ),
        Case::new(
            "duplicate-keys",
            "claude-opus-4-6",
            r#"{"input":"hi","service_tier":"priority","service_tier":"flex"}"#,
        )
        .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
        Case::new(
            "deep-nesting",
            "claude-opus-4-6",
            format!(
                r#"{{"input":"hi","tools":[{{"type":"function","name":"deep","parameters":{}0{}}}]}}"#,
                "[".repeat(200),
                "]".repeat(200)
            ),
        )
        .known_difference("serde_json refuses JSON nested over 128 levels; gjson reads it"),
    ];

    // Every shape of tool_choice, against the client's tools.
    let choices = [
        ("auto", json!("auto")),
        ("required", json!("required")),
        ("none", json!("none")),
        ("padded-string", json!(" auto ")),
        (
            "function",
            json!({ "type": "function", "name": "get_weather" }),
        ),
        (
            "function-nested",
            json!({ "type": "function", "function": { "name": "search" } }),
        ),
        (
            "function-unknown",
            json!({ "type": "function", "name": "nope" }),
        ),
        (
            "function-namespaced",
            json!({ "type": "function", "name": "read_file", "namespace": "mcp__github" }),
        ),
        (
            "function-qualified",
            json!({ "type": "function", "name": "mcp__github__read_file" }),
        ),
        ("custom", json!({ "type": "custom", "name": "apply_patch" })),
        (
            "custom-nested",
            json!({ "type": "custom", "custom": { "name": "apply_patch" } }),
        ),
        (
            "allowed-tools",
            json!({ "type": "allowed_tools", "mode": "required", "tools": [{ "type": "function", "name": "search" }] }),
        ),
        ("web-search", json!({ "type": "web_search" })),
        ("typed-auto", json!({ "type": "auto" })),
        ("number", json!(5)),
    ];
    for (name, choice) in choices {
        cases.push(Case::new(
            format!("tool-choice-{name}"),
            "claude-opus-4-6",
            json!({ "input": "Go.", "tools": client_tools(), "tool_choice": choice }).to_string(),
        ));
    }

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
        "claude-test",
    ] {
        for (name, effort) in efforts {
            cases.push(Case::new(
                format!("effort-{name}-{model}"),
                model,
                json!({ "reasoning": { "effort": effort }, "max_output_tokens": 8192, "input": "hi" })
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

fn arguments(index: u64, partial: &str) -> Value {
    block_delta(
        index,
        json!({ "type": "input_json_delta", "partial_json": partial }),
    )
}

/// The client's request that the streams answer: its tools, for names to be
/// mapped back, and fields the final event repeats.
fn original_request() -> String {
    json!({
        "model": "claude-sonnet-4-6",
        "instructions": "You are terse.",
        "input": "Go.",
        "tools": client_tools(),
        "tool_choice": "auto",
        "reasoning": { "effort": "high", "summary": "auto" },
        "max_output_tokens": 4096,
        "temperature": 0.7,
        "top_p": 1.50,
        "parallel_tool_calls": false,
        "store": true,
        "previous_response_id": "resp_prev",
        "prompt_cache_key": "cache-1",
        "safety_identifier": "safe-1",
        "service_tier": "auto",
        "truncation": "disabled",
        "max_tool_calls": 5,
        "top_logprobs": 0,
        "text": { "format": { "type": "text" }, "verbosity": "medium" },
        "metadata": { "z": "last", "a": "first" },
        "user": "user-1"
    })
    .to_string()
}

/// Named Claude event streams, and whether they answer
/// [`original_request`]. None has two tool calls open at once, which
/// upstream closes in random order.
fn event_streams() -> Vec<(&'static str, bool, Vec<Value>)> {
    let stop = json!({ "type": "message_stop" });
    let patch =
        r#"{"input":"*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch"}"#;
    vec![
        (
            "text",
            true,
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
            "text-without-request",
            false,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "Hi" })),
                block_stop(0),
                block_start(1, json!({ "type": "text", "text": "" })),
                block_delta(1, json!({ "type": "text_delta", "text": " there" })),
                block_stop(1),
                message_delta("end_turn", json!({ "output_tokens": 2 })),
                stop.clone(),
            ],
        ),
        (
            "tool-use",
            true,
            vec![
                message_start(json!({ "input_tokens": 10, "output_tokens": 1 })),
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "Checking." })),
                block_stop(0),
                block_start(
                    1,
                    json!({ "type": "tool_use", "id": "toolu_01A", "name": "get_weather", "input": {} }),
                ),
                arguments(1, "{\"city\":"),
                arguments(1, "\"Paris\"}"),
                block_stop(1),
                block_start(
                    2,
                    json!({ "type": "tool_use", "id": "toolu_01B", "name": "mcp__github__read_file", "input": {} }),
                ),
                arguments(2, "{\"path\":\"a\"}"),
                block_stop(2),
                message_delta("tool_use", json!({ "output_tokens": 40 })),
                stop.clone(),
            ],
        ),
        (
            "tool-use-unknown-name",
            true,
            vec![
                message_start(json!({ "input_tokens": 1 })),
                block_start(
                    0,
                    json!({ "type": "tool_use", "id": "toolu_01A", "name": "not_declared", "input": { "snapshot": true } }),
                ),
                block_stop(0),
                message_delta("tool_use", json!({ "output_tokens": 3 })),
                stop.clone(),
            ],
        ),
        (
            "apply-patch",
            true,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(
                    0,
                    json!({ "type": "tool_use", "id": "toolu_01P", "name": "apply_patch", "input": {} }),
                ),
                arguments(0, &patch[..20]),
                arguments(0, &patch[20..]),
                block_stop(0),
                message_delta("tool_use", json!({ "output_tokens": 30 })),
                stop.clone(),
            ],
        ),
        (
            "apply-patch-bad-input",
            true,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(
                    0,
                    json!({ "type": "tool_use", "id": "toolu_01P", "name": "apply_patch", "input": {} }),
                ),
                arguments(0, r#"{"input":5}"#),
                block_stop(0),
                message_delta("tool_use", json!({ "output_tokens": 30 })),
                stop.clone(),
            ],
        ),
        (
            "apply-patch-cut-short",
            true,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(
                    0,
                    json!({ "type": "tool_use", "id": "toolu_01P", "name": "apply_patch", "input": {} }),
                ),
                arguments(0, &patch[..30]),
            ],
        ),
        (
            "thinking",
            true,
            vec![
                message_start(json!({ "input_tokens": 10, "output_tokens": 1 })),
                block_start(0, json!({ "type": "thinking", "thinking": "" })),
                block_delta(
                    0,
                    json!({ "type": "thinking_delta", "thinking": "Let me think." }),
                ),
                block_delta(
                    0,
                    json!({ "type": "signature_delta", "signature": claude_signature("claude-sonnet-4-6") }),
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
                message_delta("end_turn", json!({ "output_tokens": 20 })),
                stop.clone(),
            ],
        ),
        (
            "web-search",
            true,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(
                    0,
                    json!({ "type": "server_tool_use", "id": "srvtoolu_01A", "name": "web_search", "input": {} }),
                ),
                arguments(0, "{\"query\":\"rust "),
                arguments(0, "serde\"}"),
                block_stop(0),
                block_start(
                    1,
                    json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_01A", "content": [
                        { "type": "web_search_result", "url": "https://serde.rs", "title": "Serde", "encrypted_content": "EqgfCioIARgBIiQ3", "page_age": "2 days ago" }
                    ] }),
                ),
                block_stop(1),
                block_start(2, json!({ "type": "text", "text": "" })),
                block_delta(
                    2,
                    json!({ "type": "citations_delta", "citation": {
                        "type": "web_search_result_location", "url": "https://serde.rs", "title": "Serde",
                        "encrypted_index": "Eo8BCioIBxgCIiQ4", "cited_text": "Serde is fast."
                    } }),
                ),
                block_delta(2, json!({ "type": "text_delta", "text": "It is fast." })),
                block_stop(2),
                message_delta(
                    "end_turn",
                    json!({ "output_tokens": 9, "server_tool_use": { "web_search_requests": 1 } }),
                ),
                stop.clone(),
            ],
        ),
        (
            "web-search-error",
            true,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(
                    0,
                    json!({ "type": "server_tool_use", "id": "srvtoolu_01A", "name": "web_search", "input": { "query": "x" } }),
                ),
                block_stop(0),
                block_start(
                    1,
                    json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_01A",
                            "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" } }),
                ),
                block_stop(1),
                message_delta("end_turn", json!({ "output_tokens": 1 })),
                stop.clone(),
            ],
        ),
        (
            "cached-usage",
            true,
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
            "overflowing-usage",
            false,
            vec![
                message_start(
                    json!({ "input_tokens": i64::MAX, "cache_read_input_tokens": i64::MAX }),
                ),
                message_delta("end_turn", json!({ "output_tokens": i64::MAX })),
                stop.clone(),
            ],
        ),
        (
            "max-tokens",
            true,
            vec![
                message_start(json!({ "input_tokens": 10 })),
                block_start(0, json!({ "type": "text", "text": "" })),
                block_delta(0, json!({ "type": "text_delta", "text": "Cut" })),
                block_stop(0),
                message_delta("max_tokens", json!({ "output_tokens": 20 })),
                stop.clone(),
            ],
        ),
        (
            "refusal",
            false,
            vec![
                message_start(json!({ "input_tokens": 1 })),
                message_delta("refusal", json!({ "output_tokens": 0 })),
                stop.clone(),
            ],
        ),
        (
            "error",
            true,
            vec![
                message_start(json!({ "input_tokens": 1 })),
                json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
            ],
        ),
        (
            "no-message-start",
            false,
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
        .map(|(name, answers, events)| {
            let lines = events
                .iter()
                .map(|event| format!("data: {event}"))
                .collect();
            let request = if answers {
                original_request()
            } else {
                String::new()
            };
            Case::new(name, "claude-sonnet-4-6", request).with_events(lines)
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
    cases.push(Case {
        translated_request: json!({ "model": "claude-opus-4-6" }).to_string(),
        ..Case::new("translated-request-only", "claude-sonnet-4-6", "").with_events(vec![
            format!("data: {}", message_start(json!({ "input_tokens": 1 }))),
            format!(
                "data: {}",
                message_delta("end_turn", json!({ "output_tokens": 1 }))
            ),
            format!("data: {stop}", stop = json!({ "type": "message_stop" })),
        ])
    });
    cases
}

/// The same streams as SSE bodies, for the non-streaming translator.
pub fn finals() -> Vec<Case> {
    event_streams()
        .into_iter()
        .map(|(name, answers, events)| {
            let body: String = events
                .iter()
                .map(|event| {
                    let kind = event["type"].as_str().unwrap_or_default();
                    format!("event: {kind}\ndata: {event}\n\n")
                })
                .collect();
            let request = if answers {
                original_request()
            } else {
                String::new()
            };
            Case::new(name, "claude-sonnet-4-6", request).with_events(vec![body])
        })
        .chain([
            Case::new("empty-body", "claude-sonnet-4-6", "").with_events(vec![String::new()]),
            Case::new("no-body", "claude-sonnet-4-6", ""),
        ])
        .collect()
}
