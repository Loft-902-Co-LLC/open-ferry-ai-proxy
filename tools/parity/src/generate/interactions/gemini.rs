//! Random input for the Gemini and Interactions translators' suites (P4
//! WP4-E, see `crate::interactions::gemini`):
//! - Interactions requests for a Gemini upstream: the parent module's, with
//!   what only these translators read added: generation configs whose keys
//!   are converted to camelCase (dots, digits, colons, escapes, underscores
//!   and non-ASCII among them), tool choices, response modalities, built-in
//!   and generic tools, system instructions of every shape, Chat Completions
//!   parts (data URLs, audio, files), media parts, native Gemini parts, and
//!   calls and results with odd arguments and `$ref` results;
//! - Gemini requests for an Interactions upstream: the Gemini request
//!   generator's ([`to_gemini`]) and our own, with parts of every kind,
//!   signatures under each name, calls with and without IDs, built-in tools
//!   under both names, system instructions and thinking configs;
//! - Gemini streams and responses for an Interactions client: parts split
//!   over chunks, finish reasons, usage under both names, alone, early or
//!   never, `data:` lines, `[DONE]` and lines that aren't JSON; and the
//!   Gemini response generator's streams;
//! - Interactions streams and responses for a Gemini client: the parent
//!   module's, with signed calls, argument deltas that aren't JSON, deltas
//!   for steps never started, `finish` events, errors with every code, SSE
//!   frames, and responses with calls, results and usage of odd shapes;
//! - Interactions requests, streams and responses passed through.
//!
//! None of the generation config keys holds `|`, `#`, `@`, `*` or `?`, and
//! no digit key is large, which the port leaves out on purpose (see its
//! deviations), and a key whose camelCase cuts a character in two holds only
//! a leaf. No count the Interactions → Gemini translators add up is out of
//! int64's range, where Go's conversion depends on the CPU and the port
//! saturates. No chunk or body is JSON cut short, which gjson reads in
//! part and the port reads as nothing. No model names antigravity.

use serde_json::{Value, json};

use super::{
    MODELS, SIGNATURES, TOOL_NAMES, event_cases, odd_value, render, request, request_cases, rng,
    text, token_count,
};
use crate::cases::Case;
use crate::generate::{NUMBERS, Rng, num, to_gemini, to_object};

/// Generation config keys, as either client writes them, and keys sjson
/// reads as more than a name.
const CONFIG_KEYS: &[&str] = &[
    "max_output_tokens",
    "maxOutputTokens",
    "temperature",
    "top_k",
    "topK",
    "stop_sequences",
    "thinking_config",
    "thinkingConfig",
    "thinking_budget",
    "includeThoughts",
    "response_mime_type",
    "responseSchema",
    "m_x",
    "mX",
    "a.b",
    "a.",
    ".a",
    "0",
    "1",
    "3",
    "-1",
    ":0",
    ":",
    "",
    "a\\.b",
    "a\\b",
    "_leading",
    "trailing_",
    "double__under",
    "x_é",
    "é_x",
    "HTTPServer",
    "ABC",
    "a b",
];

/// Values for a generation config's leaves.
fn config_leaf(rng: &mut Rng) -> Value {
    match rng.below(6) {
        0 | 1 => num(rng.pick(NUMBERS)),
        2 => text(rng).into(),
        3 => json!(rng.chance(50)),
        4 => Value::Null,
        _ => token_count(rng),
    }
}

/// Whether camelCase cuts a character of `key` in two: one after a `_` that
/// isn't ASCII. sjson can't find such a key once written, so upstream writes
/// it again for each leaf under it, and the port once (see its deviations).
fn breaks_utf8(key: &str) -> bool {
    key.split('_')
        .skip(1)
        .any(|part| part.chars().next().is_some_and(|c| !c.is_ascii()))
}

/// A generation config: keys from [`CONFIG_KEYS`] holding leaves, nested
/// objects and arrays, some empty. A key that [`breaks_utf8`] holds only a
/// leaf.
fn config(rng: &mut Rng, depth: usize) -> Value {
    let fields = (0..rng.below(5))
        .map(|_| {
            let key = rng.pick(CONFIG_KEYS);
            let value = match rng.below(if depth < 2 { 8 } else { 5 }) {
                _ if breaks_utf8(key) => config_leaf(rng),
                0..=4 => config_leaf(rng),
                5 => config(rng, depth + 1),
                6 => (0..rng.below(3))
                    .map(|_| {
                        if rng.chance(70) {
                            config_leaf(rng)
                        } else {
                            config(rng, depth + 1)
                        }
                    })
                    .collect(),
                _ => json!([]),
            };
            (key, value)
        })
        .collect();
    to_object(fields)
}

const TOOL_CHOICES: &[&str] = &[
    r#""auto""#,
    r#"" AUTO ""#,
    r#""none""#,
    r#""NONE""#,
    r#""any""#,
    r#""required""#,
    r#""validated""#,
    r#""other""#,
    r#""""#,
    "5",
    "null",
    "[]",
    r#"{"type":"function","function":{"name":"f"}}"#,
    r#"{"type":" Function ","function":{"name":" lookup "}}"#,
    r#"{"type":"function"}"#,
    r#"{"type":"function","name":"g"}"#,
    r#"{"type":"tool","name":"search"}"#,
    r#"{"type":"tool","name":{"a":1}}"#,
    r#"{"type":"any"}"#,
    r#"{"type":"auto"}"#,
    r#"{"type":"none"}"#,
    r#"{"type":"validated"}"#,
    r#"{"type":"allowed_tools","tools":["a"]}"#,
    r#"{"mode":"any"}"#,
];

const MODALITIES: &[&str] = &[
    r#""text""#,
    r#""TEXT""#,
    r#""image""#,
    r#"" Audio ""#,
    r#""video""#,
    r#""""#,
    "1",
    "null",
    r#"{"a":1}"#,
];

/// Tools as an Interactions client declares them, and values that aren't.
const INTERACTIONS_TOOLS: &[&str] = &[
    r#"{"type":"url_context"}"#,
    r#"{"type":"code_execution"}"#,
    r#"{"type":"google_search"}"#,
    r#"{"type":"web_search"}"#,
    r#"{"type":" Google_Search "}"#,
    r#"{"type":"url_context","url_context":{"a":1}}"#,
    r#"{"type":"google_search","googleSearch":{"x":[1]}}"#,
    r#"{"type":"code_execution","codeExecution":5}"#,
    r#"{"google_search":{}}"#,
    r#"{"googleSearch":{}}"#,
    r#"{"url_context":{}}"#,
    r#"{"codeExecution":{}}"#,
    r#"{"functionDeclarations":[{"name":"f"}]}"#,
    r#"{"function_declarations":[{"name":"g","parameters":{"type":"object"}}]}"#,
    r#"{"type":"function","name":"h","description":"d","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}"#,
    r#"{"type":"function","name":{"a":1},"description":5}"#,
    r#"{"type":"function"}"#,
    r#"{"z":1,"a":{"y":0.1,"b":1.50,"c":1e3,"d":-0}}"#,
    r#"{"type":"x","n":1e400,"m":123456789012345678901234567890}"#,
    r#"{"type":"","note":"<b>&</b>"}"#,
    r#""text""#,
    "5",
    "null",
    "[1]",
];

const SYSTEM_INSTRUCTIONS: &[&str] = &[
    r#""be brief""#,
    r#""""#,
    r#"{"text":"a"}"#,
    r#"{"text":{"b":[1, 2]}}"#,
    r#"{"text":5}"#,
    r#"{"parts":[{"text":"a"},{"text":"b"}]}"#,
    r#"{"parts":[{"text":"a"}],"text":"b"}"#,
    r#"{"parts":"x"}"#,
    r#"{"role":"system","parts":[{"text":"x"},{"inlineData":{}}]}"#,
    "5",
    "null",
    "[]",
];

const SERVICE_TIERS: &[&str] = &[
    r#""standard""#,
    r#"" Flex ""#,
    r#""priority""#,
    r#""""#,
    "5",
    "null",
    r#"{"a":1}"#,
];

/// Content parts only these translators read: Chat Completions parts, media
/// with URIs or URLs, inline data, and parts of no kind.
const CONTENT_PARTS: &[&str] = &[
    r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}"#,
    r#"{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}"#,
    r#"{"type":"image_url","image_url":{"url":"data:image/png,raw"}}"#,
    r#"{"type":"image_url","image_url":{"url":"data:;base64,AA=="}}"#,
    r#"{"type":"image_url","image_url":"data:image/png;base64,AA=="}"#,
    r#"{"type":"input_audio","input_audio":{"format":"wav","data":"AA=="}}"#,
    r#"{"type":"input_audio","input_audio":{"format":" FLAC ","data":"AQ=="}}"#,
    r#"{"type":"input_audio","input_audio":{"format":"pcm16","data":"Ag=="}}"#,
    r#"{"type":"input_audio","input_audio":{"format":"mp3"}}"#,
    r#"{"type":"input_audio","input_audio":{"data":"BQ=="}}"#,
    r#"{"type":"file","file":{"filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0="}}"#,
    r#"{"type":"file","file":{"filename":"notes.txt","file_data":"aGVsbG8="}}"#,
    r#"{"type":"file","file":{"filename":"image.PNG","file_data":"data:text/plain,hello"}}"#,
    r#"{"type":"file","file":{"filename":"x.unknownext","file_data":"AA=="}}"#,
    r#"{"type":"file","file":{"file_data":"JVBERi0="}}"#,
    r#"{"type":"file","file":{"filename":"a.pdf"}}"#,
    r#"{"type":" IMAGE ","mimeType":"image/png","data":"AA=="}"#,
    r#"{"type":"document","mime_type":"application/pdf","file_uri":"gs://bucket/a.pdf"}"#,
    r#"{"type":"video","fileUri":"https://example.com/v.mp4","mimeType":"video/mp4"}"#,
    r#"{"type":"audio","url":"data:audio/wav;base64,UklGRg=="}"#,
    r#"{"type":"image","url":"https://example.com/a.png"}"#,
    r#"{"type":"image","mime_type":"","data":"AA=="}"#,
    r#"{"type":"image","mime_type":"image/png","data":"","file_uri":"gs://x"}"#,
    r#"{"type":"image","mime_type":{"a":1},"data":{"b":2}}"#,
    r#"{"type":"image","inline_data":{"mime_type":"image/png","data":"AA=="}}"#,
    r#"{"inlineData":{"mimeType":"image/gif","data":"R0lG"}}"#,
    r#"{"type":"text","text":{"a":[1, 2]}}"#,
    r#"{"text":"no type"}"#,
    r#"{"type":"unknown"}"#,
];

/// Gemini parts, as an Interactions step may carry them under `parts`.
const NATIVE_PARTS: &[&str] = &[
    r#"{"text":"a"}"#,
    r#"{"inlineData":{"mimeType":"image/png","data":"AA=="}}"#,
    r#"{"fileData":{"mime_type":"application/pdf","file_uri":"gs://a"}}"#,
    r#"{"inline_data":{"mime_type":"image/png","data":"AQ=="}}"#,
    r#"{"file_data":{"mimeType":"text/plain","fileUri":"gs://b"}}"#,
    r#"{"functionCall":{"name":"f","args":{"q":1}}}"#,
    r#"{"functionResponse":{"name":"f","response":{"r":2}}}"#,
    r#"{"inlineData":{"mimeType":"","data":"AA=="}}"#,
    r#"{"other":1}"#,
];

const RESULTS: &[&str] = &[
    r##""{\"$ref\":\"#/a\"}""##,
    r##"{"$ref":"#/components/b","x":1}"##,
    r#"[1,{"a":2}]"#,
    r#""[1,2]""#,
    r#""   ""#,
    r#""plain""#,
    "5",
    "null",
    r#"{"ok":true}"#,
];

const ARGUMENTS: &[&str] = &[
    r#""{\"a\":1}""#,
    r#"" not json ""#,
    r#""""#,
    "[1]",
    r#"{"q":"x"}"#,
    "null",
];

/// Parses one of the JSON literals above.
fn literal(rng: &mut Rng, literals: &[&str]) -> Value {
    serde_json::from_str(rng.pick(literals)).expect("the literals are JSON")
}

/// An Interactions step only these translators read something special in.
fn interactions_step(rng: &mut Rng) -> Value {
    let name = rng.pick(TOOL_NAMES);
    let id = format!("call_{}", rng.below(100));
    match rng.below(8) {
        0 | 1 => {
            let parts: Vec<Value> = (0..1 + rng.below(3))
                .map(|_| literal(rng, CONTENT_PARTS))
                .collect();
            let kind = rng.pick(&["user_input", "model_output", "thought"]);
            json!({ "type": kind, "content": parts })
        }
        2 => {
            let parts: Vec<Value> = (0..1 + rng.below(3))
                .map(|_| literal(rng, NATIVE_PARTS))
                .collect();
            let kind = rng.pick(&["user_input", "model_output", "custom", ""]);
            let role = rng.pick(&["user", "model", " Assistant ", "function", ""]);
            json!({ "type": kind, "role": role, "parts": parts })
        }
        3 => {
            let mut fields: Vec<(&str, Value)> = vec![
                ("type", json!("function_result")),
                ("name", name.into()),
                (rng.pick(&["call_id", "id"]), id.into()),
            ];
            if rng.chance(85) {
                fields.push(("result", literal(rng, RESULTS)));
            }
            if rng.chance(20) {
                fields.push(("is_error", json!(rng.chance(50))));
            }
            to_object(fields)
        }
        4 => {
            let mut fields: Vec<(&str, Value)> =
                vec![("type", json!("function_call")), ("name", name.into())];
            if rng.chance(80) {
                fields.push((rng.pick(&["call_id", "id"]), id.into()));
            }
            if rng.chance(80) {
                fields.push(("arguments", literal(rng, ARGUMENTS)));
            }
            if rng.chance(40) {
                let key = rng.pick(&["signature", "thought_signature", "thoughtSignature"]);
                fields.push((key, rng.pick(SIGNATURES).into()));
            }
            to_object(fields)
        }
        5 => {
            let key = rng.pick(&["signature", "thought_signature", "thoughtSignature"]);
            json!({ "type": "thought", key: rng.pick(SIGNATURES) })
        }
        6 => {
            let role = rng.pick(&["user", "model", "assistant", "other"]);
            let steps: Vec<Value> = (0..1 + rng.below(2))
                .map(|_| interactions_step(rng))
                .collect();
            json!({ "role": role, "steps": steps })
        }
        _ => {
            let kind = rng.pick(&["model_output", "other", ""]);
            json!({ "type": kind, "text": text(rng) })
        }
    }
}

/// Adds to an Interactions request what only these translators read.
fn interactions_corners(rng: &mut Rng, body: &mut Value) {
    let Value::Object(fields) = body else {
        return;
    };
    if rng.chance(35) {
        let key = rng.pick(&["generation_config", "generationConfig"]);
        let mut config = config(rng, 0);
        if rng.chance(30)
            && let Value::Object(config) = &mut config
        {
            let key = rng.pick(&["thinking_summaries", "thinkingSummaries"]);
            let summary = rng.pick(&["auto", " None ", "detailed", "concise", ""]);
            config.insert(key.to_owned(), summary.into());
        }
        if rng.chance(20)
            && let Value::Object(config) = &mut config
        {
            let key = rng.pick(&["tool_choice", "toolChoice"]);
            config.insert(key.to_owned(), literal(rng, TOOL_CHOICES));
        }
        if rng.chance(15)
            && let Value::Object(config) = &mut config
        {
            let key = rng.pick(&["response_modalities", "responseModalities"]);
            config.insert(key.to_owned(), json!([literal(rng, MODALITIES)]));
        }
        if rng.chance(5) {
            config = rng.pick(&[json!([1, 2]), json!(5), json!("str"), Value::Null]);
        }
        fields.insert(key.to_owned(), config);
    }
    if rng.chance(15) {
        fields.insert("tool_choice".to_owned(), literal(rng, TOOL_CHOICES));
    }
    if rng.chance(15) {
        let key = rng.pick(&["response_modalities", "responseModalities"]);
        let modalities: Value = if rng.chance(90) {
            (0..rng.below(4))
                .map(|_| literal(rng, MODALITIES))
                .collect()
        } else {
            json!("text")
        };
        fields.insert(key.to_owned(), modalities);
    }
    if rng.chance(25) {
        let extra: Vec<Value> = (0..1 + rng.below(3))
            .map(|_| literal(rng, INTERACTIONS_TOOLS))
            .collect();
        match fields.get_mut("tools") {
            Some(Value::Array(tools)) => tools.extend(extra),
            _ => {
                let tools = if rng.chance(90) {
                    Value::Array(extra)
                } else {
                    json!({ "googleSearch": {} })
                };
                fields.insert("tools".to_owned(), tools);
            }
        }
    }
    if rng.chance(10) {
        fields.insert("service_tier".to_owned(), literal(rng, SERVICE_TIERS));
    }
    if rng.chance(15) {
        let key = rng.pick(&["system_instruction", "systemInstruction"]);
        fields.insert(key.to_owned(), literal(rng, SYSTEM_INSTRUCTIONS));
    }
    if rng.chance(35) {
        let steps: Vec<Value> = (0..1 + rng.below(4))
            .map(|_| interactions_step(rng))
            .collect();
        match fields.get_mut("input") {
            Some(Value::Array(input)) => {
                for step in steps {
                    let at = rng.below(input.len() + 1);
                    input.insert(at, step);
                }
            }
            _ => {
                fields.insert("input".to_owned(), Value::Array(steps));
            }
        }
    }
}

/// Builds `count` random Interactions requests for a Gemini upstream, each
/// asking for a stream or not.
pub fn interactions_request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed.rotate_left(13), index);
            let model = rng.pick(MODELS).to_owned();
            let mut body = request(&mut rng);
            interactions_corners(&mut rng, &mut body);
            let body = render(&mut rng, &body);
            let stream = rng.chance(50);
            Case::new(
                format!("interactions-to-gemini-{seed}-{index}"),
                model,
                body,
            )
            .with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Gemini tools, under both names, and values that aren't tools.
const GEMINI_TOOLS: &[&str] = &[
    r#"{"googleSearch":{}}"#,
    r#"{"google_search":{"x":[]}}"#,
    r#"{"urlContext":{}}"#,
    r#"{"url_context":{}}"#,
    r#"{"codeExecution":{}}"#,
    r#"{"code_execution":{"a":1}}"#,
    r#"{"googleSearch":{"y":1},"google_search":{}}"#,
    r#"{"urlContext":5}"#,
    r#"{"functionDeclarations":[{"name":"get_weather","description":"d","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]}"#,
    r#"{"function_declarations":[{"name":"g","parametersJsonSchema":{"type":"object"}},{"description":"no name"}]}"#,
    r#"{"functionDeclarations":{"a":{"name":"h"}}}"#,
    r#"{"functionDeclarations":[{"name":7}],"googleSearch":{}}"#,
    r#"{"name":"f","description":{"a":1},"parametersJsonSchema":{"type":"object"}}"#,
    r#"{"other":1}"#,
    r#""text""#,
    "5",
];

const GEMINI_SYSTEM_INSTRUCTIONS: &[&str] = &[
    r#""be brief""#,
    r#"{"text":"be brief","parts":[{"text":"ignored"}]}"#,
    r#"{"text":5,"parts":[{"text":"a"},{"text":""},{"text":{"b": [1, 2]}},{"inlineData":{}},{"text":7}]}"#,
    r#"{"parts":[{"text":""}]}"#,
    r#"{"role":"user","parts":[{"text":"a"},{"text":"b"}]}"#,
    r#"{"parts":"x"}"#,
    "5",
    "null",
];

const MIME_TYPES: &[&str] = &[
    "image/png",
    "IMAGE/JPEG",
    "audio/wav",
    "video/mp4",
    "application/pdf",
    "text/plain",
    "",
];

/// A thought signature under one of the keys a Gemini part carries it, if
/// any.
fn signed(rng: &mut Rng, part: &mut Value) {
    let Value::Object(fields) = part else {
        return;
    };
    let signature = rng.pick(SIGNATURES);
    match rng.below(8) {
        0..=2 => {
            fields.insert("thoughtSignature".to_owned(), signature.into());
        }
        3 => {
            fields.insert("thought_signature".to_owned(), signature.into());
        }
        4 => {
            let padded = format!(" {signature} ");
            fields.insert(
                "extra_content".to_owned(),
                json!({ "google": { "thought_signature": padded } }),
            );
        }
        _ => {}
    }
}

/// A Gemini part of any kind, as a request's contents or a response's
/// candidate holds it.
fn gemini_part(rng: &mut Rng) -> Value {
    let name = rng.pick(TOOL_NAMES);
    let id = format!("call_{}", rng.below(100));
    let mut part = match rng.below(14) {
        0..=3 => {
            let mut part = json!({ "text": text(rng) });
            if rng.chance(30) {
                part["thought"] = rng.pick(&[json!(true), json!(true), json!("true"), json!(1)]);
            }
            part
        }
        4 => json!({ "text": "" }),
        5 => json!({}),
        6 | 7 => {
            let mut call = json!({ "name": name });
            match rng.below(4) {
                0 => call["id"] = id.into(),
                1 => call["call_id"] = id.into(),
                _ => {}
            }
            if rng.chance(85) {
                call["args"] = match rng.below(5) {
                    0..=2 => json!({ "city": text(rng), "n": num(rng.pick(NUMBERS)) }),
                    3 => json!("text"),
                    _ => odd_value(rng),
                };
            }
            json!({ "functionCall": call })
        }
        8 => {
            let mut response = json!({ "name": name });
            match rng.below(3) {
                0 => response["id"] = id.into(),
                1 => response["call_id"] = id.into(),
                _ => {}
            }
            if rng.chance(85) {
                response["response"] = match rng.below(3) {
                    0 => json!({ "result": text(rng) }),
                    1 => text(rng).into(),
                    _ => odd_value(rng),
                };
            }
            json!({ "functionResponse": response })
        }
        9 | 10 => {
            let mime_type = rng.pick(MIME_TYPES);
            match rng.below(4) {
                0 => json!({ "inline_data": { "mime_type": mime_type, "data": "AA==" } }),
                1 => json!({ "inlineData": { "mime_type": mime_type, "data": "AQ==" } }),
                2 => json!({ "inlineData": { "mimeType": mime_type } }),
                _ => json!({ "inlineData": { "mimeType": mime_type, "data": "Ag==" } }),
            }
        }
        11 => json!({ "fileData": { "mimeType": rng.pick(MIME_TYPES), "fileUri": "gs://a" } }),
        12 => json!({ "text": odd_value(rng) }),
        _ => json!({ "executableCode": { "code": "x" } }),
    };
    if rng.chance(30) {
        signed(rng, &mut part);
    }
    part
}

/// A Gemini generation config in camelCase, with a thinking config.
fn gemini_config(rng: &mut Rng) -> Value {
    let mut config = config(rng, 0);
    let Value::Object(fields) = &mut config else {
        return config;
    };
    if rng.chance(50) {
        let level = rng.pick(&["low", "HIGH", " Medium ", "minimal", ""]);
        let thinking = if rng.chance(70) {
            let mut thinking = json!({ "thinkingLevel": level });
            if rng.chance(50) {
                thinking["thinkingBudget"] = token_count(rng);
            }
            if rng.chance(50) {
                thinking["includeThoughts"] = rng.pick(&[json!(true), json!(false), json!("yes")]);
            }
            thinking
        } else {
            json!({ "thinking_level": level, "include_thoughts": rng.chance(50), "thinking_budget": token_count(rng) })
        };
        let key = rng.pick(&["thinkingConfig", "thinking_config"]);
        fields.insert(key.to_owned(), thinking);
    }
    if rng.chance(20) {
        let summary = rng.pick(&["auto", "detailed", "none"]);
        fields.insert("thinkingSummaries".to_owned(), summary.into());
    }
    config
}

/// A Gemini request of our own.
fn gemini_request(rng: &mut Rng) -> Value {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(92) {
        let contents: Vec<Value> = (0..rng.below(5))
            .map(|_| {
                let role = rng.pick(&["user", "model", "function", "", " model", "assistant"]);
                let parts: Vec<Value> = (0..1 + rng.below(4)).map(|_| gemini_part(rng)).collect();
                if rng.chance(5) {
                    json!({ "role": role })
                } else {
                    json!({ "role": role, "parts": parts })
                }
            })
            .collect();
        let contents = if rng.chance(5) {
            json!({ "a": contents.first().cloned().unwrap_or_default() })
        } else {
            Value::Array(contents)
        };
        fields.push(("contents", contents));
    }
    if rng.chance(35) {
        let key = rng.pick(&["systemInstruction", "system_instruction"]);
        fields.push((key, literal(rng, GEMINI_SYSTEM_INSTRUCTIONS)));
    }
    if rng.chance(40) {
        let tools: Value = if rng.chance(95) {
            (0..1 + rng.below(3))
                .map(|_| literal(rng, GEMINI_TOOLS))
                .collect()
        } else {
            json!({ "googleSearch": {} })
        };
        fields.push(("tools", tools));
    }
    if rng.chance(50) {
        let key = rng.pick(&["generationConfig", "generation_config"]);
        fields.push((key, gemini_config(rng)));
    }
    if rng.chance(10) {
        fields.push((
            "toolConfig",
            json!({ "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": ["f"] } }),
        ));
    }
    if rng.chance(15) {
        fields.push(("model", rng.pick(MODELS).into()));
    }
    rng.shuffle(&mut fields);
    to_object(fields)
}

/// Builds `count` random Gemini requests for an Interactions upstream, each
/// asking for a stream or not: half the Gemini request generator's, half our
/// own.
pub fn gemini_request_cases(seed: u64, count: usize) -> Vec<Case> {
    let theirs = to_gemini::request_cases(seed.rotate_left(17), count);
    theirs
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut rng = rng(seed.rotate_left(19), index as u64);
            let name = format!("gemini-to-interactions-{seed}-{index}");
            if rng.chance(50) {
                return Case { name, ..case };
            }
            let model = rng.pick(MODELS).to_owned();
            let body = gemini_request(&mut rng);
            let body = render(&mut rng, &body);
            let stream = rng.chance(50);
            Case::new(name, model, body).with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Gemini usage, under either name, with any of its counts.
fn gemini_usage(rng: &mut Rng) -> (&'static str, Value) {
    if rng.chance(10) {
        return (
            "usageMetadata",
            json!({ "trafficType": "PROVISIONED_THROUGHPUT" }),
        );
    }
    let snake = rng.chance(25);
    let keys: [(&str, &str); 5] = [
        ("promptTokenCount", "prompt_token_count"),
        ("candidatesTokenCount", "candidates_token_count"),
        ("totalTokenCount", "total_token_count"),
        ("thoughtsTokenCount", "thoughts_token_count"),
        ("cachedContentTokenCount", "cached_content_token_count"),
    ];
    let mut fields = Vec::new();
    for (camel, snake_key) in keys {
        if rng.chance(70) {
            let key = if snake { snake_key } else { camel };
            fields.push((key, token_count(rng)));
        }
    }
    let key = if snake {
        "usage_metadata"
    } else {
        "usageMetadata"
    };
    (key, to_object(fields))
}

const FINISH_REASONS: &[&str] = &[
    "STOP",
    "MAX_TOKENS",
    "SAFETY",
    "MALFORMED_FUNCTION_CALL",
    "",
];

/// A Gemini chunk or response holding `parts`, and the rest given.
fn gemini_chunk(
    parts: Vec<Value>,
    finish: Option<&str>,
    usage: Option<(&str, Value)>,
    response_id: Option<&str>,
) -> Value {
    let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
    if let Some(reason) = finish {
        candidate["finishReason"] = reason.into();
    }
    let mut chunk = json!({ "candidates": [candidate] });
    if let Some((key, usage)) = usage {
        chunk[key] = usage;
    }
    if let Some(id) = response_id {
        chunk["responseId"] = id.into();
        chunk["modelVersion"] = json!("gemini-2.5-flash");
    }
    chunk
}

/// A Gemini stream of our own, and the same response whole.
fn gemini_response(rng: &mut Rng) -> (Vec<String>, String) {
    let parts: Vec<Value> = (0..rng.below(7)).map(|_| gemini_part(rng)).collect();
    let response_id = rng.chance(70).then(|| format!("resp_{}", rng.below(1000)));
    let finish = rng.chance(80).then(|| rng.pick(FINISH_REASONS));
    let usage = rng.chance(75).then(|| gemini_usage(rng));
    // Where the usage comes: with the finish reason, in a chunk of its own
    // after it, or early, in every chunk.
    let usage_at = rng.below(3);
    let mut chunks: Vec<Value> = Vec::new();
    let mut rest = parts.clone();
    while !rest.is_empty() || chunks.is_empty() {
        let take = (1 + rng.below(3)).min(rest.len());
        let these: Vec<Value> = rest.drain(..take).collect();
        let early = if usage_at == 2 { usage.clone() } else { None };
        chunks.push(gemini_chunk(these, None, early, response_id.as_deref()));
    }
    if let (Some(last), Some(reason)) = (chunks.last_mut(), finish) {
        last["candidates"][0]["finishReason"] = reason.into();
        if usage_at == 0
            && let Some((key, usage)) = &usage
        {
            last[*key] = usage.clone();
        }
    }
    if usage_at == 1
        && let Some((key, usage)) = &usage
    {
        let mut chunk = json!({ "candidates": [] });
        chunk[*key] = usage.clone();
        chunks.push(chunk);
    }
    let mut lines: Vec<String> = chunks
        .iter()
        .map(|chunk| {
            let line = render(rng, chunk);
            if rng.chance(10) {
                format!("data: {}", chunk)
            } else {
                line
            }
        })
        .collect();
    if rng.chance(8) {
        let at = rng.below(lines.len() + 1);
        lines.insert(
            at,
            rng.pick(&["", " ", "not json", ": comment", "{}"])
                .to_owned(),
        );
    }
    if rng.chance(50) {
        lines.push(
            rng.pick(&["[DONE]", " [DONE]\n", "data: [DONE]"])
                .to_owned(),
        );
        if rng.chance(10) {
            lines.push(rng.pick(&["[DONE]", "{\"candidates\":[]}"]).to_owned());
        }
    }
    let whole = gemini_chunk(parts, finish, usage, response_id.as_deref());
    let body = match rng.below(50) {
        0 => "not json".to_owned(),
        1 => String::new(),
        _ => render(rng, &whole),
    };
    (lines, body)
}

/// Builds `count` random Gemini streams for an Interactions client, and a
/// non-streaming case from a whole response for each: most our own, some
/// the Gemini response generator's.
pub fn gemini_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let (streams, finals) = to_gemini::event_cases(seed.rotate_left(23), count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (stream, last))| {
            let mut rng = rng(seed.rotate_left(29), index as u64);
            let model = rng.pick(MODELS).to_owned();
            let (events, body) = if rng.chance(30) {
                (stream.events, last.events)
            } else {
                let (events, body) = gemini_response(&mut rng);
                (events, vec![body])
            };
            let name = format!("gemini-to-interactions-{seed}-{index}");
            let case = |suffix: &str, events| Case {
                model: model.clone(),
                ..Case::response(format!("{name}-{suffix}"), "{}", events)
            };
            (case("stream", events), case("final", body))
        })
        .unzip()
}

const ERROR_CODES: &[&str] = &[
    r#""400""#,
    r#"" UNAUTHENTICATED ""#,
    r#""permission_denied""#,
    r#""not_found""#,
    r#""rate_limit_exceeded""#,
    r#""resource_exhausted""#,
    r#""canceled""#,
    r#""cancelled""#,
    r#""unavailable""#,
    r#""deadline_exceeded""#,
    r#""internal""#,
    r#""418""#,
    r#""599""#,
    r#""600""#,
    r#""+503""#,
    r#""  ""#,
    "502",
    "null",
    r#"{"a":1}"#,
];

/// An Interactions event only these translators read something special in.
fn interactions_event(rng: &mut Rng) -> Value {
    let index = json!(rng.below(6));
    match rng.below(10) {
        0 => {
            let mut step = json!({ "type": "function_call", "name": rng.pick(TOOL_NAMES) });
            let id = format!("call_{}", rng.below(100));
            let key = rng.pick(&["id", "call_id"]);
            step[key] = id.into();
            if rng.chance(60) {
                let key = rng.pick(&["signature", "thought_signature", "thoughtSignature"]);
                step[key] = rng.pick(SIGNATURES).into();
            }
            json!({ "event_type": "step.start", "index": index, "step": step })
        }
        1 | 2 => {
            let arguments = literal(rng, ARGUMENTS);
            json!({
                "event_type": "step.delta",
                "index": index,
                "delta": { "type": "arguments_delta", "arguments": arguments },
            })
        }
        3 => {
            let key = rng.pick(&["signature", "thought_signature", "thoughtSignature"]);
            let index = if rng.chance(20) { json!("2") } else { index };
            json!({
                "event_type": "step.delta",
                "index": index,
                "delta": { "type": "thought_signature", key: rng.pick(SIGNATURES) },
            })
        }
        4 => {
            let delta = match rng.below(4) {
                0 => json!({ "type": "text", "content": { "text": text(rng) } }),
                1 => json!({ "type": "text", "text": " ", "content": { "text": text(rng) } }),
                2 => json!({ "type": "thought_summary", "text": text(rng) }),
                _ => json!({ "type": "thought_summary", "content": { "text": odd_value(rng) } }),
            };
            json!({ "event_type": "step.delta", "delta": delta })
        }
        5 => {
            let usage = json!({
                rng.pick(&["total_input_tokens", "input_tokens"]): token_count(rng),
                rng.pick(&["total_output_tokens", "output_tokens"]): token_count(rng),
                "total_thought_tokens": token_count(rng),
            });
            json!({ "event_type": "finish", "metadata": { "total_usage": usage } })
        }
        6 | 7 => {
            let error = json!({ "message": text(rng), "code": literal(rng, ERROR_CODES) });
            match rng.below(4) {
                0 => json!({ "event_type": "response.failed", "error": error }),
                1 => {
                    json!({ "event_type": "interaction.failed", "interaction": { "error": error } })
                }
                2 => {
                    json!({ "event_type": "interaction.failed", "code": "401", "error": { "status": "UNAVAILABLE" } })
                }
                _ => json!({ "event_type": "response.failed" }),
            }
        }
        8 => {
            let interaction = json!({
                "id": format!("interaction_{}", rng.below(100)),
                "model": rng.pick(MODELS),
                "service_tier": literal(rng, SERVICE_TIERS),
                "usage": { "input_tokens": token_count(rng), "cached_tokens": token_count(rng) },
            });
            json!({ "event_type": "interaction.completed", "interaction": interaction })
        }
        _ => json!({ "event_type": "interaction.status_update", "status": "in_progress" }),
    }
}

/// An Interactions response only these translators read something special
/// in.
fn interactions_body(rng: &mut Rng) -> Value {
    let steps: Vec<Value> = (0..rng.below(5))
        .map(|_| {
            let name = rng.pick(TOOL_NAMES);
            match rng.below(5) {
                0 => {
                    let key = rng.pick(&["arguments", "args"]);
                    json!({ "type": "function_call", "name": name, "call_id": "c1", key: literal(rng, ARGUMENTS) })
                }
                1 => {
                    let key = rng.pick(&["result", "response"]);
                    json!({ "type": "function_result", "name": name, "id": "c1", key: literal(rng, RESULTS) })
                }
                2 => json!({ "type": rng.pick(&["thought", "model_output"]), "content": text(rng) }),
                3 => json!({ "type": "model_output", "content": literal(rng, CONTENT_PARTS) }),
                _ => json!({ "type": "other", "content": [literal(rng, CONTENT_PARTS)] }),
            }
        })
        .collect();
    let mut body = json!({ "steps": steps });
    if rng.chance(70) {
        body["id"] = json!(format!("interaction_{}", rng.below(100)));
    }
    if rng.chance(50) {
        body["model"] = rng.pick(MODELS).into();
    }
    if rng.chance(30) {
        body["service_tier"] = literal(rng, SERVICE_TIERS);
    }
    if rng.chance(50) {
        body["usage"] = json!({
            rng.pick(&["total_input_tokens", "input_tokens"]): token_count(rng),
            rng.pick(&["total_output_tokens", "output_tokens"]): token_count(rng),
            rng.pick(&["total_cached_tokens", "cached_tokens"]): token_count(rng),
        });
    }
    if rng.chance(20) {
        json!({ "interaction": body })
    } else {
        body
    }
}

/// `event` as a line, a `data:` line or an SSE frame.
fn event_line(rng: &mut Rng, event: &Value) -> String {
    match rng.below(10) {
        0 => format!("data: {event}"),
        1 => format!(
            "event: {}\ndata: {event}",
            event["event_type"].as_str().unwrap_or("")
        ),
        _ => event.to_string(),
    }
}

/// The counts the Interactions → Gemini translators add up for a total
/// when none is given.
const SUMMED_COUNTS: &[&str] = &[
    "input_tokens",
    "total_input_tokens",
    "output_tokens",
    "total_output_tokens",
];

/// Replaces, in every object of `value` without a `total_tokens`, the
/// [`SUMMED_COUNTS`] out of int64's range, and says whether it did.
fn sum_in_range(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(fields) => {
            let summed = !fields.contains_key("total_tokens");
            for (key, field) in fields.iter_mut() {
                let out_of_range = |number: &serde_json::Number| {
                    number.as_i64().is_none()
                        && number.as_f64().is_some_and(|float| {
                            !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0)
                                .contains(&float)
                        })
                };
                match field {
                    Value::Number(number)
                        if summed
                            && SUMMED_COUNTS.contains(&key.as_str())
                            && out_of_range(number) =>
                    {
                        *field = num("9007199254740993");
                        changed = true;
                    }
                    _ => changed |= sum_in_range(field),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= sum_in_range(item);
            }
        }
        _ => {}
    }
    changed
}

/// `event`, a line, `data:` line or SSE frame, with the counts the
/// translators would add up kept in int64's range: Go's int64 of a float
/// beyond it depends on the CPU, and the port saturates (see its
/// deviations), which compares alike for a count alone but not for a sum.
fn summed_in_range(event: &str) -> String {
    let at = match event.find("data: ") {
        Some(at) if at == 0 || event.starts_with("event: ") => at + "data: ".len(),
        _ => 0,
    };
    let (prefix, data) = event.split_at(at);
    let Ok(mut value) = serde_json::from_str::<Value>(data) else {
        return event.to_owned();
    };
    if sum_in_range(&mut value) {
        format!("{prefix}{value}")
    } else {
        event.to_owned()
    }
}

/// Builds `count` random Interactions streams for a Gemini client, and a
/// non-streaming case for each: the parent module's, with events and
/// responses only these translators read something special in.
pub fn interactions_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let (streams, finals) = event_cases(seed.rotate_left(31), count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (mut stream, mut last))| {
            let mut rng = rng(seed.rotate_left(37), index as u64);
            if rng.chance(50) {
                for _ in 0..1 + rng.below(4) {
                    let event = interactions_event(&mut rng);
                    let line = event_line(&mut rng, &event);
                    let at = rng.below(stream.events.len() + 1);
                    stream.events.insert(at, line);
                }
            }
            if rng.chance(10) {
                let at = rng.below(stream.events.len() + 1);
                let odd = rng.pick(&["", "  \n", ": comment", "event: step.delta\n", "not json"]);
                stream.events.insert(at, odd.to_owned());
            }
            if rng.chance(35) {
                let body = interactions_body(&mut rng);
                last.events = vec![render(&mut rng, &body)];
            } else if rng.chance(3) {
                last.events = vec![rng.pick(&["not json", ""]).to_owned()];
            }
            for event in stream.events.iter_mut().chain(&mut last.events) {
                *event = summed_in_range(event);
            }
            let name = format!("interactions-to-gemini-{seed}-{index}");
            stream.name = format!("{name}-stream");
            last.name = format!("{name}-final");
            (stream, last)
        })
        .unzip()
}

/// Builds `count` random Interactions requests to pass through, some not
/// objects.
pub fn passthrough_request_cases(seed: u64, count: usize) -> Vec<Case> {
    request_cases(seed.rotate_left(41), count)
        .into_iter()
        .enumerate()
        .map(|(index, mut case)| {
            let mut rng = rng(seed.rotate_left(43), index as u64);
            if rng.chance(5) {
                case.request = rng.pick(&["[1]", "\"x\"", "5", "null"]).to_owned();
            }
            case.name = format!("interactions-passthrough-{seed}-{index}");
            let stream = rng.chance(50);
            case.with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Builds `count` random Interactions streams to pass through, with empty
/// lines among them, and a non-streaming case for each, some not JSON.
pub fn passthrough_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let (streams, finals) = event_cases(seed.rotate_left(47), count);
    streams
        .into_iter()
        .zip(finals)
        .enumerate()
        .map(|(index, (mut stream, mut last))| {
            let mut rng = rng(seed.rotate_left(53), index as u64);
            if rng.chance(30) {
                let at = rng.below(stream.events.len() + 1);
                let odd = rng.pick(&["", " ", "\n", "not json"]);
                stream.events.insert(at, odd.to_owned());
            }
            if rng.chance(10) {
                let odd = match rng.below(3) {
                    0 => "not json".to_owned(),
                    1 => String::new(),
                    _ => format!("{} trailing", last.events.concat()),
                };
                last.events = vec![odd];
            }
            let name = format!("interactions-passthrough-{seed}-{index}");
            stream.name = format!("{name}-stream");
            last.name = format!("{name}-final");
            (stream, last)
        })
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every generator's cases for `seed`.
    fn all(seed: u64, count: usize) -> Vec<Vec<Case>> {
        let (gemini_streams, gemini_finals) = gemini_event_cases(seed, count);
        let (interactions_streams, interactions_finals) = interactions_event_cases(seed, count);
        let (passthrough_streams, passthrough_finals) = passthrough_event_cases(seed, count);
        vec![
            interactions_request_cases(seed, count),
            gemini_request_cases(seed, count),
            passthrough_request_cases(seed, count),
            gemini_streams,
            gemini_finals,
            interactions_streams,
            interactions_finals,
            passthrough_streams,
            passthrough_finals,
        ]
    }

    #[test]
    fn cases_are_reproducible_and_requests_are_json() {
        let (first, again) = (all(5, 150), all(5, 150));
        for (cases, repeated) in first.iter().zip(&again) {
            assert_eq!(cases.len(), 150);
            let mut names = std::collections::HashSet::new();
            for (a, b) in cases.iter().zip(repeated) {
                assert_eq!(
                    (&a.name, &a.model, &a.request, &a.events, &a.options),
                    (&b.name, &b.model, &b.request, &b.events, &b.options)
                );
                assert!(names.insert(a.name.clone()), "{}", a.name);
                serde_json::from_str::<Value>(&a.request).expect("a request is JSON");
            }
        }
    }

    #[test]
    fn no_model_names_antigravity() {
        for case in all(9, 100).into_iter().flatten() {
            let texts = [case.model, case.request].into_iter().chain(case.events);
            for text in texts {
                assert!(!text.to_lowercase().contains("antigravity"), "{text}");
            }
        }
    }

    #[test]
    fn summed_counts_stay_in_range() {
        let cases = [
            (
                r#"data: {"usage":{"input_tokens":1e30,"output_tokens":-123456789012345678901234567890}}"#,
                r#"data: {"usage":{"input_tokens":9007199254740993,"output_tokens":9007199254740993}}"#,
            ),
            (
                "event: finish\ndata: {\"metadata\":{\"total_usage\":{\"total_output_tokens\":1e30}}}",
                "event: finish\ndata: {\"metadata\":{\"total_usage\":{\"total_output_tokens\":9007199254740993}}}",
            ),
            (
                r#"{"usage":{"input_tokens":1e30,"total_tokens":1,"cached_tokens":1e30}}"#,
                r#"{"usage":{"input_tokens":1e30,"total_tokens":1,"cached_tokens":1e30}}"#,
            ),
            (
                r#"{"usage":{"input_tokens":9223372036854775807,"output_tokens":1e3}}"#,
                r#"{"usage":{"input_tokens":9223372036854775807,"output_tokens":1e3}}"#,
            ),
            (r#"{"text":"data: 1e30"}"#, r#"{"text":"data: 1e30"}"#),
            ("not json", "not json"),
        ];
        for (event, want) in cases {
            assert_eq!(summed_in_range(event), want, "{event}");
        }
    }

    #[test]
    fn config_keys_are_not_queries() {
        for key in CONFIG_KEYS {
            assert!(!key.contains(['|', '#', '@', '*', '?']), "{key}");
        }
    }
}
