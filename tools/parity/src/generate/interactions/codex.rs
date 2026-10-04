//! Random input for the Interactions to Codex translators' suites (P4
//! WP4-D, see `crate::interactions::codex`):
//! - [`request_cases`]: Interactions requests from the shared generator (the
//!   parent module), now and then with the fields only this translator reads
//!   added: the generation settings Codex takes, under either spelling, with
//!   reasoning levels, budgets and summaries named every way; the top-level
//!   fields that pass through; media parts of every kind, inline, by URI or
//!   in a `file`; steps nested under a role and steps of the other types it
//!   reads; and tools declared with a JSON schema, a `$schema`, names over
//!   64 bytes, or as something other than a list.
//! - [`stream_cases`]: Codex event streams: output items of every kind
//!   added, streamed in deltas and done, between unknown events, blank lines
//!   and `event:` lines, ending completed, incomplete, with `[DONE]` or not
//!   at all, each line with or without its `data:` prefix.
//! - [`final_cases`]: Codex's final event, or a bare response, with output
//!   items of every kind and usage under Codex's or Chat Completions' names.
//!
//! None has a model name with `antigravity` in it, or what the port does
//! differently on purpose (see its module docs): an integer out of `i64`'s
//! range where the translators read one (see [`in_int64_range`]), a repeated
//! key, two generation settings for the same field (or `text` and
//! `verbosity`), which upstream copies in Go's map order, JSON text
//! `serde_json` can't read, or call arguments that start as a JSON object
//! but aren't one. Function call items always have an ID, so the stream
//! translator never makes one up from the clock, which on Windows can give
//! Go the same reading twice.

use serde_json::{Map, Value, json};

use super::{SUMMARIES, THINKING_LEVELS, odd_value, render, text};
use crate::cases::Case;
use crate::generate::{EFFORTS, Rng, SERVICE_TIERS, escape_text, num, to_object};

/// Models the client asks for. None names antigravity.
const MODELS: &[&str] = &[
    "gpt-5-codex",
    "gpt-5",
    "gpt-5.1-codex-max",
    "gpt-5.4",
    " GPT-5 ",
    "",
];

/// The generation config's settings Codex takes, grouped by the field they
/// set. A request gives at most one of each group: upstream copies them in
/// Go's map order, so which of two wins changes from run to run. `text` and
/// `verbosity` share a group for the same reason.
const SETTINGS: &[&[&str]] = &[
    &["max_output_tokens", "maxOutputTokens", "max_tokens"],
    &["temperature"],
    &["top_p", "topP"],
    &["presence_penalty", "presencePenalty"],
    &["frequency_penalty", "frequencyPenalty"],
    &["parallel_tool_calls", "parallelToolCalls"],
    &["response_format", "responseFormat"],
    &["text", "verbosity"],
    &["truncation"],
    &["tool_choice", "toolChoice"],
    &["service_tier", "serviceTier"],
];

/// Token limits: what a client sends, and other types.
const TOKENS: &[&str] = &[
    "0",
    "1",
    "1024",
    "64000",
    "-1",
    "\"2048\"",
    "1.5",
    "1e3",
    "null",
    "true",
    "9007199254740993",
];

/// Sampling settings, of every type.
const KNOBS: &[&str] = &["0", "0.7", "1", "2.0", "-1", "1.50", "\"0.5\"", "null"];

/// Thinking budgets around the levels' bounds, and of other types. Those
/// out of `i64`'s range are replaced (see [`in_int64_range`]).
const BUDGETS: &[&str] = &[
    "-1", "0", "1", "512", "1024", "1025", "8192", "24576", "24577", "100000", "1024.9",
    "\"2048\"", "null", "true",
];

/// Inline data: base64, empty, and the shared texts, with control
/// characters upstream quotes with escapes gjson stops at.
const DATA: &[&str] = &[
    "aGVsbG8=",
    "",
    " ",
    "\u{0}nul \u{1f}unit \u{7f}del",
    "emoji 🚀🔥👍🏽",
    "\u{feff}byte order mark",
    "\u{85}next line\u{a0}no-break space",
    "quote \" backslash \\ slash /",
    "tab\tand\rreturn",
];

/// MIME types by family, padded and in capitals, and empty.
const MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "IMAGE/WEBP",
    "audio/wav",
    "audio/ogg",
    "audio/x-wav",
    "audio/mpeg",
    "audio/L16",
    " audio/flac ",
    "application/pdf",
    "text/plain",
    "text/csv",
    "application/json",
    "text/xml",
    "video/mp4",
    "application/octet-stream",
    "\u{b}image/png",
    "",
];

/// URIs and URLs, a data URL among them, and blank ones.
const URIS: &[&str] = &[
    "https://example.com/cat.png",
    "gs://bucket/report.pdf",
    "data:image/png;base64,aGk=",
    "files/abc-123",
    " ",
    "",
];

/// Part types that aren't a kind the translator knows by name, under which it
/// reads `inline_data` and `file_data`.
const INLINE_TYPES: &[&str] = &["inline_data", "blob", "file_data", "media"];

/// Tool names: short ones, over 64 bytes, `mcp__` ones long and short, and
/// long multi-byte ones cut in a character.
const NAMES: &[&str] = &[
    "get_weather",
    "search",
    "mcp__server__tool",
    "a_very_long_function_name_that_goes_on_and_on_well_past_the_sixty_four_byte_limit",
    "mcp__a_server_with_a_rather_long_name_indeed__and_a_tool_whose_name_is_long_too_still",
    "mcp__a_very_long_server_name_that_pushes_the_whole_name_over_the_limit__tool",
    "mcp__no_second_separator_and_long_enough_to_be_cut_at_sixty_four_bytes_x",
    "名前名前名前名前名前名前名前名前名前名前名前名前",
    "mcp__名前名前名前名前名前名前名前名前名前名前名前名前名前",
    "exactly_sixty_four_bytes_long_name_for_a_function_tool_abcdefgh",
    "exactly_sixty_five_bytes_long_name_for_a_function_tool_abcdefghi",
    "",
];

/// Roles, some the translator maps and some it doesn't.
const ROLES: &[&str] = &[
    "user",
    "model",
    "assistant",
    "system",
    "developer",
    " Model ",
    "tool",
    "",
];

/// Call IDs: plain, padded, multi-byte and blank.
const CALL_IDS: &[&str] = &["call_1", " call_2 ", "call_🚀", "fc_3", " ", ""];

/// Builds `count` request cases. Each case depends only on `seed` and its
/// index.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed, index);
            let model = rng.pick(MODELS).to_owned();
            let mut request = super::request(&mut rng);
            if let Value::Object(fields) = &mut request
                && rng.chance(70)
            {
                extras(&mut rng, fields);
            }
            in_int64_range(&mut request);
            let request = render(&mut rng, &request);
            let case = Case::new(format!("codex-{seed}-{index}"), model, request);
            if rng.chance(40) {
                let stream = rng.chance(50);
                return case.with_options(json!({ "stream": stream }));
            }
            case
        })
        .collect()
}

/// Builds `count` Codex event streams, each with a request from the shared
/// generator as the client's.
pub fn stream_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed, !index);
            let model = rng.pick(MODELS).to_owned();
            let request = super::request(&mut rng);
            let request = render(&mut rng, &request);
            let response = Response::random(&mut rng);
            let events = response.events(&mut rng);
            Case {
                model,
                ..Case::response(format!("codex-{seed}-{index}-stream"), request, events)
            }
        })
        .collect()
}

/// Builds `count` final events, or bare responses, each with a request from
/// the shared generator as the client's.
pub fn final_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(!seed, index);
            let model = rng.pick(MODELS).to_owned();
            let request = super::request(&mut rng);
            let request = render(&mut rng, &request);
            let response = Response::random(&mut rng);
            let body = response.body(&mut rng);
            Case {
                model,
                ..Case::response(format!("codex-{seed}-{index}-final"), request, vec![body])
            }
        })
        .collect()
}

fn rng(seed: u64, index: u64) -> Rng {
    Rng(seed ^ 0x434F_4445_5849_4E54 ^ index.wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
}

/// A JSON literal from one of the pools.
fn raw(text: &str) -> Value {
    serde_json::from_str(text).expect("the pools hold JSON")
}

/// Replaces every number in `value` that is out of `i64`'s range with one
/// that isn't. The translators read token counts, budgets and times as
/// integers, as gjson's `Int` does, and Go converts a float out of range
/// differently on each CPU, where the port saturates.
pub fn in_int64_range(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let text = number.to_string();
            let in_range = text.parse::<i64>().is_ok()
                || (text.contains(['.', 'e', 'E'])
                    && text
                        .parse::<f64>()
                        .is_ok_and(|float| float.abs() < 9_223_372_036_854_775_808.0));
            if !in_range {
                *value = json!(2048);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(in_int64_range),
        Value::Object(fields) => fields.values_mut().for_each(in_int64_range),
        _ => {}
    }
}

/// Adds some of the fields only this translator reads to a request.
fn extras(rng: &mut Rng, fields: &mut Map<String, Value>) {
    if rng.chance(50) {
        generation_settings(rng, fields);
    }
    if rng.chance(40) {
        top_level(rng, fields);
    }
    if rng.chance(50) {
        more_input(rng, fields);
    }
    if rng.chance(35) {
        more_tools(rng, fields);
    }
    if rng.chance(10) {
        let parts = json!([{ "text": text(rng) }, { "text": odd_value(rng) }, { "text": { "nested": [1, 2] } }]);
        fields.insert("system_instruction".into(), json!({ "parts": parts }));
        fields.remove("systemInstruction");
    }
}

/// The generation settings Codex takes, at most one for each field, and
/// reasoning named every way, in the request's generation config.
fn generation_settings(rng: &mut Rng, fields: &mut Map<String, Value>) {
    let key = if fields.contains_key("generation_config") {
        "generation_config"
    } else if fields.contains_key("generationConfig") {
        "generationConfig"
    } else {
        rng.pick(&["generation_config", "generationConfig"])
    };
    let Value::Object(config) = fields.entry(key).or_insert_with(|| json!({})) else {
        return;
    };
    for &sources in SETTINGS {
        if rng.chance(30) && !sources.iter().any(|source| config.contains_key(*source)) {
            let source = rng.pick(sources);
            config.insert(source.into(), setting(rng, source));
        }
    }
    if rng.chance(20) {
        let reasoning = match rng.below(4) {
            0 => json!({ "effort": rng.pick(EFFORTS) }),
            1 => json!({ "summary": rng.pick(SUMMARIES) }),
            2 => json!({ "effort": rng.pick(EFFORTS), "summary": rng.pick(SUMMARIES), "extra": 1 }),
            _ => odd_value(rng),
        };
        config.insert("reasoning".into(), reasoning);
    }
    if rng.chance(20) {
        config.insert("thinkingLevel".into(), rng.pick(THINKING_LEVELS).into());
    }
    if rng.chance(20) {
        let key = rng.pick(&["thinking_budget", "thinkingBudget"]);
        config.insert(key.into(), raw(rng.pick(BUDGETS)));
    }
    if rng.chance(15) {
        let key = rng.pick(&["include_thoughts", "includeThoughts"]);
        let value = rng.pick(&[
            json!(true),
            json!(false),
            json!("true"),
            json!(1),
            Value::Null,
        ]);
        config.insert(key.into(), value);
    }
    if rng.chance(15)
        && !config.contains_key("thinking_config")
        && !config.contains_key("thinkingConfig")
    {
        let key = rng.pick(&["thinking_config", "thinkingConfig"]);
        let mut thinking = Vec::new();
        if rng.chance(50) {
            let key = rng.pick(&["thinking_level", "thinkingLevel"]);
            thinking.push((key, rng.pick(THINKING_LEVELS).into()));
        }
        if rng.chance(50) {
            let key = rng.pick(&["thinking_budget", "thinkingBudget"]);
            thinking.push((key, raw(rng.pick(BUDGETS))));
        }
        if rng.chance(50) {
            let key = rng.pick(&["include_thoughts", "includeThoughts"]);
            thinking.push((key, json!(rng.chance(50))));
        }
        config.insert(key.into(), to_object(thinking));
    }
}

/// A value for the generation setting `source`.
fn setting(rng: &mut Rng, source: &str) -> Value {
    match source {
        "max_output_tokens" | "maxOutputTokens" | "max_tokens" => raw(rng.pick(TOKENS)),
        "parallel_tool_calls" | "parallelToolCalls" => {
            rng.pick(&[json!(true), json!(false), json!("true"), Value::Null])
        }
        "response_format" | "responseFormat" => match rng.below(3) {
            0 => json!({ "type": "json_schema", "name": "answer", "schema": { "type": "object" } }),
            1 => json!({ "type": "text" }),
            _ => odd_value(rng),
        },
        "text" => match rng.below(3) {
            0 => json!({ "format": { "type": "text" } }),
            1 => json!({ "verbosity": "low", "format": { "type": "json_object" } }),
            _ => odd_value(rng),
        },
        "verbosity" => rng.pick(&[json!("low"), json!("high"), json!(""), json!(1)]),
        "truncation" => rng.pick(&[json!("auto"), json!("disabled"), Value::Null]),
        "tool_choice" | "toolChoice" => rng.pick(&[
            json!("auto"),
            json!("required"),
            json!({ "type": "function", "name": "get_weather" }),
            json!(""),
        ]),
        "service_tier" | "serviceTier" => rng.pick(SERVICE_TIERS).into(),
        _ => raw(rng.pick(KNOBS)),
    }
}

/// The top-level fields that pass through, and others the translator
/// leaves out, among them a client's own `prompt_cache_key`.
fn top_level(rng: &mut Rng, fields: &mut Map<String, Value>) {
    let mut add = |rng: &mut Rng, key: &str, values: &[Value]| {
        if rng.chance(30) {
            let value = rng.pick(values);
            fields.entry(key).or_insert(value);
        }
    };
    add(
        rng,
        "parallel_tool_calls",
        &[json!(true), json!(false), json!("no")],
    );
    add(rng, "store", &[json!(false), json!(true), Value::Null]);
    add(
        rng,
        "metadata",
        &[
            json!({ "user_id": "u_1", "trace": "t" }),
            json!({}),
            json!("plain"),
        ],
    );
    add(
        rng,
        "include",
        &[
            json!(["reasoning.encrypted_content"]),
            json!([]),
            json!("x"),
        ],
    );
    add(rng, "truncation", &[json!("auto"), json!("disabled")]);
    add(
        rng,
        "tool_choice",
        &[
            json!("none"),
            json!({ "type": "function", "name": "search" }),
        ],
    );
    add(rng, "prompt_cache_key", &[json!("client-key-1"), json!("")]);
    add(
        rng,
        "service_tier",
        &[json!("priority"), json!(" FAST "), json!("flex"), json!(1)],
    );
    add(
        rng,
        "stream",
        &[json!(true), json!("true"), json!(1), Value::Null],
    );
}

/// Steps only this translator reads, after the request's own steps or in
/// their place.
fn more_input(rng: &mut Rng, fields: &mut Map<String, Value>) {
    let steps: Vec<Value> = (0..1 + rng.below(4)).map(|_| step(rng)).collect();
    match fields.get_mut("input") {
        Some(Value::Array(input)) => input.extend(steps),
        _ if rng.chance(30) => {
            let input = json!({ "role": rng.pick(ROLES), "steps": steps });
            fields.insert("input".into(), input);
        }
        _ => {
            fields.insert("input".into(), Value::Array(steps));
        }
    }
}

fn step(rng: &mut Rng) -> Value {
    match rng.below(13) {
        0 | 1 => json!({ "type": "user_input", "content": media_parts(rng) }),
        2 => json!({ "role": rng.pick(ROLES), "steps": [simple_step(rng), simple_step(rng)] }),
        3 => json!({ "type": "message", "role": rng.pick(ROLES), "content": media_parts(rng) }),
        4 => json!({ "type": rng.pick(&["assistant", " Model_Output "]), "content": text(rng) }),
        5 => {
            let mut fields: Vec<(&str, Value)> = vec![("type", json!("reasoning"))];
            if rng.chance(60) {
                fields.push(("id", rng.pick(&[json!("rs_1"), json!(7), json!("")])));
            }
            let content = match rng.below(4) {
                0 => text(rng).into(),
                1 => json!({ "text": text(rng) }),
                2 => json!([{ "text": text(rng) }, { "text": "" }, { "text": text(rng) }]),
                _ => odd_value(rng),
            };
            fields.push(("content", content));
            if rng.chance(40) {
                fields.push(("text", text(rng).into()));
            }
            to_object(fields)
        }
        6 | 7 => {
            let mut fields: Vec<(&str, Value)> = vec![
                (
                    "type",
                    rng.pick(&[json!("function_call"), json!(" FUNCTION_CALL ")]),
                ),
                ("name", rng.pick(NAMES).into()),
            ];
            fields.push((rng.pick(&["call_id", "id"]), rng.pick(CALL_IDS).into()));
            let arguments = match rng.below(4) {
                0 => json!({ "city": text(rng) }),
                1 => json!("{\"city\":\"Paris\"}"),
                2 => odd_value(rng),
                _ => json!([1, { "a": "b" }]),
            };
            fields.push((rng.pick(&["arguments", "args"]), arguments));
            to_object(fields)
        }
        8 => {
            let mut fields: Vec<(&str, Value)> = vec![(
                "type",
                rng.pick(&[json!("function_call_output"), json!("function_result")]),
            )];
            fields.push((rng.pick(&["call_id", "id"]), rng.pick(CALL_IDS).into()));
            let output = match rng.below(3) {
                0 => text(rng).into(),
                1 => json!({ "ok": true, "items": [1, 2] }),
                _ => odd_value(rng),
            };
            fields.push((rng.pick(&["output", "result"]), output));
            to_object(fields)
        }
        9 => json!({ "text": text(rng), "role": rng.pick(ROLES) }),
        10 => text(rng).into(),
        11 => json!({ "type": "model_output", "content": media_part(rng) }),
        _ => json!({ "type": "thought", "id": "th_1", "content": [{ "text": text(rng) }] }),
    }
}

/// A step that converts to a message, for nesting under a role.
fn simple_step(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 => text(rng).into(),
        1 => json!({ "type": "user_input", "content": text(rng) }),
        2 => json!({ "content": [{ "type": "text", "text": text(rng) }] }),
        _ => json!({ "type": "model_output", "content": [{ "text": text(rng) }] }),
    }
}

fn media_parts(rng: &mut Rng) -> Value {
    (0..1 + rng.below(3)).map(|_| media_part(rng)).collect()
}

/// A content part of one of the kinds the translator converts, or one it
/// drops.
fn media_part(rng: &mut Rng) -> Value {
    let mime_key = |rng: &mut Rng| rng.pick(&["mime_type", "mimeType"]);
    let mime = |rng: &mut Rng| -> Value { rng.pick(MIME_TYPES).into() };
    let data = |rng: &mut Rng| -> Value {
        if rng.chance(90) {
            rng.pick(DATA).into()
        } else {
            odd_value(rng)
        }
    };
    let uri = |rng: &mut Rng| -> Value { rng.pick(URIS).into() };
    match rng.below(16) {
        0 => json!({ "type": "image", "url": uri(rng) }),
        1 => {
            let key = rng.pick(&["file_uri", "fileUri"]);
            json!({ "type": "image", key: uri(rng), mime_key(rng): mime(rng) })
        }
        2 => {
            json!({ "type": rng.pick(&["image", "Image"]), mime_key(rng): mime(rng), "data": data(rng) })
        }
        3 => json!({ "type": "image_url", "image_url": { "url": uri(rng) } }),
        4 => json!({ "type": "audio", mime_key(rng): mime(rng), "data": data(rng) }),
        5 => {
            let audio = if rng.chance(70) {
                json!({ "data": "aGVsbG8=", "format": "wav" })
            } else {
                odd_value(rng)
            };
            json!({ "type": "input_audio", "input_audio": audio })
        }
        6 => {
            let kind = rng.pick(&["file", "document", "video"]);
            let file = json!({ "file_data": data(rng), "filename": rng.pick(&["a.pdf", ""]) });
            json!({ "type": kind, "file": file, mime_key(rng): mime(rng) })
        }
        7 => {
            let kind = rng.pick(&["file", "document", "video"]);
            let key = rng.pick(&["file_uri", "fileUri", "url"]);
            json!({ "type": kind, mime_key(rng): mime(rng), key: uri(rng) })
        }
        8 => {
            let kind = rng.pick(&["file", "document", "video", "DOCUMENT"]);
            json!({ "type": kind, mime_key(rng): mime(rng), "data": data(rng) })
        }
        9 | 10 => {
            let key = rng.pick(&["inline_data", "inlineData"]);
            let inline = json!({ mime_key(rng): mime(rng), "data": data(rng) });
            json!({ "type": rng.pick(INLINE_TYPES), key: inline })
        }
        11 => {
            let key = rng.pick(&["file_data", "fileData"]);
            let uri_key = rng.pick(&["file_uri", "fileUri"]);
            let file = json!({ mime_key(rng): mime(rng), uri_key: uri(rng) });
            json!({ "type": rng.pick(INLINE_TYPES), key: file })
        }
        12 => json!({ "type": "text", "text": odd_value(rng) }),
        13 => json!({ "type": rng.pick(&["text", ""]) }),
        14 => json!({ "inline_data": { "mime_type": "image/png", "data": "aGk=" } }),
        _ => json!({ "type": "text", "text": text(rng) }),
    }
}

/// Function declarations in the shapes the translator reads, and tools it
/// leaves alone.
fn more_tools(rng: &mut Rng, fields: &mut Map<String, Value>) {
    if rng.chance(10) {
        let tools = rng.pick(&[json!({ "name": "solo" }), json!("tools"), Value::Null]);
        fields.insert("tools".into(), tools);
        return;
    }
    let extra: Vec<Value> = (0..1 + rng.below(3)).map(|_| tool(rng)).collect();
    match fields.get_mut("tools") {
        Some(Value::Array(tools)) => tools.extend(extra),
        _ => {
            fields.insert("tools".into(), Value::Array(extra));
        }
    }
}

fn tool(rng: &mut Rng) -> Value {
    match rng.below(7) {
        0 | 1 => declaration(rng, Some("function")),
        2 => {
            let key = rng.pick(&["function_declarations", "functionDeclarations"]);
            let declarations: Vec<Value> = (0..1 + rng.below(3))
                .map(|_| declaration(rng, None))
                .collect();
            json!({ key: declarations })
        }
        3 => {
            json!({ rng.pick(&["function_declarations", "functionDeclarations"]): odd_value(rng) })
        }
        4 => declaration(rng, None),
        5 => json!({ "type": "function", "description": "No name." }),
        _ => rng.pick(&[
            json!({ "type": "web_search" }),
            json!({ "code_execution": {} }),
        ]),
    }
}

fn declaration(rng: &mut Rng, kind: Option<&str>) -> Value {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if let Some(kind) = kind {
        fields.push(("type", kind.into()));
    }
    fields.push((
        "name",
        if rng.chance(95) {
            rng.pick(NAMES).into()
        } else {
            odd_value(rng)
        },
    ));
    if rng.chance(70) {
        let description = if rng.chance(90) {
            text(rng).into()
        } else {
            odd_value(rng)
        };
        fields.push(("description", description));
    }
    if rng.chance(85) {
        let key = rng.pick(&[
            "parameters",
            "parametersJsonSchema",
            "parameters_json_schema",
        ]);
        fields.push((key, schema(rng)));
    }
    to_object(fields)
}

/// A tool's parameters: schemas with and without `$schema` and
/// `additionalProperties`, and values that aren't objects.
fn schema(rng: &mut Rng) -> Value {
    match rng.below(8) {
        0 => json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
        1 => json!({ "type": "object", "additionalProperties": true, "properties": {} }),
        2 => json!({ "type": "object", "additionalProperties": false }),
        3 => json!({
            "type": "object",
            "properties": { "days": { "type": "number", "maximum": num("1.50"), "minimum": num("1e3") } },
        }),
        4 => {
            json!({ "additionalProperties": { "type": "string" }, "$schema": "x", "type": "object" })
        }
        5 => json!([1, 2]),
        6 => odd_value(rng),
        _ => json!({}),
    }
}

/// A response Codex might give, to stream ([`Self::events`]) or give whole
/// ([`Self::body`]).
struct Response {
    id: Option<Value>,
    model: Option<Value>,
    created_at: Option<Value>,
    status: Option<Value>,
    items: Vec<Item>,
    usage: Option<Value>,
    end: End,
}

/// One of a response's output items, as it is when done.
struct Item {
    /// The item, done.
    done: Value,
    /// What the stream adds before it is done: the item as added, and the
    /// event type and pieces of its deltas.
    added: Option<Value>,
    deltas: Option<(&'static str, Vec<Value>)>,
}

/// How a stream ends.
enum End {
    Completed,
    Incomplete,
    Done,
    CompletedThenDone,
    Failed,
    Unended,
}

impl Response {
    fn random(rng: &mut Rng) -> Self {
        let id = rng.chance(85).then(|| {
            rng.pick(&[
                json!("resp_1"),
                json!("resp_🚀"),
                json!(""),
                json!(123),
                json!({ "x": 1 }),
            ])
        });
        let model = rng.chance(80).then(|| {
            rng.pick(&[
                json!("gpt-5-codex"),
                json!("gpt-5.4-2026-03-05"),
                json!(""),
                json!(5),
            ])
        });
        let created_at = rng.chance(70).then(|| {
            raw(rng.pick(&[
                "1700000000",
                "0",
                "-1",
                "\"1700000000\"",
                "1.7e9",
                "null",
                "1",
                "true",
            ]))
        });
        let status = rng.chance(85).then(|| {
            rng.pick(&[
                json!("completed"),
                json!("incomplete"),
                json!("failed"),
                json!(""),
                json!(1),
            ])
        });
        let items = (0..rng.below(5))
            .map(|index| Item::random(rng, index))
            .collect();
        let usage = rng.chance(75).then(|| usage(rng));
        let end = match rng.below(20) {
            0..=8 => End::Completed,
            9 | 10 => End::Incomplete,
            11 | 12 => End::Done,
            13..=15 => End::CompletedThenDone,
            16 => End::Failed,
            _ => End::Unended,
        };
        Self {
            id,
            model,
            created_at,
            status,
            items,
            usage,
            end,
        }
    }

    /// The response object, with its output as `output`.
    fn object(&self, status: Option<Value>, output: Value) -> Value {
        let mut fields: Vec<(&str, Value)> = Vec::new();
        if let Some(id) = &self.id {
            fields.push(("id", id.clone()));
        }
        fields.push(("object", json!("response")));
        if let Some(created_at) = &self.created_at {
            fields.push(("created_at", created_at.clone()));
        }
        if let Some(status) = status {
            fields.push(("status", status));
        }
        if let Some(model) = &self.model {
            fields.push(("model", model.clone()));
        }
        fields.push(("output", output));
        if let Some(usage) = &self.usage {
            fields.push(("usage", usage.clone()));
        }
        let mut object = to_object(fields);
        in_int64_range(&mut object);
        object
    }

    fn output(&self) -> Value {
        self.items.iter().map(|item| item.done.clone()).collect()
    }

    /// The stream's lines.
    fn events(&self, rng: &mut Rng) -> Vec<String> {
        let escape = rng.chance(10);
        let mut lines = Vec::new();
        let mut push = |rng: &mut Rng, event: Value| {
            let mut event = event;
            in_int64_range(&mut event);
            let mut json = event.to_string();
            if escape {
                json = escape_text(&json);
            }
            lines.push(line(rng, &json));
            if rng.chance(8) {
                lines.push(noise(rng));
            }
        };
        if rng.chance(85) {
            let response = self.object(Some(json!("in_progress")), json!([]));
            push(
                rng,
                json!({ "type": "response.created", "sequence_number": 0, "response": response }),
            );
        }
        if rng.chance(30) {
            let response = self.object(Some(json!("in_progress")), json!([]));
            push(
                rng,
                json!({ "type": "response.in_progress", "response": response }),
            );
        }
        for (index, item) in self.items.iter().enumerate() {
            if let Some(added) = &item.added {
                push(
                    rng,
                    json!({ "type": "response.output_item.added", "output_index": index, "item": added }),
                );
                if let Some((kind, pieces)) = &item.deltas {
                    for piece in pieces {
                        push(
                            rng,
                            json!({ "type": kind, "item_id": "item", "output_index": index, "delta": piece }),
                        );
                    }
                }
            }
            if rng.chance(85) {
                push(
                    rng,
                    json!({ "type": "response.output_item.done", "output_index": index, "item": item.done }),
                );
            }
        }
        let completed = |response: &Self, kind: &str| json!({ "type": kind, "response": response.object(response.status.clone(), response.output()) });
        match self.end {
            End::Completed => push(rng, completed(self, "response.completed")),
            End::Incomplete => push(rng, completed(self, "response.incomplete")),
            End::Done => lines.push(done_line(rng)),
            End::CompletedThenDone => {
                push(rng, completed(self, "response.completed"));
                lines.push(done_line(rng));
            }
            End::Failed => {
                push(rng, completed(self, "response.failed"));
                if rng.chance(50) {
                    lines.push(done_line(rng));
                }
            }
            End::Unended => {}
        }
        lines
    }

    /// The final event, or the response alone.
    fn body(&self, rng: &mut Rng) -> String {
        let output = match rng.below(10) {
            0 => {
                let fields: Map<String, Value> = self
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (format!("item_{index}"), item.done.clone()))
                    .collect();
                Value::Object(fields)
            }
            1 => odd_value(rng),
            _ => self.output(),
        };
        let response = self.object(self.status.clone(), output);
        let body = match rng.below(20) {
            0..=12 => {
                json!({ "type": "response.completed", "sequence_number": 9, "response": response })
            }
            13..=15 => response,
            16 => json!({ "type": "response.incomplete", "response": response }),
            17 => json!({ "response": odd_value(rng) }),
            _ => odd_value(rng),
        };
        let json = body.to_string();
        if rng.chance(10) {
            escape_text(&json)
        } else {
            json
        }
    }
}

impl Item {
    fn random(rng: &mut Rng, index: usize) -> Self {
        let id = format!("item_{index}");
        let added = rng.chance(80);
        let streamed = rng.chance(70);
        match rng.below(10) {
            0..=3 => {
                let texts: Vec<Value> = (0..1 + rng.below(3)).map(|_| piece(rng)).collect();
                let content: Vec<Value> = (0..rng.below(3))
                    .map(|_| match rng.below(6) {
                        0 => json!({ "type": "output_text", "content": text(rng) }),
                        1 => json!({ "type": "output_text", "text": odd_value(rng), "content": text(rng) }),
                        2 => json!({ "type": "refusal", "refusal": "no" }),
                        _ => json!({ "type": "output_text", "text": text(rng), "annotations": [] }),
                    })
                    .collect();
                let done = json!({ "id": id, "type": "message", "status": "completed", "role": "assistant", "content": content });
                Self {
                    added: added.then(
                        || json!({ "id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": [] }),
                    ),
                    deltas: streamed.then_some(("response.output_text.delta", texts)),
                    done,
                }
            }
            4 | 5 => {
                let pieces: Vec<Value> = (0..1 + rng.below(3)).map(|_| piece(rng)).collect();
                let mut done: Vec<(&str, Value)> =
                    vec![("id", id.clone().into()), ("type", json!("reasoning"))];
                match rng.below(5) {
                    0 => done.push(("content", text(rng).into())),
                    1 => done.push((
                        "content",
                        json!([{ "type": "reasoning_text", "text": text(rng) }, { "summary_text": text(rng) }, { "summary_text": odd_value(rng) }]),
                    )),
                    2 => done.push(("summary", text(rng).into())),
                    3 => done.push(("summary", odd_value(rng))),
                    _ => done.push((
                        "summary",
                        json!([{ "type": "summary_text", "text": text(rng) }, { "type": "summary_text", "text": text(rng) }]),
                    )),
                }
                if rng.chance(40) {
                    done.push(("encrypted_content", json!("gAAAA")));
                }
                let kind = rng.pick(&[
                    "response.reasoning_summary_text.delta",
                    "response.reasoning_text.delta",
                ]);
                Self {
                    added: added.then(|| json!({ "id": id, "type": "reasoning", "summary": [] })),
                    deltas: streamed.then_some((kind, pieces)),
                    done: to_object(done),
                }
            }
            6..=8 => {
                let kind = rng.pick(&["function_call", "function_call", "tool_call"]);
                let name: Value = if rng.chance(90) {
                    rng.pick(&["get_weather", "search", "名前", ""]).into()
                } else {
                    odd_value(rng)
                };
                // Always an ID, as `call_id` or `id` (see the module docs).
                let mut fields: Vec<(&str, Value)> = vec![("type", kind.into())];
                match rng.below(4) {
                    0 => fields.push(("id", json!(format!("fc_{index}")))),
                    1 => {
                        fields.push(("id", json!(format!("fc_{index}"))));
                        fields.push(("call_id", rng.pick(&[" ", ""]).into()));
                    }
                    _ => {
                        fields.push(("id", json!(format!("fc_{index}"))));
                        let call_id = rng.pick(&["call_1", " call_2 ", "call_🚀"]);
                        fields.push(("call_id", call_id.into()));
                    }
                }
                fields.push(("name", name));
                let arguments = arguments(rng);
                let mut added_fields = fields.clone();
                added_fields.push(("status", json!("in_progress")));
                added_fields.push(("arguments", json!("")));
                fields.push(("status", json!("completed")));
                if rng.chance(90) {
                    fields.push(("arguments", arguments.clone()));
                }
                let pieces = match &arguments {
                    Value::String(text) => split(rng, text),
                    other => vec![other.clone()],
                };
                Self {
                    added: added.then(|| to_object(added_fields)),
                    deltas: streamed.then_some(("response.function_call_arguments.delta", pieces)),
                    done: to_object(fields),
                }
            }
            _ => {
                let mut done = vec![
                    ("id", json!(id)),
                    ("type", json!("image_generation_call")),
                    ("status", json!("completed")),
                ];
                done.push((
                    "result",
                    rng.pick(&[json!("aGVsbG8="), json!(""), json!(1)]),
                ));
                if rng.chance(80) {
                    let format = rng.pick(&[
                        json!("png"),
                        json!("JPEG"),
                        json!("jpg"),
                        json!("webp"),
                        json!("gif"),
                        json!("image/avif"),
                        json!("bmp"),
                        json!(""),
                    ]);
                    done.push(("output_format", format));
                }
                Self {
                    added: added.then(|| json!({ "id": id, "type": "image_generation_call", "status": "in_progress" })),
                    deltas: None,
                    done: to_object(done),
                }
            }
        }
    }
}

/// A text delta: mostly text, sometimes a value of another type.
fn piece(rng: &mut Rng) -> Value {
    if rng.chance(90) {
        text(rng).into()
    } else {
        odd_value(rng)
    }
}

/// A call's arguments: JSON object text, other JSON, text that isn't JSON
/// (but never starts as an object), or an object.
fn arguments(rng: &mut Rng) -> Value {
    rng.pick(&[
        json!("{\"city\":\"Paris\"}"),
        json!("{ \"city\" : \"Paris\", \"days\": 1.50 }"),
        json!(" {\"padded\":true} "),
        json!("{}"),
        json!(""),
        json!("[1,2]"),
        json!("null"),
        json!("not json"),
        json!("1.50"),
        json!({ "city": "Paris", "days": num("1.50") }),
        json!(7),
    ])
}

/// `text` in up to three pieces, split at character boundaries.
fn split(rng: &mut Rng, text: &str) -> Vec<Value> {
    let chars: Vec<char> = text.chars().collect();
    let mut cuts: Vec<usize> = (0..rng.below(3))
        .map(|_| rng.below(chars.len() + 1))
        .collect();
    cuts.sort_unstable();
    let mut pieces = Vec::new();
    let mut start = 0;
    for cut in cuts.into_iter().chain([chars.len()]) {
        pieces.push(chars[start..cut].iter().collect::<String>().into());
        start = cut;
    }
    pieces
}

/// A response's usage, under Codex's names or Chat Completions', or a value
/// of another type.
fn usage(rng: &mut Rng) -> Value {
    let count = |rng: &mut Rng| {
        raw(rng.pick(&[
            "0",
            "1",
            "42",
            "1500",
            "\"12\"",
            "1.9",
            "-3",
            "null",
            "true",
            "1e3",
            "9007199254740993",
            "\"x\"",
        ]))
    };
    let mut fields: Vec<(&str, Value)> = Vec::new();
    match rng.below(6) {
        0..=2 => {
            for key in ["input_tokens", "output_tokens", "total_tokens"] {
                if rng.chance(80) {
                    fields.push((key, count(rng)));
                }
            }
            if rng.chance(60) {
                fields.push((
                    "input_tokens_details",
                    json!({ "cached_tokens": count(rng) }),
                ));
            }
            if rng.chance(60) {
                fields.push((
                    "output_tokens_details",
                    json!({ "reasoning_tokens": count(rng) }),
                ));
            }
        }
        3 => {
            for key in [
                "prompt_tokens",
                "completion_tokens",
                "total_tokens",
                "reasoning_tokens",
                "cached_tokens",
            ] {
                if rng.chance(80) {
                    fields.push((key, count(rng)));
                }
            }
        }
        4 => {}
        _ => return odd_value(rng),
    }
    to_object(fields)
}

/// `json` as a stream line: mostly after `data: `, sometimes bare, without
/// the space, padded or ending in a carriage return.
fn line(rng: &mut Rng, json: &str) -> String {
    match rng.below(20) {
        0..=13 => format!("data: {json}"),
        14 | 15 => json.to_owned(),
        16 => format!("data:{json}"),
        17 => format!("  data: {json}  "),
        _ => format!("data: {json}\r"),
    }
}

fn done_line(rng: &mut Rng) -> String {
    rng.pick(&["data: [DONE]", "[DONE]", "data:[DONE]", " data: [DONE] "])
        .to_owned()
}

/// A line that carries no event: blank, an `event:` line, a comment, an
/// empty `data:` or an unknown event.
fn noise(rng: &mut Rng) -> String {
    rng.pick(&[
        "",
        "event: response.output_text.delta",
        ": keep-alive",
        "data: ",
        "data: {\"type\":\"response.content_part.added\",\"part\":{\"type\":\"output_text\",\"text\":\"\"}}",
        "data: {\"type\":\"response.output_text.done\",\"text\":\"x\"}",
        "data: {\"type\":\"keepalive\"}",
        "data: {}",
        "data: []",
        "not json",
    ])
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_out_of_int64_range_are_replaced() {
        let mut value = json!([
            num("1e30"),
            num("123456789012345678901234567890"),
            num("9223372036854775808"),
            num("-9223372036854775809"),
            num("9.3e18"),
            num("9223372036854775807"),
            num("-9223372036854775808"),
            num("1.50"),
            num("9007199254740993"),
            { "nested": [num("1e30")] },
        ]);
        in_int64_range(&mut value);
        assert_eq!(
            value,
            json!([
                2048,
                2048,
                2048,
                2048,
                2048,
                num("9223372036854775807"),
                num("-9223372036854775808"),
                num("1.50"),
                num("9007199254740993"),
                { "nested": [2048] },
            ])
        );
    }

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let (requests, streams, finals) = (
            request_cases(5, 60),
            stream_cases(5, 60),
            final_cases(5, 60),
        );
        assert_eq!(requests.len(), 60);
        for (a, b) in requests.iter().zip(request_cases(5, 60)) {
            assert_eq!(a.request, b.request);
            assert!(
                serde_json::from_str::<Value>(&a.request).is_ok(),
                "{}",
                a.name
            );
        }
        for (a, b) in streams.iter().zip(stream_cases(5, 60)) {
            assert_eq!(a.events, b.events);
        }
        for (a, b) in finals.iter().zip(final_cases(5, 60)) {
            assert_eq!(a.events, b.events);
            assert!(
                serde_json::from_str::<Value>(&a.events[0]).is_ok(),
                "{}",
                a.name
            );
        }
    }

    #[test]
    fn names_never_contain_antigravity() {
        for case in request_cases(9, 200) {
            assert!(!case.model.to_lowercase().contains("antigravity"));
            assert!(!case.request.to_lowercase().contains("antigravity"));
        }
    }
}
