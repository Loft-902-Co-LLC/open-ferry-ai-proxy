//! Hand-written cases for the translators from Gemini `generateContent`
//! clients to Codex, Claude and Chat Completions.
//!
//! The response translators also read the streams and responses written for
//! the other translators from those providers, with the tool arguments that
//! upstream would copy as broken JSON replaced (see [`crate::generate::gemini`]).

use open_ferry_translate::codex::gemini::convert_gemini_request_to_codex;
use serde_json::{Value, json};

use super::Case;
use crate::generate::gemini::{repair_claude_input, repair_codex_case, repair_codex_final};

/// A tool name over Codex's 64-byte limit, which comes back from Codex
/// shortened.
const LONG_NAME: &str =
    "mcp__a_server_with_a_name_long_enough_to_need_shortening__search_the_files";

pub fn requests() -> Vec<Case> {
    let request = |name: &str, model: &str, stream: bool, body: Value| {
        Case::new(name, model, body.to_string()).with_options(json!({ "stream": stream }))
    };
    let weather = json!({
        "functionDeclarations": [
            { "name": "get_weather", "description": "Weather for a city", "parameters": {
                "type": "OBJECT",
                "properties": { "city": { "type": "STRING" }, "days": { "type": "INTEGER", "minimum": 1 } },
                "required": ["city"]
            } },
            { "name": LONG_NAME, "parametersJsonSchema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "additionalProperties": true
            } }
        ]
    });
    let call = |name: &str, args: Value| json!({ "functionCall": { "name": name, "args": args } });
    let answer = |name: &str, response: Value| json!({ "functionResponse": { "name": name, "response": response } });
    let user = |text: &str| json!({ "role": "user", "parts": [{ "text": text }] });
    let round_trip = json!({
        "tools": [weather],
        "contents": [
            user("Weather in Paris and Oslo?"),
            { "role": "model", "parts": [
                { "text": "Checking." },
                call("get_weather", json!({ "city": "Paris" })),
                call("get_weather", json!({ "city": "Oslo", "days": 2 }))
            ] },
            { "role": "user", "parts": [
                answer("get_weather", json!({ "result": "Sunny" })),
                answer("get_weather", json!({ "temp": 3, "unit": "C" }))
            ] },
            { "role": "model", "parts": [call(LONG_NAME, json!({ "query": "notes" }))] },
            { "role": "function", "parts": [answer(LONG_NAME, json!({ "result": { "files": ["a.txt"] } }))] }
        ]
    });

    vec![
        request(
            "text",
            "gpt-5",
            false,
            json!({ "contents": [user("Hello")] }),
        ),
        request(
            "system-instruction-both-spellings",
            "claude-sonnet-4-6",
            true,
            json!({
                "systemInstruction": { "parts": [{ "text": "camel" }, { "text": "hidden", "thought": true }] },
                "system_instruction": { "role": "system", "parts": [{ "text": "snake" }, { "text": "second" }] },
                "contents": [user("hi")]
            }),
        ),
        request(
            "tool-round-trip",
            "claude-opus-4-6",
            true,
            round_trip.clone(),
        ),
        Case::new(
            "tool-round-trip-pretty",
            "gpt-5",
            serde_json::to_string_pretty(&round_trip).expect("a Value serializes"),
        )
        .with_options(json!({ "stream": false })),
        request(
            "tool-ids",
            "claude-sonnet-4-5-20250929",
            false,
            json!({
                "contents": [
                    user("go"),
                    { "role": "model", "parts": [
                        { "functionCall": { "id": " call_9 ", "name": "a", "args": {} } },
                        { "functionCall": { "call_id": "call_8", "name": "b", "args": { "x": 1 } } },
                        { "functionCall": { "name": "c", "args": { "y": [1, 2] } } }
                    ] },
                    { "role": "user", "parts": [
                        { "functionResponse": { "call_id": "call_8", "name": "b", "response": { "result": "B" } } },
                        { "functionResponse": { "name": "c", "response": { "result": "C" } } },
                        { "functionResponse": { "id": "unknown", "name": "d", "response": { "result": "D" } } },
                        { "functionResponse": { "name": "e", "response": { "result": "E" } } }
                    ] }
                ]
            }),
        ),
        request(
            "media",
            "gpt-5",
            true,
            json!({
                "contents": [{ "role": "user", "parts": [
                    { "text": "Look" },
                    { "inlineData": { "mimeType": "image/png", "data": "iVBORw0KGgo=" } },
                    { "inline_data": { "mime_type": "audio/wav", "data": "UklGRg==" } },
                    { "inlineData": { "mimeType": "audio/mpeg", "data": "SUQz" } },
                    { "inlineData": { "mimeType": "application/pdf", "data": "JVBERi0=" } },
                    { "inlineData": { "mimeType": "video/mp4", "data": "AAAA" } },
                    { "inlineData": { "mimeType": "", "data": "AAAA" } },
                    { "fileData": { "mimeType": "image/jpeg", "fileUri": "https://example.com/cat.jpg" } },
                    { "file_data": { "mime_type": "application/pdf", "file_uri": "gs://bucket/report.pdf" } },
                    { "fileData": { "fileUri": "files/abc123" } }
                ] }]
            }),
        ),
        request(
            "thinking-level-adaptive",
            "claude-opus-4-6",
            false,
            json!({ "contents": [user("hi")], "generationConfig": { "thinkingConfig": { "thinkingLevel": "HIGH", "includeThoughts": true } } }),
        ),
        request(
            "thinking-budget",
            "claude-sonnet-4-5-20250929",
            false,
            json!({ "contents": [user("hi")], "generationConfig": { "thinkingConfig": { "thinking_budget": 8192 } } }),
        ),
        request(
            "thinking-off",
            "claude-opus-4-6",
            true,
            json!({ "contents": [user("hi")], "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } } }),
        ),
        request(
            "thinking-level-top",
            "gpt-5",
            true,
            json!({ "contents": [user("hi")], "generationConfig": { "thinking_level": " Low " } }),
        ),
        request(
            "tool-config-any-one",
            "gpt-5",
            false,
            json!({
                "tools": [weather],
                "toolConfig": { "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": [LONG_NAME] } },
                "contents": [user("hi")]
            }),
        ),
        request(
            "tool-config-none",
            "claude-opus-4-6",
            false,
            json!({
                "tools": [weather],
                "tool_config": { "function_calling_config": { "mode": "NONE" } },
                "contents": [user("hi")]
            }),
        ),
        request(
            "generation-config",
            "claude-sonnet-4-6",
            true,
            json!({
                "contents": [user("hi")],
                "generationConfig": {
                    "temperature": 0.7, "topP": 0.9, "topK": 40, "maxOutputTokens": 1024,
                    "stopSequences": ["END", "STOP"], "candidateCount": 2,
                    "responseModalities": ["TEXT", "IMAGE"]
                }
            }),
        ),
        request(
            "client-user-id",
            "claude-sonnet-4-6",
            false,
            json!({ "metadata": { "user_id": "user-1" }, "contents": [user("hi")] }),
        ),
        request(
            "client-user",
            "claude-sonnet-4-6",
            false,
            json!({ "user": "client-7", "contents": [user("hi")] }),
        ),
        request(
            "thought-parts",
            "gpt-5",
            false,
            json!({ "contents": [
                user("hi"),
                { "role": "model", "parts": [
                    { "text": "thinking", "thought": true, "thoughtSignature": "CiQB" },
                    { "text": "answer", "thoughtSignature": "CiQB" }
                ] }
            ] }),
        ),
        request("empty", "gpt-5", false, json!({})),
        request(
            "service-tier",
            "gpt-5",
            false,
            json!({ "service_tier": " Fast ", "contents": [user("hi")] }),
        ),
    ]
}

/// The Codex streams written for the Claude response translators, and
/// streams whose calls name a Gemini client's tools.
pub fn codex_streams() -> Vec<Case> {
    let original = codex_original();
    let short_name = codex_short_name(&original);
    let line = |event: Value| format!("data: {event}");
    let created = json!({ "type": "response.created", "response": { "id": "resp_1", "created_at": 1_700_000_000, "model": "gpt-5", "output": [] } });
    let call = json!({ "type": "response.output_item.done", "output_index": 1, "item": {
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": short_name,
        "arguments": "{\"query\":\"notes\"}"
    } });
    let completed = json!({ "type": "response.completed", "response": {
        "id": "resp_1", "status": "completed",
        "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "output_tokens_details": { "reasoning_tokens": 2 } }
    } });
    let names_mapped_back = vec![
        "event: response.created".to_owned(),
        line(created),
        line(json!({ "type": "response.reasoning_summary_text.delta", "delta": "Thinking" })),
        line(json!({ "type": "response.output_text.delta", "delta": "Looking." })),
        line(call),
        line(completed),
    ];
    super::hand_written_streams()
        .into_iter()
        .map(repair_codex_case)
        .chain([Case {
            events: names_mapped_back,
            ..Case::new("names-mapped-back", "gemini-2.5-pro", original.to_string())
        }])
        .collect()
}

/// The final Codex events written for the Claude response translators, and
/// ones whose calls name a Gemini client's tools.
pub fn codex_finals() -> Vec<Case> {
    let original = codex_original();
    let short_name = codex_short_name(&original);
    let completed = |kind: &str| {
        json!({ "type": kind, "response": {
            "id": "resp_2", "created_at": 1_755_225_123, "status": "completed",
            "output": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Thought" }] },
                { "type": "message", "content": [{ "type": "output_text", "text": "Done." }] },
                { "type": "function_call", "call_id": "call_2", "name": short_name, "arguments": "{\"query\":\"x\"}" }
            ],
            "usage": { "input_tokens": 3, "output_tokens": 4, "total_tokens": 7 }
        } })
        .to_string()
    };
    let case = |name: &str, event: String| Case {
        events: vec![event],
        ..Case::new(name, "gemini-2.5-pro", original.to_string())
    };
    super::hand_written_finals()
        .into_iter()
        .map(repair_codex_final)
        .chain([
            case("names-mapped-back", completed("response.completed")),
            case("incomplete", completed("response.incomplete")),
            case("not-final", completed("response.in_progress")),
        ])
        .collect()
}

/// The Claude streams written for the Chat Completions translator.
pub fn claude_streams() -> Vec<Case> {
    claude_cases().0
}

/// The Claude SSE bodies written for the Chat Completions translator.
pub fn claude_finals() -> Vec<Case> {
    claude_cases().1
}

/// The Chat Completions streams written for the passthrough.
pub fn openai_streams() -> Vec<Case> {
    super::openai_chat::streams()
}

/// The Chat Completions responses written for the passthrough.
pub fn openai_finals() -> Vec<Case> {
    super::openai_chat::finals()
}

/// The hand-written Claude streams and bodies. All but the last stream and
/// the last two bodies hold the same events in turn, so their tool input is
/// repaired together.
fn claude_cases() -> (Vec<Case>, Vec<Case>) {
    let mut streams = super::claude_chat::streams();
    let mut finals = super::claude_chat::finals();
    for index in 0..streams.len() - 1 {
        let (stream, last) = repair_claude_input(streams[index].clone(), finals[index].clone());
        streams[index] = stream;
        finals[index] = last;
    }
    (streams, finals)
}

/// A Gemini request declaring a tool whose name Codex gets shortened.
fn codex_original() -> Value {
    json!({
        "tools": [{ "functionDeclarations": [
            { "name": "get_weather", "parameters": { "type": "OBJECT" } },
            { "name": LONG_NAME, "parameters": { "type": "OBJECT" } }
        ] }],
        "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
    })
}

/// The name Codex knows [`LONG_NAME`] by, as our request translator writes it.
fn codex_short_name(original: &Value) -> String {
    let codex = convert_gemini_request_to_codex("gpt-5", original);
    codex["tools"][1]["name"]
        .as_str()
        .expect("the tool is translated")
        .to_owned()
}
