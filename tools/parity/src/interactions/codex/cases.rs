//! Hand-written cases for the Interactions to Codex translators: requests
//! with every kind of step and content part, the system instruction in each
//! form, the generation settings and reasoning named every way, tools
//! declared every way and with long names, and the fields that pass
//! through; Codex streams with every kind of output item, each way a stream
//! can end and each form of line; and final events with every kind of item,
//! call arguments of each form and usage under either set of names. The
//! port's deviations are known differences (see the parent module).

use serde_json::{Value, json};

use crate::cases::Case;

const MODEL: &str = "gpt-5-codex";

fn request(name: &str, body: Value) -> Case {
    Case::new(name, MODEL, body.to_string())
}

/// `case` with the translator's stream flag set.
fn streaming(case: Case) -> Case {
    case.with_options(json!({ "stream": true }))
}

fn known(mut case: Case, reason: &'static str) -> Case {
    case.known_difference = Some(reason);
    case
}

/// A stream case: each event after `data: `, as Codex sends it, unless it
/// is a line of its own already.
fn stream(name: &str, events: &[Value]) -> Case {
    let lines = events
        .iter()
        .map(|event| match event {
            Value::String(line) => line.clone(),
            event => format!("data: {event}"),
        })
        .collect();
    Case {
        model: MODEL.into(),
        ..Case::response(name, "{}", lines)
    }
}

/// A non-streaming case: Codex's final event, or whatever body it gave.
fn final_event(name: &str, body: Value) -> Case {
    Case {
        model: MODEL.into(),
        ..Case::response(name, "{}", vec![body.to_string()])
    }
}

fn weather_declaration() -> Value {
    json!({
        "name": "get_weather",
        "description": "Weather for a city.",
        "parameters": {
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": { "city": { "type": "string" }, "days": { "type": "integer", "maximum": 1.50 } },
            "required": ["city"]
        }
    })
}

/// Requests for the request translator.
pub fn requests() -> Vec<Case> {
    vec![
        request(
            "text-input",
            json!({ "model": "gemini-2.5-pro", "input": "hello" }),
        ),
        streaming(request("stream-option", json!({ "input": "hi" }))),
        request("stream-field", json!({ "input": "hi", "stream": true })),
        request(
            "stream-field-as-text",
            json!({ "input": "hi", "stream": "true" }),
        ),
        request("no-input", json!({ "model": "gemini-2.5-pro" })),
        request("input-not-a-list", json!({ "input": 42 })),
        request(
            "system-instruction-text",
            json!({ "system_instruction": "Be brief.", "input": "hi" }),
        ),
        request(
            "system-instruction-object",
            json!({ "systemInstruction": { "text": "Be brief.", "parts": [{ "text": "ignored" }] }, "input": "hi" }),
        ),
        request(
            "system-instruction-parts",
            json!({
                "system_instruction": { "parts": [{ "text": "Be" }, { "text": "" }, { "text": "brief." }, { "text": { "a": [1, 2] } }] },
                "systemInstruction": "not read",
                "input": "hi"
            }),
        ),
        request(
            "system-instruction-without-text",
            json!({ "system_instruction": { "parts": [{ "text": "" }] }, "input": "hi" }),
        ),
        request(
            "steps-of-every-kind",
            json!({
                "input": [
                    { "type": "user_input", "content": [{ "type": "text", "text": "What's the weather?" }] },
                    { "type": "thought", "id": "th_1", "content": [{ "text": "Think" }, { "text": "" }, { "text": "harder" }] },
                    { "type": "reasoning", "text": "from text" },
                    { "type": "function_call", "name": "get_weather", "call_id": " call_1 ", "arguments": { "city": "Paris", "days": 1.50 } },
                    { "type": "function_result", "name": "get_weather", "call_id": "call_1", "result": { "temperature": 21 } },
                    { "type": "function_call", "name": "search", "id": "call_2", "args": "{\"q\":\"x\"}" },
                    { "type": "function_call_output", "id": "call_2", "output": "found" },
                    { "type": "function_call", "name": "search" },
                    { "type": "function_result", "call_id": "call_3", "result": [1, 2] },
                    { "type": "model_output", "content": [{ "type": "text", "text": "Sunny." }] },
                    { "type": "assistant", "content": "Anything else?" },
                    { "type": "message", "role": "developer", "content": "Mind the units." },
                    { "type": "user_input", "role": "system", "text": "From the system." },
                    { "type": "unknown", "content": { "text": "object content" } },
                    { "type": " FUNCTION_CALL ", "name": "get_weather", "call_id": "call_4" },
                    "a bare string",
                    { "text": "no type, text only" },
                    { "role": "user" }
                ]
            }),
        ),
        request(
            "steps-under-roles",
            json!({
                "input": [
                    { "role": "model", "steps": [
                        "said by the model",
                        { "type": "user_input", "content": "role from the parent" },
                        { "role": "system", "steps": ["nested system"] },
                        { "role": "nobody", "steps": ["unknown role keeps the parent's"] }
                    ]},
                    { "role": " User ", "steps": [{ "content": [{ "text": "padded role" }] }] },
                    { "role": "tool", "steps": ["unknown role"] }
                ]
            }),
        ),
        request(
            "input-object-with-steps",
            json!({ "input": { "role": "assistant", "steps": [{ "content": "hi" }, "there"] } }),
        ),
        request(
            "input-single-step",
            json!({ "input": { "type": "model_output", "content": [{ "text": "one step" }] } }),
        ),
        request(
            "loosely-typed-values",
            json!({
                "input": [
                    { "type": "user_input", "content": [{ "type": "text", "text": { "nested": true } }, { "type": "text", "text": 1.50 }, { "text": null }] },
                    { "type": "function_call", "name": { "n": 1 }, "call_id": { "id": 2 }, "arguments": [1, { "a": "b" }] },
                    { "type": "function_result", "call_id": 7, "output": { "ok": true } },
                    { "type": "thought", "id": 5, "content": { "text": { "deep": 1 } } }
                ]
            }),
        ),
        request(
            "media-parts",
            json!({
                "input": [{ "type": "user_input", "content": [
                    { "type": "image", "url": "https://example.com/cat.png" },
                    { "type": "image", "fileUri": "gs://bucket/cat.png", "mime_type": "image/png" },
                    { "type": "Image", "mimeType": "image/jpeg", "data": "aGVsbG8=" },
                    { "type": "image", "mime_type": "image/png" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/dog.png" } },
                    { "type": "audio", "mime_type": "audio/wav", "data": "UklGRg==" },
                    { "type": "audio", "mime_type": "audio/ogg", "data": "T2dnUw==" },
                    { "type": "audio", "mime_type": "audio/L16", "data": "AAAA" },
                    { "type": "audio", "mime_type": "audio/mpeg" },
                    { "type": "input_audio", "input_audio": { "data": "AAAA", "format": "wav" } },
                    { "type": "input_audio" },
                    { "type": "document", "file": { "file_data": "JVBERi0=", "filename": "a.pdf" } },
                    { "type": "file", "mime_type": "text/csv", "url": "https://example.com/a.csv" },
                    { "type": "video", "mimeType": "video/mp4", "file_uri": "gs://bucket/v.mp4" },
                    { "type": "document", "mime_type": "application/json", "data": "e30=" },
                    { "type": "file", "mime_type": "application/xml", "data": "PGEvPg==" },
                    { "type": "file", "mime_type": "application/zip", "data": "UEsDBA==" },
                    { "type": "document" },
                    { "type": "text" },
                    { "type": "" }
                ]}]
            }),
        ),
        request(
            "inline-and-file-data",
            json!({
                "input": [{ "type": "user_input", "content": [
                    { "type": "inline_data", "inline_data": { "mime_type": "image/png", "data": "aGk=" } },
                    { "type": "blob", "inlineData": { "mimeType": "audio/x-wav", "data": "UklGRg==" } },
                    { "type": "blob", "inline_data": { "mime_type": "application/pdf", "data": "JVBERi0=" } },
                    { "type": "blob", "inline_data": { "mime_type": "IMAGE/PNG", "data": "aGk=" } },
                    { "type": "blob", "inline_data": { "mime_type": "image/png" } },
                    { "type": "file_data", "file_data": { "mime_type": "image/png", "file_uri": "gs://b/cat.png" } },
                    { "type": "media", "fileData": { "mimeType": "text/plain", "fileUri": "gs://b/a.txt" } },
                    { "type": "media", "file_data": { "mime_type": "video/mp4" } },
                    { "inline_data": { "mime_type": "image/png", "data": "aGk=" } }
                ]}]
            }),
        ),
        request(
            "inline-data-quoted-by-go",
            json!({
                "input": [{ "type": "user_input", "content": [
                    { "type": "blob", "inline_data": { "mime_type": "image/png", "data": "tab\there\u{0}stops" } },
                    { "type": "blob", "inline_data": { "mime_type": "application/pdf", "data": "emoji 🚀 and \u{feff}bom\u{85}" } },
                    { "type": "blob", "inline_data": { "mime_type": "\u{b}audio/wav", "data": "aGk=" } },
                    { "type": "blob", "inline_data": { "mime_type": "audio/wav", "data": "quote \" backslash \\ bell \u{7}" } }
                ]}]
            }),
        ),
        request(
            "generation-settings",
            json!({
                "input": "hi",
                "generation_config": {
                    "max_output_tokens": 1024,
                    "temperature": 0.70,
                    "top_p": 1,
                    "presence_penalty": -0.5,
                    "frequency_penalty": 1e-3,
                    "parallel_tool_calls": false,
                    "response_format": { "type": "json_schema", "schema": { "type": "object" } },
                    "verbosity": "low",
                    "truncation": "auto",
                    "tool_choice": "required",
                    "service_tier": "flex",
                    "stop_sequences": ["END"],
                    "seed": 7
                }
            }),
        ),
        request(
            "generation-settings-camel-case",
            json!({
                "input": "hi",
                "generationConfig": {
                    "serviceTier": "priority",
                    "toolChoice": { "type": "function", "name": "get_weather" },
                    "text": { "format": { "type": "text" } },
                    "responseFormat": "json",
                    "parallelToolCalls": "true",
                    "frequencyPenalty": null,
                    "presencePenalty": "0.5",
                    "topP": 0.9,
                    "maxOutputTokens": "2048"
                }
            }),
        ),
        request(
            "generation-config-not-an-object",
            json!({ "input": "hi", "generation_config": "fast", "reasoning": { "effort": "low" } }),
        ),
        request(
            "reasoning-without-config",
            json!({ "input": "hi", "reasoning": { "effort": "low", "summary": "auto" } }),
        ),
        request(
            "reasoning-levels",
            json!({
                "input": "hi",
                "generation_config": {
                    "reasoning": { "effort": "minimal", "summary": "concise", "extra": 1 },
                    "thinking_level": " HIGH ",
                    "thinkingLevel": "low",
                    "thinking_summaries": " NONE "
                }
            }),
        ),
        request(
            "reasoning-level-from-config",
            json!({
                "input": "hi",
                "generation_config": { "thinking_config": { "thinkingLevel": "medium", "includeThoughts": true } }
            }),
        ),
        request(
            "reasoning-from-budgets",
            json!({
                "input": "hi",
                "generationConfig": {
                    "thinking_level": "  ",
                    "thinkingConfig": { "thinkingBudget": 8192, "include_thoughts": false }
                }
            }),
        ),
        request(
            "reasoning-budgets-of-every-size",
            json!({
                "input": "hi",
                "generation_config": { "thinking_budget": -1, "thinkingBudget": 0, "include_thoughts": "true", "includeThoughts": true }
            }),
        ),
        request(
            "reasoning-budget-as-text",
            json!({ "input": "hi", "generation_config": { "thinkingBudget": "24577", "thinkingSummaries": "AUTO" } }),
        ),
        request(
            "reasoning-not-an-object",
            json!({ "input": "hi", "generation_config": { "reasoning": "high", "thinking_level": "low" } }),
        ),
        request(
            "reasoning-as-a-list",
            json!({ "input": "hi", "generation_config": { "reasoning": [1], "thinking_level": "low", "thinking_summaries": "auto" } }),
        ),
        request(
            "tools-declared-every-way",
            json!({
                "input": "hi",
                "tools": [
                    { "type": "function", "name": "lookup", "description": 1.50, "parametersJsonSchema": { "type": "object", "additionalProperties": true } },
                    { "function_declarations": [weather_declaration(), { "description": "no name" }] },
                    { "functionDeclarations": [{ "name": "search", "parameters_json_schema": { "type": "object", "additionalProperties": false } }] },
                    { "function_declarations": "not a list" },
                    { "name": "params_list", "parameters": [1, 2] },
                    { "name": "params_null", "parameters": null },
                    { "name": "params_text", "parameters": "schema" },
                    { "name": "no_params" },
                    { "type": "google_search" }
                ]
            }),
        ),
        request(
            "tools-keep-the-clients-tool-choice",
            json!({ "input": "hi", "tools": [weather_declaration()], "generation_config": { "tool_choice": "none" } }),
        ),
        request(
            "tools-builtin-only",
            json!({ "input": "hi", "tools": [{ "type": "google_search" }, { "url_context": {} }] }),
        ),
        request(
            "tools-not-a-list",
            json!({ "input": "hi", "tools": { "name": "solo" } }),
        ),
        request("tools-empty", json!({ "input": "hi", "tools": [] })),
        request(
            "long-tool-names",
            json!({
                "input": [
                    { "type": "function_call", "name": "mcp__a_server_with_a_rather_long_name_indeed__and_a_tool_whose_name_is_long_too_still", "call_id": "call_1", "arguments": {} },
                    { "type": "function_call", "name": "名前名前名前名前名前名前名前名前名前名前名前名前", "call_id": "call_2", "arguments": "{}" }
                ],
                "tools": [
                    { "name": "a_very_long_function_name_that_goes_on_and_on_well_past_the_sixty_four_byte_limit" },
                    { "name": "mcp__a_server_with_a_rather_long_name_indeed__and_a_tool_whose_name_is_long_too_still" },
                    { "name": "mcp__a_very_long_server_name_that_pushes_the_whole_name_over_the_limit__tool" },
                    { "name": "mcp__no_second_separator_and_long_enough_to_be_cut_at_sixty_four_bytes_x" },
                    { "name": "mcp__short__tool" },
                    { "name": "exactly_sixty_four_bytes_long_name_for_a_function_tool_abcdefgh" },
                    { "name": "名前名前名前名前名前名前名前名前名前名前名前名前" },
                    { "name": "mcp__名前名前名前名前名前名前名前名前名前名前名前名前名前" }
                ]
            }),
        ),
        request(
            "top-level-fields",
            json!({
                "input": "hi",
                "tool_choice": { "type": "function", "name": "get_weather" },
                "parallel_tool_calls": true,
                "store": false,
                "metadata": { "user_id": "u_1" },
                "include": ["reasoning.encrypted_content"],
                "truncation": "disabled",
                "service_tier": " Fast ",
                "generation_config": { "truncation": "auto", "tool_choice": "auto", "service_tier": "flex" }
            }),
        ),
        request(
            "service-tiers-not-priority",
            json!({ "input": "hi", "service_tier": "flex", "generation_config": { "serviceTier": "default" } }),
        ),
        request(
            "service-tier-not-text",
            json!({ "input": "hi", "service_tier": 1 }),
        ),
        // Upstream adds no identity, and leaves out a client's own
        // `prompt_cache_key`; so does the port.
        request(
            "no-identity-added",
            json!({
                "input": "hi",
                "prompt_cache_key": "client-key-1",
                "session_id": "s_1",
                "conversation": "c_1",
                "client_metadata": { "installation_id": "i_1" },
                "user": "u_1"
            }),
        ),
        known(
            request(
                "settings-for-the-same-field",
                json!({
                    "input": "hi",
                    "generation_config": {
                        "max_output_tokens": 1, "maxOutputTokens": 2, "max_tokens": 3,
                        "top_p": 0.1, "topP": 0.2,
                        "presence_penalty": 0.1, "presencePenalty": 0.2,
                        "frequency_penalty": 0.1, "frequencyPenalty": 0.2,
                        "parallel_tool_calls": true, "parallelToolCalls": false,
                        "response_format": "a", "responseFormat": "b",
                        "text": { "format": { "type": "text" } }, "verbosity": "high",
                        "tool_choice": "auto", "toolChoice": "none",
                        "service_tier": "flex", "serviceTier": "default"
                    }
                }),
            ),
            "upstream copies the generation settings in Go's map order, so which of two for the same field wins changes from run to run; the port takes the last listed",
        ),
        known(
            Case::new(
                "repeated-key",
                MODEL,
                r#"{"input":"first","input":"second","generation_config":{"thinking_level":"low","thinking_level":"high"}}"#,
            ),
            "gjson reads the first of a repeated key; serde_json keeps the last",
        ),
        known(
            request(
                "budget-out-of-int64-range",
                json!({ "input": "hi", "generation_config": { "thinking_budget": 1e30 } }),
            ),
            "Go's int64(1e30) depends on the CPU; the port saturates",
        ),
    ]
}

/// A Codex response in progress, as `response.created` carries it.
fn created(id: &str) -> Value {
    json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": { "id": id, "object": "response", "created_at": 1_700_000_000, "status": "in_progress", "model": "gpt-5.4", "output": [] }
    })
}

fn completed(usage: Value) -> Value {
    json!({
        "type": "response.completed",
        "response": { "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5.4", "output": [], "usage": usage }
    })
}

fn codex_usage() -> Value {
    json!({
        "input_tokens": 12,
        "input_tokens_details": { "cached_tokens": 4 },
        "output_tokens": 30,
        "output_tokens_details": { "reasoning_tokens": 9 },
        "total_tokens": 42
    })
}

fn added(item: Value) -> Value {
    json!({ "type": "response.output_item.added", "output_index": 0, "item": item })
}

fn done(item: Value) -> Value {
    json!({ "type": "response.output_item.done", "output_index": 0, "item": item })
}

fn delta(kind: &str, delta: Value) -> Value {
    json!({ "type": kind, "item_id": "item_1", "output_index": 0, "delta": delta })
}

fn message(text: &str) -> Value {
    json!({ "id": "msg_1", "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
}

fn call(kind: &str, ids: Value, arguments: &str) -> Value {
    let mut item = json!({ "type": kind, "name": "get_weather", "arguments": arguments });
    if let (Value::Object(item), Value::Object(ids)) = (&mut item, ids) {
        item.extend(ids);
    }
    item
}

/// Streams for the stream translator.
pub fn streams() -> Vec<Case> {
    let text = "response.output_text.delta";
    let summary = "response.reasoning_summary_text.delta";
    let reasoning = "response.reasoning_text.delta";
    let arguments = "response.function_call_arguments.delta";
    vec![
        stream(
            "text",
            &[
                created("resp_1"),
                json!({ "type": "response.in_progress", "response": { "id": "resp_1" } }),
                added(
                    json!({ "id": "msg_1", "type": "message", "role": "assistant", "content": [] }),
                ),
                delta(text, json!("Hello")),
                delta(text, json!(", world")),
                done(message("Hello, world")),
                completed(codex_usage()),
            ],
        ),
        stream(
            "text-from-the-done-item",
            &[
                created("resp_1"),
                added(json!({ "id": "msg_1", "type": "message", "content": [] })),
                done(
                    json!({ "type": "message", "content": [{ "text": "one" }, { "text": "" }, { "content": "two" }, { "text": 3 }] }),
                ),
                completed(json!({})),
            ],
        ),
        stream(
            "reasoning-then-text",
            &[
                created("resp_1"),
                added(json!({ "id": "rs_1", "type": "reasoning", "summary": [] })),
                delta(summary, json!("Thinking")),
                delta(reasoning, json!(" more")),
                done(
                    json!({ "id": "rs_1", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Thinking more" }] }),
                ),
                delta(text, json!("Answer")),
                completed(codex_usage()),
            ],
        ),
        stream(
            "reasoning-from-the-done-item",
            &[
                created("resp_1"),
                done(
                    json!({ "type": "reasoning", "content": [{ "text": "first" }, { "summary_text": "second" }, { "summary_text": { "x": 1 } }] }),
                ),
                done(
                    json!({ "type": "reasoning", "summary": [{ "text": "a" }, { "text": "" }, { "text": "b" }] }),
                ),
                done(json!({ "type": "reasoning", "summary": "plain" })),
                done(json!({ "type": "reasoning", "encrypted_content": "gAAAA" })),
                completed(json!(null)),
            ],
        ),
        stream(
            "function-call",
            &[
                created("resp_1"),
                added(call(
                    "function_call",
                    json!({ "id": "fc_1", "call_id": "call_1" }),
                    "",
                )),
                delta(arguments, json!("{\"city\":")),
                delta(arguments, json!("\"Paris\"}")),
                done(call(
                    "function_call",
                    json!({ "id": "fc_1", "call_id": "call_1" }),
                    "{\"city\":\"Paris\"}",
                )),
                added(call(
                    "tool_call",
                    json!({ "id": " fc_2 ", "call_id": " " }),
                    "",
                )),
                done(call(
                    "tool_call",
                    json!({ "id": " fc_2 ", "call_id": " " }),
                    "{}",
                )),
                completed(codex_usage()),
            ],
        ),
        stream(
            "function-call-done-only",
            &[
                created("resp_1"),
                done(call(
                    "function_call",
                    json!({ "call_id": "call_🚀" }),
                    "{\"q\":1}",
                )),
                json!("data: [DONE]"),
            ],
        ),
        // The only ID made up from the clock: neither the delta nor an item
        // added before it names the call.
        stream(
            "function-call-without-an-id",
            &[
                created("resp_1"),
                delta(arguments, json!("{}")),
                delta(text, json!("then text")),
                completed(json!({})),
            ],
        ),
        stream(
            "function-call-named-by-the-item-added",
            &[
                added(call("function_call", json!({ "call_id": "call_1" }), "")),
                delta(text, json!("text closes the call")),
                delta(arguments, json!("{\"more\":true}")),
                completed(json!({})),
            ],
        ),
        stream(
            "image-generation",
            &[
                created("resp_1"),
                added(
                    json!({ "id": "ig_1", "type": "image_generation_call", "status": "in_progress" }),
                ),
                done(
                    json!({ "id": "ig_1", "type": "image_generation_call", "result": "aGVsbG8=", "output_format": "jpeg" }),
                ),
                done(
                    json!({ "type": "image_generation_call", "result": "aGk=", "output_format": "image/avif" }),
                ),
                done(
                    json!({ "type": "image_generation_call", "result": "aGk=", "output_format": "WEBP" }),
                ),
                done(json!({ "type": "image_generation_call", "result": "aGk=" })),
                done(json!({ "type": "image_generation_call", "result": "" })),
                completed(json!({})),
            ],
        ),
        stream(
            "incomplete",
            &[
                created("resp_1"),
                delta(text, json!("cut")),
                json!({ "type": "response.incomplete", "response": { "id": "resp_1", "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" }, "usage": { "input_tokens": 3, "output_tokens": 0 } } }),
            ],
        ),
        stream(
            "done-without-completed",
            &[
                created("resp_1"),
                delta(text, json!("hi")),
                json!("data: [DONE]"),
            ],
        ),
        stream(
            "completed-then-done",
            &[
                created("resp_1"),
                completed(codex_usage()),
                json!("data: [DONE]"),
                json!("[DONE]"),
            ],
        ),
        stream(
            "line-forms",
            &[
                json!(created("resp_1").to_string()),
                json!(format!("data:{}", delta(text, json!("no space")))),
                json!(format!("  data: {}  ", delta(text, json!(" padded")))),
                json!(format!(
                    "data: {}\r",
                    delta(text, json!(" carriage return"))
                )),
                json!(""),
                json!("event: response.output_text.delta"),
                json!(": keep-alive"),
                json!("data: "),
                json!("data: {}"),
                json!("data: []"),
                json!("not json"),
                json!(" data:[DONE] "),
            ],
        ),
        stream(
            "events-before-created",
            &[
                delta(text, json!("early")),
                created("resp_late"),
                completed(json!({})),
            ],
        ),
        stream(
            "created-without-a-response",
            &[
                json!({ "type": "response.created" }),
                json!({ "type": "response.created", "response": { "id": "resp_ignored" } }),
                completed(json!({})),
            ],
        ),
        stream(
            "created-at-forms",
            &[
                json!({ "type": "response.created", "response": { "id": "resp_1", "created_at": "1700000000", "model": "" } }),
                json!({ "type": "response.completed", "response": { "status": "", "usage": { "prompt_tokens": 5, "completion_tokens": 6, "reasoning_tokens": 2, "cached_tokens": 1 } } }),
            ],
        ),
        stream(
            "loosely-typed-values",
            &[
                json!({ "type": "response.created", "response": { "id": { "x": 1 }, "model": 5, "created_at": 1.7e9 } }),
                delta(text, json!({ "not": "text" })),
                delta(summary, json!(12)),
                added(
                    json!({ "type": "function_call", "name": { "n": 1 }, "call_id": { "c": 2 } }),
                ),
                delta(arguments, json!({ "a": [1, 2] })),
                json!({ "type": "response.completed", "response": { "status": 7, "usage": { "input_tokens": "12", "output_tokens": 1.9, "total_tokens": "x", "input_tokens_details": { "cached_tokens": true } } } }),
            ],
        ),
        stream(
            "usage-not-an-object",
            &[created("resp_1"), completed(json!("lots"))],
        ),
        stream(
            "unknown-and-failed-events",
            &[
                created("resp_1"),
                json!({ "type": "response.content_part.added", "part": { "type": "output_text", "text": "" } }),
                json!({ "type": "response.output_text.done", "text": "x" }),
                done(json!({ "type": "web_search_call", "id": "ws_1" })),
                json!({ "type": "response.failed", "response": { "error": { "message": "boom" } } }),
                json!({ "type": "error", "message": "boom" }),
            ],
        ),
        stream("empty", &[]),
        known(
            stream(
                "unreadable-event",
                &[json!(
                    r#"data: {"type":"response.created","response":{"id":"resp_1"}"#
                )],
            ),
            "an event that isn't valid JSON reads as one with no fields; gjson reads what it can",
        ),
    ]
}

/// Final events for the non-streaming translator.
pub fn finals() -> Vec<Case> {
    vec![
        final_event(
            "every-item-kind",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "object": "response",
                    "created_at": 1_700_000_000,
                    "status": "completed",
                    "model": "gpt-5.4",
                    "output": [
                        { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Thinking" }] },
                        message("Hello"),
                        { "type": "message", "content": [{ "text": "a" }, { "text": "" }, { "content": "b" }, { "type": "refusal", "refusal": "no" }] },
                        call("function_call", json!({ "id": "fc_1", "call_id": "call_1" }), "{\"city\":\"Paris\",\"days\":1.50}"),
                        { "type": "tool_call", "id": " fc_2 ", "name": "search", "arguments": { "q": "x" } },
                        { "type": "image_generation_call", "result": "aGVsbG8=", "output_format": "webp" },
                        { "type": "web_search_call", "id": "ws_1" }
                    ],
                    "usage": codex_usage()
                }
            }),
        ),
        final_event(
            "bare-response",
            json!({ "id": "resp_1", "status": "incomplete", "model": "gpt-5.4", "output": [message("cut")] }),
        ),
        final_event(
            "no-id-or-model",
            json!({ "type": "response.completed", "response": { "output": [message("hi")] } }),
        ),
        final_event(
            "argument-forms",
            json!({ "response": { "id": "resp_1", "output": [
                call("function_call", json!({ "call_id": "call_1" }), ""),
                call("function_call", json!({ "call_id": "call_2" }), "[1,2]"),
                call("function_call", json!({ "call_id": "call_3" }), "not json"),
                call("function_call", json!({ "call_id": "call_4" }), "null"),
                call("function_call", json!({ "call_id": "call_5" }), " {\"padded\":true} "),
                { "type": "function_call", "name": "no_arguments", "call_id": "call_6" },
                { "type": "function_call", "name": "number_arguments", "call_id": "call_7", "arguments": 7 },
                { "type": "function_call", "id": "", "call_id": " " }
            ] } }),
        ),
        final_event(
            "reasoning-forms",
            json!({ "response": { "id": "resp_1", "output": [
                { "type": "reasoning", "content": "as text" },
                { "type": "reasoning", "content": [{ "text": "a" }, { "summary_text": "b" }, { "summary_text": { "c": 1 } }] },
                { "type": "reasoning", "content": 5, "summary": "summary instead" },
                { "type": "reasoning", "summary": [{ "text": "" }] },
                { "type": "reasoning" }
            ] } }),
        ),
        final_event(
            "usage-chat-names",
            json!({ "response": { "id": "resp_1", "output": [], "usage": { "prompt_tokens": 5, "completion_tokens": 6, "reasoning_tokens": 2, "cached_tokens": 1 } } }),
        ),
        final_event(
            "usage-without-details",
            json!({ "response": { "id": "resp_1", "usage": { "input_tokens": 0, "output_tokens": 0 } } }),
        ),
        final_event(
            "usage-loosely-typed",
            json!({ "response": { "id": "resp_1", "usage": { "input_tokens": "12", "output_tokens": 1.9, "total_tokens": true, "output_tokens_details": { "reasoning_tokens": "3" } } } }),
        ),
        final_event(
            "output-as-an-object",
            json!({ "response": { "id": "resp_1", "output": { "first": message("from an object"), "second": "skipped" } } }),
        ),
        final_event(
            "loosely-typed-fields",
            json!({ "response": { "id": { "x": 1 }, "model": 5, "status": 7, "output": "none" } }),
        ),
        final_event("response-null", json!({ "response": null })),
        final_event("body-not-an-object", json!("text")),
        Case {
            model: MODEL.into(),
            ..Case::response("no-body", "{}", Vec::new())
        },
        known(
            final_event(
                "broken-arguments",
                json!({ "response": { "id": "resp_1", "output": [call("function_call", json!({ "call_id": "call_1" }), "{\"broken\":")] } }),
            ),
            "upstream writes call arguments that start as a JSON object but aren't one into its response as they are, which isn't JSON; the port writes {}",
        ),
    ]
}
