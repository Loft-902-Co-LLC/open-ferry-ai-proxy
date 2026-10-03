//! Hand-written cases for the Responses → Chat Completions request translator.

use serde_json::{Value, json};

use super::Case;

const MODEL: &str = "gpt-4o";

/// A patch as `apply_patch` takes it.
const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// Tools as a Codex client declares them: functions, a namespace, the
/// `apply_patch` custom tool, a freeform custom tool and web search.
fn client_tools() -> Value {
    json!([
        { "type": "function", "name": "get_weather", "description": "Weather.", "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] } },
        { "type": "function", "name": "search", "strict": false, "parameters": { "type": "object", "properties": {} } },
        { "type": "namespace", "name": "mcp__github", "description": "GitHub.", "tools": [
            { "type": "function", "name": "read_file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } },
            { "name": "mcp__github__list" }
        ]},
        { "type": "custom", "name": "apply_patch", "description": "Edit files.", "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" } },
        { "type": "custom", "name": "shell", "description": "Run a command." },
        { "type": "web_search" }
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

fn custom_call(id: &str, name: &str, input: Value) -> Value {
    json!({ "type": "custom_tool_call", "call_id": id, "name": name, "input": input })
}

fn output(id: &str, output: Value) -> Value {
    json!({ "type": "function_call_output", "call_id": id, "output": output })
}

fn custom_output(id: &str, output: Value) -> Value {
    json!({ "type": "custom_tool_call_output", "call_id": id, "output": output })
}

fn reasoning(texts: &[&str]) -> Value {
    let summary: Vec<Value> = texts
        .iter()
        .map(|text| json!({ "type": "summary_text", "text": text }))
        .collect();
    json!({ "type": "reasoning", "summary": summary, "encrypted_content": "gAAAAABencrypted" })
}

/// A request with these input items, as JSON text.
fn items(items: Vec<Value>) -> String {
    json!({ "input": items }).to_string()
}

fn case(name: &str, request: impl Into<String>) -> Case {
    Case::new(format!("chat-{name}"), MODEL, request)
}

/// The Responses requests written for the Claude translators, which this one
/// reads too, and requests aimed at what only this one does.
pub fn requests() -> Vec<Case> {
    let mut cases: Vec<Case> = super::claude_responses::requests()
        .into_iter()
        .map(|mut case| {
            // The Claude cases' known differences are in fields this
            // translator doesn't read, except for nesting serde_json refuses.
            if case.name != "deep-nesting" {
                case.known_difference = None;
            }
            case
        })
        .collect();

    cases.extend([
        case(
            "typical-codex-request",
            json!({
                "model": "gpt-5-codex",
                "instructions": "You are a coding agent.",
                "input": [
                    { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "Sandbox: read-only." }] },
                    user("Weather in Paris, then add hello.txt."),
                    reasoning(&["Checking the weather first."]),
                    call("call_1", "get_weather", "{\"city\":\"Paris\"}"),
                    output("call_1", json!("18°C")),
                    reasoning(&["Now the file."]),
                    custom_call("call_2", "apply_patch", json!(PATCH)),
                    custom_output("call_2", json!("Done.")),
                    assistant("It is 18°C, and hello.txt is added."),
                    user("Thanks!")
                ],
                "tools": client_tools(),
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "reasoning": { "effort": "High", "summary": "auto" },
                "max_output_tokens": 4096,
                "store": false,
                "stream": true,
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "cache-1"
            })
            .to_string(),
        ),
        case(
            "message-shapes",
            items(vec![
                json!({ "role": "user", "content": "No type." }),
                json!({ "type": "", "role": "assistant", "content": "Empty type." }),
                json!({ "type": "message", "role": "system", "content": [{ "type": "input_text", "text": "System." }] }),
                json!({ "type": "message", "role": "developer", "content": "Developer." }),
                json!({ "type": "message", "role": "user", "content": null }),
                json!({ "type": "message", "role": "user", "content": 5 }),
                json!({ "type": "message", "role": "user" }),
                json!({ "type": "message", "role": "user", "content": [
                    { "text": "No part type." },
                    { "type": "", "text": "Empty part type." },
                    { "type": "input_text", "text": { "b": 1, "a": [1.50] } },
                    { "type": "refusal", "refusal": "No." },
                    { "type": "input_file", "file_id": "file_1" },
                    "bare string",
                    5
                ]}),
                json!({ "type": "message", "role": "Assistant", "content": "Odd case." }),
                json!({ "content": "Neither type nor role." }),
                json!({ "type": "item_reference", "id": "msg_1" }),
                json!("hello"),
                Value::Null
            ]),
        ),
        case(
            "video-parts",
            items(vec![json!({ "type": "message", "role": "user", "content": [
                { "type": "input_video", "video_url": "https://example.com/a.mp4" },
                { "type": "video_url", "video_url": { "url": "https://example.com/b.mp4", "processing": { "fps": 1 } }, "processing": { "fps": 2.50 } },
                { "type": "input_video", "video_url": { "url": "https://example.com/c.mp4" }, "processing": "auto" },
                { "type": "input_video" },
                { "type": "input_video", "video_url": 5 },
                { "type": "video_url", "video_url": null, "processing": null },
                { "type": "video_url", "video_url": ["a"] }
            ]})]),
        ),
        case(
            "image-details",
            items(vec![json!({ "type": "message", "role": "user", "content": [
                { "type": "input_image", "image_url": "https://example.com/a.png" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "auto" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "low" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "high" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "original" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": " HIGH " },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "ORİGİNAL" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "bogus" },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": 5 },
                { "type": "input_image", "image_url": "https://example.com/a.png", "detail": null },
                { "type": "input_image", "image_url": { "url": "https://example.com/a.png" } },
                { "type": "input_image", "image_url": 5 },
                { "type": "input_image" }
            ]})]),
        ),
        case(
            "reasoning-content",
            items(vec![
                user("Hi."),
                json!({ "type": "message", "role": "assistant", "content": "Let me look.", "reasoning_content": "Thought." }),
                call("call_1", "search", "{}"),
                output("call_1", json!("found")),
                json!({ "type": "message", "role": "assistant", "content": "Again.", "reasoning_content": { "b": 1, "a": 2 } }),
                json!({ "type": "function_call", "call_id": "call_2", "name": "search", "arguments": "{}", "reasoning_content": "Call reasoning." }),
                output("call_2", json!("found")),
                json!({ "type": "message", "role": "user", "content": "User reasoning?", "reasoning_content": "Ignored." })
            ]),
        ),
        case(
            "reasoning-items",
            items(vec![
                user("One."),
                reasoning(&["First ", "part."]),
                reasoning(&["Second."]),
                reasoning(&["Second."]),
                json!({ "type": "reasoning", "summary": [] }),
                assistant("Answer one."),
                user("Two."),
                reasoning(&["Dangling."]),
                user("Three."),
                json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": " [reasoning unavailable] " }, { "type": "reasoning_text", "text": "Other." }, "bare"] }),
                json!({ "type": "reasoning", "summary": "summary" }),
                json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": { "a": 1 } }, { "type": "summary_text", "text": " tail" }] }),
                reasoning(&["Trailing."])
            ]),
        ),
        case(
            "reasoning-placeholder",
            json!({
                "reasoning": { "effort": "medium" },
                "input": [
                    user("Weather?"),
                    call("call_1", "get_weather", "{\"city\":\"Paris\"}"),
                    output("call_1", json!("18°C")),
                    assistant("18°C."),
                    user("And Rome?"),
                    reasoning(&["Rome next."]),
                    call("call_2", "get_weather", "{\"city\":\"Rome\"}"),
                    output("call_2", json!("21°C")),
                    call("call_3", "get_weather", "{\"city\":\"Oslo\"}"),
                    output("call_3", json!("5°C"))
                ],
                "tools": client_tools()
            })
            .to_string(),
        ),
        case(
            "calls-join-the-assistant-message",
            items(vec![
                user("Go."),
                json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Calling." }], "reasoning_content": "Plan." }),
                json!({ "type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{}", "reasoning_content": "More plan." }),
                custom_call("call_2", "shell", json!("ls")),
                output("call_1", json!("one")),
                custom_output("call_2", json!("two")),
                assistant("No calls here."),
                json!({ "type": "item_reference", "id": "msg_1" }),
                call("call_3", "search", "{}"),
                output("call_3", json!("three"))
            ]),
        ),
        case(
            "tool-output-images",
            items(vec![
                call("call_1", "screenshot", "{}"),
                output("call_1", json!([
                    { "type": "input_text", "text": "Here:" },
                    { "type": "input_image", "image_url": " https://example.com/a.png ", "detail": "original" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/b.png", "detail": "LOW" } },
                    { "type": "input_file", "file_id": "file_1" },
                    "plain",
                    { "b": 1.50, "a": [] }
                ])),
                call("call_2", "screenshot", "{}"),
                output("call_2", json!(r#"[{"type":"output_text","text":"As text:"},{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}]"#)),
                call("call_3", "screenshot", "{}"),
                output("call_3", json!("[\n  {\n    \"type\": \"input_image\",\n    \"image_url\": \"https://example.com/c.png\"\n  },\n  {\n    \"type\": \"other\",\n    \"x\": 1.50\n  }\n]")),
                call("call_4", "screenshot", "{}"),
                // An image without a URL: not images, so text.
                output("call_4", json!([{ "type": "input_image", "image_url": "  " }, { "type": "input_text", "text": "x" }])),
                call("call_5", "screenshot", "{}"),
                // Text that isn't a string: not images either.
                output("call_5", json!([{ "type": "input_image", "image_url": "https://example.com/d.png" }, { "type": "input_text", "text": 5 }])),
                call("call_6", "screenshot", "{}"),
                output("call_6", json!([{ "type": "image_url", "image_url": { "url": "https://example.com/e.png", "detail": 5 } }])),
                call("call_7", "screenshot", "{}"),
                output("call_7", json!(r#"[{"type":"input_image","image_url":"https://example.com/f.png""#))
            ]),
        ),
        case(
            "custom-tool-outputs",
            items(vec![
                custom_call("call_1", "shell", json!("ls")),
                custom_output("call_1", json!([{ "type": "input_text", "text": "a" }, "b", { "text": { "c": 1 } }, { "type": "input_file" }])),
                custom_call("call_2", "shell", json!("ls")),
                custom_output("call_2", json!([{ "type": "input_text", "text": "see:" }, { "type": "input_image", "image_url": "https://example.com/a.png" }])),
                custom_call("call_3", "shell", json!("ls")),
                custom_output("call_3", json!(r#"[{"type":"input_image","image_url":"https://example.com/b.png","detail":"high"}]"#)),
                custom_call("call_4", "shell", json!("ls")),
                custom_output("call_4", json!({ "b": 1.50, "a": true })),
                custom_call("call_5", "shell", json!("ls")),
                custom_output("call_5", json!(5)),
                custom_call("call_6", "shell", json!("ls")),
                json!({ "type": "custom_tool_call_output", "call_id": "call_6" })
            ]),
        ),
        case(
            "orphan-outputs",
            items(vec![
                user("Hi."),
                output("call_nowhere", json!("Card from another thread.")),
                output("call_blank", json!("   ")),
                output("call_empty_parts", json!([])),
                custom_output("call_parts", json!([{ "type": "input_text", "text": "Joined " }, "text"])),
                output("call_image", json!([{ "type": "input_image", "image_url": "https://example.com/a.png" }])),
                json!({ "type": "function_call_output", "output": { "a": 1 } }),
                json!({ "type": "function_call_output", "call_id": "call_none" })
            ]),
        ),
        case(
            "duplicate-outputs",
            items(vec![
                call("call_1", "search", "{}"),
                call("call_2", "search", "{}"),
                user("Interjection."),
                output("call_1", json!("first")),
                output("call_1", json!("again")),
                output("call_2", json!("second"))
            ]),
        ),
        case(
            "outputs-moved-back",
            items(vec![
                user("Go."),
                call("call_1", "search", "{}"),
                call("call_2", "get_weather", "{}"),
                user("Meanwhile."),
                output("call_2", json!("second")),
                assistant("Thinking."),
                output("call_1", json!("first")),
                call("call_3", "search", "{}"),
                call("call_3", "search", "{}"),
                output("call_3", json!("third"))
            ]),
        ),
        case(
            "outputs-without-ids",
            items(vec![
                call("call_1", "search", "{}"),
                call("call_2", "get_weather", "{}"),
                json!({ "type": "function_call_output", "name": "get_weather", "output": "by name" }),
                json!({ "type": "function_call_output", "output": "by order" }),
                user("Next."),
                call("call_3", "search", "{}"),
                json!({ "type": "function_call_output", "id": "fco_1", "output": "fco id" }),
                user("Last."),
                call("call_4", "search", "{}"),
                call("call_5", "search", "{}"),
                json!({ "type": "function_call_output", "output": "one of two" })
            ]),
        ),
        case(
            "call-ids",
            items(vec![
                json!({ "type": "function_call", "tool_call_id": " call_padded ", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call_output", "callId": "call_padded", "output": "a" }),
                json!({ "type": "function_call", "id": "fc_1", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call_output", "call_id": "fc_1", "output": "b" }),
                json!({ "type": "function_call", "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call_output", "output": "c" }),
                json!({ "type": "function_call", "call_id": 12345, "name": "search", "arguments": "{}" }),
                json!({ "type": "function_call_output", "call_id": "12345", "output": "d" })
            ]),
        ),
        case(
            "call-names-and-arguments",
            json!({
                "tools": client_tools(),
                "input": [
                    call("call_1", " get_weather ", "{ \"city\" : \"Paris\" }"),
                    json!({ "type": "function_call", "call_id": "call_2", "name": "read_file", "arguments": { "path": "a.txt" } }),
                    json!({ "type": "function_call", "call_id": "call_3", "name": "read_file", "namespace": "mcp__github", "arguments": "{}" }),
                    json!({ "type": "function_call", "call_id": "call_4", "name": "read_file", "namespace": " mcp__github ", "arguments": "{}" }),
                    json!({ "type": "function_call", "call_id": "call_5", "name": "list", "namespace": "unknown", "arguments": "{}" }),
                    json!({ "type": "function_call", "call_id": "call_6", "name": "mcp__github__read_file", "arguments": 5 }),
                    json!({ "type": "function_call", "call_id": "call_7", "arguments": "{}" }),
                    json!({ "type": "function_call", "call_id": "call_8", "name": 5 }),
                    custom_call("call_9", "list", json!("x")),
                    json!({ "type": "custom_tool_call", "call_id": "call_10", "name": "shell", "namespace": " ", "input": "y" })
                ]
            })
            .to_string(),
        ),
        case(
            "custom-tool-input-escapes",
            items(
                [
                    json!("echo '<b>' && cat a > b"),
                    json!("café 🚀"),
                    json!("quote \" backslash \\"),
                    json!("line\nbreak\ttab\u{1}"),
                    json!("\u{2028}separators\u{2029}"),
                    json!("\u{7f}del"),
                    json!(""),
                    json!(5),
                    json!(true),
                    Value::Null,
                ]
                .into_iter()
                .enumerate()
                .flat_map(|(i, input)| {
                    let id = format!("call_{i}");
                    [
                        custom_call(&id, "shell", input),
                        custom_output(&id, json!("ok")),
                    ]
                })
                .collect(),
            ),
        ),
        case(
            "custom-tool-input-object",
            serde_json::to_string_pretty(&json!({ "input": [
                custom_call("call_1", "shell", json!({ "command": ["ls", "-la"] }))
            ] }))
            .expect("a Value always serializes"),
        )
        .known_difference(
            "custom tool input that isn't a string: upstream copies its JSON text into the arguments, we write it compactly",
        ),
        case(
            "namespaces",
            json!({
                "tools": [
                    { "type": "namespace", "name": "n".repeat(70), "tools": [
                        { "type": "function", "name": "read", "parameters": { "type": "object" } },
                        { "type": "custom", "name": "write" }
                    ]},
                    { "type": "namespace", "name": format!("m{}", "n".repeat(70)), "tools": [
                        { "type": "function", "name": "read" },
                        { "type": "function", "name": format!("m{}__read", "n".repeat(70)) }
                    ]},
                    { "type": "namespace", "name": "browser", "tools": [
                        { "type": "function", "name": "read" },
                        { "type": "function", "name": "browser" },
                        { "type": "function", "name": "mcp__x__open" },
                        { "type": "web_search" }
                    ]},
                    { "type": "namespace", "name": "tools__", "tools": [{ "name": "find" }] },
                    { "type": " namespace ", "name": " spaced ", "tools": [{ "name": " padded " }] },
                    { "type": "namespace", "tools": [{ "name": "orphan" }] },
                    { "type": "namespace", "name": "empty", "tools": {} }
                ],
                "input": [
                    call("call_1", "read", "{}"),
                    json!({ "type": "function_call", "call_id": "call_2", "name": "read", "namespace": "n".repeat(70), "arguments": "{}" }),
                    json!({ "type": "function_call", "call_id": "call_3", "name": "read", "namespace": format!("m{}", "n".repeat(70)), "arguments": "{}" }),
                    call("call_4", &format!("{}__read", "n".repeat(70)), "{}"),
                    call("call_5", "browser__read", "{}"),
                    json!({ "type": "function_call", "call_id": "call_6", "name": "read", "namespace": "nowhere", "arguments": "{}" }),
                    custom_call("call_7", "write", json!("text")),
                    call("call_8", "find", "{}")
                ],
                "tool_choice": { "type": "function", "name": "read", "namespace": format!("m{}", "n".repeat(70)) }
            })
            .to_string(),
        ),
        case(
            "cut-inside-a-character",
            json!({
                "tools": [{ "type": "namespace", "name": "é".repeat(40), "tools": [
                    { "type": "function", "name": "read_" },
                    { "type": "function", "name": "write" }
                ]}],
                "input": [json!({ "type": "function_call", "call_id": "call_1", "name": "read_", "namespace": "é".repeat(40), "arguments": "{}" })]
            })
            .to_string(),
        ),
        case(
            "long-and-colliding-names",
            json!({
                "tools": [
                    { "type": "function", "name": "a".repeat(64) },
                    { "type": "function", "name": format!("x{}", "a".repeat(64)) },
                    { "type": "function", "name": format!("y{}", "a".repeat(64)) },
                    { "type": "function", "name": format!("x{}", "a".repeat(64)), "description": "Same tool again." },
                    { "type": "function", "name": format!("{}__{}", "q".repeat(10), "b".repeat(62)) },
                    { "type": "function", "name": format!("{}{}", "z".repeat(10), "_".repeat(64)) }
                ],
                "input": [
                    call("call_1", &format!("y{}", "a".repeat(64)), "{}"),
                    call("call_2", &format!("w{}", "a".repeat(64)), "{}"),
                    call("call_3", &"a".repeat(64), "{}")
                ],
                "tool_choice": { "type": "function", "function": { "name": format!("x{}", "a".repeat(64)) } }
            })
            .to_string(),
        ),
        case(
            "tool-declarations",
            json!({
                "tools": [
                    { "type": "function", "function": { "name": "nested", "description": "Chat style.", "parameters": { "type": "object" } } },
                    { "type": "function", "name": "top", "function": { "name": "nested_name", "description": "Nested description.", "parametersJsonSchema": { "type": "object" } } },
                    { "name": "untyped", "input_schema": { "type": "object", "properties": {} } },
                    { "type": " function ", "name": "padded_type", "parametersJsonSchema": { "type": "object" } },
                    { "type": "Function", "name": "wrong_case" },
                    { "type": "function", "name": "null_parameters", "parameters": null },
                    { "type": "function", "name": "string_parameters", "parameters": "schema" },
                    { "type": "function", "name": "no_parameters", "description": { "b": 1, "a": 2 } },
                    { "type": "function", "name": "  " },
                    { "type": "custom", "name": "freeform", "description": "Any text.", "format": { "type": "text" } },
                    { "type": " custom ", "name": " apply_patch " },
                    { "type": "mcp", "server_label": "deepwiki" },
                    { "type": "web_search" },
                    5,
                    "Bash",
                    null
                ],
                "input": "Go."
            })
            .to_string(),
        ),
        case(
            "schema-key-order-and-numbers",
            r#"{"input":"Go.","tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"n":{"type":"number","minimum":1.50,"maximum":1e3,"default":9007199254740993,"multipleOf":0.10}},"required":["n"],"additionalProperties":false}}]}"#,
        ),
        case(
            "additional-tools",
            json!({
                "tools": [{ "type": "function", "name": "search" }],
                "input": [
                    { "type": "additional_tools", "tools": [
                        { "type": "function", "name": "lookup", "parameters": { "type": "object" } },
                        { "type": "function", "name": "search", "description": "Again." },
                        { "type": "namespace", "name": "mcp__docs", "tools": [{ "name": "fetch" }] }
                    ]},
                    { "type": "additional_tools", "tools": null },
                    { "type": " additional_tools ", "tools": [{ "type": "function", "name": "ignored" }] },
                    call("call_1", "fetch", "{}"),
                    output("call_1", json!("fetched"))
                ]
            })
            .to_string(),
        ),
        case(
            "additional-tools-only",
            items(vec![
                json!({ "type": "additional_tools", "tools": [{ "type": "custom", "name": "apply_patch" }] }),
                custom_call("call_1", "apply_patch", json!(PATCH))
            ]),
        ),
    ]);

    // text.format, as response_format.
    let formats = [
        ("text", json!({ "type": "text" })),
        ("json-object", json!({ "type": "json_object" })),
        (
            "json-schema",
            json!({ "type": "json_schema", "name": "weather", "description": "The weather.", "strict": true, "schema": { "type": "object", "properties": { "b": { "type": "number", "minimum": 1.50 }, "a": {} } } }),
        ),
        (
            "json-schema-loose",
            json!({ "schema": "schema", "strict": "true", "description": { "b": 1, "a": 2.50 }, "name": 5, "type": "json_schema" }),
        ),
        (
            "json-schema-large-numbers",
            json!({ "type": "json_schema", "name": 123456789012345678901234567890_u128, "strict": 9007199254740993_u64 }),
        ),
        ("json-schema-bare", json!({ "type": "json_schema" })),
        (
            "grammar",
            json!({ "type": "grammar", "grammar": "start: /.+/" }),
        ),
        ("upper-case", json!({ "type": "JSON_OBJECT" })),
        ("no-type", json!({ "name": "x" })),
        ("string", json!("json_object")),
        ("null", Value::Null),
    ];
    for (name, format) in formats {
        cases.push(case(
            &format!("text-format-{name}"),
            json!({ "input": "Go.", "text": { "format": format, "verbosity": "low" } }).to_string(),
        ));
    }

    // max_output_tokens, copied as written.
    for (name, limit) in [
        ("integer", "1024"),
        ("decimal", "1.50"),
        ("exponent", "1e3"),
        ("huge", "1e400"),
        ("string", r#""2048""#),
        ("null", "null"),
        ("object", r#"{ "b": 1, "a": 2.50 }"#),
    ] {
        cases.push(case(
            &format!("max-output-tokens-{name}"),
            format!(r#"{{"input":"Go.","max_output_tokens":{limit}}}"#),
        ));
    }

    // parallel_tool_calls, kept only with tools.
    for (name, value) in [
        ("true", json!(true)),
        ("false", json!(false)),
        ("string-true", json!("true")),
        ("string-yes", json!("yes")),
        ("one", json!(1)),
        ("null", Value::Null),
    ] {
        cases.push(case(
            &format!("parallel-tool-calls-{name}"),
            json!({ "input": "Go.", "tools": client_tools(), "parallel_tool_calls": value })
                .to_string(),
        ));
    }
    cases.push(case(
        "parallel-tool-calls-without-tools",
        json!({ "input": "Go.", "parallel_tool_calls": true, "tool_choice": "required" })
            .to_string(),
    ));

    // What turns reasoning on, for the placeholder on tool call turns.
    let calls = || {
        json!([
            call("call_1", "search", "{}"),
            output("call_1", json!("found"))
        ])
    };
    for (name, fields) in [
        ("effort", json!({ "reasoning": { "effort": "low" } })),
        (
            "effort-none",
            json!({ "reasoning": { "effort": " NONE " } }),
        ),
        (
            "effort-false",
            json!({ "reasoning": { "effort": "false" } }),
        ),
        (
            "effort-unicode",
            json!({ "reasoning": { "effort": "MAXİMUM" } }),
        ),
        ("top-level-effort", json!({ "reasoning_effort": "high" })),
        ("top-level-effort-zero", json!({ "reasoning_effort": "0" })),
        (
            "summary-only",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        ("empty-object", json!({ "reasoning": {} })),
        ("string", json!({ "reasoning": "high" })),
        ("string-false", json!({ "reasoning": " FALSE " })),
        ("null", json!({ "reasoning": null })),
    ] {
        let mut request = fields;
        request["input"] = calls();
        cases.push(case(&format!("reasoning-{name}"), request.to_string()));
    }

    // Upstream's behaviour we document instead of matching.
    cases.extend([
        case(
            "reasoning-empty-object-with-space",
            format!(r#"{{"reasoning":{{ }},"input":{}}}"#, calls()),
        )
        .known_difference(
            "an empty reasoning object written with a space: upstream turns reasoning on",
        ),
        case("duplicate-input", r#"{"input":"first","input":"second"}"#)
            .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
        case(
            "output-with-unpaired-surrogate",
            items(vec![
                call("call_1", "screenshot", "{}"),
                output(
                    "call_1",
                    json!(format!(
                        r#"[{{"type":"input_image","image_url":"https://example.com/a.png"}},{{"type":"input_text","text":"{}ud800"}}]"#,
                        '\\'
                    )),
                ),
            ]),
        )
        .known_difference(
            "serde_json refuses an unpaired surrogate escape in a tool output's JSON; gjson reads it",
        ),
        case(
            "output-nested-deeply",
            items(vec![
                call("call_1", "screenshot", "{}"),
                output(
                    "call_1",
                    json!(format!(
                        r#"[{{"type":"input_image","image_url":"https://example.com/a.png"}},{}0{}]"#,
                        "[".repeat(200),
                        "]".repeat(200)
                    )),
                ),
            ]),
        )
        .known_difference("serde_json refuses JSON nested over 128 levels; gjson reads it"),
        case(
            "schema-number-beyond-float64",
            r#"{"input":"Go.","tools":[{"type":"function","name":"f","parameters":{"type":"number","maximum":1e400}}]}"#,
        )
        .known_difference("Go can't write a number beyond float64 in a tool schema"),
        case(
            "text-format-number-beyond-float64",
            r#"{"input":"Go.","text":{"format":{"type":"json_schema","name":"x","strict":1e400}}}"#,
        )
        .known_difference("Go can't write a number beyond float64 in text.format"),
    ]);
    cases
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn names_are_unique_and_requests_are_json_where_expected() {
        let cases = requests();
        let mut names = HashSet::new();
        for case in &cases {
            assert!(names.insert(case.name.as_str()), "{} repeated", case.name);
        }
        for case in cases.iter().filter(|case| case.name.starts_with("chat-")) {
            let parsed = serde_json::from_str::<Value>(&case.request);
            let refused = matches!(
                case.name.as_str(),
                "chat-output-nested-deeply" | "chat-output-with-unpaired-surrogate"
            );
            assert!(parsed.is_ok() || refused, "{} is not JSON", case.name);
        }
    }
}
