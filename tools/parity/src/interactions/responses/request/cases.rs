//! Hand-written cases for the Responses and Interactions request
//! translators: every kind of input item and step, tools declared every way,
//! the `automation_update` tool left out, each `tool_choice` form, the
//! generation settings, loosely typed values, and, as known differences, the
//! deviations the port documents.

use serde_json::{Value, json};

use crate::cases::Case;

const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

fn request(name: &str, model: &str, body: Value) -> Case {
    Case::new(name, model, body.to_string())
}

/// `case` with the translator's stream flag set.
fn streaming(case: Case) -> Case {
    case.with_options(json!({ "stream": true }))
}

fn known(mut case: Case, reason: &'static str) -> Case {
    case.known_difference = Some(reason);
    case
}

fn weather_tool() -> Value {
    json!({
        "type": "function",
        "name": "get_weather",
        "description": "Weather for a city.",
        "parameters": {
            "type": "object",
            "properties": { "city": { "type": "string" }, "days": { "type": "integer", "maximum": 1.50 } },
            "required": ["city"]
        }
    })
}

fn codex_app_tools() -> Value {
    json!([
        weather_tool(),
        { "type": "namespace", "name": "mcp__codex_app", "description": "Codex app.", "tools": [
            { "type": "function", "name": "automation_update", "description": "Update an automation.", "parameters": { "type": "object" } },
            { "type": "function", "name": "list_automations", "description": "List automations.", "parameters": { "type": "object" } }
        ]},
        { "type": "function", "name": "mcp__codex_app__automation_update", "description": "Declared directly." }
    ])
}

/// Requests for the Responses → Interactions translator.
pub fn responses_requests() -> Vec<Case> {
    let mut cases = vec![
        request(
            "text-input",
            "gemini-2.5-flash",
            json!({ "model": "gpt-5", "instructions": "Be brief.", "input": "hello" }),
        ),
        request(
            "request-model-when-none-given",
            " ",
            json!({ "model": "gemini-3-pro-preview", "input": "hi", "stream": true }),
        ),
        streaming(request(
            "stream-option",
            "gemini-2.5-pro",
            json!({ "input": "hi" }),
        )),
        streaming(request(
            "stream-field-wins",
            "gemini-2.5-pro",
            json!({ "input": "hi", "stream": false }),
        )),
        request(
            "conversation",
            "gemini-2.5-pro",
            json!({
                "instructions": { "content": [
                    { "type": "input_text", "text": "Be " },
                    { "type": "input_text", "text": "brief." }
                ]},
                "previous_response_id": "resp_1",
                "environment": { "id": "env_1" },
                "agent_config": { "type": "dynamic", "thinking_summaries": "auto" },
                "input": [
                    { "type": "message", "role": "developer", "content": "Use the tools." },
                    { "type": "message", "role": "user", "content": [
                        { "type": "input_text", "text": "What's the weather?" },
                        { "type": "input_image", "image_url": "data:image/png;base64,aGVsbG8=" },
                        { "type": "input_image", "image_url": "https://example.com/cat.png" },
                        { "type": "input_image", "data": "aGVsbG8=", "mime_type": "image/jpeg" },
                        { "type": "input_image", "url": "data:image/webp,raw" },
                        { "type": "input_file", "file_id": "file_1" },
                        { "type": "refusal", "text": "kept as text" }
                    ]},
                    { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Checking." }] },
                    { "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" },
                    { "type": "function_call_output", "call_id": "call_1", "output": "{\"temp\":1.50}" },
                    { "type": "function_call", "id": "call_2", "namespace": "mcp__github", "name": "read_file", "arguments": { "path": "README.md" } },
                    { "type": "function_call_output", "call_id": "call_2", "output": "plain text" },
                    { "type": "custom_tool_call", "call_id": "call_3", "name": "apply_patch", "input": PATCH },
                    { "type": "custom_tool_call_output", "call_id": "call_3", "output": "Done" },
                    { "type": "custom_tool_call", "call_id": "call_4", "name": "run", "arguments": "not json" },
                    { "type": "function_call_output", "call_id": "call_unknown", "output": [{ "type": "input_text", "text": "orphan" }] },
                    { "type": "function_call_output", "id": "call_4", "name": "renamed", "result": "{\"broken\":" },
                    { "type": "function_call_output", "call_id": "call_1" },
                    { "type": "input_text", "text": "loose text" },
                    { "type": "output_text", "text": "loose answer" },
                    { "type": "output_image", "image_url": "data:;base64,aGk=" },
                    { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "thinking" }] },
                    { "role": "assistant", "content": "no type, so user input" },
                    { "type": "message", "role": "user", "content": [] }
                ],
                "tools": [
                    weather_tool(),
                    { "type": "namespace", "name": "mcp__github", "tools": [
                        { "type": "function", "name": "read_file", "description": "Read a file.", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } }
                    ]},
                    { "type": "custom", "name": "apply_patch", "description": "Edit files.", "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" } },
                    { "type": "custom", "name": "run" }
                ]
            }),
        ),
        request(
            "tools-flattened",
            "gemini-2.5-pro",
            json!({
                "input": [
                    { "type": "additional_tools", "tools": [
                        { "type": "function", "name": "get_weather", "description": "Loses to the top level." },
                        { "type": "function", "name": "extra", "description": "Only here." }
                    ]},
                    { "type": "message", "role": "user", "content": "hi" }
                ],
                "tools": [
                    { "type": "function", "function": { "name": "nested", "description": "Nested.", "parametersJsonSchema": { "type": "object" } } },
                    { "name": "schema_only", "input_schema": { "type": "object", "properties": { "q": { "type": "string" } } } },
                    weather_tool(),
                    { "type": "function", "name": "get_weather", "description": "A repeat, which loses." },
                    { "type": "namespace", "name": "browser", "description": "Browser.", "children": [
                        { "type": "function", "name": "open", "parameters": { "type": "object" } },
                        { "type": "custom", "name": "click", "description": "Click." },
                        { "type": "web_search" },
                        { "name": "mcp__other__tool" },
                        { "name": "browser__back" }
                    ]},
                    { "type": "namespace", "name": "tools__", "tools": [{ "type": "function", "name": "x" }] },
                    { "type": "namespace", "name": " ", "tools": [{ "type": "function", "name": "bare" }] },
                    { "type": "web_search", "filters": { "allowed_domains": ["docs.rs"] } },
                    { "type": "function", "name": "  " },
                    { "type": "function", "name": " padded " }
                ]
            }),
        ),
        request(
            "automation-update-dropped",
            "gemini-2.5-pro",
            json!({
                "input": "hi",
                "tools": codex_app_tools(),
                "tool_choice": { "type": "function", "namespace": "mcp__codex_app", "name": "automation_update" }
            }),
        ),
        request(
            "automation-update-choice-by-full-name",
            "gemini-2.5-pro",
            json!({
                "input": "hi",
                "tools": codex_app_tools(),
                "tool_choice": { "type": "function", "name": " MCP__CODEX_APP__AUTOMATION_UPDATE " }
            }),
        ),
        request(
            "automation-update-in-additional-tools",
            "devin",
            json!({
                "input": [
                    { "type": "additional_tools", "tools": [
                        { "type": "namespace", "name": " MCP__Codex_App ", "tools": [
                            { "type": "function", "name": "Automation_Update" },
                            { "type": "function", "name": "view" }
                        ]}
                    ]}
                ],
                "tool_choice": { "type": "custom", "custom": { "name": "automation_update", "namespace": "mcp__codex_app" } }
            }),
        ),
        request(
            "automation-update-other-choice-kept",
            "gemini-2.5-pro",
            json!({
                "input": "hi",
                "tools": codex_app_tools(),
                "tool_choice": { "type": "function", "function": { "name": "list_automations", "namespace": "mcp__codex_app" } }
            }),
        ),
        request(
            "tool-choice-string",
            "gemini-2.5-pro",
            json!({ "input": "hi", "tools": [weather_tool()], "tool_choice": "required" }),
        ),
        request(
            "tool-choice-custom",
            "gemini-2.5-pro",
            json!({ "input": "hi", "tool_choice": { "type": "custom", "custom": { "name": "click", "namespace": "browser" } } }),
        ),
        request(
            "tool-choice-allowed-tools",
            "gemini-2.5-pro",
            json!({ "input": "hi", "tool_choice": { "type": "allowed_tools", "mode": "auto", "tools": [{ "type": "function", "name": "get_weather" }] } }),
        ),
        request(
            "tool-choice-null",
            "gemini-2.5-pro",
            json!({ "input": "hi", "tool_choice": null }),
        ),
        request(
            "generation-config",
            "gemini-2.5-pro",
            json!({
                "input": "hi",
                "reasoning": { "effort": " HIGH ", "summary": "detailed" },
                "text": { "format": { "type": "json_schema", "name": "answer", "schema": { "type": "object", "properties": { "answer": { "type": "string" } } } } },
                "max_tokens": "2048",
                "max_completion_tokens": 10,
                "temperature": "0.7",
                "top_p": 1.50,
                "presence_penalty": -0.5,
                "frequency_penalty": true,
                "stop": ["END", "STOP"]
            }),
        ),
        request(
            "response-format-wins-over-text",
            "gemini-2.5-pro",
            json!({
                "input": "hi",
                "response_format": { "type": "json_object" },
                "text": { "format": { "type": "text" } },
                "max_output_tokens": 1e3,
                "temperature": null,
                "top_p": "abc",
                "stop": "END"
            }),
        ),
        request(
            "odd-values",
            "gemini-2.5-pro",
            json!({
                "instructions": { "text": 12 },
                "previous_response_id": "  ",
                "previous_interaction_id": "interaction_2",
                "environment_id": 7,
                "stream": "true",
                "reasoning": { "effort": 5, "summary": false },
                "input": [
                    { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": 1.50 }, { "type": "input_text" }] },
                    { "type": "function_call", "call_id": 9, "name": "get_weather", "arguments": "null" },
                    { "type": "function_call_output", "call_id": 9, "output": 42 },
                    { "type": "custom_tool_call", "call_id": "call_5", "name": "run", "input": 3 },
                    { "type": "function_call", "name": "no_id", "arguments": "" },
                    { "type": "function_call", "call_id": " ", "id": "call_6", "name": "", "arguments": "[1,2]" }
                ]
            }),
        ),
        request(
            "single-item-input",
            "gemini-2.5-pro",
            json!({ "input": { "type": "message", "role": "model", "content": { "type": "output_text", "text": "one part" } } }),
        ),
        request(
            "input-of-another-type",
            "gemini-2.5-pro",
            json!({ "input": 5, "instructions": ["not", "text"] }),
        ),
        request(
            "array-request",
            "gemini-2.5-pro",
            json!([weather_tool(), { "type": "custom", "name": "run" }]),
        ),
        Case::new(
            "pretty-printed",
            "gemini-2.5-pro",
            serde_json::to_string_pretty(&json!({
                "instructions": { "role": "system", "parts": ["no text"] },
                "previous_response_id": { "id": "resp_3" },
                "input": [
                    { "type": "message", "role": "user", "content": [
                        { "type": "input_text", "text": { "nested": [1, 2] } },
                        { "type": "input_image", "image_url": { "url": "https://example.com/a.png" } }
                    ]},
                    { "type": "function_call", "call_id": { "id": 1 }, "namespace": { "ns": true }, "name": "tool", "arguments": "{ \"a\" : 1 }" },
                    { "type": "custom_tool_call", "call_id": "call_7", "name": "run", "input": { "cmd": "ls" } }
                ],
                "tools": [{ "type": "function", "name": "described", "description": { "text": "an object" } }],
                "tool_choice": { "type": "function", "name": { "odd": "name" } }
            }))
            .expect("a Value always serializes"),
        ),
    ];
    cases.extend(responses_known_differences());
    cases
}

/// The deviations the Responses → Interactions port documents.
fn responses_known_differences() -> Vec<Case> {
    vec![
        known(
            request(
                "tool-descriptions-kept",
                "gemini-2.5-pro",
                json!({
                    "input": "hi",
                    "tools": [
                        { "type": "function", "name": "exec_command", "description": "Runs a command in a PTY, returning output or a session ID for ongoing interaction." },
                        { "type": "function", "name": "write_stdin", "description": "Writes characters to an existing unified exec session and returns recent output." }
                    ]
                }),
            ),
            "tool descriptions are passed on as written; upstream rewrites two well-known ones, which only disguises the client",
        ),
        known(
            request(
                "antigravity-model",
                "gemini-3-pro-antigravity",
                json!({ "input": "hi", "max_output_tokens": 100, "temperature": 0.5 }),
            ),
            "an Antigravity model is handled like any other; upstream moves the token limit to agent_config and drops the sampling knobs",
        ),
        known(
            request(
                "infinite-temperature",
                "gemini-2.5-pro",
                json!({ "input": "hi", "temperature": serde_json::from_str::<Value>("1e400").expect("valid JSON") }),
            ),
            "a temperature that isn't finite is left out; upstream writes +Inf, which isn't JSON",
        ),
        known(
            request(
                "unpaired-surrogate-arguments",
                "gemini-2.5-pro",
                json!({ "input": [{ "type": "function_call", "call_id": "call_1", "name": "f", "arguments": format!("{{\"q\":\"{}ud800\"}}", '\\') }] }),
            ),
            "arguments serde_json can't read are passed on as a string; upstream embeds them, with an unpaired surrogate escape",
        ),
        known(
            Case::new(
                "repeated-key",
                "gemini-2.5-pro",
                r#"{"input":"first","input":"second"}"#,
            ),
            "when a key repeats, the last value counts; gjson reads the first",
        ),
    ]
}

/// Requests for the Interactions → Responses translator.
pub fn interactions_requests() -> Vec<Case> {
    vec![
        request(
            "steps",
            "gpt-5",
            json!({
                "model": "gemini-2.5-pro",
                "system_instruction": { "parts": [{ "text": "Be " }, { "text": "brief." }] },
                "previous_interaction_id": "interaction_1",
                "environment_id": "env_1",
                "agent_config": { "type": "dynamic" },
                "input": [
                    { "type": "user_input", "content": [
                        { "type": "text", "text": "Describe these." },
                        { "type": "image", "mime_type": "image/png", "data": "aGVsbG8=" },
                        { "type": "image", "uri": "https://example.com/cat.png" },
                        { "type": "image", "image_url": "https://example.com/dog.png" },
                        { "type": "image", "data": "aGVsbG8=" },
                        { "type": "audio", "mime_type": "audio/wav", "data": "UklGRg==" },
                        { "type": "audio", "mime_type": "wav" },
                        { "type": "audio" },
                        { "type": "video", "url": "https://example.com/v.mp4", "filename": "v.mp4" },
                        { "type": "document", "mime_type": "application/pdf", "data": "JVBERi0=", "filename": "a.pdf" },
                        { "type": "document", "file_data": "data:text/plain;base64,aGk=" },
                        { "text": "untyped" },
                        { "type": "unknown", "text": "skipped" }
                    ]},
                    { "type": "thought", "signature": "c2ln", "content": [
                        { "type": "text", "text": "Thinking" },
                        { "type": "text", "content": { "text": "nested" } },
                        { "type": "text", "text": "" }
                    ]},
                    { "type": "thought", "content": "a string thought" },
                    { "type": "model_output", "content": [{ "type": "text", "text": "Let me check." }, { "type": "image", "url": "https://example.com/out.png" }, { "type": "video", "data": "AAAA", "mime_type": "video/mp4" }] },
                    { "type": "function_call", "id": "call_1", "name": "get_weather", "arguments": { "city": "Paris", "days": 1.50 } },
                    { "type": "function_result", "call_id": "call_1", "name": "get_weather", "result": { "temp": 1.50 } },
                    { "type": "function_call", "call_id": "call_2", "name": "search", "arguments": "{\"q\":\"x\"}" },
                    { "type": "function_result", "call_id": "call_2", "output": "plain" },
                    { "type": "function_call", "name": "no_arguments" },
                    { "type": "function_result", "id": "call_3" },
                    { "type": "model_output", "content": "string content" },
                    { "type": "user_input", "content": { "first": { "type": "text", "text": "an object's values" } } },
                    { "type": "user_input", "content": 5 },
                    "loose string",
                    { "type": "unknown" }
                ],
                "generation_config": { "thinking_level": " HIGH ", "thinking_summaries": "auto", "tool_choice": "any" },
                "tool_choice": "none",
                "response_modalities": ["TEXT"],
                "service_tier": "flex",
                "response_format": { "type": "text" }
            }),
        ),
        request(
            "tools",
            "gpt-5",
            json!({
                "input": "hi",
                "tools": [
                    { "type": "function", "name": "get_weather", "description": "Weather.", "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } },
                    { "function_declarations": [{ "name": "a", "description": "A" }, { "name": "", "description": "skipped" }, { "name": "b" }] },
                    { "type": "function", "function": { "name": "nested", "description": "Nested.", "parameters": { "type": "object" } } },
                    { "name": "schema", "parametersJsonSchema": { "type": "object" } },
                    { "name": "described", "description": 5 },
                    { "type": "google_search" },
                    "not a tool"
                ],
                "tool_choice": { "type": "function", "name": "get_weather" }
            }),
        ),
        request(
            "thinking-config-camel-case",
            "gpt-5",
            json!({ "input": "hi", "generation_config": { "thinking_level": 5, "thinkingConfig": { "thinkingLevel": "Medium" } } }),
        ),
        request(
            "thinking-config-snake-case",
            "gpt-5",
            json!({ "input": "hi", "generation_config": { "thinking_config": { "thinking_level": "low" }, "thinking_summaries": 1 } }),
        ),
        request(
            "string-input",
            "",
            json!({ "model": "gemini-2.5-flash", "input": "hello", "stream": "true", "system_instruction": "Be brief." }),
        ),
        request(
            "object-input",
            "gpt-5",
            json!({ "input": { "type": "user_input", "content": "one" }, "system_instruction": { "text": "Hi" } }),
        ),
        request(
            "input-of-another-type",
            "gpt-5",
            json!({ "input": true, "system_instruction": 7, "tools": { "not": "a list" } }),
        ),
        streaming(request(
            "stream-option",
            "gpt-5",
            json!({ "input": "hi", "stream": false }),
        )),
        request(
            "previous-response-fallback",
            "gpt-5",
            json!({
                "input": "hi",
                "previous_interaction_id": "  ",
                "previous_response_id": "resp_2",
                "environment": { "id": "env_2" },
                "service_tier": 1,
                "generation_config": "not an object"
            }),
        ),
        Case::new(
            "pretty-printed",
            "gpt-5",
            serde_json::to_string_pretty(&json!({
                "input": [
                    { "type": "function_call", "call_id": { "id": 1 }, "name": { "n": 1 }, "arguments": { "nested": { "list": [1, 2] } } },
                    { "type": "function_result", "call_id": "call_1", "name": "f", "result": [{ "type": "text", "text": "x" }] },
                    { "type": "user_input", "content": [{ "type": "text", "text": { "an": "object" } }] }
                ],
                "system_instruction": { "parts": [{ "text": { "a": 1 } }, { "text": "after" }] },
                "tools": [{ "name": "tool", "description": { "an": "object" } }]
            }))
            .expect("a Value always serializes"),
        ),
        known(
            Case::new(
                "repeated-key",
                "gpt-5",
                r#"{"input":"first","input":"second"}"#,
            ),
            "when a key repeats, the last value counts; gjson reads the first",
        ),
    ]
}
