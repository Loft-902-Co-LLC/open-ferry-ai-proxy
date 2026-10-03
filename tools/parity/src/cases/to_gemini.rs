//! Hand-written cases for the translators to a Gemini upstream: the
//! passthrough, Claude and Chat Completions ones.

use serde_json::{Value, json};

use super::{Case, escaped};
use crate::generate::to_gemini::json_arguments;

const MODEL: &str = "gemini-2.5-pro";

/// Gemini requests the generator is unlikely to build in one piece, half of
/// them for a client that streams.
pub fn gemini_requests() -> Vec<Case> {
    let call =
        |name: &str| json!({ "functionCall": { "name": name, "args": { "city": "Paris" } } });
    let response = |name: Value| json!({ "functionResponse": { "name": name, "response": { "result": "18°C" } } });
    let schema = json!({ "type": "object", "properties": { "city": { "type": "string" } } });
    let cases = vec![
        Case::new(
            "backfill-response-names",
            MODEL,
            json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "Weather in Paris and Rome?" }] },
                    { "role": "model", "parts": [call("get_weather"), call("search"), { "text": "Looking." }] },
                    { "role": "user", "parts": [response(json!("")), response(json!("  ")), response(json!("kept"))] },
                    { "role": "user", "parts": [response(json!(""))] }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "backfill-missing-names",
            MODEL,
            json!({
                "contents": [
                    { "role": "model", "parts": [call("get_weather")] },
                    { "parts": [{ "functionResponse": { "response": { "result": 1 } } }, response(Value::Null)] },
                    { "role": "model", "parts": [{ "text": "Done." }] }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "backfill-more-responses-than-calls",
            MODEL,
            json!({
                "contents": [
                    { "role": "model", "parts": [call("a")] },
                    { "role": "function", "parts": [response(json!("")), response(json!("")), response(json!(""))] }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "backfill-names-not-text",
            MODEL,
            serde_json::to_string_pretty(&json!({
                "contents": [
                    { "role": "model", "parts": [
                        { "functionCall": { "name": { "a": [1, 2] }, "args": {} } },
                        { "functionCall": { "name": 5, "args": {} } },
                        { "functionCall": { "args": {} } }
                    ]},
                    { "role": "user", "parts": [response(json!("")), response(json!("")), response(json!(""))] }
                ]
            }))
            .expect("serializable"),
        ),
        Case::new(
            "object-contents",
            MODEL,
            json!({
                "contents": {
                    "0": { "parts": [{ "text": "Hi" }] },
                    "1": { "parts": [{ "text": "Hello" }] },
                    "2": { "role": "user", "parts": [{ "text": "Bye" }] }
                }
            })
            .to_string(),
        ),
        Case::new(
            "no-contents",
            MODEL,
            json!({
                "generationConfig": { "responseSchema": { "type": "OBJECT" } },
                "tools": [{ "functionDeclarations": [{ "name": "f", "parameters": schema }] }]
            })
            .to_string(),
        ),
        Case::new(
            "both-declaration-keys",
            MODEL,
            json!({
                "contents": [{ "role": "user", "parts": [{ "text": "Hi" }] }],
                "tools": [
                    { "function_declarations": [{ "name": "a", "parameters": schema }], "functionDeclarations": [{ "name": "b", "parameters": schema }] },
                    { "functionDeclarations": [{ "name": "c", "parameters": schema, "parametersJsonSchema": { "type": "object" } }] },
                    { "googleSearch": {} }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "both-schema-keys",
            MODEL,
            json!({
                "contents": [{ "role": "user", "parts": [{ "text": "Hi" }] }],
                "generationConfig": {
                    "responseSchema": { "type": "OBJECT", "properties": { "a": { "type": "STRING" } } },
                    "responseJsonSchema": { "type": "object" },
                    "responseMimeType": "application/json"
                },
                "generation_config": { "responseSchema": { "type": "OBJECT" } }
            })
            .to_string(),
        ),
        Case::new(
            "thought-signatures",
            MODEL,
            json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "Hi", "thoughtSignature": "c2lnbmF0dXJl" }] },
                    { "role": "model", "parts": [
                        { "text": "Thinking", "thought": true, "thoughtSignature": "not base64!" },
                        { "functionCall": { "name": "f", "args": {} }, "thoughtSignature": "" },
                        { "functionCall": { "name": "g", "args": {} }, "thought_signature": "EqQBCkgIBxAB" },
                        { "text": "Done", "thoughtSignature": 5 }
                    ]}
                ]
            })
            .to_string(),
        ),
        Case::new(
            "roles-to-fix",
            MODEL,
            json!({
                "contents": [
                    { "parts": [{ "text": "a" }] },
                    { "parts": [{ "text": "b" }] },
                    { "role": "", "parts": [{ "text": "c" }] },
                    { "role": "MODEL", "parts": [{ "text": "d" }] },
                    { "role": 5, "parts": [{ "text": "e" }] },
                    "not a content",
                    { "parts": [{ "text": "f" }] }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "pretty-and-escaped",
            MODEL,
            serde_json::to_string_pretty(&json!({
                "contents": [{ "role": "user", "parts": [{ "text": "café 🚀 / done" }] }],
                "systemInstruction": { "parts": [{ "text": "Be terse." }] }
            }))
            .expect("serializable")
            .replace('é', &escaped('é'))
            .replace('🚀', &escaped('🚀')),
        ),
    ];
    let mut cases: Vec<Case> = cases;
    for (name, settings) in [
        (
            "safety-settings-given",
            r#""safetySettings":[{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_ONLY_HIGH"}]"#,
        ),
        ("safety-settings-empty", r#""safetySettings":[]"#),
        ("safety-settings-null", r#""safetySettings":null"#),
        ("safety-settings-text", r#""safetySettings":"x""#),
        (
            "safety-settings-snake-case",
            r#""safety_settings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"}]"#,
        ),
    ] {
        cases.push(Case::new(
            name,
            MODEL,
            format!(r#"{{"contents":[{{"role":"user","parts":[{{"text":"Hi"}}]}}],{settings}}}"#),
        ));
    }
    for (name, body) in [
        ("contents-number", r#"{"contents":5}"#),
        ("contents-text", r#"{"contents":"hi"}"#),
        ("contents-null", r#"{"contents":null}"#),
        ("contents-empty", r#"{"contents":[]}"#),
        ("contents-empty-object", r#"{"contents":{}}"#),
        ("empty-object", "{}"),
        ("array-body", "[]"),
        ("null-body", "null"),
        ("number-body", "5"),
        ("string-body", r#""hi""#),
    ] {
        cases.push(Case::new(name, MODEL, body));
    }
    with_streaming(cases)
}

/// The Claude requests written for the Codex translator, for a Gemini model,
/// and requests aimed at what only this translator reads. Each runs in both
/// the plain and compatibility suites, half of them for a client that
/// streams.
pub fn claude_requests() -> Vec<Case> {
    let tools = json!([
        { "name": "Bash", "description": "Run a command.", "input_schema": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] } },
        { "name": "web search", "input_schema": { "type": "object", "properties": { "q": { "type": "string" } }, "additionalProperties": false } },
        { "name": "strict_tool", "strict": true, "input_schema": { "type": "object", "properties": {} } }
    ]);
    let user = |content: Value| json!({ "role": "user", "content": content });
    let assistant = |content: Value| json!({ "role": "assistant", "content": content });
    let request = |model: &str, fields: Value| {
        let mut request = json!({ "model": "claude-sonnet-4-5", "max_tokens": 1024, "messages": [user(json!("Hi"))] });
        if let (Value::Object(request), Value::Object(fields)) = (&mut request, fields) {
            request.extend(fields);
        }
        Case::new("", model, request.to_string())
    };
    let named = |name: &str, case: Case| Case {
        name: name.to_owned(),
        ..case
    };

    // Their known differences are the Codex translator's; of them only the
    // duplicate key matters here.
    let mut cases: Vec<Case> = super::hand_written()
        .into_iter()
        .map(|case| Case {
            model: MODEL.to_owned(),
            known_difference: (case.name == "duplicate-keys")
                .then_some("gjson reads the first duplicate key; serde_json keeps the last"),
            ..case
        })
        .collect();
    cases.extend([
        named(
            "adaptive-thinking-with-effort",
            request(
                MODEL,
                json!({ "thinking": { "type": "adaptive" }, "output_config": { "effort": " High " } }),
            ),
        ),
        named(
            "adaptive-thinking-catalog-budget",
            request(MODEL, json!({ "thinking": { "type": "adaptive" } })),
        ),
        named(
            "adaptive-thinking-unknown-model",
            request("unknown-model", json!({ "thinking": { "type": "auto" } })),
        ),
        named(
            "adaptive-thinking-effort-not-text",
            request(
                "gemini-3-pro-preview",
                json!({ "thinking": { "type": "adaptive" }, "output_config": { "effort": 5 } }),
            ),
        ),
        named(
            "enabled-thinking-budget",
            request(
                MODEL,
                json!({ "thinking": { "type": "enabled", "budget_tokens": 2048 } }),
            ),
        ),
        named(
            "enabled-thinking-budget-as-text",
            request(
                MODEL,
                json!({ "thinking": { "type": "enabled", "budget_tokens": "2048" } }),
            ),
        ),
        named(
            "enabled-thinking-fractional-budget",
            request(
                MODEL,
                json!({ "thinking": { "type": "enabled", "budget_tokens": 1024.9 } }),
            ),
        ),
        named(
            "disabled-thinking",
            request(MODEL, json!({ "thinking": { "type": "disabled" } })),
        ),
        named(
            "sampling",
            request(
                MODEL,
                json!({ "temperature": 0.2, "top_p": 0.9, "top_k": 40, "stop_sequences": ["END"] }),
            ),
        ),
        named(
            "sampling-not-numbers",
            request(
                MODEL,
                json!({ "temperature": "0.2", "top_p": null, "top_k": true }),
            ),
        ),
        named(
            "tools-and-choice-any",
            request(
                MODEL,
                json!({ "tools": tools, "tool_choice": { "type": "any" } }),
            ),
        ),
        named(
            "tool-choice-tool",
            request(
                MODEL,
                json!({ "tools": tools, "tool_choice": { "type": "tool", "name": "web search" } }),
            ),
        ),
        named(
            "tool-choice-none",
            request(
                MODEL,
                json!({ "tools": tools, "tool_choice": { "type": "none" } }),
            ),
        ),
        named(
            "tool-choice-auto",
            request(
                MODEL,
                json!({ "tools": tools, "tool_choice": { "type": "auto", "disable_parallel_tool_use": true } }),
            ),
        ),
        named(
            "web-search-tool",
            request(
                MODEL,
                json!({ "tools": [{ "type": "web_search_20250305", "name": "web_search", "max_uses": 3 }] }),
            ),
        ),
        named(
            "tool-turn",
            request(
                MODEL,
                json!({
                    "tools": tools,
                    "messages": [
                        user(json!("List files, then search.")),
                        assistant(json!([
                            { "type": "thinking", "thinking": "Let me look.", "signature": "c2lnbmF0dXJl" },
                            { "type": "text", "text": "Listing." },
                            { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } },
                            { "type": "tool_use", "id": "toolu_2", "name": "web search", "input": { "q": "x" } }
                        ])),
                        user(json!([
                            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\nb.txt" },
                            { "type": "tool_result", "tool_use_id": "toolu_2", "content": [{ "type": "text", "text": "{\"$ref\":\"#/x\"}" }], "is_error": true },
                            { "type": "text", "text": "Thanks." }
                        ]))
                    ]
                }),
            ),
        ),
        named(
            "tool-result-unknown-id",
            request(
                MODEL,
                json!({
                    "messages": [
                        user(json!([{ "type": "tool_result", "tool_use_id": "toolu_missing", "content": { "a": 1 } }]))
                    ]
                }),
            ),
        ),
        named(
            "system-mid-conversation",
            request(
                MODEL,
                json!({
                    "system": [{ "type": "text", "text": "You are terse." }, { "type": "text", "text": "" }],
                    "messages": [
                        user(json!("Hi")),
                        { "role": "system", "content": "Mind the time." },
                        assistant(json!("Hello")),
                        user(json!("Bye"))
                    ]
                }),
            ),
        ),
        named(
            "system-as-text",
            request(MODEL, json!({ "system": "Be brief." })),
        ),
        named(
            "image-and-document",
            request(
                MODEL,
                json!({
                    "messages": [user(json!([
                        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } },
                        { "type": "image", "source": { "type": "url", "url": "https://example.com/a.png" } },
                        { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "JVBERi0=" } },
                        { "type": "text", "text": "Describe these." }
                    ]))]
                }),
            ),
        ),
        named(
            "thinking-blocks",
            request(
                "gemini-3-pro-preview",
                json!({
                    "thinking": { "type": "enabled", "budget_tokens": 4096 },
                    "messages": [
                        user(json!("Hi")),
                        assistant(json!([
                            { "type": "thinking", "thinking": "Plan.", "signature": "EqQBCkgIBxAB" },
                            { "type": "redacted_thinking", "data": "abc" },
                            { "type": "thinking", "thinking": "", "signature": "" },
                            { "type": "text", "text": "Hello" }
                        ])),
                        user(json!("Again"))
                    ]
                }),
            ),
        ),
        named(
            "output-format",
            request(
                MODEL,
                json!({ "output_config": { "format": { "type": "json_schema", "schema": { "type": "object", "properties": { "a": { "type": "string" } } } } } }),
            ),
        ),
        named(
            "trailing-assistant",
            request(
                MODEL,
                json!({ "messages": [user(json!("Hi")), assistant(json!("Hel"))] }),
            ),
        ),
    ]);
    with_streaming(cases)
}

/// The Chat Completions requests written for the Codex translator, for a
/// Gemini model and with every tool call's arguments made JSON, and requests
/// aimed at what only this translator reads, half of them for a client that
/// streams.
pub fn chat_requests() -> Vec<Case> {
    let user = |content: Value| json!({ "role": "user", "content": content });
    let call = |id: &str, name: &str, arguments: &str| json!({ "id": id, "type": "function", "function": { "name": name, "arguments": arguments } });
    let function = |name: &str| json!({ "type": "function", "function": { "name": name, "parameters": { "type": "object", "properties": { "q": { "type": "string" } } } } });
    let request = |name: &str, fields: Value| {
        let mut request = json!({ "model": "gpt-5", "messages": [user(json!("Hi"))] });
        if let (Value::Object(request), Value::Object(fields)) = (&mut request, fields) {
            request.extend(fields);
        }
        Case::new(name, MODEL, request.to_string())
    };

    // Their known differences are the Codex translator's; none of them
    // matters here.
    let mut cases: Vec<Case> = super::chat::requests()
        .into_iter()
        .filter_map(|case| {
            let mut request: Value = serde_json::from_str(&case.request).ok()?;
            let original = request.clone();
            json_arguments(&mut request);
            let text = if request == original {
                case.request
            } else {
                request.to_string()
            };
            Some(Case {
                model: MODEL.to_owned(),
                request: text,
                known_difference: None,
                ..case
            })
        })
        .collect();
    cases.extend([
        request(
            "system-only",
            json!({ "messages": [{ "role": "system", "content": "Be terse." }] }),
        ),
        request(
            "system-developer-and-later",
            json!({ "messages": [
                { "role": "system", "content": "Be terse." },
                { "role": "developer", "content": [{ "type": "text", "text": "Use metric." }] },
                user(json!("Hi")),
                { "role": "developer", "content": "Mind the time." },
                { "role": "assistant", "content": "Hello" },
                { "role": "system", "content": [{ "type": "text", "text": "Later." }, { "type": "image_url", "image_url": { "url": "data:image/png;base64,QQ==" } }] },
                user(json!("Bye"))
            ] }),
        ),
        request("reasoning-effort-auto", json!({ "reasoning_effort": " Auto " })),
        request("reasoning-effort-high", json!({ "reasoning_effort": "high" })),
        request(
            "sampling-and-candidates",
            json!({ "temperature": 0.3, "top_p": 0.8, "top_k": 20, "max_completion_tokens": 512, "n": 3, "stop": ["END"] }),
        ),
        request(
            "max-tokens-wins",
            json!({ "max_tokens": 100, "max_completion_tokens": 200, "n": 1 }),
        ),
        request(
            "response-format-json-schema",
            json!({
                "generationConfig": { "responseSchema": { "type": "OBJECT" }, "temperature": 1 },
                "response_format": { "type": "json_schema", "json_schema": { "name": "a", "strict": true, "schema": { "type": "object", "properties": { "a": { "type": "string" } } } } }
            }),
        ),
        request(
            "response-format-json-object",
            json!({ "response_format": { "type": "json_object" } }),
        ),
        request(
            "modalities-and-image-config",
            json!({ "modalities": ["text", "image"], "image_config": { "aspect_ratio": "16:9", "image_size": "2K" } }),
        ),
        request(
            "media-parts",
            json!({ "messages": [user(json!([
                { "type": "text", "text": "Look:" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" } },
                { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
                { "type": "video_url", "video_url": { "url": "data:video/mp4;base64,AAAAIGZ0eXA=" } },
                { "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": "mp3" } },
                { "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": "pcm16" } },
                { "type": "file", "file": { "filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBERi0=" } },
                { "type": "file", "file": { "filename": "notes.txt", "file_data": "aGVsbG8=" } },
                { "type": "file", "file": { "file_id": "file-abc" } }
            ]))] }),
        ),
        request(
            "assistant-reasoning-and-signatures",
            json!({
                "tools": [function("get_weather"), function("web search")],
                "messages": [
                    user(json!("Weather?")),
                    {
                        "role": "assistant",
                        "reasoning_content": "Let me check.",
                        "content": "Checking.",
                        "tool_calls": [
                            { "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }, "extra_content": { "google": { "thought_signature": "c2lnbmF0dXJl" } } },
                            { "id": "call_2", "type": "function", "function": { "name": "web search", "arguments": "{}", "extra_content": { "google": { "thought_signature": "EqQBCkgIBxAB" } } } },
                            { "id": "call_3", "type": "function", "function": { "name": "get_weather", "arguments": "[1]" }, "thoughtSignature": "c2Vjb25k" },
                            { "id": "call_4", "type": "function", "function": { "name": "get_weather", "arguments": "{\"a\":1.50}" }, "thought_signature": "" }
                        ]
                    },
                    { "role": "tool", "tool_call_id": "call_2", "content": "found" },
                    { "role": "tool", "tool_call_id": "call_1", "content": [{ "type": "text", "text": "18°C" }] },
                    { "role": "tool", "tool_call_id": "call_3", "content": { "a": 1 } },
                    { "role": "tool", "tool_call_id": "call_unknown", "content": "lost" },
                    { "role": "assistant", "content": "Done." },
                    { "role": "tool", "tool_call_id": "call_4", "content": "late" },
                    user(json!("Thanks"))
                ]
            }),
        ),
        request(
            "trailing-assistant",
            json!({ "messages": [user(json!("Hi")), { "role": "assistant", "content": "Hel" }] }),
        ),
        request(
            "google-tools",
            json!({ "tools": [
                function("lookup"),
                { "google_search": {} },
                { "code_execution": {} },
                { "url_context": {} },
                { "type": "function", "function": { "name": "schema_array", "parametersJsonSchema": [1] } },
                { "type": "function", "function": { "name": "schema_given", "parametersJsonSchema": { "type": "object" } } },
                { "type": "function", "function": { "name": "no_schema" } },
                { "type": "function", "strict": true, "function": { "name": "strict_tool", "parameters": { "type": "object" } } }
            ] }),
        ),
        request(
            "allowed-tools-required",
            json!({
                "tools": [function("a"), function("b c")],
                "tool_choice": { "type": "allowed_tools", "allowed_tools": { "mode": "required", "tools": [{ "type": "function", "function": { "name": "b c" } }] } }
            }),
        ),
        request(
            "allowed-tools-strict",
            json!({
                "tools": [{ "type": "function", "strict": true, "function": { "name": "a", "parameters": { "type": "object" } } }],
                "tool_choice": { "type": "allowed_tools", "mode": "auto", "tools": [{ "type": "function", "name": "a" }] }
            }),
        ),
        request(
            "tool-choice-function",
            json!({ "tools": [function("web search")], "tool_choice": { "type": "function", "function": { "name": "web search" } } }),
        ),
        request(
            "tool-choice-required",
            json!({ "tools": [function("a")], "tool_choice": "required" }),
        ),
        request(
            "duplicate-sanitized-names",
            json!({ "tools": [function("web search"), function("web_search")], "tool_choice": "auto" }),
        ),
        request(
            "parallel-tool-calls-off",
            json!({
                "tools": [function("a")],
                "tool_choice": { "type": "allowed_tools", "allowed_tools": { "mode": "required", "tools": [{ "type": "function", "function": { "name": "a" } }] } },
                "parallel_tool_calls": false
            }),
        ),
        request(
            "tool-call-signature-bypass",
            json!({ "messages": [
                user(json!("Hi")),
                { "role": "assistant", "tool_calls": [call("call_1", "f", "{}")] },
                { "role": "tool", "tool_call_id": "call_1", "content": "ok" }
            ] }),
        ),
        request(
            "empty-arguments",
            json!({ "messages": [
                user(json!("Hi")),
                { "role": "assistant", "tool_calls": [call("call_1", "f", "")] },
                { "role": "tool", "tool_call_id": "call_1", "content": "ok" }
            ] }),
        )
        .known_difference("upstream copies arguments that aren't JSON into its output as they are"),
    ]);
    with_streaming(cases)
}

/// Half of `cases` for a client that streams.
fn with_streaming(cases: Vec<Case>) -> Vec<Case> {
    cases
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let stream = index % 2 == 0;
            case.with_options(json!({ "stream": stream }))
        })
        .collect()
}

// --- Responses ---

fn text(text: &str) -> Value {
    json!({ "text": text })
}

fn thought(text: &str) -> Value {
    json!({ "text": text, "thought": true })
}

fn signature(signature: &str) -> Value {
    json!({ "thoughtSignature": signature })
}

fn function_call(name: &str, args: Value) -> Value {
    json!({ "functionCall": { "name": name, "args": args } })
}

fn usage(prompt: i64, candidates: i64, thoughts: i64, cached: i64) -> Value {
    json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": candidates,
        "thoughtsTokenCount": thoughts,
        "cachedContentTokenCount": cached,
        "totalTokenCount": prompt + candidates + thoughts
    })
}

/// A chunk with one candidate holding `parts`.
fn chunk(parts: Vec<Value>) -> Value {
    json!({
        "candidates": [{ "content": { "role": "model", "parts": parts }, "index": 0 }],
        "modelVersion": "gemini-2.5-pro",
        "responseId": "resp_abc"
    })
}

/// A last chunk: `parts`, the finish reason and usage.
fn last(parts: Vec<Value>, reason: &str, usage: Value) -> Value {
    json!({
        "candidates": [{ "content": { "role": "model", "parts": parts }, "finishReason": reason, "index": 0 }],
        "usageMetadata": usage,
        "modelVersion": "gemini-2.5-pro",
        "responseId": "resp_abc"
    })
}

fn with(mut value: Value, key: &str, field: Value) -> Value {
    value[key] = field;
    value
}

/// Gemini streams any translator reads, as chunks.
fn chunk_streams() -> Vec<(&'static str, Vec<Value>)> {
    let small = usage(10, 5, 0, 0);
    vec![
        (
            "text",
            vec![
                chunk(vec![text("Hel")]),
                chunk(vec![text("lo")]),
                last(vec![], "STOP", small.clone()),
            ],
        ),
        (
            "text-in-last-chunk",
            vec![last(vec![text("Hello")], "STOP", small.clone())],
        ),
        (
            "thinking-then-text",
            vec![
                chunk(vec![thought("Let me think.")]),
                chunk(vec![signature("c2lnbmF0dXJl")]),
                chunk(vec![text("Answer.")]),
                last(vec![], "STOP", usage(10, 5, 7, 0)),
            ],
        ),
        (
            "signed-text",
            vec![
                chunk(vec![
                    json!({ "text": "Signed.", "thoughtSignature": "c2lnbmF0dXJl" }),
                ]),
                chunk(vec![
                    json!({ "text": "", "thoughtSignature": "EqQBCkgIBxAB" }),
                ]),
                last(vec![text("Plain.")], "STOP", small.clone()),
            ],
        ),
        (
            "tool-call",
            vec![
                chunk(vec![text("Searching.")]),
                last(
                    vec![function_call("web_search", json!({ "q": "café" }))],
                    "STOP",
                    small.clone(),
                ),
            ],
        ),
        (
            "parallel-tool-calls",
            vec![last(
                vec![
                    function_call("Bash", json!({ "command": "ls" })),
                    function_call("_Edit", json!({})),
                    function_call("undeclared", json!([1])),
                ],
                "STOP",
                small.clone(),
            )],
        ),
        (
            "tool-call-continued",
            vec![
                chunk(vec![function_call("Bash", json!({ "command": "ls" }))]),
                chunk(vec![
                    json!({ "functionCall": { "args": { "more": true } } }),
                ]),
                chunk(vec![
                    json!({ "functionCall": { "name": "", "args": { "x": 1 } } }),
                ]),
                last(vec![], "STOP", small.clone()),
            ],
        ),
        (
            "tool-call-without-args",
            vec![last(
                vec![json!({ "functionCall": { "name": "Bash" } })],
                "STOP",
                small.clone(),
            )],
        ),
        (
            "max-tokens-with-cache",
            vec![
                chunk(vec![text("Long")]),
                last(vec![text(" answer")], "MAX_TOKENS", usage(100, 50, 25, 40)),
            ],
        ),
        (
            "cache-beyond-prompt",
            vec![last(vec![text("Hi")], "STOP", usage(10, 5, 0, 40))],
        ),
        (
            "usage-without-finish",
            vec![
                chunk(vec![text("Hi")]),
                with(chunk(vec![text("!")]), "usageMetadata", small.clone()),
            ],
        ),
        (
            "finish-without-usage",
            vec![
                chunk(vec![text("Hi")]),
                json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }] }),
            ],
        ),
        (
            "usage-only-chunk",
            vec![
                chunk(vec![text("Hi")]),
                json!({ "candidates": [{ "finishReason": "STOP" }] }),
                json!({ "usageMetadata": small }),
            ],
        ),
        (
            "finish-reason-in-args",
            vec![
                chunk(vec![function_call(
                    "Bash",
                    json!({ "finishReason": "STOP" }),
                )]),
                with(chunk(vec![]), "usageMetadata", usage(3, 2, 0, 0)),
            ],
        ),
        ("safety", vec![last(vec![], "SAFETY", usage(3, 0, 0, 0))]),
        (
            "two-candidates",
            vec![
                json!({ "candidates": [
                    { "content": { "parts": [text("A")] }, "index": 0 },
                    { "content": { "parts": [text("B"), function_call("Bash", json!({}))] }, "index": 1 }
                ], "createTime": "2025-01-02T03:04:05.5Z" }),
                json!({ "candidates": [
                    { "content": { "parts": [] }, "finishReason": "STOP", "index": 0 },
                    { "content": { "parts": [] }, "finishReason": "MAX_TOKENS", "index": 1 }
                ], "usageMetadata": usage(10, 5, 0, 0) }),
            ],
        ),
        (
            "inline-data",
            vec![last(
                vec![
                    json!({ "inlineData": { "mimeType": "image/png", "data": "iVBORw0KGgo=" } }),
                    json!({ "inline_data": { "mime_type": "image/jpeg", "data": "/9j/4AAQ" } }),
                    json!({ "inlineData": { "data": "UklGRg==" } }),
                    json!({ "inlineData": { "mimeType": "image/webp", "data": "" } }),
                ],
                "STOP",
                small.clone(),
            )],
        ),
        (
            "audio-transcription",
            vec![last(
                vec![json!({ "audioTranscription": { "text": "spoken words" } })],
                "STOP",
                small.clone(),
            )],
        ),
        (
            "create-times",
            vec![
                with(
                    chunk(vec![text("a")]),
                    "createTime",
                    json!("2025-01-02T03:04:05Z"),
                ),
                with(chunk(vec![text("b")]), "createTime", json!("not a time")),
                with(
                    chunk(vec![text("c")]),
                    "createTime",
                    json!("2024-02-29T12:00:00.123456789-07:30"),
                ),
                with(
                    last(vec![], "STOP", small.clone()),
                    "createTime",
                    json!("2025-02-29T00:00:00Z"),
                ),
            ],
        ),
        (
            "odd-parts",
            vec![last(
                vec![
                    json!(5),
                    Value::Null,
                    json!({}),
                    json!({ "text": 5 }),
                    json!({ "functionCall": null }),
                    json!({ "thought": true }),
                ],
                "STOP",
                small.clone(),
            )],
        ),
        (
            "no-candidates",
            vec![
                json!({ "modelVersion": "gemini-2.5-pro" }),
                json!({ "candidates": [] }),
            ],
        ),
        (
            "escaped",
            vec![last(
                vec![text("café ☕ \u{2028} </script>")],
                "stop",
                small,
            )],
        ),
    ]
}

/// The client's original request: tools declared by names Gemini doesn't
/// accept as they are.
fn original_request(chat: bool) -> String {
    let names = ["web search", "Bash", "__Edit", "outil_météo"];
    let tools: Vec<Value> = names
        .iter()
        .map(|name| {
            if chat {
                json!({ "name": name, "type": "function", "function": { "name": name } })
            } else {
                json!({ "name": name, "input_schema": { "type": "object" } })
            }
        })
        .collect();
    json!({ "model": MODEL, "tools": tools, "messages": [{ "role": "user", "content": "Hi" }] })
        .to_string()
}

/// Gemini streams as `data:` lines, for the passthrough translator.
pub fn gemini_streams() -> Vec<Case> {
    let mut cases: Vec<Case> = chunk_streams()
        .into_iter()
        .map(|(name, chunks)| Case::response(name, "", data_lines(&chunks)))
        .collect();
    cases.push(Case::response(
        "line-kinds",
        "",
        vec![
            String::new(),
            ": keep-alive".to_owned(),
            "event: message".to_owned(),
            format!("data:{}", text("x")),
            format!("data:   {}   ", text("y")),
            text("bare").to_string(),
            "data:".to_owned(),
            "data: [DONE]".to_owned(),
            "[DONE]".to_owned(),
            " [DONE]".to_owned(),
            "data: {not json".to_owned(),
        ],
    ));
    cases
}

/// Whole Gemini responses, for the passthrough translator.
pub fn gemini_finals() -> Vec<Case> {
    let mut cases = bodies("");
    cases.push(Case::response("not-json", "", vec!["not json".to_owned()]));
    cases.push(Case::response(
        "pretty",
        "",
        vec![
            serde_json::to_string_pretty(&last(vec![text("Hi")], "STOP", usage(1, 1, 0, 0)))
                .expect("serializable"),
        ],
    ));
    cases
}

/// Gemini streams as the Gemini executor passes them to the Claude
/// translator, each chunk's JSON then `[DONE]`, and one as Vertex AI's
/// passes them, each line as it came.
pub fn claude_streams() -> Vec<Case> {
    let request = original_request(false);
    let mut cases: Vec<Case> = chunk_streams()
        .into_iter()
        .map(|(name, chunks)| Case::response(name, &request, bare_lines(&chunks)))
        .collect();
    for (name, chunks) in chunk_streams().into_iter().take(6) {
        cases.push(Case::response(
            format!("no-request-{name}"),
            "",
            bare_lines(&chunks),
        ));
    }
    cases.extend([
        Case::response("done-only", &request, vec!["[DONE]".to_owned()]),
        Case::response("empty-stream", &request, Vec::new()),
        Case::response(
            "blank-lines",
            &request,
            vec![
                String::new(),
                chunk(vec![text("Hi")]).to_string(),
                String::new(),
                "[DONE]".to_owned(),
            ],
        ),
        Case::response(
            "no-done",
            &request,
            vec![
                chunk(vec![text("Hi")]).to_string(),
                last(vec![], "STOP", usage(1, 1, 0, 0)).to_string(),
            ],
        ),
        Case::response(
            "sse-lines",
            &request,
            vec![
                format!("data: {}", chunk(vec![text("Hi")])),
                String::new(),
                ": keep-alive".to_owned(),
                format!("data:{}", last(vec![text("!")], "STOP", usage(1, 1, 0, 0))),
                String::new(),
                "data: [DONE]".to_owned(),
                "[DONE]".to_owned(),
            ],
        ),
        Case::response(
            "case-mapped-names",
            &request,
            bare_lines(&[last(
                vec![
                    function_call("BASH", json!({})),
                    function_call("web_SEARCH", json!({})),
                    function_call("outil_m_t_o", json!({})),
                ],
                "STOP",
                usage(1, 1, 0, 0),
            )]),
        ),
    ]);
    cases
}

/// Whole Gemini responses, for the Claude non-streaming translator.
pub fn claude_finals() -> Vec<Case> {
    let mut cases = bodies(&original_request(false));
    cases.push(Case::response(
        "zero-usage",
        original_request(false),
        vec![json!({ "candidates": [{ "content": { "parts": [text("Hi")] }, "finishReason": "STOP" }] }).to_string()],
    ));
    cases.push(Case::response(
        "no-request",
        "",
        vec![
            last(
                vec![function_call("web_search", json!({}))],
                "STOP",
                usage(1, 1, 0, 0),
            )
            .to_string(),
        ],
    ));
    cases
}

/// Gemini streams as `data:` lines, for the Chat Completions translator.
pub fn chat_streams() -> Vec<Case> {
    let request = original_request(true);
    let mut cases: Vec<Case> = chunk_streams()
        .into_iter()
        .map(|(name, chunks)| Case::response(name, &request, data_lines(&chunks)))
        .collect();
    for (name, chunks) in chunk_streams().into_iter().take(6) {
        cases.push(Case::response(
            format!("no-request-{name}"),
            "",
            data_lines(&chunks),
        ));
    }
    cases.extend([
        Case::response(
            "line-kinds",
            &request,
            vec![
                String::new(),
                ": keep-alive".to_owned(),
                format!("data:{}", text("x")),
                text("bare").to_string(),
                "data: [DONE]".to_owned(),
                format!("data: {}", text("after")),
            ],
        ),
        Case::response(
            "function-declared-names",
            json!({ "tools": [{ "type": "function", "function": { "name": "web search" } }] })
                .to_string(),
            data_lines(&[last(
                vec![function_call("web_search", json!({}))],
                "STOP",
                usage(1, 1, 0, 0),
            )]),
        ),
    ]);
    cases
}

/// Whole Gemini responses, for the Chat Completions non-streaming
/// translator.
pub fn chat_finals() -> Vec<Case> {
    let mut cases = bodies(&original_request(true));
    cases.push(Case::response(
        "lowercase-finish",
        "",
        vec![json!({ "candidates": [{ "content": { "parts": [text("Hi")] }, "finishReason": "MAX_TOKENS" }], "createTime": "2025-01-02T03:04:05Z" }).to_string()],
    ));
    cases
}

/// Each stream's chunks merged into a whole response, and bodies that
/// aren't responses.
fn bodies(request: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = chunk_streams()
        .into_iter()
        .map(|(name, chunks)| {
            let body = chunks.last().cloned().unwrap_or_default();
            let parts: Vec<Value> = chunks
                .iter()
                .filter_map(|chunk| chunk.pointer("/candidates/0/content/parts"))
                .filter_map(Value::as_array)
                .flatten()
                .cloned()
                .collect();
            let body = with(body, "candidates", {
                let mut candidates = chunks
                    .last()
                    .and_then(|chunk| chunk.get("candidates").cloned())
                    .unwrap_or_else(|| json!([{}]));
                if let Some(candidate) = candidates.get_mut(0).filter(|c| c.is_object()) {
                    candidate["content"] = json!({ "role": "model", "parts": parts });
                }
                candidates
            });
            Case::response(name, request, vec![body.to_string()])
        })
        .collect();
    for (name, body) in [
        ("empty-body", ""),
        ("array-body", "[]"),
        ("null-body", "null"),
        ("empty-object", "{}"),
    ] {
        cases.push(Case::response(name, request, vec![body.to_owned()]));
    }
    cases
}

fn data_lines(chunks: &[Value]) -> Vec<String> {
    let mut lines: Vec<String> = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}"))
        .collect();
    lines.push("data: [DONE]".to_owned());
    lines
}

fn bare_lines(chunks: &[Value]) -> Vec<String> {
    let mut lines: Vec<String> = chunks.iter().map(Value::to_string).collect();
    lines.push("[DONE]".to_owned());
    lines
}
